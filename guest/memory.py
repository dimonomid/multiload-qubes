import pathlib, subprocess, time
domid = subprocess.check_output(['xenstore-read', 'domid'], text=True).strip()
# When the collector closes the connection, the next print raises BrokenPipeError
# and exits this script. Closing the panel plugin alone does not stop the collector.
while True:
    fields = {}
    for line in pathlib.Path('/proc/meminfo').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('MemTotal', 'MemAvailable', 'SwapTotal', 'SwapFree'):
            fields[key] = int(value.split()[0])
    # Send the running domain ID so dom0 can reject a report from the wrong VM
    # after a stop/restart. dom0 supplies the qube name; we do not send one.
    print('M2', domid, fields['MemTotal'], fields['MemAvailable'],
          fields['SwapTotal'], fields['SwapFree'], flush=True)
    time.sleep(1)
