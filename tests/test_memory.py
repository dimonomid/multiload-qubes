import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("collector", Path(__file__).resolve().parents[1] / "collector/collector.py")
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)


def domain(domid, handle=1, **flags):
    return dict(domid=domid, handle=[handle], mem_kb=1000, cpu_time=0, **flags)


class FakeThread:
    def is_alive(self):
        return False

    def join(self, timeout):
        pass


class FakeReader:
    def __init__(self, name, domid):
        self.name, self.domid = name, domid
        self.started = self.stopped = False
        self.reading = 300
        self.thread = FakeThread()

    def start(self):
        self.started = True

    def stop(self):
        self.stopped = True

    def value(self):
        return self.reading


class FakeXs:
    def __init__(self):
        self.data = {f'/local/domain/{i}/name': f'qube-{i}'.encode() for i in range(1, 6)}

    def read(self, tx, path):
        return self.data.get(path)


class MemoryProtocol(unittest.TestCase):
    def test_actual_usage_and_zero(self):
        tracker = c.MemoryTracker(23)
        self.assertEqual(tracker.update(b'M1 23 1000 400'), 600)
        self.assertEqual(tracker.update(b'M1 23 1000 1000'), 0)

    def test_invalid_and_wrong_domain_frames(self):
        for frame in [b'M1 24 1000 400', b'M1 23 0 0', b'M1 23 100 101',
                      b'M1 23 -1 0', b'M1 23 NaN 0', b'M1 23 1', b'\xff', b'x' * 257]:
            with self.subTest(frame=frame), self.assertRaises((ValueError, UnicodeError)):
                c.MemoryTracker(23).update(frame)

    def test_guest_script_excludes_swap_and_uses_available(self):
        with tempfile.TemporaryDirectory() as tmp:
            meminfo = Path(tmp) / 'meminfo'
            meminfo.write_text('MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\nSwapFree: 0 kB\n')
            script = c.MEMORY_SCRIPT.replace("subprocess.check_output(['xenstore-read', 'domid'], text=True).strip()", "'23'")
            script = script.replace("'/proc/meminfo'", repr(str(meminfo)))
            script = script.replace('while True:', 'for _ in range(1):').replace('time.sleep(1)', '')
            frame = subprocess.check_output([sys.executable, '-c', script], timeout=5)
            self.assertEqual(c.MemoryTracker(23).update(frame), 600)

    def test_missing_memavailable_is_not_treated_as_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'meminfo'
            path.write_text('MemTotal: 1000 kB\n')
            with self.assertRaises(ValueError):
                c.dom0_used(path)


class Lifecycle(unittest.TestCase):
    def setUp(self):
        self.xs = FakeXs()
        self.pool = c.MemoryPool(self.xs, FakeReader)

    def tearDown(self):
        self.pool.close()

    def test_start_stop_pause_and_no_repeated_connections(self):
        self.pool.sync([domain(0), domain(1), domain(2, paused=True)])
        self.assertEqual(list(self.pool.readers), [1])
        reader = self.pool.readers[1]
        self.assertTrue(reader.started)
        self.pool.sync([domain(1)])
        self.assertIs(self.pool.readers[1], reader)
        self.assertEqual(self.pool.used(domain(1)), 300)
        self.pool.sync([domain(1, paused=True)])
        self.assertTrue(reader.stopped)
        self.assertIsNone(self.pool.used(domain(1)))
        self.pool.sync([domain(1)])
        self.assertIsNot(self.pool.readers[1], reader)
        self.pool.sync([])
        self.assertEqual(self.pool.readers, {})

    def test_reused_domid_discards_old_reading(self):
        self.pool.sync([domain(1)])
        old = self.pool.readers[1]
        self.assertIsNone(self.pool.used(domain(1, handle=2)))
        self.pool.sync([domain(1, handle=2)])
        self.assertTrue(old.stopped)
        self.assertIsNot(self.pool.readers[1], old)

    def test_internal_domains_shutdown_and_bad_names_are_skipped(self):
        self.xs.data['/local/domain/1/image/device-model-domid'] = b'2'
        self.xs.data['/local/domain/4/name'] = b'bad;command'
        self.pool.sync([domain(1), domain(2), domain(3, shutdown=True), domain(4)])
        self.assertEqual(list(self.pool.readers), [1])

    def test_failure_to_resolve_one_domain_does_not_block_others(self):
        original = self.xs.read
        def read(tx, path):
            if '/1/' in path:
                raise OSError('Domain gone')
            return original(tx, path)
        self.xs.read = read
        self.pool.sync([domain(1), domain(2)])
        self.assertEqual(list(self.pool.readers), [2])

    def test_resource_limit_and_capacity_recovery(self):
        self.pool.limit = 1
        self.pool.sync([domain(1), domain(2)])
        self.assertEqual(len(self.pool.readers), 1)
        self.pool.sync([domain(2)])
        self.assertEqual(list(self.pool.readers), [2])

    @patch.object(c, 'dom0_used', return_value=400)
    def test_aggregation_clamps_ballooning_and_counts_unknown(self, _):
        class Xc:
            def physinfo(self):
                return dict(total_memory=4000, free_memory=900, nr_cpus=4)
            def domain_getinfo(self, *_):
                return [domain(0), domain(1), domain(2, paused=True)]
        self.pool.sync([domain(1)])
        self.pool.readers[1].reading = 1500
        fields = c.Collector(Xc(), self.xs, memory=self.pool).sample(1).split()
        self.assertEqual(fields[3:8], ['4000', '3000', '1400', '1000', '100'])
        self.pool.readers[1].reading = None
        fields = c.Collector(Xc(), self.xs, memory=self.pool).sample(1).split()
        self.assertEqual(fields[5:7], ['400', '2000'])


class Connection(unittest.TestCase):
    def test_memory_stream_staleness_and_cleanup(self):
        real_popen = subprocess.Popen
        commands, processes = [], []
        def launch(args, **kwargs):
            commands.append(args)
            proc = real_popen([sys.executable, '-u', '-c',
                              "import time; print('M1 23 1000 400', flush=True); time.sleep(20)"], **kwargs)
            processes.append(proc)
            return proc
        reader = c.MemoryReader('dev', 23)
        with patch.object(c.subprocess, 'Popen', side_effect=launch):
            reader.start()
            try:
                deadline = time.monotonic() + 3
                while reader.value() is None and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(reader.value(), 600)
                self.assertEqual(len(commands), 1)
                self.assertIn('--no-autostart', commands[0])
                self.assertIn('dev', commands[0])
                with reader.lock:
                    reader.latest = (time.monotonic() - 6, 600)
                self.assertIsNone(reader.value())
            finally:
                reader.close()
        self.assertFalse(reader.thread.is_alive())
        self.assertIsNotNone(processes[0].poll())


if __name__ == '__main__':
    unittest.main()
