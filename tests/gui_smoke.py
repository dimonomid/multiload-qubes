"""Optional isolated GUI checks; does not change the user's panel/configuration."""
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="multiload-qubes-gui-") as tmp:
    env = dict(os.environ, XDG_CONFIG_HOME=tmp, XDG_CACHE_HOME=tmp, G_DEBUG="fatal-warnings", LC_ALL="C.UTF-8")
    for command in [
        ["build/multiload-qubes-preview", "--self-test"],
        ["build/multiload-qubes-preview", "--preferences"],
        ["build/panel-smoke", str(root / "build/libmultiload-qubes.so")],
    ]:
        command_env = dict(env, MQ_PREVIEW_SECONDS="1") if "--preferences" in command else env
        subprocess.run(["xvfb-run", "-a", *command], cwd=root, env=command_env, check=True, timeout=20)
