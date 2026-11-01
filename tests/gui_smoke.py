"""Optional isolated GUI checks; does not change the user's panel/configuration."""
import os
import shlex
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
cargo_test = [*shlex.split(os.environ.get("CARGO", "cargo")), "test", "--offline", "--features", "panel", "--lib"]
# Compile before starting timed GUI commands; a fresh build can take much longer.
subprocess.run([*cargo_test, "--no-run"], cwd=root, check=True)
with tempfile.TemporaryDirectory(prefix="multiload-qubes-gui-") as tmp:
    env = dict(os.environ, XDG_CONFIG_HOME=tmp, XDG_CACHE_HOME=tmp, G_DEBUG="fatal-warnings", LC_ALL="C.UTF-8")
    for command in [
        [*cargo_test, "panel::gui_tests::preferences_and_rendering", "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"],
        ["build/multiload-qubes-preview", "--preferences"],
        ["build/panel-smoke", str(root / "build/libmultiload-qubes.so")],
    ]:
        command_env = dict(env, MQ_PREVIEW_SECONDS="1") if "--preferences" in command else env
        subprocess.run(["xvfb-run", "-a", *command], cwd=root, env=command_env, check=True, timeout=20)
