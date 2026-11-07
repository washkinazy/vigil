#!/usr/bin/env python3
"""Exercise the actual tray protocol on a private bus with a dry-run daemon."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

if sys.argv[1:] != ["--private-bus"]:
    os.execvp("dbus-run-session", ["dbus-run-session", "--", sys.executable, __file__, "--private-bus"])

root = Path(__file__).resolve().parents[1]

def wait_for(check):
    for _ in range(100):
        try:
            if result := check():
                return result
        except (subprocess.CalledProcessError, StopIteration, FileNotFoundError):
            pass
        time.sleep(0.1)
    raise AssertionError("tray condition did not become true")

with tempfile.TemporaryDirectory(prefix="vigil-tray-") as directory:
    path = Path(directory)
    socket = path / "control.sock"
    args = [str(root / "target/debug/vigild"), "--dry-run", "--socket", str(socket), "--config", str(root / "config.example.toml")]
    ctl = [str(root / "target/debug/vigilctl"), "--socket", str(socket)]
    daemon = subprocess.Popen(args)
    tray = subprocess.Popen(ctl + ["tray"])
    def status():
        return json.loads(subprocess.check_output(ctl + ["status", "--json"], text=True))
    def names():
        return subprocess.check_output(["busctl", "--user", "--no-legend", "--no-pager", "list"], text=True)
    def bus(*args):
        out = subprocess.check_output(["busctl", "--user", "--json=short", "--", *args], text=True)
        return json.loads(out) if out.strip() else None
    try:
        name = wait_for(lambda: next(line.split()[0] for line in names().splitlines() if line.startswith("org.kde.StatusNotifierItem-")))
        def title():
            return bus("get-property", name, "/StatusNotifierItem", "org.kde.StatusNotifierItem", "Title")["data"]
        def layout():
            return bus("call", name, "/MenuBar", "com.canonical.dbusmenu", "GetLayout", "iias", "0", "-1", "0")["data"][1]
        def item(label, node=None):
            node = layout() if node is None else node
            if node[1].get("label", {}).get("data") == label:
                return node
            for child in node[2]:
                found = item(label, child["data"])
                if found is not None:
                    return found
            return None
        def click(label):
            ident = item(label)[0]
            bus("call", name, "/MenuBar", "com.canonical.dbusmenu", "Event", "isvu", str(ident), "clicked", "i", "0", "0")
        wait_for(lambda: "Unavailable" not in title())
        assert item("Keep awake")[1]["toggle-state"]["data"] == 0
        assert all(not child["data"][2] for child in layout()[2]), "menu must stay flat"
        assert [item(f"Keep awake for {n} minutes") is not None for n in (30, 60, 90)] == [True] * 3
        bus("call", name, "/StatusNotifierItem", "org.kde.StatusNotifierItem", "Activate", "ii", "0", "0")
        wait_for(lambda: "Keep awake" in title())
        assert status()["caffeine"] is not None
        for minutes in (30, 60, 90):
            click(f"Keep awake for {minutes} minutes")
            wait_for(lambda: f"{minutes} min" in title())
            current = status()["caffeine"]
            assert minutes * 60 - 10 <= current["remaining_seconds"] <= minutes * 60
        subprocess.check_call(ctl + ["keep-awake", "1h"])
        wait_for(lambda: "60 min" in title())
        click("Keep awake")
        wait_for(lambda: status()["caffeine"] is None)
        wait_for(lambda: "Keep awake" not in title())
        daemon.terminate()
        daemon.wait(timeout=5)
        wait_for(lambda: "Unavailable" in title())
        assert item("Keep awake")[1]["enabled"]["data"] is False
        daemon = subprocess.Popen(args)
        wait_for(lambda: "Unavailable" not in title())
        assert status()["caffeine"] is None
        assert tray.poll() is None
    finally:
        tray.terminate()
        tray.wait(timeout=5)
        if daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=5)
print("Tray integration passed: activation, menu durations, shared caffeine state, disconnect/reconnect.")
