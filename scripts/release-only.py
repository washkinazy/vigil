#!/usr/bin/env python3
"""Identify release metadata changes; any other change requires the full matrix."""

import json
import re
import subprocess
import sys
import tomllib


def read(ref, path):
    return subprocess.check_output(["git", "show", f"{ref}:{path}"], text=True)


def normalized(path, text):
    if path == "Cargo.toml":
        data = tomllib.loads(text)
        data["workspace"]["package"].pop("version")
        return data
    if path == "Cargo.lock":
        data = tomllib.loads(text)
        for package in data["package"]:
            if "source" not in package and package["name"] in {"vigil-core", "vigild", "vigilctl"}:
                package.pop("version")
        return data
    if path == "packaging/vigil.spec":
        return re.sub(r"^Version:.*$", "Version:", text, flags=re.MULTILINE)
    if path == ".release-please-manifest.json":
        data = json.loads(text)
        data.pop(".")
        return data
    return None


def release_only(base, head):
    paths = subprocess.check_output(
        ["git", "diff", "--name-only", base, head], text=True
    ).splitlines()
    allowed = {"VERSION", "CHANGELOG.md", "Cargo.toml", "Cargo.lock",
               "packaging/vigil.spec", ".release-please-manifest.json"}
    if not paths or not set(paths) <= allowed:
        return False
    for path in paths:
        before, after = read(base, path), read(head, path)
        if normalized(path, before) != normalized(path, after):
            return False
    return True


if __name__ == "__main__":
    try:
        result = release_only(*sys.argv[1:])
    except (KeyError, ValueError, subprocess.CalledProcessError):
        result = False
    print("true" if result else "false")
