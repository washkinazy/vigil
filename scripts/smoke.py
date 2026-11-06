#!/usr/bin/env python3
"""Exercise a separate dry-run instance against the current desktop session."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="vigil-smoke-", dir=os.environ["XDG_RUNTIME_DIR"]) as directory:
    socket = Path(directory) / "control.sock"
    daemon = subprocess.Popen([
        str(root / "target/debug/vigild"), "--dry-run", "--config",
        str(root / "config.example.toml"), "--socket", str(socket),
    ])
    ctl = [str(root / "target/debug/vigilctl"), "--socket", str(socket)]
    def call(*args):
        return subprocess.check_output(ctl + list(args), text=True)
    def status():
        return json.loads(call("status", "--json"))
    try:
        for _ in range(100):
            if socket.exists():
                break
            if daemon.poll() is not None:
                raise RuntimeError("dry-run daemon exited")
            time.sleep(0.05)
        assert status()["dry_run"]
        call("keep-awake", "2s", "--scope", "suspend")
        assert any(i["id"] == "manual" and i["scope"] == ["suspend"] for i in status()["inhibitors"])
        time.sleep(3)
        assert not any(i["id"] == "manual" for i in status()["inhibitors"])
        call("toggle")
        assert any(i["id"] == "manual" for i in status()["inhibitors"])
        call("toggle")
        assert not any(i["id"] == "manual" for i in status()["inhibitors"])
        for command in ("lock", "suspend"):
            call(command)
            assert status()["last_action"] == f"dry-run: would {command.title()}"
        assert isinstance(json.loads(call("waybar"))["tooltip"], str)
        for args in [("unknown",), ("keep-awake", "0"), ("keep-awake", "1h", "--scope", "bad")]:
            assert subprocess.run(ctl + list(args), capture_output=True).returncode != 0
        duplicate = subprocess.run(daemon.args, capture_output=True, text=True, timeout=5)
        assert duplicate.returncode != 0 and "already owns" in duplicate.stderr
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
    assert not socket.exists(), "socket must be cleaned on shutdown"
print("Dry-run integration passed: control, expiry, scopes, actions, Waybar, errors, exclusive socket and shutdown.")
