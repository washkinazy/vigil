#!/usr/bin/env python3
"""Validate the shared release version without updating manifests or dependencies."""

import argparse
import re
import subprocess
import tomllib
from pathlib import Path

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--tag")
args = parser.parse_args()
version = (root / "VERSION").read_text().strip()
if not re.fullmatch(r"\d+\.\d+\.\d+", version):
    raise SystemExit(f"Invalid release version: {version}")
workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
if workspace["package"]["version"] != version:
    raise SystemExit("VERSION and Cargo workspace version differ")
lock = tomllib.loads((root / "Cargo.lock").read_text())
for member in workspace["members"]:
    package = tomllib.loads((root / member / "Cargo.toml").read_text())["package"]
    if package["version"] != {"workspace": True}:
        raise SystemExit(f"{member} must inherit the workspace version")
    entries = [
        p for p in lock["package"] if p["name"] == package["name"] and "source" not in p
    ]
    if len(entries) != 1 or entries[0]["version"] != version:
        raise SystemExit(f"Cargo.lock version mismatch for {package['name']}")
spec_version = subprocess.check_output(
    [
        "rpmspec",
        "--query",
        "--srpm",
        "--qf",
        "%{VERSION}",
        str(root / "packaging/vigil.spec"),
    ],
    text=True,
)
if spec_version != version:
    raise SystemExit("RPM and Cargo versions differ")
if args.tag and args.tag != f"v{version}":
    raise SystemExit(f"Tag {args.tag} does not match v{version}")
print(f"Release versions agree: {version}")
