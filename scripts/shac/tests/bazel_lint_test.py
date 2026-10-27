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

    def test_register_check_targets_without_supports_allowlist_raises(
        self,
    ) -> None:
        class InvalidTargetsWithoutAllowlist(framework.BazelCheck):
            name = "invalid_targets_without_allowlist"
            file_kinds = frozenset({framework.FileKind.BUILD})
            supports_allowlist = False
            supports_targets_allowlist = True

        with self.assertRaisesRegex(
            ValueError,
            "'supports_targets_allowlist = True' requires 'supports_allowlist = True'",
        ):
            framework.register_check(InvalidTargetsWithoutAllowlist)

    def test_supports_allowlist_false_rejects_allowlist_and_skips_loading(
        self,
    ) -> None:
        check = SimpleCheckForTests()
        self.assertFalse(check.supports_allowlist)

        # `run_check_on_contents` fails if `supports_allowlist` is False and an
        # allowlist instance is passed.
        with self.assertRaisesRegex(
            ValueError,
            "does not support allowlists \\(supports_allowlist = False\\)",
        ):
            framework.run_check_on_contents(
                check,
                'x = "//foo:bar"\n',
                "src/BUILD.bazel",
                allowlist=allowlist_mod.Allowlist(),
            )

        with mock.patch("importlib.import_module"), mock.patch.dict(
            framework.REGISTRY,
            {"simple_check_for_tests": check},
            clear=True,
        ), tempfile.TemporaryDirectory() as tmpdir:
            build_file = Path(tmpdir) / "BUILD.bazel"
            build_file.write_text('tool = "//foo:bar"\n')
            allowlists_dir = Path(tmpdir) / "allowlists"
            allowlists_dir.mkdir()

            # When no allowlist file exists on disk, `main.main` does not call
            # `load_allowlist` and passes `allowlist=None`.
            with mock.patch.object(
                main, "load_allowlist"
            ) as mock_load_al, io.StringIO() as mock_stdout, mock.patch.object(
                sys, "stdout", mock_stdout
            ):
                ret = main.main(
                    [
                        "--check",
                        "simple_check_for_tests",
                        "--root",
                        str(tmpdir),
                        "--allowlists-dir",
                        str(allowlists_dir),
                        "--json",
                        str(build_file),
                    ]
                )
                self.assertEqual(ret, main.EXIT_OK)
                mock_load_al.assert_not_called()

            # When an allowlist file exists on disk for a check with
            # `supports_allowlist = False`, `main.main` fails with EXIT_ERROR.
            (allowlists_dir / "simple_check_for_tests.json").write_text(
                json.dumps({"entries": []})
            )
            with io.StringIO() as mock_stderr, mock.patch.object(
                sys, "stderr", mock_stderr
            ):
                ret = main.main(
                    [
                        "--check",
                        "simple_check_for_tests",
                        "--root",
                        str(tmpdir),
                        "--allowlists-dir",
                        str(allowlists_dir),
                        "--json",
                        str(build_file),
                    ]
                )
                self.assertEqual(ret, main.EXIT_ERROR)
                self.assertIn(
                    "allowlist file is not permitted for check 'simple_check_for_tests'",
                    mock_stderr.getvalue(),
                )

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
        content = '''
# Full-line comment with "//:bad/comment"
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
        content = """
load("//build:defs.bzl", "my_macro", _alias = "other")

visibility("private")

TOP_VAR = "//:bad/top_var"
ASSIGNED_SELECT = select({
    "//conditions:default": ["//:bad/in_assigned_select"],
})
ASSIGNED_NAMED = helper_with_name(
    name = "assigned_helper_name",
    src = "//:bad/in_assigned_named_call",
)

my_macro(
    name = "target_a",
    srcs = ["//:bad/direct_arg"]
    + select({
        "//conditions:default": ["//:bad/in_select"],
    })
    + glob(["//:bad/in_glob"])
    + select({
        "//conditions:default": glob(["//:bad/in_select_glob"]),
    }),
    data = compute_data("//:bad/in_value_fn")
    + outer_helper(
        inner_kw = select({
            "//conditions:default": ["//:bad/in_intermediate_kw_select"],
        }),
    ),
    copts = make_config(
        name = "inner_config_name",
        opt = "//:bad/in_named_value_fn",
    ),
    msg = (
        "//:bad/multi"
        + "_line"
    ),
)

exports_files(
    ["//:bad/in_exports_files"],
    visibility = select({
        "//conditions:default": ["//:bad/in_exports_files_select"],
    }),
)

expect_includes(deps = ["//:bad/in_expect_includes"])

register_extension_info(
    name = "non_target_stmt_name",
    label = "//:bad/in_non_target_with_name",
)

filegroup(
    name = dynamic_name_var,
    srcs = ["//:bad/in_dynamic_name_target"],
)

def _helper_fn():
    return select({
        "//conditions:default": ["//:bad/in_return_select"],
    })

def _my_rule_impl(ctx):
    out = ctx.actions.declare_file("//:bad/in_rule_impl_assign")
    ctx.actions.run(executable = "//:bad/in_rule_impl_run")
    ctx.actions.step(
        name = "action_step_name",
        cmd = "//:bad/in_rule_impl_named_stmt",
    )

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {
        "_tool": attr.label(default = "//:bad/in_rule_def_attr"),
    },
    doc = "//:bad/in_rule_def_doc",
)

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
        self.assertEqual(ctx.loads[0].raw_module_label, "//build:defs.bzl")
        self.assertEqual(
            [(local, exported) for local, exported, _ in ctx.loads[0].symbols],
            [("my_macro", "my_macro"), ("_alias", "other")],
        )

        # Top-level calls (`is_top_level` is True only for call statements whose
        # parent is `ast.Expr` directly under `ast.Module`; calls assigned to
        # variables at module scope such as `my_rule = rule(...)`,
        # `ASSIGNED_SELECT = select(...)`, and `ASSIGNED_NAMED = helper_with_name(...)`
        # have `ast.Assign` as their parent and therefore have `is_top_level = False`).
        top_level_calls = [c for c in ctx.calls if c.is_top_level]
        self.assertEqual(
            [c.func_name for c in top_level_calls],
            [
                "visibility",
                "my_macro",
                "exports_files",
                "expect_includes",
                "register_extension_info",
                "filegroup",
            ],
        )
        macro_call = top_level_calls[1]
        self.assertEqual(macro_call.target_name, "target_a")
        self.assertIn("srcs", macro_call.keywords)

        by_val = {s.value: s for s in ctx.strings}
        self.assertTrue(by_val["//build:defs.bzl"].is_load_source)
        self.assertFalse(by_val["//:bad/direct_arg"].is_load_source)
        self.assertTrue(by_val["//:ignored_in_compare"].in_compare)
        self.assertTrue(by_val["//:ignored_in_fail"].in_fail)

        # Verify that StringInfo attributes match FileContext methods for every string.
        for s in ctx.strings:
            self.assertEqual(ctx.enclosing_call(s.node), s.enclosing_call)
            self.assertEqual(
                ctx.enclosing_statement_call(s.node), s.enclosing_statement_call
            )

        def call_summary(
            call: framework.CallInfo | None,
        ) -> tuple[str, str | None] | None:
            if call is None:
                return None
            return (call.func_name, call.target_name)

        expected_enclosing_pairs = {
            # SAME (None, None): load() source and symbol strings are represented by
            # LoadInfo rather than CallInfo, so both functions return None.
            "//build:defs.bzl": (None, None),
            "my_macro": (None, None),
            "other": (None, None),
            # SAME (None, None): Top-level variable assignment outside any call.
            "//:bad/top_var": (None, None),
            # DIFFER: Call assigned to a variable (`ASSIGNED_SELECT = select(...)`).
            # enclosing_call returns `select`, whereas enclosing_statement_call
            # returns None because the statement is `ast.Assign`, not `ast.Expr`.
            "//:bad/in_assigned_select": (("select", None), None),
            # DIFFER: Named call assigned to a variable (`ASSIGNED_NAMED = helper_with_name(name = ...)`).
            # enclosing_call returns `helper_with_name` (with target_name), whereas
            # enclosing_statement_call returns None because the call is assigned.
            "//:bad/in_assigned_named_call": (
                ("helper_with_name", "assigned_helper_name"),
                None,
            ),
            # SAME: Direct argument to a rule/macro statement call (`my_macro`).
            "//:bad/direct_arg": (
                ("my_macro", "target_a"),
                ("my_macro", "target_a"),
            ),
            # DIFFER: Nested inside `select(...)` within `my_macro`.
            # enclosing_call is the inner `select`; enclosing_statement_call is `my_macro`.
            "//:bad/in_select": (
                ("select", None),
                ("my_macro", "target_a"),
            ),
            # DIFFER: Nested inside `glob(...)` within `my_macro`.
            # enclosing_call is the inner `glob`; enclosing_statement_call is `my_macro`.
            "//:bad/in_glob": (
                ("glob", None),
                ("my_macro", "target_a"),
            ),
            # DIFFER: Nested inside `glob(...)` inside `select(...)` within `my_macro`.
            # enclosing_call is the innermost `glob`; enclosing_statement_call is `my_macro`.
            "//:bad/in_select_glob": (
                ("glob", None),
                ("my_macro", "target_a"),
            ),
            # DIFFER: Argument to a value-returning helper function inside `my_macro`.
            # enclosing_call is `compute_data`; enclosing_statement_call is `my_macro`.
            "//:bad/in_value_fn": (
                ("compute_data", None),
                ("my_macro", "target_a"),
            ),
            # DIFFER: Argument to a value-returning helper function that itself has a
            # `name` parameter inside `my_macro`. enclosing_call is the inner
            # `make_config` (target_name="inner_config_name"), while
            # enclosing_statement_call is the outer `my_macro` (target_name="target_a").
            "//:bad/in_named_value_fn": (
                ("make_config", "inner_config_name"),
                ("my_macro", "target_a"),
            ),
            # SAME: Direct argument to a statement call without a `name` parameter
            # (`exports_files`). Both return `exports_files` with target_name=None.
            "//:bad/in_exports_files": (
                ("exports_files", None),
                ("exports_files", None),
            ),
            # DIFFER: Nested inside `select(...)` within `exports_files(...)`.
            # enclosing_call is `select`; enclosing_statement_call is `exports_files`.
            "//:bad/in_exports_files_select": (
                ("select", None),
                ("exports_files", None),
            ),
            # SAME: Direct argument to `expect_includes(...)` (no `name` parameter).
            "//:bad/in_expect_includes": (
                ("expect_includes", None),
                ("expect_includes", None),
            ),
            # SAME: Direct argument to a non-target statement call that happens to
            # have a `name` parameter (`register_extension_info`).
            "//:bad/in_non_target_with_name": (
                ("register_extension_info", "non_target_stmt_name"),
                ("register_extension_info", "non_target_stmt_name"),
            ),
            # SAME: Direct argument to a target call whose `name` is a dynamic
            # variable rather than a string literal (`target_name` is None).
            "//:bad/in_dynamic_name_target": (
                ("filegroup", None),
                ("filegroup", None),
            ),
            # DIFFER: Call inside a `return` statement (`return select(...)`).
            # enclosing_call is `select`; enclosing_statement_call is None because
            # the enclosing statement is `ast.Return`, not `ast.Expr`.
            "//:bad/in_return_select": (("select", None), None),
            # DIFFER: Assigned call inside a rule `implementation` function
            # (`out = ctx.actions.declare_file(...)`). enclosing_call is
            # `ctx.actions.declare_file`; enclosing_statement_call is None.
            "//:bad/in_rule_impl_assign": (
                ("ctx.actions.declare_file", None),
                None,
            ),
            # SAME: Statement call without `name` inside a rule `implementation`
            # function (`ctx.actions.run(...)`).
            "//:bad/in_rule_impl_run": (
                ("ctx.actions.run", None),
                ("ctx.actions.run", None),
            ),
            # SAME: Statement call with `name` inside a rule `implementation`
            # function (`ctx.actions.step(name = "action_step_name", ...)`).
            "//:bad/in_rule_impl_named_stmt": (
                ("ctx.actions.step", "action_step_name"),
                ("ctx.actions.step", "action_step_name"),
            ),
            # DIFFER: Nested `attr.label(...)` inside `my_rule = rule(...)`.
            # enclosing_call is `attr.label`; enclosing_statement_call is None
            # because `my_rule = rule(...)` is an `ast.Assign` statement.
            "//:bad/in_rule_def_attr": (("attr.label", None), None),
            # DIFFER: Direct argument to `my_rule = rule(doc = ...)`.
            # enclosing_call is `rule`; enclosing_statement_call is None because
            # `rule(...)` is the right-hand side of an `ast.Assign` statement.
            "//:bad/in_rule_def_doc": (("rule", None), None),
            # SAME (None, None): String inside an `if` comparison condition.
            "//:ignored_in_compare": (None, None),
            # SAME: Argument to `fail(...)` statement call.
            "//:ignored_in_fail": (("fail", None), ("fail", None)),
        }
        enclosing_pairs = {
            val: (
                call_summary(by_val[val].enclosing_call),
                call_summary(by_val[val].enclosing_statement_call),
            )
            for val in expected_enclosing_pairs
        }
        self.assertEqual(enclosing_pairs, expected_enclosing_pairs)

        expected_keyword_args = {
            # Strings outside calls or in positional arguments have keyword_arg=None:
            "//build:defs.bzl": None,
            "my_macro": None,
            "//:bad/top_var": None,
            "//:bad/in_assigned_select": None,
            "//:bad/in_exports_files": None,
            "//:bad/in_return_select": None,
            "//:bad/in_rule_impl_assign": None,
            "//:ignored_in_compare": None,
            "//:ignored_in_fail": None,
            # Keyword argument on an assigned call (`enclosing` is `helper_with_name`,
            # `enclosing_statement_call` is None):
            "//:bad/in_assigned_named_call": "src",
            # Direct keyword argument on a statement call (`my_macro`):
            "//:bad/direct_arg": "srcs",
            "//:bad/in_value_fn": "data",
            # Keyword argument on `enclosing_statement_call` (`my_macro` or `exports_files`)
            # when `enclosing` is a positional wrapper (`select(...)` or `glob(...)`):
            "//:bad/in_select": "srcs",
            "//:bad/in_glob": "srcs",
            "//:bad/in_select_glob": "srcs",
            "//:bad/in_exports_files_select": "visibility",
            # When `enclosing` (`select`) is inside an intermediate helper call's
            # keyword argument (`outer_helper(inner_kw = select(...))`) inside
            # `enclosing_statement_call` (`my_macro(data = ...)`), `inner_kw` is
            # skipped because `outer_helper` is neither `enclosing` nor
            # `enclosing_statement_call`, resolving to `"data"` on `my_macro`:
            "//:bad/in_intermediate_kw_select": "data",
            # Keyword argument on inner `enclosing` call (`make_config(opt = ...)`)
            # takes precedence over outer `enclosing_statement_call` (`copts = ...`):
            "//:bad/in_named_value_fn": "opt",
            # Keyword arguments on other statement calls and nested `attr.label`:
            "//:bad/in_expect_includes": "deps",
            "//:bad/in_non_target_with_name": "label",
            "//:bad/in_dynamic_name_target": "srcs",
            "//:bad/in_rule_impl_run": "executable",
            "//:bad/in_rule_impl_named_stmt": "cmd",
            "//:bad/in_rule_def_attr": "default",
            "//:bad/in_rule_def_doc": "doc",
        }
        actual_keyword_args = {
            val: by_val[val].keyword_arg for val in expected_keyword_args
        }
        self.assertEqual(actual_keyword_args, expected_keyword_args)

    def test_normalize_repo_prefix_and_load_info(self) -> None:
        self.assertEqual(
            framework.normalize_repo_prefix("@rules_cc//cc:defs.bzl"),
            "@rules_cc//cc:defs.bzl",
        )
        self.assertEqual(
            framework.normalize_repo_prefix("@@rules_cc//cc:defs.bzl"),
            "@rules_cc//cc:defs.bzl",
        )
        self.assertEqual(
            framework.normalize_repo_prefix("//build:defs.bzl"),
            "//build:defs.bzl",
        )
        self.assertEqual(
            framework.normalize_repo_prefix("@//build:defs.bzl"),
            "//build:defs.bzl",
        )
        self.assertEqual(
            framework.normalize_repo_prefix("@@//build:defs.bzl"),
            "//build:defs.bzl",
        )

        content = """
load("@@rules_cc//cc:defs.bzl", "cc_library", _cc_bin = "cc_binary")
load("@@//build/bazel/rules:defs.bzl", "my_rule")
"""
        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
        )
        self.assertEqual(len(ctx.loads), 2)
        self.assertEqual(ctx.loads[0].module_label, "@rules_cc//cc:defs.bzl")
        self.assertEqual(
            ctx.loads[0].raw_module_label, "@@rules_cc//cc:defs.bzl"
        )
        self.assertEqual(
            ctx.loads[1].module_label, "//build/bazel/rules:defs.bzl"
        )
        self.assertEqual(
            ctx.loads[1].raw_module_label, "@@//build/bazel/rules:defs.bzl"
        )
        self.assertEqual(
            ctx.loaded_symbols,
            {
                "cc_library": ("@rules_cc//cc:defs.bzl", "cc_library"),
                "_cc_bin": ("@rules_cc//cc:defs.bzl", "cc_binary"),
                "my_rule": ("//build/bazel/rules:defs.bzl", "my_rule"),
            },
        )

    def test_automatic_target_attribution(self) -> None:
        content = """
load(
    "//bad:defs.bzl",
    _rule = "bad_rule",
    _const = "BAD_CONST",
    _shared = "SHARED_CONST",
)
load(
    "//partially_used:defs.bzl",
    _used_sym = "USED_SYM",
    _unused_sym = "UNUSED_IN_PARTIAL",
)
load("//outside:defs.bzl", _outside_sym = "OUTSIDE_SYM")
load("//unused:defs.bzl", _unused = "UNUSED")

TOP_VAR = "//bad:top_var"
ASSIGNED_SELECT = select({
    "//conditions:default": ["//bad:in_assigned_select"],
})
ASSIGNED_NAMED = helper_with_name(
    name = "assigned_helper_name",
    src = "//bad:in_assigned_named_call",
)

_rule(
    name = "target_one",
    srcs = ["//bad:direct_arg", _shared, _used_sym],
    deps = select({
        "//conditions:default": ["//bad:in_select"],
    }),
)

filegroup(
    name = "target_two",
    srcs = glob(["//bad:in_glob"]) + select({
        "//conditions:default": glob(["//bad:in_select_glob"]),
    }),
    data = compute_data("//bad:in_value_fn") + [_const, _shared],
    copts = make_config(
        name = "inner_config_name",
        opt = "//bad:in_named_value_fn",
    ),
)

exports_files(
    ["//bad:in_exports_files", _outside_sym],
    visibility = select({
        "//conditions:default": ["//bad:in_exports_files_select"],
    }),
)

expect_includes(deps = ["//bad:in_expect_includes"])

register_extension_info(
    name = "non_target_stmt_name",
    label = "//bad:in_non_target_with_name",
)

filegroup(
    name = dynamic_name_var,
    srcs = ["//bad:in_dynamic_name_target"],
)

def _my_rule_impl(ctx):
    out = ctx.actions.declare_file("//bad:in_rule_impl_assign")
    ctx.actions.run(executable = "//bad:in_rule_impl_run")
    ctx.actions.step(
        name = "action_step_name",
        cmd = "//bad:in_rule_impl_named_stmt",
    )

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {
        "_tool": attr.label(default = "//bad:in_rule_def_attr"),
    },
    doc = "//bad:in_rule_def_doc",
)
"""

        class TargetAwareCheck(framework.BazelCheck):
            name = "target_aware_check"
            file_kinds = frozenset(
                {framework.FileKind.BUILD, framework.FileKind.BZL}
            )
            supports_allowlist = True
            supports_targets_allowlist = True

            def check_load(
                self, fc: framework.FileContext, load: framework.LoadInfo
            ) -> None:
                for _, exported, arg_node in load.symbols:
                    fc.report(
                        f"bad symbol {exported}",
                        arg_node,
                        level="error",
                        url=None,
                    )

            def check_call(
                self, fc: framework.FileContext, call: framework.CallInfo
            ) -> None:
                if call.func_name in ("_rule", "exports_files"):
                    fc.report(
                        f"bad call {call.func_name}",
                        call.node,
                        level="error",
                        url=None,
                    )

            def check_string(
                self, fc: framework.FileContext, s: framework.StringInfo
            ) -> None:
                if s.value.startswith(
                    (
                        "//bad:",
                        "//partially_used:",
                        "//outside:",
                        "//unused:",
                    )
                ):
                    fc.report_string(
                        f"bad string {s.value}",
                        s.node,
                        s.value,
                        level="error",
                        url=None,
                    )

        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
            supports_targets_allowlist=True,
        )
        check = TargetAwareCheck()
        for load in ctx.loads:
            check.check_load(ctx, load)
        for call in ctx.calls:
            check.check_call(ctx, call)
        for s in ctx.strings:
            check.check_string(ctx, s)

        by_msg = {rf.finding["message"]: rf.targets for rf in ctx._raw_findings}
        self.assertEqual(
            by_msg,
            {
                # --- Loaded symbols (check_load) ---
                # Loaded symbol used as the rule/macro callable for target_one:
                "bad symbol bad_rule": ("target_one",),
                # Loaded symbol referenced only inside target_two:
                "bad symbol BAD_CONST": ("target_two",),
                # Loaded symbol referenced inside both target_one and target_two
                # (requires both targets to be allowlisted to suppress the load finding):
                "bad symbol SHARED_CONST": ("target_one", "target_two"),
                # Loaded symbol referenced only inside target_one:
                "bad symbol USED_SYM": ("target_one",),
                # Loaded symbol that is never referenced in the file cannot be
                # attributed to any target:
                "bad symbol UNUSED_IN_PARTIAL": (),
                # Loaded symbol referenced inside a call without a `name` argument
                # (`exports_files`) is not scoped to a named target, so it gets ():
                "bad symbol OUTSIDE_SYM": (),
                # Loaded symbol that is unused gets ():
                "bad symbol UNUSED": (),
                # --- Rule/function calls (check_call) ---
                # Named rule/macro call (`_rule(name = "target_one", ...)`):
                "bad call _rule": ("target_one",),
                # Built-in function without a `name` attribute (`exports_files(...)`)
                # does not define a named target, so its findings get () and cannot
                # be suppressed by a `"targets"` allowlist entry:
                "bad call exports_files": (),
                # --- load() module label strings (check_string with is_load_source=True) ---
                # All symbols loaded from `//bad:defs.bzl` (`_rule`, `_const`, `_shared`)
                # are used exclusively by named targets `target_one` and `target_two`:
                "bad string //bad:defs.bzl": ("target_one", "target_two"),
                # NON-OBVIOUS: `//partially_used:defs.bzl` loads `_used_sym` (used by
                # `target_one`) and `_unused_sym` (unused). Because `_unused_sym` is not
                # scoped to a named target, the `load()` module label finding as a whole
                # gets () and is not suppressed even if `target_one` is allowlisted:
                "bad string //partially_used:defs.bzl": (),
                # Symbol loaded from `//outside:defs.bzl` is used in `exports_files()`
                # (which has no `name`), so the module label gets ():
                "bad string //outside:defs.bzl": (),
                # Symbol loaded from `//unused:defs.bzl` is unused, so the module label
                # gets ():
                "bad string //unused:defs.bzl": (),
                # --- Strings in module-level assignments ---
                # Top-level variable assignment outside any call:
                "bad string //bad:top_var": (),
                # String inside `select(...)` assigned to a module-level variable:
                "bad string //bad:in_assigned_select": (),
                # NON-OBVIOUS: String inside a call with `name = "assigned_helper_name"`
                # that is assigned to a variable (`ASSIGNED_NAMED = helper_with_name(...)`).
                # Because the statement is an assignment (`ast.Assign`) rather than a call
                # statement (`ast.Expr`), `enclosing_statement_call` is None and targets is ():
                "bad string //bad:in_assigned_named_call": (),
                # --- Strings inside named target calls ---
                # Direct string argument to `_rule(name = "target_one", ...)`:
                "bad string //bad:direct_arg": ("target_one",),
                # String nested inside `select({...})` in `target_one`:
                "bad string //bad:in_select": ("target_one",),
                # String nested inside `glob([...])` in `target_two`:
                "bad string //bad:in_glob": ("target_two",),
                # String nested inside `glob([...])` inside `select({...})` in `target_two`:
                "bad string //bad:in_select_glob": ("target_two",),
                # String passed to a normal value-returning helper function
                # (`compute_data(...)`) inside `target_two`:
                "bad string //bad:in_value_fn": ("target_two",),
                # String passed to a value-returning helper function that happens to
                # have its own `name = "inner_config_name"` parameter (`make_config(...)`)
                # inside `target_two`. Attribution uses `enclosing_statement_call`
                # (`filegroup`), so it resolves to `("target_two",)` rather than
                # `("inner_config_name",)`:
                "bad string //bad:in_named_value_fn": ("target_two",),
                # --- Strings in calls without `name` or non-target calls ---
                # Direct string in `exports_files(...)` (no `name` parameter):
                "bad string //bad:in_exports_files": (),
                # String inside `select(...)` in `exports_files(...)` (no `name` parameter):
                "bad string //bad:in_exports_files_select": (),
                # Direct string in `expect_includes(...)` (no `name` parameter):
                "bad string //bad:in_expect_includes": (),
                # NON-OBVIOUS: A statement-level function call (`register_extension_info`)
                # that does not actually define a Bazel target in the build graph, but
                # passes a literal `name = "non_target_stmt_name"` keyword argument.
                # Because AST analysis cannot distinguish rules/macros from other
                # statement calls with `name`, findings inside it are attributed to
                # `("non_target_stmt_name",)`:
                "bad string //bad:in_non_target_with_name": (
                    "non_target_stmt_name",
                ),
                # NON-OBVIOUS: A target call (`filegroup(name = dynamic_name_var, ...)`)
                # whose `name` is a variable (or expression like `"prefix_" + x`) rather
                # than a string literal. Because the AST does not evaluate variables or
                # fold expressions, `target_name` is None and findings get ():
                "bad string //bad:in_dynamic_name_target": (),
                # --- Strings in rule `implementation` functions and `rule()` definitions ---
                # Assigned call (`out = ctx.actions.declare_file(...)`) inside
                # `_my_rule_impl`: parent is `ast.Assign`, so targets is ():
                "bad string //bad:in_rule_impl_assign": (),
                # Statement call without `name` (`ctx.actions.run(...)`) inside
                # `_my_rule_impl`: `target_name` is None, so targets is ():
                "bad string //bad:in_rule_impl_run": (),
                # NON-OBVIOUS: Statement call with a literal `name` argument
                # (`ctx.actions.step(name = "action_step_name", ...)`) inside
                # `_my_rule_impl`. Because `enclosing_statement_call` matches any
                # `ast.Expr` call statement at any nesting depth, it attributes the
                # finding to `("action_step_name",)`:
                "bad string //bad:in_rule_impl_named_stmt": (
                    "action_step_name",
                ),
                # String inside `attr.label(default = ...)` in `my_rule = rule(...)`:
                # `my_rule = rule(...)` is an `ast.Assign` statement, so
                # `enclosing_statement_call` is None and targets is ():
                "bad string //bad:in_rule_def_attr": (),
                # String in `doc = ...` of `my_rule = rule(...)`: parent of `rule(...)`
                # is `ast.Assign`, so targets is ():
                "bad string //bad:in_rule_def_doc": (),
            },
        )

        # Verify end-to-end filtering in `run_check_on_contents` on a partially
        # allowlisted file:
        # 1. When only `target_one` is allowlisted, findings attributed solely to
        #    `target_one` (e.g. `bad symbol bad_rule`, `bad call _rule`,
        #    `bad string //bad:direct_arg`, `bad string //bad:in_select`,
        #    `bad symbol USED_SYM`) are suppressed, whereas findings shared with
        #    `target_two` (`bad symbol SHARED_CONST`, `bad string //bad:defs.bzl`),
        #    findings on `target_two`, and unattributed `()` findings remain reported.
        with tempfile.TemporaryDirectory() as tmpdir:
            al_path = Path(tmpdir) / "target_aware_check.json"
            al_path.write_text(
                json.dumps(
                    {
                        "entries": [
                            {
                                "bug": "https://fxbug.dev/570203009",
                                "path": "src/BUILD.bazel",
                                "targets": ["target_one"],
                            }
                        ]
                    }
                )
            )
            al_one = allowlist_mod.load_allowlist(
                al_path, supports_targets_allowlist=True
            )
            findings_one = {
                f["message"]
                for f in framework.run_check_on_contents(
                    check, content, "src/BUILD.bazel", allowlist=al_one
                )
            }
            for suppressed_msg in (
                "bad symbol bad_rule",
                "bad symbol USED_SYM",
                "bad call _rule",
                "bad string //bad:direct_arg",
                "bad string //bad:in_select",
            ):
                self.assertNotIn(suppressed_msg, findings_one)
            for kept_msg in (
                "bad symbol SHARED_CONST",
                "bad symbol BAD_CONST",
                "bad string //bad:defs.bzl",
                "bad string //bad:in_glob",
                "bad call exports_files",
                "bad string //bad:in_exports_files",
            ):
                self.assertIn(kept_msg, findings_one)

            # 2. When both `target_one` and `target_two` are allowlisted, the
            #    shared load symbol (`SHARED_CONST`) and shared load module label
            #    (`//bad:defs.bzl`) are also suppressed, while unattributed `()`
            #    findings (such as `exports_files`) are still reported.
            al_path.write_text(
                json.dumps(
                    {
                        "entries": [
                            {
                                "bug": "https://fxbug.dev/570203009",
                                "path": "src/BUILD.bazel",
                                "targets": ["target_one", "target_two"],
                            }
                        ]
                    }
                )
            )
            al_both = allowlist_mod.load_allowlist(
                al_path, supports_targets_allowlist=True
            )
            findings_both = {
                f["message"]
                for f in framework.run_check_on_contents(
                    check, content, "src/BUILD.bazel", allowlist=al_both
                )
            }
            self.assertNotIn("bad symbol SHARED_CONST", findings_both)
            self.assertNotIn("bad string //bad:defs.bzl", findings_both)
            self.assertNotIn("bad string //bad:in_glob", findings_both)
            self.assertIn("bad call exports_files", findings_both)
            self.assertIn("bad string //bad:in_exports_files", findings_both)

    def test_targets_using_symbol(self) -> None:
        content = """\
load("//defs:defs.bzl", _rule = "RULE", _shared = "SHARED", _unused = "UNUSED")
load("//other:defs.bzl", _outside = "OUTSIDE")

_rule(name = "target_one", deps = [_shared])
filegroup(name = "target_two", deps = [_shared])
exports_files([_outside])
"""
        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
            supports_targets_allowlist=True,
        )
        self.assertEqual(ctx._targets_using_symbol("_rule"), ("target_one",))
        self.assertEqual(
            ctx._targets_using_symbol("_shared"), ("target_one", "target_two")
        )
        self.assertEqual(ctx._targets_using_symbol("_unused"), ())
        self.assertEqual(ctx._targets_using_symbol("_outside"), ())
        self.assertEqual(ctx._targets_using_symbol("nonexistent_symbol"), ())

    def test_targets_using_load(self) -> None:
        content = """\
load("//used:defs.bzl", _rule = "RULE", _shared = "SHARED")
load("//outside:defs.bzl", _outside = "OUTSIDE")
load("//empty:defs.bzl")

_rule(name = "target_one", deps = [_shared])
filegroup(name = "target_two", deps = [_shared])
exports_files([_outside])
"""
        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
            supports_targets_allowlist=True,
        )
        self.assertEqual(
            ctx._targets_using_load(ctx.loads[0]), ("target_one", "target_two")
        )
        self.assertEqual(ctx._targets_using_load(ctx.loads[1]), ())
        self.assertEqual(ctx._targets_using_load(ctx.loads[2]), ())

    def test_infer_targets(self) -> None:
        content = """\
load("//used:defs.bzl", _rule = "RULE", _shared = "SHARED")

_rule(name = "target_one", deps = [_shared])
filegroup(name = "target_two", deps = [_shared])
"""
        tree = ast.parse(content, filename="src/BUILD.bazel")
        ctx = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
            supports_targets_allowlist=True,
        )
        self.assertEqual(ctx._infer_targets(None), ())
        self.assertEqual(
            ctx._infer_targets(ctx.loads[0].node), ("target_one", "target_two")
        )
        ctx_no_targets = framework.FileContext(
            path="src/BUILD.bazel",
            kind=framework.FileKind.BUILD,
            tree=tree,
            lines=content.splitlines(),
            supports_targets_allowlist=False,
        )
        self.assertEqual(ctx_no_targets._infer_targets(ctx.loads[0].node), ())
        self.assertEqual(ctx_no_targets._infer_targets(ctx.calls[0].node), ())

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
