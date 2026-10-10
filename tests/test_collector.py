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
    def test_swap_is_excluded(self):
        self.assertEqual(c.physical_used(b"800", b"300", 1000), 500)

    def test_missing_swap_is_unknown(self):
        self.assertIsNone(c.physical_used(b"800", None, 1000))

    def test_untrusted_reports(self):
        for value in [b"-1", b"NaN", b"1\n", b"9" * 100, b"\xff", "１２", None]:
            self.assertIsNone(c.physical_used(value, b"0", 1000))

    def test_threshold_skew_and_ballooning(self):
        self.assertIsNone(c.physical_used(b"100", b"200", 1000))
        self.assertEqual(c.physical_used(b"1500", b"0", 1000), 1000)

    def test_dom0_excludes_cache_and_swap(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "meminfo"
            path.write_text("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 200 kB\nSwapTotal: 500 kB\nSwapFree: 0 kB\n")
            self.assertEqual(c.dom0_used(path), 650)

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
    def test_snapshot_accounts_for_unknown_domains(self, _):
        xs = FakeXs()
        collector = c.Collector(FakeXc(), xs)
        p = collector.sample(1).split()
        self.assertEqual(p[0], "MQ1")
        self.assertEqual(p[2:], ["unknown", "4000", "3000", "900", "1000", "100", "1"])
        self.assertEqual(xs.ended, 2)

    @patch.object(c, "dom0_used", return_value=400)
    def test_failed_read_aborts_transaction(self, _):
        xs = FakeXs()
        xs.read = lambda *_: (_ for _ in ()).throw(OSError("domain gone"))
        with self.assertRaises(OSError):
            c.Collector(FakeXc(), xs).sample(1)
        self.assertEqual(xs.ended, 1)

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
