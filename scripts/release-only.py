#!/usr/bin/env python3
"""Select full builds conservatively, excluding documentation and CI-only changes."""

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
            if "source" not in package and package["name"] in {
                "vigil-core",
                "vigild",
                "vigilctl",
            }:
                package.pop("version")
        return data
    if path == "packaging/vigil.spec":
        return re.sub(r"^Version:.*$", "Version:", text, flags=re.MULTILINE)
    if path == ".release-please-manifest.json":
        data = json.loads(text)
        data.pop(".")
        return data
    return None


def changed_paths(base, head):
    return (
        subprocess.check_output(
            ["git", "diff", "--no-renames", "--name-only", "-z", base, head], text=True
        )
        .rstrip("\0")
        .split("\0")
    )


def release_only(base, head):
    paths = changed_paths(base, head)
    allowed = {
        "VERSION",
        "CHANGELOG.md",
        "Cargo.toml",
        "Cargo.lock",
        "packaging/vigil.spec",
        ".release-please-manifest.json",
    }
    if not paths or not set(paths) <= allowed:
        return False
    for path in paths:
        before, after = read(base, path), read(head, path)
        if normalized(path, before) != normalized(path, after):
            return False
    return True


def requires_build(base, head, release_pr=False):
    # Compare the PR's own changes, not unrelated commits added to main.
    base = subprocess.check_output(["git", "merge-base", base, head], text=True).strip()
    if release_pr and release_only(base, head):
        return False
    ci_files = {
        "scripts/release-only.py",
        "scripts/test-ci-selection.py",
        "scripts/auto-merge-release.py",
        "scripts/check-version.py",
        "scripts/test-release.cjs",
        "release-please-config.json",
        ".release-please-manifest.json",
    }
    for path in changed_paths(base, head):
        if (
            path.endswith(".md")
            or path.startswith(("docs/", ".github/"))
            or path in ci_files
        ):
            continue
        return True
    return False


if __name__ == "__main__":
    try:
        result = requires_build(
            sys.argv[1], sys.argv[2], "--release-pr" in sys.argv[3:]
        )
    except (KeyError, ValueError, subprocess.CalledProcessError):
        result = True
    print("true" if result else "false")
