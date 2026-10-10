#!/usr/bin/python3
"""Read-only Xen/Qubes collector with a persistent sys-net counter reader.

Only this service needs Xen privileges. The panel reads an atomic numeric snapshot.
All memory quantities are KiB. Guest reports are untrusted, bounded integers.
"""
import argparse
import logging
import math
import os
from pathlib import Path
import tempfile
import select
import shlex
import signal
import subprocess
import threading
import time


def number(raw):
    if isinstance(raw, bytes):
        try:
            raw = raw.decode("ascii")
        except UnicodeDecodeError:
            return None
    if not isinstance(raw, str) or not 1 <= len(raw) <= 20:
        return None
    if not raw.isascii() or not raw.isdecimal():
        return None
    value = int(raw)
    return value if value <= 2**63 - 1 else None


def physical_used(demand, swap, allocated):
    """meminfo = resident non-cache usage + swap; swapinfo = used swap.

    Never assume zero swap when swapinfo is absent (older agents). Reports
    are thresholded separately; allow small skew by clamping to allocation.
    """
    demand, swap = number(demand), number(swap)
    if demand is None or swap is None or demand < swap:
        return None
    return min(demand - swap, allocated)


def dom0_used(path=Path("/proc/meminfo")):
    fields = {}
    for line in path.read_text().splitlines():
        key, _, value = line.partition(":")
        if value.split():
            fields[key] = int(value.split()[0])
    # Same resident-memory definition as Qubes' meminfo-writer, without swap.
    return max(0, fields["MemTotal"] - fields["MemFree"]
               - fields["Buffers"] - fields["Cached"])


class CpuTracker:
    def __init__(self):
        self.previous = {}
        self.time = None

    def update(self, domains, now, cpus):
        current = {}
        delta = 0
        for domain in domains:
            # Identity includes Xen's UUID handle: domids are reused after shutdown.
            key = (domain["domid"], tuple(domain.get("handle", ())))
            counter = domain["cpu_time"]
            current[key] = counter
            if key in self.previous:
                delta += max(0, counter - self.previous[key])
        elapsed = None if self.time is None else now - self.time
        self.previous, self.time = current, now
        if elapsed is None or elapsed <= 0 or cpus <= 0:
            return None
        return min(100.0, max(0.0, delta / 1e9 / elapsed / cpus * 100.0))


# Executed through the existing qubes.VMShell service. No guest installation.
NETWORK_SCRIPT = r"""
import pathlib, time
boot = pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()
while True:
    rows = []
    for dev in sorted(pathlib.Path('/sys/class/net').iterdir()):
        if dev.name == 'lo' or dev.name.startswith('vif') or not (dev / 'device').exists():
            continue
        if '/virtual/' in str(dev.resolve()):
            continue
        try:
            idx = int((dev / 'ifindex').read_text())
            rx = int((dev / 'statistics/rx_bytes').read_text())
            tx = int((dev / 'statistics/tx_bytes').read_text())
            rows.append(f'{idx}:{rx}:{tx}')
        except (OSError, ValueError):
            continue
    print('N1', time.monotonic_ns(), boot, ','.join(rows) or '-', flush=True)
    time.sleep(1)
"""


def parse_network(line):
    if len(line) > 8192:
        raise ValueError('Oversized network sample')
    parts = line.decode('ascii').split()
    if len(parts) != 4 or parts[0] != 'N1':
        raise ValueError('Invalid network sample')
    stamp = number(parts[1])
    boot = parts[2]
    if stamp is None or len(boot) != 36 or any(x not in '0123456789abcdef-' for x in boot):
        raise ValueError('Invalid network identity or timestamp')
    counters = {}
    if parts[3] != '-':
        for item in parts[3].split(','):
            fields = item.split(':')
            if len(fields) != 3:
                raise ValueError('Invalid network counters')
            idx, rx, tx = map(number, fields)
            if None in (idx, rx, tx) or idx == 0 or idx in counters or len(counters) >= 64:
                raise ValueError('Invalid network interface')
            counters[idx] = (rx, tx)
    return stamp, boot, counters


class NetworkTracker:
    def __init__(self):
        self.previous = None

    def update(self, line):
        current = parse_network(line)
        previous, self.previous = self.previous, current
        stamp, boot, counters = current
        if previous is None or not counters:
            return None
        old_stamp, old_boot, old = previous
        if boot != old_boot or counters.keys() != old.keys() or stamp <= old_stamp:
            return None
        elapsed = (stamp - old_stamp) / 1e9
        if not 0.1 <= elapsed <= 5:
            return None
        rx = tx = 0
        for idx, (r, t) in counters.items():
            before_r, before_t = old[idx]
            if r < before_r or t < before_t:
                return None
            rx += r - before_r
            tx += t - before_t
        rates = (rx / elapsed, tx / elapsed)
        if any(v > 1e15 for v in rates):
            raise ValueError('Implausible network rate')
        return rates


