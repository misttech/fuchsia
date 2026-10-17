# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/bazel.star."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_default_applicable_licenses",
    "_bazel_disallowed_workspace_root_package_labels",
    "_bazel_no_glob",
    "_bazel_no_recursive_glob",
    "_bazel_no_repo_rules_canonical_names",
)
load(
    "//scripts/shac/bazel_lint/checks/tests/checks_test.star",
    _test_bazel_lint_checks = "test_bazel_lint_checks",
)

_ALLOWLISTS = testing.file(
    content = json.encode({
        "ignored_files": ["ignored/BUILD.bazel"],
        "ignored_prefixes": ["third_party"],
    }),
    affected = False,
)

_LICENSES_MSG = "BUILD.bazel files must include `default_applicable_licenses = [\"//:license\"]` within a `package()` call to set the default file-level license."

def test_default_applicable_licenses_missing():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "filegroup(name = \"foo\")\n",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _LICENSES_MSG,
            filepath = "foo/BUILD.bazel",
        ),
    ))

def test_default_applicable_licenses_in_other_call():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "filegroup(default_applicable_licenses = [\"//:license\"])\n",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _LICENSES_MSG,
            filepath = "foo/BUILD.bazel",
        ),
    ))

def test_default_applicable_licenses_present():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "package(\n    default_applicable_licenses = [\"//:license\"],\n)\n",
    })
    asserts.eq(res.findings, ())

def test_default_applicable_licenses_ignored():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "ignored/BUILD.bazel": "",
        "third_party/foo/BUILD.bazel": "",
        # Matches the prefix as a string, but not as a directory.
        "third_party_foo/BUILD.bazel": "",
    })
    asserts.eq(
        [f.filepath for f in res.findings],
        ["third_party_foo/BUILD.bazel"],
    )

def test_no_repo_rules_canonical_names():
    res = testing.run(_bazel_no_repo_rules_canonical_names, files = {
        "foo.bzl": "x = 1\ny = \"@@+_repo_rules3+internal_sdk//:foo\"\n",
        "foo.py": "LABEL = \"@@+_repo_rules+internal_sdk//:foo\"\n",
        # Other file types aren't checked.
        "foo.md": "@@+_repo_rules3+internal_sdk\n",
    })
    asserts.eq(
        [(f.filepath, f.level, f.line, f.col, f.end_col) for f in res.findings],
        [("foo.bzl", "error", 2, 9, 22), ("foo.py", "error", 1, 13, 25)],
    )

def test_no_repo_rules_canonical_names_apparent_name():
    res = testing.run(_bazel_no_repo_rules_canonical_names, files = {
        "BUILD.bazel": "x = \"@internal_sdk//:foo\"\n",
    })
    asserts.eq(res.findings, ())

def _glob_allowlist(*files):
    return testing.file(
        content = json.indent(json.encode({"allowed_paths": list(files)}), indent = "    ") + "\n",
        affected = False,
    )

def test_no_glob():
    res = testing.run(_bazel_no_glob, files = {
        "build/bazel/shac/glob_allowlist.json": _glob_allowlist(),
        "foo/BUILD.bazel": "x = 1\nfilegroup(srcs = glob([\"*.txt\"]))\n",
        "foo/defs.bzl": "def f():\n    return native.glob([\"*.txt\"])\n",
    })
    asserts.eq(
        [(f.filepath, f.level, f.line, f.col, f.end_col) for f in res.findings],
        [("foo/BUILD.bazel", "error", 2, 18, 23), ("foo/defs.bzl", "error", 2, 19, 24)],
    )

def test_no_glob_allowed():
    res = testing.run(_bazel_no_glob, files = {
        "build/bazel/shac/glob_allowlist.json": _glob_allowlist(
            "allowed/BUILD.bazel",
            "allowed_dir/",
        ),
        # Allowlisted files may keep using glob(), even in changed lines.
        "allowed/BUILD.bazel": "filegroup(srcs = glob([\"*\"]))\n",
        # So may any file under an allowlisted directory, including new ones.
        "allowed_dir/new/BUILD.bazel": "filegroup(srcs = glob([\"*\"]))\n",
        # Comments and similarly named functions aren't flagged.
        "foo/BUILD.bazel": "# Don't use glob() here.\nmy_glob(srcs = [])\n",
        # Upstream third-party code isn't checked.
        "third_party/foo/BUILD.bazel": "filegroup(srcs = glob([\"*\"]))\n",
        "src/third_party/bar/BUILD.bazel": "filegroup(srcs = glob([\"*\"]))\n",
        # Other file types aren't checked.
        "foo/check.star": "ctx.scm.affected_files(glob = [\"*.bazel\"])\nglob([])\n",
    })
    asserts.eq(res.findings, ())

