#!/usr/bin/python3
"""Read-only Xen/Qubes collector with persistent guest memory and network readers.

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


def memory_used(total, available):
    total, available = number(total), number(available)
    if total is None or available is None or total == 0 or available > total:
        raise ValueError('Invalid memory totals')
    return total - available


def dom0_used(path=Path("/proc/meminfo")):
    fields = {}
    for line in path.read_text().splitlines():
        key, _, value = line.partition(":")
        if value.split():
            fields[key] = value.split()[0]
    return memory_used(fields.get('MemTotal'), fields.get('MemAvailable'))


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


MEMORY_SCRIPT = r"""
import pathlib, subprocess, time
domid = subprocess.check_output(['xenstore-read', 'domid'], text=True).strip()
while True:
    fields = {}
    for line in pathlib.Path('/proc/meminfo').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('MemTotal', 'MemAvailable'):
            fields[key] = int(value.split()[0])
    print('M1', domid, fields['MemTotal'], fields['MemAvailable'], flush=True)
    time.sleep(1)
"""


class MemoryTracker:
    def __init__(self, domid):
        self.domid = domid

    def update(self, line):
        if len(line) > 256:
            raise ValueError('Oversized memory sample')
        fields = line.decode('ascii').split()
        if len(fields) != 4 or fields[0] != 'M1' or number(fields[1]) != self.domid:
            raise ValueError('Invalid memory sample or changed domain')
        return memory_used(fields[2], fields[3])


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


class GuestReader:
    """Nonblocking for Xen collection; bounded reads and reconnect after failure."""
    def __init__(self, qube, script, tracker_factory):
        self.qube = qube
        self.script = script
        self.tracker_factory = tracker_factory
        self.stop_event = threading.Event()
        self.lock = threading.Lock()
        self.latest = None
        self.thread = threading.Thread(target=self.run, name=f"metrics-{qube}", daemon=True)

    def start(self):
        self.thread.start()

    def value(self):
        with self.lock:
            if self.latest is None or time.monotonic() - self.latest[0] > 5:
                return None
            return self.latest[1]

    def stop(self):
        self.stop_event.set()

    def close(self):
        self.stop()
        self.thread.join(timeout=8)

    def run(self):
        command = 'python3 -u -c ' + shlex.quote(self.script)
        while not self.stop_event.is_set():
            proc = None
            try:
                proc = subprocess.Popen(
                    ['qvm-run', '--pass-io', '--no-gui', '--no-autostart', self.qube, command],
                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL, start_new_session=True)
                tracker, buffer = self.tracker_factory(), b''
                last = time.monotonic()
                budget, budget_time = 8.0, last
                while not self.stop_event.is_set():
                    if time.monotonic() - last > 5:
                        raise TimeoutError('Guest reporter stopped responding')
                    ready, _, _ = select.select([proc.stdout], [], [], 0.25)
                    if not ready:
                        continue
                    chunk = os.read(proc.stdout.fileno(), 8193)
                    if not chunk:
                        break
                    buffer += chunk
                    if len(buffer) > 8192:
                        raise ValueError('Oversized guest stream')
                    while b'\n' in buffer:
                        line, buffer = buffer.split(b'\n', 1)
                        now = time.monotonic()
                        budget = min(8.0, budget + (now - budget_time) * 4.0) - 1
                        budget_time = now
                        if budget < 0:
                            raise ValueError('Guest stream exceeds sample rate limit')
                        rate = tracker.update(line)
                        last = time.monotonic()
                        with self.lock:
                            self.latest = (last, rate)
            except (OSError, ValueError, UnicodeError, TimeoutError):
                logging.warning('Guest source %s unavailable; retrying', self.qube)
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


class NetworkReader(GuestReader):
    def __init__(self, qube='sys-net'):
        super().__init__(qube, NETWORK_SCRIPT, NetworkTracker)

    def rates(self):
        return self.value()


class MemoryReader(GuestReader):
    def __init__(self, qube, domid):
        super().__init__(qube, MEMORY_SCRIPT, lambda: MemoryTracker(domid))


def qube_name(raw):
    if isinstance(raw, bytes):
        try:
            raw = raw.decode('ascii')
        except UnicodeError:
            return None
    if not isinstance(raw, str) or not 1 <= len(raw) <= 31:
        return None
    if not raw[0].isascii() or not raw[0].isalpha():
        return None
    if not all(ch.isascii() and (ch.isalnum() or ch in '-_') for ch in raw):
        return None
    return raw


class MemoryPool:
    """One bounded stream per running guest; identity changes discard old data."""
    def __init__(self, xs, reader_factory=MemoryReader, limit=128):
        self.xs = xs
        self.reader_factory = reader_factory
        self.limit = limit
        self.readers = {}
        self.identities = {}
        self.retired = []

    def sync(self, domains):
        # Device-model helper domains have no Qubes agent. Their allocation stays unknown.
        stubdoms = set()
        for domain in domains:
            try:
                stub = number(self.xs.read('', f"/local/domain/{domain['domid']}/image/device-model-domid"))
                if stub is not None:
                    stubdoms.add(stub)
            except Exception:
                continue
        active = {}
        for domain in domains:
            domid = domain['domid']
            if domid == 0 or domid in stubdoms or any(domain.get(flag, False) for flag in ('paused', 'shutdown', 'dying')):
                continue
            try:
                name = qube_name(self.xs.read('', f'/local/domain/{domid}/name'))
            except Exception:
                continue
            if name is not None:
                active[domid] = (domid, tuple(domain.get('handle', ())), name)
        for domid, key in list(self.identities.items()):
            if active.get(domid) != key:
                reader = self.readers.pop(domid)
                reader.stop()
                self.retired.append(reader)
                del self.identities[domid]
        self.retired = [r for r in self.retired if r.thread.is_alive()]
        for domid, key in active.items():
            if domid not in self.readers and len(self.readers) + len(self.retired) < self.limit:
                reader = self.reader_factory(key[2], domid)
                self.readers[domid], self.identities[domid] = reader, key
                reader.start()

    def used(self, domain):
        domid = domain['domid']
        key = self.identities.get(domid)
        if key is None or key[1] != tuple(domain.get('handle', ())):
            return None
        return self.readers[domid].value()

    def close(self):
        readers = list(self.readers.values()) + self.retired
        for reader in readers:
            reader.stop()
        deadline = time.monotonic() + 8
        for reader in readers:
            reader.thread.join(timeout=max(0, deadline - time.monotonic()))
        self.readers.clear()
        self.identities.clear()
        self.retired.clear()


class Collector:
    def __init__(self, xc, xs, network=None, memory=None):
        self.xc, self.xs = xc, xs
        self.network = network
        self.memory = memory
        self.cpu = CpuTracker()

    def sample(self, period):
        host = self.xc.physinfo()
        domains = self.xc.domain_getinfo(0, 65536)
        # Raw domains include stubdomains exactly once; do not add Qubes aggregates.
        domains = [d for d in domains if not d.get("dying", False)]
        cpu = self.cpu.update(domains, time.monotonic(), host["nr_cpus"])
        total = int(host["total_memory"])
        if self.memory:
            self.memory.sync(domains)
        allocated = used = unknown = 0
        for domain in domains:
            allocation = int(domain["mem_kb"])
            if allocation < 0:
                raise ValueError("Negative Xen allocation")
            allocated += allocation
            usage = None
            if domain["domid"] == 0:
                usage = min(dom0_used(), allocation)
            elif self.memory:
                reported = self.memory.used(domain)
                if reported is not None:
                    usage = min(reported, allocation)
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
    parser.add_argument("--no-guest-memory", action="store_true", help="Show only dom0 usage; guest allocation stays unknown")
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
    memory = None if args.no_guest_memory else MemoryPool(collector.xs)
    collector.memory = memory
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
        if memory:
            memory.close()
        if network:
            network.close()


if __name__ == "__main__":
    main()
