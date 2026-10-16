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

_OUT_OF_SYNC = "BUILD.gn is out of sync with BUILD.bazel"

_WITH_DIRECTORY_LIST = [testing.any_args, "--check_only", testing.any_args, "--directory_list", testing.any_args]

def _run(files, retcode = 1, stderr = "", expected_bazel2gn_cmd = _WITH_DIRECTORY_LIST):
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
    """Out-of-sync affected directories are reported on their BUILD.gn."""
    res = _run(
        files = {
            "BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
            "BUILD.bazel": testing.file(action = "M"),
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M"),
        },
        stderr = """\
FATAL: syncing in batch mode:
* syncing directory foo: %s
foo diff
* syncing directory .: %s
root diff
""" % (_OUT_OF_SYNC, _OUT_OF_SYNC),
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
        stderr = "FATAL: syncing in batch mode:\n* syncing directory foo: formatting foo/BUILD.bazel: no such file\n",
    )
    asserts.eq(res.findings, (
        testing.finding(message = "bazel2gn failed:\n* syncing directory foo: formatting foo/BUILD.bazel: no such file"),
    ))

def test_modified_tool_checks_all():
    res = _run(
        files = {
            "build/tools/bazel2gn/cmd/main.go": testing.file(action = "M"),
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, affected = False),
        },
        # Without --directory_list, bazel2gn checks all directories.
        expected_bazel2gn_cmd = [testing.any_args, "--check_only", testing.any_args],
        stderr = "* syncing directory foo: %s\n" % _OUT_OF_SYNC,
    )
    asserts.eq([f.filepath for f in res.findings], ["foo/BUILD.gn"])

def test_other_errors():
    res = _run(
        files = {
            "foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M"),
            "bar/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M"),
        },
        stderr = """\
FATAL: syncing in batch mode:
* syncing directory foo: %s
The following BUILD files are not in sync:
  - /abs/bar/BUILD.gn
* syncing directory bar: parsing bar/BUILD.bazel: syntax error
""" % _OUT_OF_SYNC,
    )
    asserts.eq([f.filepath for f in res.findings], ["foo/BUILD.gn", ""])
    asserts.eq(
        res.findings[1].message,
        "bazel2gn failed:\n* syncing directory bar: parsing bar/BUILD.bazel: syntax error",
    )

def test_unparseable_error():
    res = _run(
        files = {"foo/BUILD.gn": testing.file(content = _SENTINEL_GN, action = "M")},
        stderr = "FATAL: --gn_bin is required\n",
    )
    asserts.eq(res.findings, (
        testing.finding(message = "bazel2gn failed:\nFATAL: --gn_bin is required"),
    ))
