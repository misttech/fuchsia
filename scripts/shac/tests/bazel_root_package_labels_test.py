#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_root_package_labels."""

import io
import json
import pathlib
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Add checks directory so root_package_labels can be imported directly.
sys.path.insert(
    0,
    str(
        pathlib.Path(__file__).resolve().parent.parent / "bazel_lint" / "checks"
    ),
)

import root_package_labels as checker


class TestBazelRootPackageLabels(unittest.TestCase):
    """Tests for bazel_root_package_labels."""

    def test_get_correct_label(self) -> None:
        self.assertEqual(
            checker._get_correct_label("//:foo/bar:baz"),
            ("//foo/bar:baz", "foo/bar"),
        )
        self.assertEqual(
            checker._get_correct_label("//:foo/bar/baz"),
            ("//foo/bar:baz", "foo/bar"),
        )

    def test_is_label_permitted_workspace_root_exports(self) -> None:
        self.assertTrue(
            checker._is_label_permitted(
                "//:prebuilt/third_party/clang/bin/clang", "src/BUILD.bazel"
            )
        )
        self.assertTrue(
            checker._is_label_permitted(
                "//:fuchsia_build_generated/jiri_snapshot.xml",
                "build/bazel/toplevel.BUILD.bazel",
            )
        )
        self.assertFalse(
            checker._is_label_permitted("//:foo/bar", "src/BUILD.bazel")
        )

    def test_is_label_permitted_external_repos(self) -> None:
        clang_file = "build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/repository_utils.bzl"
        rust_file = "build/bazel/toolchains/rust/rust.BUILD.bazel"
        update_rust_file = "build/bazel/update-rustc-third-party/BUILD.bazel"
        sdk_file = "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/workspace/sdk_templates/fuchsia_sdk.BUILD.bazel"

        self.assertTrue(checker._is_label_permitted("//:bin/clang", clang_file))
        self.assertTrue(checker._is_label_permitted("//:bin/rustc", rust_file))
        self.assertTrue(
            checker._is_label_permitted(
                "//:rust_toolchain/bin/cargo", update_rust_file
            )
        )
        self.assertTrue(
            checker._is_label_permitted("//:meta/manifest.json", sdk_file)
        )
        self.assertTrue(
            checker._is_label_permitted("//:arch/x64/sysroot", sdk_file)
        )

        # Disallowed in a normal file
        self.assertFalse(
            checker._is_label_permitted("//:bin/clang", "src/BUILD.bazel")
        )

    def test_check_file_contents_detects_disallowed_labels(self) -> None:
        content = """# Comment line //:foo/bar/baz
load("//:foo/bar.bzl", "symbol")
normal_target = "//src/lib:lib"
valid_root = "//:license"
external_ref = "@prebuilt_clang//:bin/clang"
src = '//:bar/baz' # comment //:foo/bar
my_rule(url = "https://example.com/#readme", dep = "//:baz/qux")
my_rule(url = "https://example.com/#readme") # dep = "//:skipped/in/comment"
my_rule(url = 'https://example.com/#doc', tool = '//:tools/helper')
my_rule(val = 'foo "#" bar', extra = "//:extra/target")
"""
        findings = checker._check_file_contents(content, "src/BUILD.bazel")
        self.assertEqual(len(findings), 5)

        self.assertEqual(findings[0]["line"], 2)
        self.assertEqual(
            findings[0]["message"],
            "Disallowed workspace root package label: `//:foo/bar.bzl`. "
            "Use `//foo:bar.bzl` instead, and add a `BUILD.bazel` file to `//foo` if needed.",
        )

        self.assertEqual(findings[1]["line"], 6)
        self.assertEqual(
            findings[1]["message"],
            "Disallowed workspace root package label: `//:bar/baz`. "
            "Use `//bar:baz` instead, and add a `BUILD.bazel` file to `//bar` if needed.",
        )

        self.assertEqual(findings[2]["line"], 7)
        self.assertEqual(
            findings[2]["message"],
            "Disallowed workspace root package label: `//:baz/qux`. "
            "Use `//baz:qux` instead, and add a `BUILD.bazel` file to `//baz` if needed.",
        )

        self.assertEqual(findings[3]["line"], 9)
        self.assertEqual(
            findings[3]["message"],
            "Disallowed workspace root package label: `//:tools/helper`. "
            "Use `//tools:helper` instead, and add a `BUILD.bazel` file to `//tools` if needed.",
        )

        self.assertEqual(findings[4]["line"], 10)
        self.assertEqual(
            findings[4]["message"],
            "Disallowed workspace root package label: `//:extra/target`. "
            "Use `//extra:target` instead, and add a `BUILD.bazel` file to `//extra` if needed.",
        )

    def test_check_file_contents_syntax_error_returns_finding(self) -> None:
        content = "def unclosed_parenthesis("
        findings = checker._check_file_contents(content, "src/BUILD.bazel")
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["filepath"], "src/BUILD.bazel")
        self.assertEqual(findings[0]["line"], 1)
        self.assertEqual(findings[0]["col"], 25)
        self.assertEqual(findings[0]["level"], "error")
        self.assertIn("Syntax error:", findings[0]["message"])

    def test_check_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            file_path = Path(tmpdir) / "BUILD.bazel"
            file_path.write_text('tool = "//:foo/bar"\n')
            findings = checker.check_file(file_path)
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["line"], 1)
            self.assertEqual(
                findings[0]["message"],
                "Disallowed workspace root package label: `//:foo/bar`. "
                "Use `//foo:bar` instead, and add a `BUILD.bazel` file to `//foo` if needed.",
            )

    def test_check_file_with_root_permitted_external_repo(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            rel_path = "build/bazel/toolchains/rust/rust.BUILD.bazel"
            file_path = root / rel_path
            file_path.parent.mkdir(parents=True, exist_ok=True)
            file_path.write_text('tool = "//:bin/rustc"\n')
            findings = checker.check_file(file_path, root=root)
            self.assertEqual(findings, [])

    def test_cli_clean_returns_0_and_empty_json_list(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            file_path = Path(tmpdir) / "BUILD.bazel"
            file_path.write_text('tool = "//foo:bar"\n')
            with (
                mock.patch("sys.argv", ["checker", "--json", str(file_path)]),
                mock.patch(
                    "sys.stdout", new_callable=io.StringIO
                ) as mock_stdout,
            ):
                retcode = checker.main()
                self.assertEqual(retcode, 0)
                data = json.loads(mock_stdout.getvalue())
                self.assertIsInstance(data, list)
                self.assertEqual(data, [])

    def test_cli_findings_returns_1_and_json_list(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            file_path = Path(tmpdir) / "BUILD.bazel"
            file_path.write_text('tool = "//:foo/bar"\n')
            with (
                mock.patch("sys.argv", ["checker", "--json", str(file_path)]),
                mock.patch(
                    "sys.stdout", new_callable=io.StringIO
                ) as mock_stdout,
            ):
                retcode = checker.main()
                self.assertEqual(retcode, 1)
                data = json.loads(mock_stdout.getvalue())
                self.assertIsInstance(data, list)
                self.assertEqual(len(data), 1)
                self.assertEqual(
                    data[0]["message"],
                    "Disallowed workspace root package label: `//:foo/bar`. "
                    "Use `//foo:bar` instead, and add a `BUILD.bazel` file to `//foo` if needed.",
                )

    def test_get_correct_label_invalid_raises_value_error(self) -> None:
        with self.assertRaises(ValueError):
            checker._get_correct_label("//foo:bar")
        with self.assertRaises(ValueError):
            checker._get_correct_label("//:foo")

    def test_check_file_nonexistent_raises_file_not_found(self) -> None:
        with self.assertRaises(FileNotFoundError):
            checker.check_file("nonexistent/BUILD.bazel")

    def test_cli_error_returns_2(self) -> None:
        with (
            mock.patch("sys.argv", ["checker", "nonexistent/BUILD.bazel"]),
            mock.patch("sys.stderr", new_callable=io.StringIO) as mock_stderr,
        ):
            retcode = checker.main()
            self.assertEqual(retcode, 2)
            self.assertEqual(
                mock_stderr.getvalue(),
                "Error checking files: File not found: nonexistent/BUILD.bazel\n",
            )


if __name__ == "__main__":
    unittest.main()
