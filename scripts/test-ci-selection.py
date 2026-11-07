#!/usr/bin/env python3
"""Exercise CI routing without running builds or contacting GitHub."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "selection", Path(__file__).with_name("release-only.py")
)
selection = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selection)


class SelectionTests(unittest.TestCase):
    def route(self, paths, release_pr=False, release_only=False):
        with (
            patch.object(
                selection.subprocess, "check_output", return_value="ancestor\n"
            ),
            patch.object(selection, "changed_paths", return_value=paths),
            patch.object(selection, "release_only", return_value=release_only),
        ):
            return selection.requires_build("base", "head", release_pr)

    def test_ci_and_documentation_skip_build(self):
        self.assertFalse(
            self.route(
                [
                    ".github/workflows/check.yml",
                    "packaging/COPR.md",
                    "scripts/release-only.py",
                    "scripts/test-ci-selection.py",
                ]
            )
        )

    def test_runtime_build_and_unknown_paths_require_build(self):
        for path in [
            "crates/vigild/src/main.rs",
            "Cargo.lock",
            "Cargo.toml",
            "packaging/vigil.spec",
            "packaging/vigil.service",
            "scripts/install.sh",
            "scripts/smoke.py",
            "justfile",
            "config.example.toml",
            "new-file",
        ]:
            with self.subTest(path=path):
                self.assertTrue(self.route(["README.md", path]))

    def test_release_exception_requires_metadata_and_identity(self):
        self.assertFalse(self.route(["VERSION"], True, True))
        self.assertTrue(self.route(["VERSION"], False, True))
        self.assertTrue(self.route(["packaging/vigil.spec"], True, False))

    def test_package_behavior_is_not_release_metadata(self):
        before = "Version: 0.2.0\nRelease: 1\n"
        self.assertEqual(
            selection.normalized("packaging/vigil.spec", before),
            selection.normalized(
                "packaging/vigil.spec", before.replace("0.2.0", "0.3.0")
            ),
        )
        self.assertNotEqual(
            selection.normalized("packaging/vigil.spec", before),
            selection.normalized(
                "packaging/vigil.spec", before.replace("Release: 1", "Release: 2")
            ),
        )

    def test_dependency_versions_are_not_release_metadata(self):
        before = (
            '[[package]]\nname = "dependency"\nversion = "1.0.0"\nsource = "registry"\n'
        )
        self.assertNotEqual(
            selection.normalized("Cargo.lock", before),
            selection.normalized("Cargo.lock", before.replace("1.0.0", "2.0.0")),
        )


if __name__ == "__main__":
    unittest.main()
