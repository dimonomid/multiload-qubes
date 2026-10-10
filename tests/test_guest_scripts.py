"""Execute the unchanged guest payloads locally with fixture paths."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class GuestScripts(unittest.TestCase):
    def test_memory_uses_available_not_swap(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'meminfo'
            path.write_text('MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\nSwapFree: 0 kB\n')
            script = (ROOT / 'guest/memory.py').read_text()
            script = script.replace("subprocess.check_output(['xenstore-read', 'domid'], text=True).strip()", "'23'")
            script = script.replace("'/proc/meminfo'", repr(str(path)))
            script = script.replace('while True:', 'for _ in range(1):').replace('time.sleep(1)', '')
            line = subprocess.check_output([sys.executable, '-c', script], timeout=5)
            self.assertEqual(line.split(), [b'M2', b'23', b'1000', b'400', b'500', b'0'])

    def test_network_selects_only_physical_interfaces(self):
        with tempfile.TemporaryDirectory() as tmp:
            for name, idx in [('eth0', 2), ('wlan0', 3), ('vif5.0', 4), ('lo', 1), ('tun0', 5)]:
                dev = Path(tmp) / name
                (dev / 'statistics').mkdir(parents=True)
                if name != 'tun0':
                    (dev / 'device').mkdir()
                (dev / 'ifindex').write_text(str(idx))
                (dev / 'statistics/rx_bytes').write_text('1000')
                (dev / 'statistics/tx_bytes').write_text('2000')
            script = (ROOT / 'guest/network.py').read_text().replace("'/sys/class/net'", repr(tmp))
            script = script.replace('while True:', 'for _ in range(1):').replace('time.sleep(1)', '')
            line = subprocess.check_output([sys.executable, '-c', script], timeout=5)
            self.assertEqual(line.split()[-1], b'2:1000:2000,3:1000:2000')


if __name__ == '__main__':
    unittest.main()
