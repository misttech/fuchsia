#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for casfs.py."""

import contextlib
import io
import pathlib
import unittest
from unittest import mock

from build.rbe import casfs


class CasfsTest(unittest.TestCase):
    """Tests for RBE casfs configuration loading, CLI, and boolean parsing."""

    def test_casfs_config_to_env_dict(self) -> None:
        self.assertEqual(
            casfs.CasfsConfig(virtualize=False).to_env_dict(),
            {casfs.ENV_CASFS_VIRTUALIZE: "0"},
        )
        self.assertEqual(
            casfs.CasfsConfig(virtualize=True).to_env_dict(),
            {casfs.ENV_CASFS_VIRTUALIZE: "1"},
        )

    def test_load_from_config_file(self) -> None:
        test_cases = [
            ("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=1\n", True),
            ("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=0\n", False),
            ("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=true\n", True),
            ("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=false\n", False),
            ("export FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=yes\n", True),
            ("# comment\nFX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=no\n", False),
            ("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=invalid\n", False),
            ("unrelated=1\n", False),
        ]
        source_dir = pathlib.Path("/tmp/fuchsia")
        for content, expected in test_cases:
            with mock.patch.object(
                pathlib.Path, "exists", return_value=True
            ), mock.patch.object(
                pathlib.Path, "read_text", return_value=content
            ):
                cfg = casfs.load(source_dir, {})
                self.assertEqual(
                    cfg.virtualize, expected, f"Failed parsing: {content!r}"
                )

    def test_load_missing_file(self) -> None:
        with mock.patch.object(pathlib.Path, "exists", return_value=False):
            cfg = casfs.load(pathlib.Path("/tmp/fuchsia"), {})
            self.assertFalse(cfg.virtualize)

    def test_load_env_precedence(self) -> None:
        source_dir = pathlib.Path("/tmp/fuchsia")
        with mock.patch.object(
            pathlib.Path, "exists", return_value=True
        ), mock.patch.object(
            pathlib.Path,
            "read_text",
            return_value="FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE=1\n",
        ):
            self.assertTrue(casfs.load(source_dir, {}).virtualize)
            # Empty env value falls back to config file
            self.assertTrue(
                casfs.load(
                    source_dir, {casfs.ENV_CASFS_VIRTUALIZE: ""}
                ).virtualize
            )
            # Explicit false/0 in env overrides config file
            self.assertFalse(
                casfs.load(
                    source_dir, {casfs.ENV_CASFS_VIRTUALIZE: "0"}
                ).virtualize
            )
            self.assertFalse(
                casfs.load(
                    source_dir, {casfs.ENV_CASFS_VIRTUALIZE: "false"}
                ).virtualize
            )
            # Invalid env value falls back to config file preference
            self.assertTrue(
                casfs.load(
                    source_dir, {casfs.ENV_CASFS_VIRTUALIZE: "invalid"}
                ).virtualize
            )

    def test_main_cli(self) -> None:
        with mock.patch.object(pathlib.Path, "exists", return_value=False):
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                rc = casfs.main(
                    ["--source-dir", "/tmp/fuchsia"],
                    {casfs.ENV_CASFS_VIRTUALIZE: "true"},
                )
            self.assertEqual(rc, 0)
            self.assertEqual(stdout.getvalue().strip(), "1")


if __name__ == "__main__":
    unittest.main()
