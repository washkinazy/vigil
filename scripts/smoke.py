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
    # Resolve config paths in child processes without mutating the desktop environment.
    home = Path(directory) / "home"
    custom = Path(directory) / "custom-config"
    for config_home in (None, "", str(custom)):
        env = dict(os.environ, HOME=str(home))
        env.pop("XDG_CONFIG_HOME", None)
        if config_home is not None:
            env["XDG_CONFIG_HOME"] = config_home
        expected = (Path(config_home) if config_home else home / ".config") / "vigil/config.toml"
        expected.parent.mkdir(parents=True, exist_ok=True)
        expected.write_text(config.read_text())
        result = subprocess.check_output([str(root / "target/debug/vigild"), "--check-config"], env=env, text=True)
        assert str(expected) in result
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
        assert "Suspend:" in call("explain", "suspend")
        call("keep-awake", "1h")
        call("keep-awake", "2s")
        assert 0 < status()["caffeine"]["remaining_seconds"] <= 2
        assert json.loads(call("waybar"))["class"] == "manual"
        time.sleep(3)
        assert status()["caffeine"] is None
        call("toggle")
        assert status()["caffeine"]["remaining_seconds"] is None
        call("toggle")
        assert status()["caffeine"] is None
        # Every concurrent toggle is serialized by the daemon.
        toggles = [subprocess.Popen(ctl + ["toggle"], stdout=subprocess.DEVNULL) for _ in range(10)]
        assert all(p.wait(timeout=10) == 0 for p in toggles)
        assert status()["caffeine"] is None
        clients = [subprocess.Popen(ctl + ["keep-awake", "1h"], stdout=subprocess.DEVNULL) for _ in range(10)]
        assert all(p.wait(timeout=10) == 0 for p in clients)
        assert 3500 < status()["caffeine"]["remaining_seconds"] <= 3600
        config.write_text("[timeouts]\nlock_seconds = 0\n")
        call("reload")
        current = status()
        assert current["caffeine"] is not None
        assert not next(t for t in current["timers"] if t["action"] == "lock")["enabled"]
        config.write_text("typo = true")
        assert subprocess.run(ctl + ["reload"], capture_output=True).returncode != 0
        assert not next(t for t in status()["timers"] if t["action"] == "lock")["enabled"]
        call("automatic")
        assert status()["caffeine"] is None
        for command in ("lock", "suspend"):
            call(command)
            assert status()["last_action"] == f"dry-run: would Action({command.title()})"
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
print("Dry-run integration passed: control, caffeine replacement/expiry, actions, Waybar, errors, exclusive socket and shutdown.")
