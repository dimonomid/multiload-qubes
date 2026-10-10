"""Optional isolated GUI checks; does not change the user's panel/configuration."""
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="multiload-qubes-gui-") as tmp:
    env = dict(os.environ, XDG_CONFIG_HOME=tmp, XDG_CACHE_HOME=tmp, G_DEBUG="fatal-warnings")
    for command in [
        ["build/multiload-qubes-preview", "--self-test"],
        ["build/panel-smoke", str(root / "build/libmultiload-qubes.so")],
    ]:
        subprocess.run(["xvfb-run", "-a", *command], cwd=root, env=env, check=True, timeout=20)