class NetworkReader:
    """Nonblocking for Xen collection; bounded reads and reconnect after failure."""
    def __init__(self, qube='sys-net'):
        self.qube = qube
        self.stop_event = threading.Event()
        self.lock = threading.Lock()
        self.latest = None
        self.thread = threading.Thread(target=self.run, daemon=True)

    def start(self):
        self.thread.start()

    def rates(self):
        with self.lock:
            if self.latest is None or time.monotonic() - self.latest[0] > 5:
                return None
            return self.latest[1]

    def close(self):
        self.stop_event.set()
        self.thread.join(timeout=8)

    def run(self):
        command = 'python3 -u -c ' + shlex.quote(NETWORK_SCRIPT)
        while not self.stop_event.is_set():
            proc = None
            try:
                proc = subprocess.Popen(
                    ['qvm-run', '--pass-io', '--no-gui', '--no-autostart', self.qube, command],
                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL, start_new_session=True)
                tracker, buffer = NetworkTracker(), b''
                last = time.monotonic()
                budget, budget_time = 8.0, last
                while not self.stop_event.is_set():
                    if time.monotonic() - last > 5:
                        raise TimeoutError('Network reporter stopped responding')
                    ready, _, _ = select.select([proc.stdout], [], [], 0.25)
                    if not ready:
                        continue
                    chunk = os.read(proc.stdout.fileno(), 8193)
                    if not chunk:
                        break
                    buffer += chunk
                    if len(buffer) > 8192:
                        raise ValueError('Oversized network stream')
                    while b'\n' in buffer:
                        line, buffer = buffer.split(b'\n', 1)
                        now = time.monotonic()
                        budget = min(8.0, budget + (now - budget_time) * 4.0) - 1
                        budget_time = now
                        if budget < 0:
                            raise ValueError('Network stream exceeds sample rate limit')
                        rate = tracker.update(line)
                        last = time.monotonic()
                        with self.lock:
                            self.latest = (last, rate)
            except (OSError, ValueError, UnicodeError, TimeoutError):
                logging.warning('Network source %s unavailable; retrying', self.qube)
            finally:
                with self.lock:
                    self.latest = None
                if proc is not None:
                    try:
                        os.killpg(proc.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        try:
                            os.killpg(proc.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        proc.wait()
                    proc.stdout.close()
            self.stop_event.wait(3)


class Collector:
    def __init__(self, xc, xs, network=None):
        self.xc, self.xs = xc, xs
        self.network = network
        self.cpu = CpuTracker()

    def sample(self, period):
        host = self.xc.physinfo()
        domains = self.xc.domain_getinfo(0, 65536)
        # Raw domains include stubdomains exactly once; do not add Qubes aggregates.
        domains = [d for d in domains if not d.get("dying", False)]
        cpu = self.cpu.update(domains, time.monotonic(), host["nr_cpus"])
        total = int(host["total_memory"])
        allocated = used = unknown = 0
        for domain in domains:
            allocation = int(domain["mem_kb"])
            if allocation < 0:
                raise ValueError("Negative Xen allocation")
            allocated += allocation
            usage = None
            if domain["domid"] == 0:
                usage = min(dom0_used(), allocation)
            else:
                base = f"/local/domain/{int(domain['domid'])}/memory"
                # One XenStore read transaction gives a consistent pair of keys.
                tx = self.xs.transaction_start()
                try:
                    usage = physical_used(self.xs.read(tx, base + "/meminfo"),
                                          self.xs.read(tx, base + "/swapinfo"), allocation)
                finally:
                    self.xs.transaction_end(tx, True)
            if usage is None:
                unknown += allocation
            else:
                used += usage
        if not 0 < total or allocated > total:
            raise ValueError("Inconsistent Xen memory snapshot; retry next interval")
        free = max(0, int(host["free_memory"]))
        overhead = max(0, total - allocated - free)
        cpu_text = "unknown" if cpu is None else f"{cpu:.4f}"
        rates = self.network.rates() if self.network else None
        net = "unknown unknown" if rates is None else f"{rates[0]:.3f} {rates[1]:.3f}"
        return (f"MQ2 {time.time():.6f} {cpu_text} {total} {allocated} "
                f"{used} {unknown} {overhead} {period} {net}\n")


def publish(path, text):
    fd, name = tempfile.mkstemp(prefix=".metrics-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            stream.write(text)
            os.fchmod(stream.fileno(), 0o644)
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true", help="Print a snapshot and exit")
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--output", type=Path, default=Path("/run/multiload-qubes/metrics"))
    parser.add_argument("--network-qube", default="sys-net", help="Qube owning the physical network interfaces")
    parser.add_argument("--no-network", action="store_true")
    args = parser.parse_args()
    if not args.network_qube or args.network_qube.startswith("-") or not all(x.isascii() and (x.isalnum() or x in "-_") for x in args.network_qube):
        parser.error("invalid network qube name")
    if not math.isfinite(args.interval) or not 0.25 <= args.interval <= 10:
        parser.error("interval must be between 0.25 and 10 seconds")
    # Imported here so pure accounting tests also run on non-Xen dev machines.
    try:
        import xen.lowlevel.xc
        import xen.lowlevel.xs
        collector = Collector(xen.lowlevel.xc.xc(), xen.lowlevel.xs.xs())
    except (ImportError, OSError, RuntimeError) as error:
        parser.exit(1, f"Xen collector requires dom0 Xen access: {error}\n")
    if args.once:
        print(collector.sample(args.interval), end="")
        return
    args.output.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    logging.basicConfig(level=logging.INFO)
    network = None if args.no_network else NetworkReader(args.network_qube)
    def stop(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    collector.network = network
    if network:
        network.start()
    try:
        while True:
            start = time.monotonic()
            try:
                publish(args.output, collector.sample(args.interval))
            except Exception:
                # Leave the timestamp intact so the panel flags stale Xen data.
                logging.exception("Cannot collect Xen metrics")
            time.sleep(max(0, args.interval - (time.monotonic() - start)))
    except KeyboardInterrupt:
        pass
    finally:
        if network:
            network.close()


if __name__ == "__main__":
    main()
