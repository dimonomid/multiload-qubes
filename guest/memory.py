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
