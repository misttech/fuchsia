#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for build/regenerator.py functions."""

import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

# Import regenerator.py as a module.
_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import regenerator


class ContentHashTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._dir = Path(self._td.name)
        self.source_dir = self._dir / "source"
        self.source_dir.mkdir()

        self.build_dir = self.source_dir / "out" / "not-default"
        self.build_dir.mkdir(parents=True)

        self.output_dir = self._dir / "output"
        self.output_dir.mkdir()

        (self.source_dir / "foo.txt").write_text("FOO")
        (self.source_dir / "subdir").mkdir()
        (self.source_dir / "subdir" / "blob").write_bytes(b"012345678")
        (self.source_dir / "subdir" / "script.sh").write_text(
            "#!/bin/sh\necho 42\n"
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def _test(
        self,
        content_hashes: list[dict[str, T.Any]],
        expected_input_paths: list[Path],
        expected_hash_filenames: list[str],
    ) -> None:
        input_paths = regenerator.generate_bazel_content_hash_files(
            self.source_dir, self.build_dir, self.output_dir, content_hashes
        )

        self.assertListEqual(sorted(input_paths), expected_input_paths)

        hash_filenames = sorted(os.listdir(self.output_dir))
        self.assertListEqual(hash_filenames, expected_hash_filenames)

    def test_generate_bazel_content_hash_files(self) -> None:
        _TEST_CASES: list[dict[str, T.Any]] = [
            {
                "name": "empty",
                "content_hashes": [],
                "expected_inputs": [],
                "expected_hash_filenames": [],
            },
            {
                "name": "single_file",
                "content_hashes": [
                    {
                        "source_paths": ["//foo.txt"],
                        "repo_name": "foo_repo",
                    }
                ],
                "expected_inputs": [self.source_dir / "foo.txt"],
                "expected_hash_filenames": ["foo_repo.hash"],
            },
            {
                "name": "single_directory",
                "content_hashes": [
                    {
                        "source_paths": ["//subdir"],
                        "repo_name": "subdir_repo",
                    }
                ],
                "expected_inputs": [
                    self.source_dir / "subdir" / "blob",
                    self.source_dir / "subdir" / "script.sh",
                ],
                "expected_hash_filenames": ["subdir_repo.hash"],
            },
            {
                "name": "file_and_directory",
                "content_hashes": [
                    {
                        "source_paths": ["//foo.txt", "//subdir"],
                        "repo_name": "file_and_subdir_repo",
                    }
                ],
                "expected_inputs": [
                    self.source_dir / "foo.txt",
                    self.source_dir / "subdir" / "blob",
                    self.source_dir / "subdir" / "script.sh",
                ],
                "expected_hash_filenames": ["file_and_subdir_repo.hash"],
            },
            {
                "name": "exclude_suffixes",
                "content_hashes": [
                    {
                        "source_paths": ["//foo.txt", "//subdir"],
                        "repo_name": "exclude_suffixes",
                        "exclude_suffixes": [".sh"],
                    }
                ],
                "expected_inputs": [
                    self.source_dir / "foo.txt",
                    self.source_dir / "subdir" / "blob",
                ],
                "expected_hash_filenames": ["exclude_suffixes.hash"],
            },
            {
                "name": "multiple_entries",
                "content_hashes": [
                    {
                        "source_paths": ["//foo.txt"],
                        "repo_name": "foo",
                    },
                    {
                        "source_paths": ["//subdir"],
                        "repo_name": "subdir",
                    },
                ],
                "expected_inputs": [
                    self.source_dir / "foo.txt",
                    self.source_dir / "subdir" / "blob",
                    self.source_dir / "subdir" / "script.sh",
                ],
                "expected_hash_filenames": ["foo.hash", "subdir.hash"],
            },
        ]
        for test_case in _TEST_CASES:
            output_dir = self.output_dir / test_case["name"]
            output_dir.mkdir()

            input_paths = regenerator.generate_bazel_content_hash_files(
                self.source_dir,
                self.build_dir,
                output_dir,
                test_case["content_hashes"],
            )

            msg = f"For {test_case['name']} case."
            self.assertListEqual(
                sorted(input_paths), test_case["expected_inputs"], msg=msg
            )

            hash_filenames = sorted(os.listdir(output_dir))
            self.assertListEqual(
                hash_filenames, test_case["expected_hash_filenames"], msg=msg
            )

    def test_interpret_gn_path(self) -> None:
        _TEST_CASES: list[dict[str, T.Any]] = [
            {
                "name": "source path",
                "gn_path": "//src/foo",
                "expected": self.source_dir / "src" / "foo",
            },
            {
                "name": "absolute path",
                "gn_path": str(self.source_dir / "absolute"),
                "expected": (self.source_dir / "absolute").resolve(),
            },
            {
                "name": "relative dir",
                "gn_path": "gen/foo.stamp",
                "expected": self.build_dir / "gen" / "foo.stamp",
            },
        ]
        for test_case in _TEST_CASES:
            self.assertEqual(
                regenerator.interpret_gn_path(
                    test_case["gn_path"], self.source_dir, self.build_dir
                ),
                test_case["expected"],
                msg=test_case["name"],
            )


class GnDiagnosticsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_unresolved_package_hyphen_mismatch(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text(
            'fuchsia_package("eval-test") {\n  deps = [ ":component" ]\n}\n'
        )

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages(//build/toolchain/fuchsia:x64)\n"
            "  needs //examples/components/eval_test:eval_test(//build/toolchain/fuchsia:x64)\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(
            hints,
            [
                'Hint: Found fuchsia_package("eval-test"). In GN, directory labels '
                "resolve to ':eval_test'. Consider defining "
                'fuchsia_package("eval_test") { package_name = "eval-test" }'
            ],
        )

    def test_unresolved_package_ignores_comments(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text(
            '# fuchsia_package("eval-test")\n'
            "# fuchsia_package('eval-test')\n"
        )

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:eval_test\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(hints, [])

    def test_unresolved_package_no_mismatch(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text(
            'fuchsia_package("eval_test") {\n  package_name = "eval-test"\n}\n'
        )

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:eval_test\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(hints, [])

    def test_unresolved_package_non_directory_target(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text('fuchsia_package("eval-test") {}\n')

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:other_target\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(hints, [])

    def test_unresolved_package_deduplicates_hints(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text('fuchsia_package("eval-test") {}\n')

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:target_a\n"
            "  needs //examples/components/eval_test:eval_test(//build/toolchain:x64)\n"
            "//:target_b\n"
            "  needs //examples/components/eval_test:eval_test(//build/toolchain:x64)\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(len(hints), 1)

    def test_unresolved_group_target(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text('group("eval-test") {}\n')

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:eval_test\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(
            hints,
            [
                'Hint: Found group("eval-test"). In GN, directory labels '
                "resolve to ':eval_test'. Consider defining group(\"eval_test\")"
            ],
        )

    def test_unresolved_explicit_target_label(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text('group("sub-target") {}\n')

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:sub_target\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(
            hints,
            [
                'Hint: Found group("sub-target"). Consider defining '
                'group("sub_target")'
            ],
        )

    def test_unresolved_target_ignores_non_target_keywords(self) -> None:
        pkg_dir = self.fuchsia_dir / "examples" / "components" / "eval_test"
        pkg_dir.mkdir(parents=True)
        build_gn = pkg_dir / "BUILD.gn"
        build_gn.write_text(
            'import("//examples/components/eval_test/eval-test.gni")\n'
        )

        gn_output = (
            "ERROR Unresolved dependencies.\n"
            "//:developer_universe_packages\n"
            "  needs //examples/components/eval_test:eval_test\n"
        )

        hints = regenerator.check_gn_unresolved_package_dependencies(
            gn_output, self.fuchsia_dir
        )
        self.assertEqual(hints, [])


if __name__ == "__main__":
    unittest.main()
