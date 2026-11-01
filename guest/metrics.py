# Run inside a Linux qube. Each collector request produces one RAM/swap report.
import pathlib, subprocess, sys
domid = subprocess.check_output(['xenstore-read', 'domid'], text=True).strip()
# Closing the collector connection ends stdin and stops this script. Removing
# the panel plugin alone does not stop the collector or its connections.
while True:
    # Wait for dom0 rather than running a separate sampling timer.
    command = sys.stdin.readline(16)
    if command != 'sample\n':
        break
    fields = {}
    for line in pathlib.Path('/proc/meminfo').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('MemTotal', 'MemAvailable', 'SwapTotal', 'SwapFree'):
            fields[key] = int(value.split()[0])
    # Send the running domain ID so dom0 can reject a report from the wrong VM
    # after a stop/restart. dom0 supplies the qube name; we do not send one.
    print('M2', domid, fields['MemTotal'], fields['MemAvailable'],
          fields['SwapTotal'], fields['SwapFree'], flush=True)
