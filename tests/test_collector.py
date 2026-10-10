import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("collector", Path(__file__).resolve().parents[1] / "collector/collector.py")
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)


def domain(domid, cpu=0, memory=1000, handle=1):
    return {"domid": domid, "cpu_time": cpu, "mem_kb": memory, "handle": [handle]}


class Accounting(unittest.TestCase):
    def test_memory_available_excludes_reclaimable_cache(self):
        self.assertEqual(c.memory_used(b"1000", b"400"), 600)

    def test_untrusted_reports(self):
        for value in [b"-1", b"NaN", b"1\n", b"9" * 100, b"\xff", "１２", None]:
            with self.assertRaises(ValueError):
                c.memory_used(value, b"0")

    def test_dom0_excludes_cache_and_swap(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "meminfo"
            path.write_text("MemTotal: 1000 kB\nMemAvailable: 400 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 200 kB\nSwapTotal: 500 kB\nSwapFree: 0 kB\n")
            self.assertEqual(c.dom0_used(path), 600)

    def test_cpu_normalization(self):
        cpu = c.CpuTracker()
        self.assertIsNone(cpu.update([domain(0), domain(1)], 10, 4))
        self.assertEqual(cpu.update([domain(0, 1_000_000_000), domain(1, 1_000_000_000)], 11, 4), 50)

    def test_cpu_reset_reused_id_and_new_domain(self):
        cpu = c.CpuTracker()
        cpu.update([domain(0, 100), domain(1, 100)], 1, 1)
        self.assertEqual(cpu.update([domain(0, 50), domain(1, 500, handle=2), domain(2, 900)], 2, 1), 0)

    def test_cpu_domain_shutdown_and_invalid_clock(self):
        cpu = c.CpuTracker()
        cpu.update([domain(0), domain(1, 9_000_000_000)], 1, 2)
        self.assertEqual(cpu.update([domain(0, 1_000_000_000)], 2, 2), 50)
        self.assertIsNone(cpu.update([domain(0)], 2, 2))


class FakeXc:
    def physinfo(self):
        return {"total_memory": 4000, "free_memory": 900, "nr_cpus": 4}

    def domain_getinfo(self, first, maximum):
        return [domain(0), domain(1), domain(2)]


class FakeXs:
    def __init__(self):
        self.ended = 0

    def transaction_start(self):
        return 1

    def transaction_end(self, tx, abort):
        self.ended += 1

    def read(self, tx, path):
        if "/1/" in path:
            return b"800" if path.endswith("meminfo") else b"300"
        return None


class Integration(unittest.TestCase):
    @patch.object(c, "dom0_used", return_value=400)
    def test_network_rates_do_not_change_xen_accounting(self, _):
        class Network:
            def rates(self):
                return (1234.5, 678.25)
        sample = c.Collector(FakeXc(), FakeXs(), Network()).sample(1).split()
        self.assertEqual(sample[-2:], ["1234.500", "678.250"])
        self.assertEqual(sample[3:8], ["4000", "3000", "400", "2000", "100"])

    @patch.object(c, "dom0_used", return_value=400)
    def test_snapshot_accounts_for_unknown_domains(self, _):
        xs = FakeXs()
        collector = c.Collector(FakeXc(), xs)
        p = collector.sample(1).split()
        self.assertEqual(p[0], "MQ2")
        self.assertEqual(p[2:], ["unknown", "4000", "3000", "400", "2000", "100", "1", "unknown", "unknown"])
        self.assertEqual(xs.ended, 0)

    def test_atomic_publish_and_permissions(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics"
            c.publish(path, "first")
            c.publish(path, "second")
            self.assertEqual(path.read_text(), "second")
            self.assertEqual(path.stat().st_mode & 0o777, 0o644)
            self.assertEqual(list(Path(tmp).iterdir()), [path])


if __name__ == "__main__":
    unittest.main()
