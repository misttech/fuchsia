#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT_DIR = Path(__file__).parent
sys.path.insert(0, str(_SCRIPT_DIR))

import bazel_tests_utils
from build_utils import BazelPaths, MockCommandRunner


class BazelTestsUtilsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._td.name) / "fuchsia"
        self.fuchsia_dir.mkdir()
        (self.fuchsia_dir / ".jiri_manifest").write_text("")

        self.build_dir = self.fuchsia_dir / "out" / "build_dir"
        self.build_dir.mkdir(parents=True)

        BazelPaths.write_topdir_config_for_test(self.fuchsia_dir, "gen/bazel")
        self.bazel_paths = BazelPaths(self.fuchsia_dir, self.build_dir)

        # Create the directories that BazelPaths properties expect
        self.bazel_paths.launcher.parent.mkdir(parents=True, exist_ok=True)
        self.bazel_paths.launcher.write_text("#!/bin/bash\nexit 0")
        self.bazel_paths.execroot.mkdir(parents=True, exist_ok=True)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_generate_tests_json(self) -> None:
        mock_runner = MockCommandRunner()

        # Create some fake test info that matches what cquery would return
        test_info = {
            "name": "my_test",
            "label": "//src/my_test:my_test",
            "launcher_execroot_path": "bin/my_test",
            "runtime_deps_json_execroot_path": "bin/my_test.runtime_deps.json",
            "os": "linux",
            "cpu": "x64",
        }

        mock_runner.push_result(stdout=json.dumps(test_info))

        tests_json = bazel_tests_utils.generate_tests_json(
            self.bazel_paths, command_runner=mock_runner
        )

        self.assertEqual(len(tests_json), 1)
        entry = tests_json[0]
        self.assertEqual(entry["test"]["name"], "my_test")
        self.assertEqual(entry["test"]["label"], "//src/my_test:my_test")

        # Verify path conversion
        # bazel_paths.execroot is fuchsia_dir/out/build_dir/gen/bazel/output_base/execroot/_main
        # path is relative to ninja_build_dir (fuchsia_dir/out/build_dir)
        expected_path = os.path.relpath(
            self.bazel_paths.execroot / "bin/my_test",
            self.bazel_paths.ninja_build_dir,
        )
        self.assertEqual(entry["test"]["path"], expected_path)

        expected_deps_path = os.path.relpath(
            self.bazel_paths.execroot / "bin/my_test.runtime_deps.json",
            self.bazel_paths.ninja_build_dir,
        )
        self.assertEqual(entry["test"]["runtime_deps_path"], expected_deps_path)

    def test_generate_tests_json_multiple_entries(self) -> None:
        mock_runner = MockCommandRunner()
        test1 = {
            "name": "test1",
            "label": "//t1",
            "launcher_execroot_path": "p1",
            "runtime_deps_json_execroot_path": "d1",
            "os": "linux",
            "cpu": "x64",
        }
        test2 = {
            "name": "test2",
            "label": "//t2",
            "launcher_execroot_path": "p2",
            "runtime_deps_json_execroot_path": "d2",
            "os": "linux",
            "cpu": "x64",
        }

        mock_runner.push_result(
            stdout=json.dumps(test1) + "\n" + json.dumps(test2)
        )

        tests_json = bazel_tests_utils.generate_tests_json(
            self.bazel_paths, command_runner=mock_runner
        )

        self.assertEqual(len(tests_json), 2)
        self.assertEqual(tests_json[0]["test"]["name"], "test1")
        self.assertEqual(tests_json[1]["test"]["name"], "test2")

    def test_generate_tests_json_failure(self) -> None:
        mock_runner = MockCommandRunner()
        mock_runner.push_result(returncode=1, stderr="Bazel error")

        with self.assertRaisesRegex(
            RuntimeError, "Failed to run bazel query: Bazel error"
        ):
            bazel_tests_utils.generate_tests_json(
                self.bazel_paths, command_runner=mock_runner
            )


if __name__ == "__main__":
    unittest.main()
