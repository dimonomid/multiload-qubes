import pathlib, sys, time
boot = pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()
# Closing the collector connection ends stdin and stops this script. Removing
# the panel plugin alone does not stop the collector or its connections.
while True:
    command = sys.stdin.readline(16)
    if command != 'sample\n':
        break
    rows = []
    # Count physical NICs only. Adding virtual links would count the same
    # traffic again as it passes between qubes or through a bridge.
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
