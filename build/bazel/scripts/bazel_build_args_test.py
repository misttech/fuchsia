# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for bazel_build_args.py."""

import io
import os
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import bazel_build_args


class ParseAqueryOutputTest(unittest.TestCase):
    def test_parse_aquery_no_filter(self) -> None:
        aquery_output = """
{
  "targets": [
    { "id": 1, "label": "//src:main" },
    { "id": 2, "label": "//src:lib" }
  ],
  "configuration": [
    { "id": 1, "mnemonic": "k8-fastbuild" }
  ],
  "actions": [
    {
      "targetId": 1,
      "mnemonic": "CppCompile",
      "configurationId": 1,
      "arguments": ["exec", "clang", "-c", "src/main.cc", "-o", "main.o"],
      "environmentVariables": [
        { "key": "PATH", "value": "/bin" }
      ]
    },
    {
      "targetId": 1,
      "mnemonic": "CppLink",
      "configurationId": 1,
      "arguments": ["exec", "clang", "main.o", "-o", "bin"]
    }
  ]
}
"""
        result = bazel_build_args.parse_aquery_output(aquery_output)

        self.assertEqual(len(result.actions), 2)
        self.assertEqual(result.actions[0].mnemonic, "CppCompile")
        self.assertEqual(result.actions[0].target, "//src:main")
        self.assertEqual(result.actions[0].env_vars, {"PATH": "/bin"})
        self.assertListEqual(
            result.actions[0].args,
            [
                "exec",
                "clang",
                "-c",
                "src/main.cc",
                "-o",
                "main.o",
            ],
        )

        self.assertEqual(result.actions[1].mnemonic, "CppLink")
        self.assertListEqual(
            result.actions[1].args,
            [
                "exec",
                "clang",
                "main.o",
                "-o",
                "bin",
            ],
        )

    def test_parse_aquery_with_mnemonic_filter(self) -> None:
        aquery_output = """
{
  "targets": [
    { "id": 1, "label": "//src:main" },
    { "id": 2, "label": "//src:lib" }
  ],
  "configuration": [
    { "id": 1, "mnemonic": "k8-fastbuild" }
  ],
  "actions": [
    {
      "targetId": 1,
      "mnemonic": "CppCompile",
      "configurationId": 1,
      "arguments": ["exec", "clang", "-c", "src/main.cc"]
    },
    {
      "targetId": 2,
      "mnemonic": "CppLink",
      "configurationId": 1,
      "arguments": ["exec", "clang", "main.o", "-o", "bin"]
    }
  ]
}
"""
        result = bazel_build_args.parse_aquery_output(
            aquery_output,
            filter_mnemonics=["CppCompile"],
        )

        self.assertEqual(len(result.actions), 1)
        self.assertEqual(result.actions[0].mnemonic, "CppCompile")

    def test_parse_aquery_malformed_json_returns_empty(self) -> None:
        suppressed_stderr = io.StringIO()
        orig_stderr = sys.stderr
        sys.stderr = suppressed_stderr
        try:
            result = bazel_build_args.parse_aquery_output("malformed { json }")
        finally:
            sys.stderr = orig_stderr

        self.assertEqual(len(result.actions), 0)


class ExpandArgsFromDiskTest(unittest.TestCase):
    def test_recursive_disk_expansion(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            execroot = Path(tmp_dir)

            # Create a standard .params file
            params_file = execroot / "target.params"
            params_file.write_text("-DCOMPILE_FLAG\n@nested.flags\n")

            # Create a nested custom flags file
            nested_file = execroot / "nested.flags"
            nested_file.write_text("-DNESTED_FLAG\n--env-file\nenv.vars\n")

            # Create an env file
            env_file = execroot / "env.vars"
            env_file.write_text("KEY=VAL\n")

            raw_args = [
                "exec",
                "clang",
                "@target.params",
                "-o",
                "output",
            ]

            result = bazel_build_args.expand_args_from_disk(
                raw_args, {}, str(execroot)
            )

            self.assertListEqual(
                result.expanded_args,
                [
                    "exec",
                    "clang",
                    "-DCOMPILE_FLAG",
                    "-DNESTED_FLAG",
                    "-o",
                    "output",
                ],
            )
            self.assertListEqual(result.env_vars, ["KEY=VAL"])
            self.assertEqual(len(result.warnings), 0)

    def test_cycle_detection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            execroot = Path(tmp_dir)

            # Create a cycling params file
            params_file = execroot / "cycle.params"
            params_file.write_text("@cycle.params\n")

            raw_args = ["@cycle.params"]

            result = bazel_build_args.expand_args_from_disk(
                raw_args, {}, str(execroot)
            )

            self.assertListEqual(result.expanded_args, ["@cycle.params"])
            self.assertListEqual(result.env_vars, [])
            self.assertEqual(len(result.warnings), 1)
            self.assertEqual(
                result.warnings[0],
                "Cycle detected in response file @cycle.params: cycle.params -> cycle.params",
            )

    def test_missing_file_reports_warning(self) -> None:
        result = bazel_build_args.expand_args_from_disk(
            ["@missing.params"], {}, "/non_existent_root"
        )

        self.assertListEqual(result.expanded_args, ["@missing.params"])
        self.assertListEqual(result.env_vars, [])
        self.assertEqual(len(result.warnings), 1)
        self.assertIn("was not found on disk", result.warnings[0])

    def test_safe_path_normalization_prevents_false_positives(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            execroot = Path(tmp_dir)

            pwd_sub_dir = execroot / "mypwd"
            pwd_sub_dir.mkdir()

            nested_file = pwd_sub_dir / "target.params"
            nested_file.write_text("-DCOMPILE_FLAG\n")

            raw_args = ["@mypwd/target.params"]

            result = bazel_build_args.expand_args_from_disk(
                raw_args, {}, str(execroot)
            )

            self.assertListEqual(result.expanded_args, ["-DCOMPILE_FLAG"])
            self.assertListEqual(result.env_vars, [])
            self.assertEqual(len(result.warnings), 0)

    def test_normalize_path_strips_env_pwd_but_keeps_folder_pwd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            execroot = Path(tmp_dir)

            # Directory named "pwd" should be kept
            pwd_dir = execroot / "pwd"
            pwd_dir.mkdir()

            file_in_pwd = pwd_dir / "target.params"
            file_in_pwd.write_text("-DPWD_FOLDER_FLAG\n")

            # File in root to be found via stripped ${pwd}/ prefix
            file_in_root = execroot / "target.params"
            file_in_root.write_text("-DROOT_FLAG\n")

            # Test that ${pwd}/ prefix gets stripped and targets file in root
            result_stripped = bazel_build_args.expand_args_from_disk(
                ["@${pwd}/target.params"], {}, str(execroot)
            )
            self.assertListEqual(result_stripped.expanded_args, ["-DROOT_FLAG"])

            # Test that pwd/ prefix is NOT stripped and targets file in pwd/ directory
            result_not_stripped = bazel_build_args.expand_args_from_disk(
                ["@pwd/target.params"], {}, str(execroot)
            )
            self.assertListEqual(
                result_not_stripped.expanded_args, ["-DPWD_FOLDER_FLAG"]
            )


if __name__ == "__main__":
    unittest.main()
