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
    config = Path(directory) / "config.toml"
    config.write_text((root / "config.example.toml").read_text())
    daemon = subprocess.Popen([
        str(root / "target/debug/vigild"), "--dry-run", "--config",
        str(config), "--socket", str(socket),
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
        # Concurrent clients must each flip state exactly once.
        toggles = [subprocess.Popen(ctl + ["toggle"], stdout=subprocess.DEVNULL) for _ in range(10)]
        assert all(p.wait(timeout=10) == 0 for p in toggles)
        assert not any(i["id"] == "manual" for i in status()["inhibitors"])
        call("keep-awake", "1h")
        config.write_text("[timeouts]\nlock_seconds = 0\n")
        call("reload")
        current = status()
        assert any(i["id"] == "manual" for i in current["inhibitors"])
        assert not next(t for t in current["timers"] if t["action"] == "lock")["enabled"]
        config.write_text("typo = true")
        assert subprocess.run(ctl + ["reload"], capture_output=True).returncode != 0
        assert not next(t for t in status()["timers"] if t["action"] == "lock")["enabled"]
        call("automatic")
        for command in ("lock", "suspend"):
            call(command)
            assert status()["last_action"] == f"dry-run: would {command.title()}"
        assert isinstance(json.loads(call("waybar"))["tooltip"], str)
        for args in [("unknown",), ("keep-awake", "0"), ("keep-awake", "1h", "--scope", "bad")]:
            assert subprocess.run(ctl + list(args), capture_output=True).returncode != 0
        config.write_text((root / "config.example.toml").read_text())
        duplicate = subprocess.run(daemon.args, capture_output=True, text=True, timeout=5)
        assert duplicate.returncode != 0 and "already owns" in duplicate.stderr
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
    assert not socket.exists(), "socket must be cleaned on shutdown"
print("Dry-run integration passed: control, expiry, scopes, actions, Waybar, errors, exclusive socket and shutdown.")
