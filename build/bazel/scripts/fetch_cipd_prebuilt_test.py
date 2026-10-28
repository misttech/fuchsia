#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for fetch_cipd_prebuilt.py."""

import stat
import tempfile
import unittest
from pathlib import Path

from build.bazel.scripts import fetch_cipd_prebuilt


class FetchCipdPrebuiltTest(unittest.TestCase):
    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self._temp_dir.name)
        self.fuchsia_dir = self.root / "fuchsia"
        self.jiri_bin = self.fuchsia_dir / ".jiri_root" / "bin" / "jiri"
        self.jiri_bin.parent.mkdir(parents=True, exist_ok=True)
        self.args_log = self.root / "jiri_args.txt"
        self._write_fake_jiri(exit_code=0)

    def tearDown(self) -> None:
        self._temp_dir.cleanup()

    def _write_fake_jiri(self, exit_code: int = 0) -> None:
        script = f"""#!/bin/sh
echo "$@" > "{self.args_log}"
exit {exit_code}
"""
        self.jiri_bin.write_text(script)
        self.jiri_bin.chmod(
            self.jiri_bin.stat().st_mode
            | stat.S_IXUSR
            | stat.S_IXGRP
            | stat.S_IXOTH
        )

    def test_single_file_output(self) -> None:
        prebuilt_file = self.fuchsia_dir / "prebuilt" / "foo" / "bar.bin"
        prebuilt_file.parent.mkdir(parents=True, exist_ok=True)
        prebuilt_file.write_text("binary-content")

        out_file = self.root / "bazel-out" / "bar.bin"
        rc = fetch_cipd_prebuilt.main(
            [
                "--fuchsia-dir",
                str(self.fuchsia_dir),
                "--path",
                "//prebuilt/foo/bar.bin",
                "--output-file",
                str(out_file),
            ]
        )
        self.assertEqual(rc, 0)
        self.assertEqual(
            self.args_log.read_text().strip(),
            "fetch-package prebuilt/foo/bar.bin",
        )
        self.assertTrue(out_file.is_file())
        self.assertEqual(out_file.read_text(), "binary-content")

    def test_directory_output(self) -> None:
        prebuilt_dir = self.fuchsia_dir / "prebuilt" / "drivers" / "bundle"
        (prebuilt_dir / "sub").mkdir(parents=True, exist_ok=True)
        (prebuilt_dir / "config.json").write_text('{"name": "bundle"}')
        (prebuilt_dir / "sub" / "driver.far").write_text("far-data")
        (prebuilt_dir / "sub" / "driver_link.far").symlink_to("driver.far")

        out_dir = self.root / "bazel-out" / "bundle"
        rc = fetch_cipd_prebuilt.main(
            [
                "--fuchsia-dir",
                str(self.fuchsia_dir),
                "--path",
                "prebuilt/drivers/bundle",
                "--output-dir",
                str(out_dir),
            ]
        )
        self.assertEqual(rc, 0)
        self.assertEqual(
            self.args_log.read_text().strip(),
            "fetch-package prebuilt/drivers/bundle",
        )
        self.assertTrue((out_dir / "config.json").is_file())
        self.assertEqual(
            (out_dir / "config.json").read_text(), '{"name": "bundle"}'
        )
        self.assertEqual(
            (out_dir / "sub" / "driver.far").read_text(), "far-data"
        )
        self.assertTrue((out_dir / "sub" / "driver_link.far").is_symlink())
        self.assertEqual(
            (out_dir / "sub" / "driver_link.far").readlink(),
            Path("driver.far"),
        )

    def test_invalid_path_rejected(self) -> None:
        out_file = self.root / "bazel-out" / "bar.bin"
        rc = fetch_cipd_prebuilt.main(
            [
                "--fuchsia-dir",
                str(self.fuchsia_dir),
                "--path",
                "src/not_prebuilt/bar.bin",
                "--output-file",
                str(out_file),
            ]
        )
        self.assertNotEqual(rc, 0)
        self.assertFalse(self.args_log.exists())

    def test_jiri_failure_propagated(self) -> None:
        self._write_fake_jiri(exit_code=2)
        out_file = self.root / "bazel-out" / "bar.bin"
        rc = fetch_cipd_prebuilt.main(
            [
                "--fuchsia-dir",
                str(self.fuchsia_dir),
                "--path",
                "prebuilt/foo/bar.bin",
                "--output-file",
                str(out_file),
            ]
        )
        self.assertEqual(rc, 2)
        self.assertFalse(out_file.exists())

    def test_missing_jiri_root(self) -> None:
        empty_dir = self.root / "empty"
        empty_dir.mkdir()
        out_file = self.root / "bazel-out" / "bar.bin"
        rc = fetch_cipd_prebuilt.main(
            [
                "--fuchsia-dir",
                str(empty_dir),
                "--path",
                "prebuilt/foo/bar.bin",
                "--output-file",
                str(out_file),
            ]
        )
        self.assertEqual(rc, 1)


if __name__ == "__main__":
    unittest.main()
