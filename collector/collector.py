#!/usr/bin/python3
"""Read-only Xen/Qubes collector; never executes commands inside guests.

Only this service needs Xen privileges. The panel reads an atomic numeric snapshot.
All memory quantities are KiB. Guest reports are untrusted, bounded integers.
"""
import argparse
import logging
import math
import os
from pathlib import Path
import tempfile
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


class Collector:
    def __init__(self, xc, xs):
        self.xc, self.xs = xc, xs
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
        return (f"MQ1 {time.time():.6f} {cpu_text} {total} {allocated} "
                f"{used} {unknown} {overhead} {period}\n")


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
    args = parser.parse_args()
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
    while True:
        start = time.monotonic()
        try:
            publish(args.output, collector.sample(args.interval))
        except Exception:
            # Leave the previous timestamp intact: the panel will flag stale data.
            logging.exception("Cannot collect Xen metrics")
        time.sleep(max(0, args.interval - (time.monotonic() - start)))


if __name__ == "__main__":
    main()
