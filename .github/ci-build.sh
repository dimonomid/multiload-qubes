#!/usr/bin/env bash
set -euo pipefail

version="$(python3 - <<'PY'
from pathlib import Path
import os
import tomllib

version = tomllib.loads(Path('Cargo.toml').read_text())['package']['version']
spec_version = next(line.split()[1] for line in Path('packaging/multiload-qubes.spec').read_text().splitlines() if line.startswith('Version:'))
if version != spec_version:
    raise SystemExit('Cargo and RPM versions do not match')
tag = os.environ.get('RELEASE_TAG', '')
if tag and tag != f'v{version}':
    raise SystemExit(f'Release tag {tag} does not match version {version}')
print(version)
PY
)"

cargo fetch --locked
make check gui-test

if [[ "${PACKAGE_RPM:-false}" == true ]]; then
    make rpm
    rm -rf build/release
    mkdir -p build/release
    cp build/rpm/RPMS/x86_64/multiload-qubes-"${version}"-*.rpm build/release/
    (cd build/release && sha256sum -- *.rpm > SHA256SUMS)
fi
