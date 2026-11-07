#!/usr/bin/env python3
"""Enable checked auto-merge only for the generated, version-only release PR."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess

spec = importlib.util.spec_from_file_location(
    "selection", Path(__file__).with_name("release-only.py")
)
selection = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selection)


def eligible(pr, repo):
    return (
        pr["state"] == "open"
        and not pr["draft"]
        and pr["base"]["ref"] == "main"
        and pr["head"]["ref"] == "release-please--branches--main"
        and pr["head"]["repo"] is not None
        and pr["head"]["repo"]["full_name"] == repo
        and any(label["name"] == "autorelease: pending" for label in pr["labels"])
    )


def main():
    repo = os.environ["GH_REPO"]
    prs = json.loads(
        subprocess.check_output(
            [
                "gh",
                "api",
                "--method",
                "GET",
                f"repos/{repo}/pulls",
                "-f",
                "state=open",
                "-f",
                "base=main",
                "-f",
                f"head={repo.split('/')[0]}:release-please--branches--main",
            ],
            text=True,
        )
    )
    for pr in prs:
        if not eligible(pr, repo):
            continue
        base, head = pr["base"]["sha"], pr["head"]["sha"]
        subprocess.run(["git", "fetch", "origin", base, head], check=True)
        ancestor = subprocess.check_output(
            ["git", "merge-base", base, head], text=True
        ).strip()
        if not selection.release_only(ancestor, head):
            print(
                f"PR #{pr['number']} includes non-release changes; leaving merge manual"
            )
            continue
        subprocess.run(
            [
                "gh",
                "pr",
                "merge",
                str(pr["number"]),
                "--repo",
                repo,
                "--auto",
                "--squash",
                "--match-head-commit",
                head,
            ],
            check=True,
        )


if __name__ == "__main__":
    main()
