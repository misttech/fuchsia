#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from collections.abc import Iterable
from unittest import mock

import list_packages


class TestPrintPackages(unittest.TestCase):
    def validate(self, input: Iterable[str], output: str) -> None:
        f = io.StringIO()
        with contextlib.redirect_stdout(f):
            list_packages.print_packages(input)

        self.assertEqual(f.getvalue(), output)

    def test_single_package(self) -> None:
        self.validate(["package"], "package\n")

    def test_multiple_packages(self) -> None:
        self.validate(
            ["package0", "package2", "package1", "package3"],
            """package0
package1
package2
package3
""",
        )


class TestExtractPackages(unittest.TestCase):
    def test_extract_packages_from_listing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest1_path = os.path.join(temp_dir, "pkg1_manifest.json")
            manifest2_path = os.path.join(temp_dir, "pkg2_manifest.json")
            with open(manifest1_path, "w") as f:
                json.dump({"package": {"name": "alpha"}}, f)
            with open(manifest2_path, "w") as f:
                json.dump({"package": {"name": "beta"}}, f)

            listing = {
                "content": {
                    "manifests": ["pkg1_manifest.json", "pkg2_manifest.json"]
                }
            }
            packages = list(
                list_packages.extract_packages_from_listing(
                    listing, lambda s: True, build_dir=temp_dir
                )
            )
            self.assertEqual(packages, ["alpha", "beta"])


class TestMainErrorHandling(unittest.TestCase):
    @mock.patch.dict(os.environ, {}, clear=True)
    @mock.patch.object(sys, "argv", ["list_packages.py"])
    def test_unset_fuchsia_build_dir(self) -> None:
        with self.assertRaises(RuntimeError) as cm:
            list_packages.main()
        self.assertEqual(
            str(cm.exception),
            'Environment variable "FUCHSIA_BUILD_DIR" is not set.',
        )

    @mock.patch.object(sys, "argv", ["list_packages.py"])
    def test_missing_manifest_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with mock.patch.dict(os.environ, {"FUCHSIA_BUILD_DIR": temp_dir}):
                manifest_list_path = os.path.join(
                    temp_dir, "all_package_manifests.list"
                )
                with self.assertRaises(RuntimeError) as cm:
                    list_packages.main()
                self.assertEqual(
                    str(cm.exception),
                    f"'{manifest_list_path}' not found. Run 'fx build' or 'fx build updates' to assemble package manifests.",
                )


class TestMain(unittest.TestCase):
    @mock.patch.object(sys, "argv", ["list_packages.py"])
    def test_main_success(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest_path = os.path.join(temp_dir, "pkg_manifest.json")
            with open(manifest_path, "w") as f:
                json.dump({"package": {"name": "my-package"}}, f)
            manifest_list_path = os.path.join(
                temp_dir, "all_package_manifests.list"
            )
            with open(manifest_list_path, "w") as f:
                json.dump({"content": {"manifests": ["pkg_manifest.json"]}}, f)

            f = io.StringIO()
            with mock.patch.dict(os.environ, {"FUCHSIA_BUILD_DIR": temp_dir}):
                with contextlib.redirect_stdout(f):
                    list_packages.main()
            self.assertEqual(f.getvalue(), "my-package\n")


if __name__ == "__main__":
    unittest.main()
