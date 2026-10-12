#!/usr/bin/env python3
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import tempfile
import unittest
from pathlib import Path
from unittest import mock

import minimal_workspace
import remote_services_utils


class MainTests(unittest.TestCase):
    def test_generate(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            topdir = td_path / "topdir"
            fuchsia_dir = td_path / "fuchsia"
            bazel_bin = td_path / "bazel_bin"
            bazel_bin.touch()

            # Create mock source directory tree expected by minimal_workspace
            (
                fuchsia_dir / "third_party" / "bazel_vendor" / "bazel_skylib+"
            ).mkdir(parents=True)
            (fuchsia_dir / "LICENSE").touch()

            with mock.patch.object(
                remote_services_utils,
                "generate_remote_services_bazelrc",
                return_value=set(),
            ) as mock_remote_services:
                status = minimal_workspace.main(
                    [
                        f"--topdir={topdir}",
                        f"--fuchsia-dir={fuchsia_dir}",
                        f"--bazel-bin={bazel_bin}",
                    ]
                )

            self.assertEqual(status, 0)
            mock_remote_services.assert_called_once()
            self.assertTrue((topdir / "bazel").is_symlink())
            self.assertTrue(
                (
                    topdir
                    / "workspace"
                    / "fuchsia_build_generated"
                    / "args.json"
                ).is_file()
            )

            bazelrc_path = topdir / "workspace" / ".bazelrc"
            self.assertTrue(bazelrc_path.is_file())
            bazelrc_content = bazelrc_path.read_text()
            self.assertIn("import remote_services.bazelrc", bazelrc_content)
            self.assertIn(
                "--platforms=//build/bazel/platforms:common", bazelrc_content
            )
            self.assertIn(
                "--host_platform=//build/bazel/platforms:host", bazelrc_content
            )


if __name__ == "__main__":
    unittest.main()
