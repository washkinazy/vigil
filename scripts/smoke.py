#!/usr/bin/env python3
"""Exercise a separate dry-run instance against the current desktop session."""
import json
import select
import shutil
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
        str(config), "--socket", str(socket), "--state-file", str(Path(directory) / "manual.json"),
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
        assert "Suspend:" in call("explain", "suspend")
        first = call("keep-awake", "2s", "--scope", "suspend", "--reason", "Download").strip()
        second = call("keep-awake", "--reason", "Presentation <&>").strip()
        assert first != second
        assert len(status()["manual_inhibitors"]) == 2
        assert not (Path(directory) / "manual.json").exists()
        assert any(i["id"] == first and i["scope"] == ["suspend"] for i in status()["inhibitors"])
        assert "Presentation &lt;&amp;&gt;" in json.loads(call("waybar"))["tooltip"]
        time.sleep(3)
        assert [i["id"] for i in status()["manual_inhibitors"]] == [second]
        call("toggle")
        assert json.loads(call("waybar"))["class"] == "manual"
        assert len(status()["manual_inhibitors"]) == 2
        call("toggle")
        assert [i["id"] for i in status()["manual_inhibitors"]] == [second]
        assert json.loads(call("waybar"))["class"] == "inhibited"
        # Concurrent clients must each flip state exactly once, preserving other requests.
        toggles = [subprocess.Popen(ctl + ["toggle"], stdout=subprocess.DEVNULL) for _ in range(10)]
        assert all(p.wait(timeout=10) == 0 for p in toggles)
        assert [i["id"] for i in status()["manual_inhibitors"]] == [second]
        clients = [subprocess.Popen(ctl + ["keep-awake", "1h"], stdout=subprocess.PIPE, text=True) for _ in range(10)]
        ids = [p.communicate(timeout=10)[0].strip() for p in clients]
        assert all(p.returncode == 0 for p in clients)
        assert len(set(ids)) == 10
        assert len(status()["manual_inhibitors"]) == 11
        for ident in ids:
            call("release", ident)
        assert [i["id"] for i in status()["manual_inhibitors"]] == [second]
        assert subprocess.run(ctl + ["release", first], capture_output=True).returncode != 0
        config.write_text("[timeouts]\nlock_seconds = 0\n")
        call("reload")
        current = status()
        assert [i["id"] for i in current["manual_inhibitors"]] == [second]
        assert not next(t for t in current["timers"] if t["action"] == "lock")["enabled"]
        config.write_text("typo = true")
        assert subprocess.run(ctl + ["reload"], capture_output=True).returncode != 0
        assert not next(t for t in status()["timers"] if t["action"] == "lock")["enabled"]
        call("automatic")
        assert not status()["manual_inhibitors"]
        for command in ("lock", "suspend"):
            call(command)
            assert status()["last_action"] == f"dry-run: would {command.title()}"
        assert isinstance(json.loads(call("waybar"))["tooltip"], str)
        for args in [("unknown",), ("explain",), ("explain", "invalid"), ("release",), ("keep-awake", "--reason"), ("keep-awake", "--reason", "bad\nlabel"), ("keep-awake", "0"), ("keep-awake", "1h", "--scope", "bad")]:
            assert subprocess.run(ctl + list(args), capture_output=True).returncode != 0
        # Simulate an RPM replacing the persistent watcher, without touching installed files.
        watcher_path = Path(directory) / "vigilctl"
        replacement = Path(directory) / "vigilctl.new"
        shutil.copy2(root / "target/debug/vigilctl", watcher_path)
        watcher = subprocess.Popen([str(watcher_path), "--socket", str(socket), "waybar", "--watch"], stdout=subprocess.PIPE, text=True)
        try:
            assert select.select([watcher.stdout], [], [], 10)[0], "watcher did not emit status"
            json.loads(watcher.stdout.readline())
            shutil.copy2(root / "target/debug/vigilctl", replacement)
            os.replace(replacement, watcher_path)
            assert watcher.wait(timeout=10) == 0
        finally:
            if watcher.poll() is None:
                watcher.terminate()
                watcher.wait(timeout=5)
        config.write_text((root / "config.example.toml").read_text())
        duplicate = subprocess.run(daemon.args, capture_output=True, text=True, timeout=5)
        assert duplicate.returncode != 0 and "already owns" in duplicate.stderr
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
    assert not socket.exists(), "socket must be cleaned on shutdown"
print("Dry-run integration passed: control, expiry, scopes, actions, Waybar, errors, exclusive socket and shutdown.")