def test_no_glob_stale_allowlist():
    res = testing.run(_bazel_no_glob, files = {
        "build/bazel/shac/glob_allowlist.json": _glob_allowlist(
            "clean/BUILD.bazel",
            "clean_dir/",
            "deleted/BUILD.bazel",
            "unaffected/BUILD.bazel",
            "unaffected_dir/",
        ),
        "clean/BUILD.bazel": "# Used to call glob().\nfilegroup(srcs = [\"a\"])\n",
        "clean_dir/a/BUILD.bazel": "filegroup(srcs = [\"a\"])\n",
        # Only Bazel files count toward a directory entry.
        "clean_dir/a/check.star": "glob([])\n",
        # A directory entry is fine as long as any file under it uses glob().
        "unaffected_dir/a/BUILD.bazel": testing.file(
            content = "filegroup(srcs = [\"a\"])\n",
            affected = False,
        ),
        "unaffected_dir/b/defs.bzl": testing.file(
            content = "def f():\n    return native.glob([\"*\"])\n",
            affected = False,
        ),
        # Unaffected allowlisted files are still checked for staleness.
        "unaffected/BUILD.bazel": testing.file(
            content = "filegroup(srcs = glob([\"*\"]))\n",
            affected = False,
        ),
    })
    asserts.eq(
        [(f.filepath, f.line, f.message) for f in res.findings],
        [
            (
                "build/bazel/shac/glob_allowlist.json",
                3,
                "clean/BUILD.bazel no longer uses `glob()`; remove it from the allowlist.",
            ),
            (
                "build/bazel/shac/glob_allowlist.json",
                4,
                "clean_dir/ contains no Bazel files that use `glob()`; remove it from the allowlist.",
            ),
            (
                "build/bazel/shac/glob_allowlist.json",
                5,
                "deleted/BUILD.bazel doesn't exist; remove it from the allowlist.",
            ),
        ],
    )

def _recursive_glob_allowlist(*files):
    return testing.file(
        content = json.indent(json.encode({"allowed_paths": list(files)}), indent = "    ") + "\n",
        affected = False,
    )

def test_no_recursive_glob():
    res = testing.run(_bazel_no_recursive_glob, files = {
        "build/bazel/shac/recursive_glob_allowlist.json": _recursive_glob_allowlist(),
        "foo/BUILD.bazel": """filegroup(srcs = glob(["*.txt"]))
filegroup(srcs = glob(["a/**/*.txt"]))
""",
        # Patterns outside glob() calls and in single quotes are caught too.
        "foo/defs.bzl": """def f(root):
    return ['{}/**'.format(root)]
""",
    })
    asserts.eq(
        [(f.filepath, f.level, f.line, f.col, f.end_col) for f in res.findings],
        [("foo/BUILD.bazel", "error", 2, 24, 36), ("foo/defs.bzl", "error", 2, 13, 20)],
    )

def test_no_recursive_glob_allowed():
    res = testing.run(_bazel_no_recursive_glob, files = {
        "build/bazel/shac/recursive_glob_allowlist.json": _recursive_glob_allowlist(
            "allowed/BUILD.bazel",
            "allowed_dir/",
        ),
        "allowed/BUILD.bazel": "filegroup(srcs = glob([\"**\"]))\n",
        "allowed_dir/new/BUILD.bazel": "filegroup(srcs = glob([\"**\"]))\n",
        # kwargs unpacking, Markdown bold text, and comments aren't recursive
        # globs, even next to other strings.
        "foo/defs.bzl": """def f(**kwargs):
    g(name = "a", **kwargs, tags = ["b"])
    x = "it's" + y(**kwargs) + 'z'
    doc = "**Deprecated**: use bar."  # Not glob(["**"]).
""",
        "third_party/foo/BUILD.bazel": "filegroup(srcs = glob([\"**\"]))\n",
    })
    asserts.eq(res.findings, ())

def test_no_recursive_glob_stale_allowlist():
    res = testing.run(_bazel_no_recursive_glob, files = {
        "build/bazel/shac/recursive_glob_allowlist.json": _recursive_glob_allowlist(
            "clean/BUILD.bazel",
            "unaffected/BUILD.bazel",
        ),
        "clean/BUILD.bazel": "filegroup(srcs = glob([\"*\"]))\n",
        "unaffected/BUILD.bazel": testing.file(
            content = "filegroup(srcs = glob([\"**\"]))\n",
            affected = False,
        ),
    })
    asserts.eq(
        [(f.filepath, f.line, f.message) for f in res.findings],
        [(
            "build/bazel/shac/recursive_glob_allowlist.json",
            3,
            "clean/BUILD.bazel no longer uses recursive glob patterns; remove it from the allowlist.",
        )],
    )

