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
BOOT = "12345678-1234-1234-1234-123456789abc"


def frame(second, counters="2:100:200", boot=BOOT):
    return f"N1 {int(second * 1e9)} {boot} {counters}".encode()


class Rates(unittest.TestCase):
    def test_rates_and_multiple_interfaces(self):
        tracker = c.NetworkTracker()
        self.assertIsNone(tracker.update(frame(1, "2:100:200,3:50:60")))
        self.assertEqual(tracker.update(frame(3, "2:1100:600,3:550:260")), (750, 300))

    def test_zero_traffic_is_distinct_from_unavailable(self):
        tracker = c.NetworkTracker()
        tracker.update(frame(1))
        self.assertEqual(tracker.update(frame(2)), (0, 0))
        self.assertIsNone(tracker.update(frame(3, "-")))

    def test_counter_resets_interface_changes_and_reboots(self):
        for new in [frame(2, "2:1:1"), frame(2, "3:100:200"),
                    frame(2, boot="aaaaaaaa-1234-1234-1234-123456789abc"), frame(1), frame(20)]:
            tracker = c.NetworkTracker()
            tracker.update(frame(1))
            self.assertIsNone(tracker.update(new))

    def test_malformed_or_hostile_frames(self):
        for raw in [b"N1 NaN boot 1:2:3", b"\xff", b"x" * 8193,
                    frame(1, "2:-1:3"), frame(1, "2:1:2,2:3:4"),
                    frame(1, "0:1:2"), frame(1, "2:1:2:3"),
                    frame(1, "2:" + "9" * 40 + ":2")]:
            with self.subTest(raw=raw[:100]), self.assertRaises((ValueError, UnicodeError)):
                c.parse_network(raw)

    def test_reader_discards_stale_rates(self):
        reader = c.NetworkReader()
        reader.latest = (10, (100, 200))
        with patch.object(c.time, "monotonic", return_value=12):
            self.assertEqual(reader.rates(), (100, 200))
        with patch.object(c.time, "monotonic", return_value=16):
            self.assertIsNone(reader.rates())


class GuestSelection(unittest.TestCase):
    def test_only_device_backed_nonvirtual_interfaces_are_counted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, idx in [("eth0", 2), ("wlan0", 3), ("vif5.0", 4), ("lo", 1), ("tun0", 5)]:
                dev = root / name
                (dev / "statistics").mkdir(parents=True)
                if name != "tun0":
                    (dev / "device").mkdir()
                (dev / "ifindex").write_text(str(idx))
                (dev / "statistics/rx_bytes").write_text("1000")
                (dev / "statistics/tx_bytes").write_text("2000")
            script = c.NETWORK_SCRIPT.replace("'/sys/class/net'", repr(tmp))
            script = script.replace("while True:", "for _ in range(1):").replace("time.sleep(1)", "")
            line = subprocess.check_output([sys.executable, "-c", script], timeout=5)
            _, _, counters = c.parse_network(line)
            self.assertEqual(counters, {2: (1000, 2000), 3: (1000, 2000)})


class Connection(unittest.TestCase):
    def test_reconnect_establishes_a_fresh_baseline(self):
        real_popen = subprocess.Popen
        launches = []
        first = f"print({frame(10, '2:90000:90000').decode()!r}, flush=True)"
        second = (f"import time; print({frame(1, '2:100:200').decode()!r}, flush=True); time.sleep(.05); "
                  f"print({frame(2, '2:110:220').decode()!r}, flush=True); time.sleep(20)")
        def launch(args, **kwargs):
            script = first if not launches else second
            proc = real_popen([sys.executable, "-u", "-c", script], **kwargs)
            launches.append(proc)
            return proc
        reader = c.NetworkReader()
        real_wait = reader.stop_event.wait
        with patch.object(c.subprocess, "Popen", side_effect=launch), \
             patch.object(reader.stop_event, "wait", side_effect=lambda seconds: real_wait(min(seconds, .05))):
            reader.start()
            try:
                deadline = time.monotonic() + 3
                while reader.rates() != (10, 20) and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(reader.rates(), (10, 20))
                self.assertGreaterEqual(len(launches), 2)
            finally:
                reader.close()
        self.assertTrue(all(proc.poll() is not None for proc in launches))

    def test_persistent_stream_and_cleanup(self):
        # Exercise the real pipe reader/process cleanup without a Qubes host.
        real_popen = subprocess.Popen
        commands = []
        script = (f"import time; print({frame(1).decode()!r}, flush=True); time.sleep(.05); "
                  f"print({frame(2, '2:1100:2200').decode()!r}, flush=True); time.sleep(20)")
        processes = []
        def launch(args, **kwargs):
            commands.append(args)
            proc = real_popen([sys.executable, "-u", "-c", script], **kwargs)
            processes.append(proc)
            return proc
        reader = c.NetworkReader()
        with patch.object(c.subprocess, "Popen", side_effect=launch):
            reader.start()
            try:
                deadline = time.monotonic() + 3
                while reader.rates() is None and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(reader.rates(), (1000, 2000))
                self.assertEqual(len(commands), 1)
                self.assertIn("--no-autostart", commands[0])
                self.assertIn("sys-net", commands[0])
            finally:
                reader.close()
        self.assertFalse(reader.thread.is_alive())
        self.assertIsNotNone(processes[0].poll())


if __name__ == "__main__":
    unittest.main()
