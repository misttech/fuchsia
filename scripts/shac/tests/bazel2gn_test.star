# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the _verify_bazel2gn check in //scripts/shac/bazel_migration.star."""

load("//scripts/shac/bazel_migration.star", "_verify_bazel2gn")

_SENTINEL_GN = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"

_VARS = {"fuchsia_dir": ".", "fuchsia_build_dir": "out"}

_TOOL_PATHS = testing.file(
    content = json.encode([
        {"name": "bazel2gn", "os": os, "cpu": cpu, "path": "bazel2gn", "label": "//build/tools/bazel2gn"}
        for os in ["linux", "mac"]
        for cpu in ["x64", "arm64"]
    ]),
    affected = False,
)

# Example messages, as produced by bazel2gn.
_NO_BAZEL_FILE = "BUILD.gn has the bazel2gn sentinel but there's no BUILD.bazel next to it"
_BAD_CRATE = "crate \":lib\" of rustc_test is not defined in the same BUILD.bazel"

_WITH_DIRECTORY_LIST = [testing.any_args, "--check_only", "--json", testing.any_args, "--directory_list", testing.any_args]

def _run(files, retcode = 1, diagnostics = None, stdout = "", stderr = "", expected_bazel2gn_cmd = _WITH_DIRECTORY_LIST):
    """Runs the check with a mocked bazel2gn, which must match `expected_bazel2gn_cmd`."""
    return testing.run(
        _verify_bazel2gn,
        files = files | {
            "out/bazel2gn": testing.file(affected = False),
            "out/tool_paths.json": _TOOL_PATHS,
        },
        vars = _VARS,
        exec_mocks = [
            testing.exec_mock(
                cmd = expected_bazel2gn_cmd,
                retcode = retcode,
                stdout = json.encode(diagnostics) if diagnostics != None else stdout,
                stderr = stderr,
            ),
        ],
    )

def test_skipped():
    # No exec mocks, so this fails if bazel2gn is run.
    res = testing.run(_verify_bazel2gn, vars = _VARS, files = {
        "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
        "foo/foo.cc": testing.file(action = "M"),
        "bar/BUILD.gn": testing.file(content = "group(\"bar\") {}\n", action = "M"),
        "bar/BUILD.bazel": testing.file(action = "M"),
        # BUILD.bazel without BUILD.gn.
        "baz/BUILD.bazel": testing.file(action = "A"),
        # Deleted BUILD.gn.
        "qux/BUILD.gn": testing.file(action = "D"),
    })
    asserts.eq(res.findings, ())

def test_in_sync():
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        retcode = 0,
    )
    asserts.eq(res.findings, ())

def test_out_of_sync():
    """Out-of-sync directories are reported on their BUILD.gn."""
    res = _run(
        files = {
            "BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
            "BUILD.bazel": testing.file(action = "M"),
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M"),
        },
        diagnostics = [
            {"directory": ".", "file": "BUILD.gn", "out_of_sync": True, "message": "root diff"},
            {"directory": "foo", "file": "foo/BUILD.gn", "out_of_sync": True, "message": "foo diff"},
        ],
    )
    fix = "fx bazel2gn -d . -d foo\n"
    asserts.eq([f.filepath for f in res.findings], ["BUILD.gn", "foo/BUILD.gn"])
    asserts.true(res.findings[0].message.endswith(fix + "\nroot diff"))
    asserts.true(res.findings[1].message.endswith(fix + "\nfoo diff"))

def test_deleted_bazel_file():
    """A deleted BUILD.bazel next to a bazel2gn-managed BUILD.gn is checked."""
    res = _run(
        files = {
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
            "foo/BUILD.bazel": testing.file(action = "D"),
        },
        diagnostics = [{"directory": "foo", "file": "foo/BUILD.gn", "message": _NO_BAZEL_FILE}],
    )
    asserts.eq(res.findings, (
        testing.finding(filepath = "foo/BUILD.gn", message = "bazel2gn failed: " + _NO_BAZEL_FILE),
    ))

def test_untracked_file():
    """Findings on files outside this repository go in the message instead."""
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        diagnostics = [
            {"directory": "vendor/x", "file": "vendor/x/BUILD.bazel", "line": 3, "message": _BAD_CRATE},
            {"directory": "vendor/y", "file": "vendor/y/BUILD.gn", "message": _NO_BAZEL_FILE},
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(message = "vendor/x/BUILD.bazel:3: bazel2gn failed: " + _BAD_CRATE),
        testing.finding(message = "vendor/y/BUILD.gn: bazel2gn failed: " + _NO_BAZEL_FILE),
    ))

def test_modified_tool_checks_all():
    res = _run(
        files = {
            "build/tools/bazel2gn/cmd/main.go": testing.file(action = "M"),
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
        },
        # Without --directory_list, bazel2gn checks all directories.
        expected_bazel2gn_cmd = [testing.any_args, "--check_only", "--json", testing.any_args],
        diagnostics = [{"directory": "foo", "file": "foo/BUILD.gn", "out_of_sync": True, "message": "diff"}],
    )
    asserts.eq([f.filepath for f in res.findings], ["foo/BUILD.gn"])

def test_other_errors():
    """Errors other than out-of-sync BUILD.gn files start with the error itself."""
    res = _run(
        files = {
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
            "foo/BUILD.bazel": testing.file(content = "load()\n\nrustc_test()\n", action = "M"),
            "bar/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M"),
        },
        diagnostics = [
            {"directory": "bar", "file": "bar/BUILD.gn", "message": _NO_BAZEL_FILE},
            {"directory": "foo", "file": "foo/BUILD.bazel", "line": 3, "message": _BAD_CRATE},
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(filepath = "bar/BUILD.gn", message = "bazel2gn failed: " + _NO_BAZEL_FILE),
        testing.finding(filepath = "foo/BUILD.bazel", line = 3, message = "bazel2gn failed: " + _BAD_CRATE),
    ))

def test_error_without_file():
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        diagnostics = [{"directory": "/elsewhere", "message": "directory is not inside --fuchsia_dir"}],
    )
    asserts.eq(res.findings, (
        testing.finding(message = "bazel2gn failed: directory is not inside --fuchsia_dir"),
    ))

def test_fatal_error():
    """bazel2gn prints no diagnostics if it fails before syncing directories."""
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        stderr = "FATAL: --gn_bin is required\n",
    )
    asserts.eq(res.findings, (
        testing.finding(message = "bazel2gn failed: FATAL: --gn_bin is required"),
    ))

def test_non_json_output():
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        stdout = "panic: oops\n",
    )
    asserts.eq(res.findings, (
        testing.finding(message = "bazel2gn failed: panic: oops"),
    ))
