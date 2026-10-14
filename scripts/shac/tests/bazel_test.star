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

def test_disallowed_workspace_root_package_labels():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "foo/BUILD.bazel": "x = \"//:foo/bar\"\n",
            "third_party/foo/src/BUILD.bazel": "",
            "README.md": "",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--root",
                    testing.root,
                    "--json",
                    "foo/BUILD.bazel",
                ],
                retcode = 1,
                stdout = json.encode([{
                    "filepath": "foo/BUILD.bazel",
                    "message": "bad label",
                    "line": 1,
                    "col": 6,
                    "end_col": 16,
                }]),
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "bad label",
            filepath = "foo/BUILD.bazel",
            line = 1,
            col = 6,
            end_col = 16,
        ),
    ))

def test_disallowed_workspace_root_package_labels_inconsistent_retcode():
    asserts.fails(
        lambda: testing.run(
            _bazel_disallowed_workspace_root_package_labels,
            files = {"BUILD.bazel": ""},
            exec_mocks = [
                testing.exec_mock(
                    cmd = [testing.any_args, "--root", testing.root, "--json", "BUILD.bazel"],
                    retcode = 1,
                    stdout = "[]",
                ),
            ],
        ),
        "exited with code 1, but reported 0 findings",
    )

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
