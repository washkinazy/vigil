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
        wait_for(lambda: "Enabled" in title())
        assert item("Enabled")[1]["toggle-state"]["data"] == 1
        assert all(not child["data"][2] for child in layout()[2]), "menu must stay flat"
        assert [item(f"Disable for {n} minutes") is not None for n in (30, 60, 90)] == [True] * 3
        bus("call", name, "/StatusNotifierItem", "org.kde.StatusNotifierItem", "Activate", "ii", "0", "0")
        wait_for(lambda: "Disabled" in title())
        assert any(m["id"] == "manual:toggle" for m in status()["manual_inhibitors"])
        for minutes in (30, 60, 90):
            click(f"Disable for {minutes} minutes")
            wait_for(lambda: f"{minutes} min" in title())
            current = next(m for m in status()["manual_inhibitors"] if m["id"] == "manual:toggle")
            assert minutes * 60 - 10 <= current["remaining_seconds"] <= minutes * 60
        other = subprocess.check_output(ctl + ["keep-awake", "--reason", "Download"], text=True).strip()
        click("Enabled")
        wait_for(lambda: "Enabled" in title())
        assert [m["id"] for m in status()["manual_inhibitors"]] == [other]
        daemon.terminate()
        daemon.wait(timeout=5)
        wait_for(lambda: "Unavailable" in title())
        assert item("Enabled")[1]["enabled"]["data"] is False
        daemon = subprocess.Popen(args)
        wait_for(lambda: "Enabled" in title())
        assert not status()["manual_inhibitors"]
        assert tray.poll() is None
    finally:
        tray.terminate()
        tray.wait(timeout=5)
        if daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=5)
print("Tray integration passed: activation, menu durations, independent requests, disconnect/reconnect.")