# The bazel_lint framework tests use a real registered check and sample
# violation content to verify framework-level behaviors in _make_bazel_lint_check
# (such as file glob exclusions and allowlists).
# They use a constant for the file contents and override the allowlist to ensure
# the tests are actually testing the paths.
_FRAMEWORK_TEST_CHECK = _bazel_disallowed_workspace_root_package_labels
_FRAMEWORK_TEST_CHECK_ALLOWLIST = (
    "scripts/shac/bazel_lint/allowlists/bazel_disallowed_workspace_root_package_labels.json"
)
_FRAMEWORK_TEST_EMPTY_ALLOWLIST = testing.file(
    content = json.encode({"entries": []}),
    affected = False,
)
_FRAMEWORK_TEST_VIOLATION = 'x = "//:foo/bar"\n'

def test_bazel_lint_framework_excluded_globs():
    # Uses a real registered check with violation content to verify that files
    # matching framework _EXCLUDED_GLOBS (!/third_party/**/src/** and
    # !/third_party/bazel_vendor/**) are excluded by _make_bazel_lint_check()
    # and never passed to the linter, while files not matching them are checked.
    # Provide an empty allowlist to isolate framework glob behavior from the
    # check's disk allowlist, ensuring that findings (or lack thereof) are
    # governed strictly by _EXCLUDED_GLOBS rather than an allowlist.
    res = testing.run(
        _FRAMEWORK_TEST_CHECK,
        files = {
            _FRAMEWORK_TEST_CHECK_ALLOWLIST: _FRAMEWORK_TEST_EMPTY_ALLOWLIST,
            # Files matching framework _EXCLUDED_GLOBS are excluded and produce no findings:
            "third_party/foo/src/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "third_party/bazel_vendor/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "third_party/bazel_vendor/foo/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            # Files that do not match _EXCLUDED_GLOBS are not excluded and produce findings:
            "third_party/foo/source/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "third_party/bazel/vendor/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
        },
    )
    asserts.eq(
        [(f.filepath, f.line) for f in res.findings],
        [
            ("third_party/bazel/vendor/BUILD.bazel", 1),
            ("third_party/foo/source/BUILD.bazel", 1),
        ],
    )

def test_bazel_lint_framework_ignored_unmatched_files():
    # Uses a real registered check (registered with applies_to_globs = _ALL_BAZEL_FILES)
    # to verify that files not matching the check's file globs (such as README.md
    # containing violation strings) are ignored by shac and not checked.
    res = testing.run(
        _FRAMEWORK_TEST_CHECK,
        files = {
            "README.md": _FRAMEWORK_TEST_VIOLATION,
        },
    )
    asserts.eq(res.findings, ())

def test_bazel_lint_framework_vendor_paths_not_excluded():
    # Tests that vendor paths are not excluded by default by _make_bazel_lint_check.
    # Provide an empty allowlist to isolate framework glob behavior from the
    # check's disk allowlist (which temporarily exempts vendor/).
    res = testing.run(
        _FRAMEWORK_TEST_CHECK,
        files = {
            _FRAMEWORK_TEST_CHECK_ALLOWLIST: _FRAMEWORK_TEST_EMPTY_ALLOWLIST,
            "vendor/foo/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "vendor/foo/src/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "vendor/google/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "vendor/google/src/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
        },
    )
    asserts.eq(
        [(f.filepath, f.line) for f in res.findings],
        [
            ("vendor/foo/BUILD.bazel", 1),
            ("vendor/foo/src/BUILD.bazel", 1),
            ("vendor/google/BUILD.bazel", 1),
            ("vendor/google/src/BUILD.bazel", 1),
        ],
    )

def test_bazel_lint_framework_allowlist():
    allowlist = testing.file(
        content = json.encode({
            "entries": [
                {
                    "bug": "https://fxbug.dev/565897544",
                    "path_prefix": "allowlisted_dir/",
                },
                {
                    "bug": "https://fxbug.dev/565897544",
                    "path": "allowlisted_file/BUILD.bazel",
                },
            ],
        }),
        affected = False,
    )
    res = testing.run(
        _FRAMEWORK_TEST_CHECK,
        files = {
            _FRAMEWORK_TEST_CHECK_ALLOWLIST: allowlist,
            "allowlisted_dir/foo/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
            "allowlisted_file/BUILD.bazel": _FRAMEWORK_TEST_VIOLATION,
        },
    )
    asserts.eq(res.findings, ())

test_bazel_lint_checks = _test_bazel_lint_checks
