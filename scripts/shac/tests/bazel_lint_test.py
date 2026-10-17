#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the bazel_lint framework."""

import ast
import io
import json
import pathlib
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Add scripts/shac/bazel_lint so modules can be imported directly when run
# outside GN's python_host_test archive.
sys.path.insert(
    0,
    str(pathlib.Path(__file__).resolve().parent.parent / "bazel_lint"),
)

import allowlist as allowlist_mod
import framework
import main


class SimpleCheckForTests(framework.BazelCheck):
    """Simple BazelCheck implementation for unit testing the framework."""

    name = "simple_check_for_tests"
    file_kinds = frozenset(
        {
            framework.FileKind.BUILD,
            framework.FileKind.BZL,
            framework.FileKind.MODULE,
            framework.FileKind.WORKSPACE,
        }
    )
    policy_exempt_globs = ("**/some_other_repo/**", "tools/**")

    def check_string(
        self, fc: framework.FileContext, s: framework.StringInfo
    ) -> None:
        if s.is_docstring:
            return
        if s.value.startswith("//:bad/"):
            fc.report_string(
                f"Disallowed label: {s.value}",
                s.node,
                s.value,
                level="error",
                url=None,
            )


class TestAllowlist(unittest.TestCase):
    """Tests for bazel_lint.allowlist."""

    def test_missing_allowlist_file_returns_empty(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            al = allowlist_mod.load_allowlist(
                Path(tmpdir) / "nonexistent_check.json",
                supports_targets_allowlist=False,
            )
            self.assertEqual(al.entries, ())
            self.assertFalse(al.is_file_allowlisted("src/BUILD.bazel"))

    def test_valid_allowlist_matching(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            al_path = Path(tmpdir) / "my_check.json"
            al_path.write_text(
                json.dumps(
                    {
                        "entries": [
                            {
                                "bug": "https://fxbug.dev/570203009",
                                "comment": "Subtree exemption",
                                "path_prefix": "vendor/google/",
                            },
                            {
                                "bug": "https://fxbug.dev/570203009",
                                "path": "src/bar/BUILD.bazel",
                                "targets": ["allowed_target"],
                            },
                            {
                                "bug": "https://fxbug.dev/570203009",
                                "path": "src/whole_file/BUILD.bazel",
                            },
                        ]
                    }
                )
            )
            al = allowlist_mod.load_allowlist(
                al_path, supports_targets_allowlist=True
            )

            # path_prefix without targets exempts the whole subtree.
            self.assertTrue(
                al.is_file_allowlisted("vendor/google/bar/BUILD.bazel")
            )
            self.assertFalse(al.is_file_allowlisted("vendor/other/BUILD.bazel"))

            # path without targets exempts the whole file.
            self.assertTrue(
                al.is_file_allowlisted("src/whole_file/BUILD.bazel")
            )

            # path with targets does NOT exempt the whole file, only specific targets.
            self.assertFalse(al.is_file_allowlisted("src/bar/BUILD.bazel"))
            self.assertTrue(
                al.is_finding_allowlisted(
                    "src/bar/BUILD.bazel", ("allowed_target",)
                )
            )
            self.assertFalse(
                al.is_finding_allowlisted(
                    "src/bar/BUILD.bazel", ("other_target",)
                )
            )
            self.assertFalse(
                al.is_finding_allowlisted("src/bar/BUILD.bazel", ())
            )

    def test_invalid_allowlist_schemas(self) -> None:
        cases = [
            (
                "missing bug",
                {"entries": [{"path": "a/BUILD.bazel"}]},
                False,
                "'bug' must be a string matching",
            ),
            (
                "bad bug url",
                {
                    "entries": [
                        {
                            "bug": "http://example.com/123",
                            "path": "a/BUILD.bazel",
                        }
                    ]
                },
                False,
                "'bug' must be a string matching",
            ),
            (
                "unknown key",
                {
                    "entries": [
                        {
                            "bug": "https://fxbug.dev/123",
                            "path": "a/BUILD.bazel",
                            "explanation": "wrong key name",
                        }
                    ]
                },
                False,
                "unknown keys",
            ),
            (
                "path_prefix without trailing slash",
                {
                    "entries": [
                        {
                            "bug": "https://fxbug.dev/123",
                            "path_prefix": "src/foo",
                        }
                    ]
                },
                False,
                "must be a non-empty directory prefix ending with '/'",
            ),
            (
                "targets on unsupported check",
                {
                    "entries": [
                        {
                            "bug": "https://fxbug.dev/123",
                            "path": "a/BUILD.bazel",
                            "targets": ["foo"],
                        }
                    ]
                },
                False,
                "'targets' is not supported for this check",
            ),
            (
                "no scope specified",
                {"entries": [{"bug": "https://fxbug.dev/123"}]},
                False,
                "must specify exactly one of 'path' or 'path_prefix'",
            ),
            (
                "disallowed vendor path",
                {
                    "entries": [
                        {
                            "bug": "https://fxbug.dev/123",
                            "path": "vendor/google/foo/BUILD.bazel",
                        }
                    ]
                },
                False,
                "starting with 'vendor' must be one of",
            ),
            (
                "disallowed vendor path_prefix",
                {
                    "entries": [
                        {
                            "bug": "https://fxbug.dev/123",
                            "path_prefix": "vendor/google/foo/",
                        }
                    ]
                },
                False,
                "starting with 'vendor' must be one of",
            ),
        ]
        for name, payload, supports_targets, expected_err in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                al_path = Path(tmp) / "test_check.json"
                al_path.write_text(json.dumps(payload))
                with self.assertRaisesRegex(
                    allowlist_mod.AllowlistError, expected_err
                ):
                    allowlist_mod.load_allowlist(
                        al_path,
                        supports_targets_allowlist=supports_targets,
                    )


class TestFramework(unittest.TestCase):
    """Tests for bazel_lint.framework and bazel_lint.main."""

    def test_classify_file(self) -> None:
        self.assertEqual(
            framework.classify_file("foo/BUILD.bazel"),
            framework.FileKind.BUILD,
        )
        self.assertEqual(
            framework.classify_file("foo/bar.BUILD.bazel"),
            framework.FileKind.BUILD,
        )
        self.assertEqual(
            framework.classify_file("foo/BUILD"),
            framework.FileKind.BUILD,
        )
        self.assertEqual(
            framework.classify_file("foo/bar.BUILD"),
            framework.FileKind.BUILD,
        )
        self.assertEqual(
            framework.classify_file("foo/defs.bzl"),
            framework.FileKind.BZL,
        )
        self.assertEqual(
            framework.classify_file("MODULE.bazel"),
            framework.FileKind.MODULE,
        )
        self.assertEqual(
            framework.classify_file("foo.MODULE.bazel"),
            framework.FileKind.MODULE,
        )
        self.assertEqual(
            framework.classify_file("WORKSPACE.bazel"),
            framework.FileKind.WORKSPACE,
        )
        self.assertEqual(
            framework.classify_file("WORKSPACE"),
            framework.FileKind.WORKSPACE,
        )
        self.assertEqual(
            framework.classify_file("WORKSPACE.bzlmod"),
            framework.FileKind.WORKSPACE,
        )
        self.assertEqual(
            framework.classify_file("foo.WORKSPACE.bazel"),
            framework.FileKind.WORKSPACE,
        )
        self.assertEqual(
            framework.classify_file("foo/script.py"),
            framework.FileKind.PYTHON,
        )
        for invalid_path in (
            "README.md",
            "foo/BUILD.gn",
            "foo.bzlmod",
            "foo.WORKSPACE",
            "foo.workspace",
        ):
            with self.subTest(invalid_path=invalid_path):
                with self.assertRaises(ValueError):
                    framework.classify_file(invalid_path)

    def test_file_kind_mismatch_raises_value_error(self) -> None:
        check = SimpleCheckForTests()
        with tempfile.TemporaryDirectory() as tmpdir:
            py_file = Path(tmpdir) / "foo.py"
            py_file.write_text("x = 1\n")
            with self.assertRaisesRegex(
                ValueError, "does not accept FileKind.PYTHON"
            ):
                main._check_file(
                    py_file, check=check, root=None, allowlist=None
                )

    def test_is_path_exempt_by_policy(self) -> None:
        check = SimpleCheckForTests()
        self.assertTrue(
            check.is_path_exempt_by_policy("some_other_repo/foo/BUILD.bazel")
        )
        self.assertTrue(
            check.is_path_exempt_by_policy(
                "vendor/some_other_repo/foo/BUILD.bazel"
            )
        )
        self.assertFalse(
            check.is_path_exempt_by_policy(
                "vendor/my_some_other_repo/foo/BUILD.bazel"
            )
        )
        self.assertTrue(check.is_path_exempt_by_policy("tools/foo/BUILD.bazel"))

    def test_register_check_python_overriding_check_load_raises(self) -> None:
        class InvalidPythonCheckLoad(framework.BazelCheck):
            name = "invalid_python_check_load"
            file_kinds = frozenset({framework.FileKind.PYTHON})

            def check_load(
                self, fc: framework.FileContext, load: framework.LoadInfo
            ) -> None:
                pass

        with self.assertRaisesRegex(
            TypeError,
            "checks accepting FileKind.PYTHON cannot override check_load",
        ):
            framework.register_check(InvalidPythonCheckLoad)

    def test_register_check_python_check_module_allowed(self) -> None:
        class ValidPythonCheckModule(framework.BazelCheck):
            name = "valid_python_check_module"
            file_kinds = frozenset({framework.FileKind.PYTHON})

            def check_module(self, fc: framework.FileContext) -> None:
                if not fc.lines:
                    fc.report("Empty file", level="warning", url=None)

        framework.register_check(ValidPythonCheckModule)
        self.assertIn("valid_python_check_module", framework.REGISTRY)
        del framework.REGISTRY["valid_python_check_module"]

    def test_docstring_and_comment_handling(self) -> None:
        content = '''# Full-line comment with "//:bad/comment"
"""Module docstring //:bad/module."""

class MyClass:
    """Class docstring //:bad/class."""

def my_func():
    """Function docstring //:bad/func."""
    return "//:bad/code"  # Inline comment with "//:bad/inline"
'''

        class AllStringsCheck(framework.BazelCheck):
            name = "all_strings_check"
            file_kinds = frozenset({framework.FileKind.PYTHON})

            def check_string(
                self, fc: framework.FileContext, s: framework.StringInfo
            ) -> None:
                fc.report_string(
                    f"Found {s.value} (is_docstring={s.is_docstring})",
                    s.node,
                    level="error",
                    url=None,
                )

        class SkipDocstringsCheck(framework.BazelCheck):
            name = "skip_docstrings_check"
            file_kinds = frozenset({framework.FileKind.PYTHON})

            def check_string(
                self, fc: framework.FileContext, s: framework.StringInfo
            ) -> None:
                if s.is_docstring:
                    return
                fc.report_string(
                    f"Found {s.value}", s.node, level="error", url=None
                )

        # Comments are stripped by the AST and never passed to check_string;
        # docstrings are included with is_docstring=True.
        all_findings = framework.run_check_on_contents(
            AllStringsCheck(), content, "foo/script.py"
        )
        self.assertEqual(
            [f["message"] for f in all_findings],
            [
                "Found Module docstring //:bad/module. (is_docstring=True)",
                "Found Class docstring //:bad/class. (is_docstring=True)",
                "Found Function docstring //:bad/func. (is_docstring=True)",
                "Found //:bad/code (is_docstring=False)",
            ],
        )

        non_docstring_findings = framework.run_check_on_contents(
            SkipDocstringsCheck(), content, "foo/script.py"
        )
        self.assertEqual(
            [f["message"] for f in non_docstring_findings],
            ["Found //:bad/code"],
        )

    def test_file_context_extraction_and_spans(self) -> None:
        content = """load("//build:defs.bzl", "my_macro", _alias = "other")

visibility("private")

my_macro(
    name = "target_a",
    srcs = ["//:bad/single_line"],
    msg = (
        "//:bad/multi"
        + "_line"
    ),
)

def _helper():
    nested_call()

if x == "//:ignored_in_compare":
    fail("//:ignored_in_fail")
"""
        tree = ast.parse(content, filename="src/defs.bzl")
        ctx = framework.FileContext(
            path="src/defs.bzl",
            kind=framework.FileKind.BZL,
            tree=tree,
            lines=content.splitlines(),
        )

        # Loads
        self.assertEqual(len(ctx.loads), 1)
        self.assertEqual(ctx.loads[0].module_label, "//build:defs.bzl")
        self.assertEqual(
            [(local, exported) for local, exported, _ in ctx.loads[0].symbols],
            [("my_macro", "my_macro"), ("_alias", "other")],
        )

        # Calls
        top_level_calls = [c for c in ctx.calls if c.is_top_level]
        self.assertEqual(
            [c.func_name for c in top_level_calls],
            ["visibility", "my_macro"],
        )
        macro_call = top_level_calls[1]
        self.assertEqual(macro_call.target_name, "target_a")
        self.assertIn("srcs", macro_call.keywords)

        # Strings context flags
        by_val = {s.value: s for s in ctx.strings}
        self.assertTrue(by_val["//build:defs.bzl"].is_load_source)
        self.assertFalse(by_val["//:bad/single_line"].is_load_source)
        self.assertEqual(
            by_val["//:bad/single_line"].enclosing_call, macro_call
        )
        self.assertTrue(by_val["//:ignored_in_compare"].in_compare)
        self.assertTrue(by_val["//:ignored_in_fail"].in_fail)

    def test_report_and_report_string_url(self) -> None:
        content = 'my_macro(name = "t", srcs = ["//:bad"])\n'
        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
        )
        call = ctx.calls[0]
        string_node = ctx.strings[1].node

        ctx.report(
            "Call message.",
            call.node,
            level="error",
            url="https://example.com/docs#call",
        )
        ctx.report(
            "Call message without URL.",
            call.node,
            level="warning",
            url=None,
        )
        ctx.report_string(
            "String message.",
            string_node,
            level="error",
            url="https://example.com/docs#string",
        )
        ctx.report_string(
            "String message without URL.",
            string_node,
            level="warning",
            url=None,
        )

        self.assertEqual(
            [rf.finding["message"] for rf in ctx._raw_findings],
            [
                "Call message. See https://example.com/docs#call.",
                "Call message without URL.",
                "String message. See https://example.com/docs#string.",
                "String message without URL.",
            ],
        )

    def test_syntax_error_returns_finding(self) -> None:
        findings = framework.run_check_on_contents(
            SimpleCheckForTests(),
            "def unclosed_parenthesis(",
            "src/BUILD.bazel",
        )
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["filepath"], "src/BUILD.bazel")
        self.assertEqual(findings[0]["line"], 1)
        self.assertEqual(findings[0]["col"], 25)
        self.assertEqual(findings[0]["level"], "error")
        self.assertIn("Syntax error:", findings[0]["message"])

    def test_cli_clean_and_findings_and_errors(self) -> None:
        with mock.patch("importlib.import_module"), mock.patch.dict(
            framework.REGISTRY,
            {"simple_check_for_tests": SimpleCheckForTests()},
            clear=True,
        ), tempfile.TemporaryDirectory() as tmpdir:
            clean_file = Path(tmpdir) / "BUILD.bazel"
            clean_file.write_text('tool = "//foo:bar"\n')
            with io.StringIO() as mock_stdout, mock.patch.object(
                sys, "stdout", mock_stdout
            ):
                ret = main.main(
                    [
                        "--check",
                        "simple_check_for_tests",
                        "--root",
                        str(tmpdir),
                        "--json",
                        str(clean_file),
                    ]
                )
                self.assertEqual(ret, main.EXIT_OK)
                self.assertEqual(json.loads(mock_stdout.getvalue()), [])

            bad_file = Path(tmpdir) / "BUILD.bazel"
            bad_file.write_text('tool = "//:bad/bar"\n')
            with io.StringIO() as mock_stdout, mock.patch.object(
                sys, "stdout", mock_stdout
            ):
                ret = main.main(
                    [
                        "--check",
                        "simple_check_for_tests",
                        "--root",
                        str(tmpdir),
                        "--json",
                        str(bad_file),
                    ]
                )
                self.assertEqual(ret, main.EXIT_FINDINGS)
                data = json.loads(mock_stdout.getvalue())
                self.assertEqual(len(data), 1)

            with io.StringIO() as mock_stderr, mock.patch.object(
                sys, "stderr", mock_stderr
            ):
                ret = main.main(
                    [
                        "--check",
                        "simple_check_for_tests",
                        "--root",
                        str(tmpdir),
                        "nonexistent/BUILD.bazel",
                    ]
                )
                self.assertEqual(ret, main.EXIT_ERROR)
                self.assertIn("File not found", mock_stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
