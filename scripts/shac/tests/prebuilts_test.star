# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/prebuilts.star."""

load("//scripts/shac/prebuilts.star", "check_no_new_direct_prebuilts")

def test_checked_files_and_findings():
    """Verifies matching files are forwarded to ratchet.py and findings are emitted."""
    result = testing.run(
        check_no_new_direct_prebuilts,
        files = {
            "src/foo/BUILD.gn": 'inputs = [ "//prebuilt/third_party/jq/linux-x64/bin/jq" ]\n',
            "src/foo/driver.cc": '#include "prebuilt/touch/firmware.h"\n',
            "src/foo/lib.rs": "",
            "tools/foo/main.go": "",
            "scripts/helper.sh": "",
            "src/foo/CMakeLists.txt": "",
            "tools/devshell/my_cmd": "",
            "tools/devshell/README.md": "//prebuilt/ignored/in/devshell/docs\n",
            "tools/devshell/OWNERS": "*",
            "src/foo/testdata/fixture.cc": '#include "prebuilt/ignored.h"\n',
            "build/prebuilt/on_demand_migration/foo.py": '"//prebuilt/ignored"\n',
            "third_party/foo/bar.py": '"//prebuilt/ignored"\n',
            "docs/README.md": "//prebuilt/ignored/in/docs\n",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--root",
                    testing.root,
                    "--json",
                    "--files-from",
                    testing.any_args,
                ],
                retcode = 1,
                stdout = json.encode([{
                    "level": "error",
                    "filepath": "src/foo/BUILD.gn",
                    "line": 1,
                    "col": 12,
                    "message": "Direct //prebuilt reference is not allowed.",
                }]),
            ),
        ],
    )
    asserts.eq(result.findings, (
        testing.finding(
            level = "error",
            filepath = "src/foo/BUILD.gn",
            line = 1,
            col = 12,
            message = "Direct //prebuilt reference is not allowed.",
        ),
    ))

def test_no_checked_files_skips_exec():
    """Verifies ratchet.py is not invoked when only excluded or non-source files changed."""
    result = testing.run(
        check_no_new_direct_prebuilts,
        files = {
            "docs/README.md": "//prebuilt/ignored/in/docs\n",
            "tools/devshell/README.md": "//prebuilt/ignored/in/devshell/docs\n",
            "tools/devshell/OWNERS": "*",
            "src/foo/README": "See prebuilt/third_party/clang.\n",
            "src/foo/meta/component.cml": "//prebuilt/ignored/in/cml\n",
            "src/foo/testdata/sample.cc": '#include "prebuilt/ignored.h"\n',
            "build/bazel/rules/cipd_prebuilt.bzl": '"//prebuilt/ignored"\n',
            "build/prebuilt/foo.py": '"//prebuilt/ignored/in/build/prebuilt"\n',
            "third_party/foo/bar.py": '"//prebuilt/ignored/in/third_party"\n',
        },
    )
    asserts.eq(result.findings, ())

def test_inconsistent_retcode_fails():
    """Verifies an inconsistent exit code from ratchet.py fails loudly."""
    asserts.fails(
        lambda: testing.run(
            check_no_new_direct_prebuilts,
            files = {"src/foo/BUILD.gn": ""},
            exec_mocks = [
                testing.exec_mock(
                    cmd = [
                        testing.any_args,
                        "--root",
                        testing.root,
                        "--json",
                        "--files-from",
                        testing.any_args,
                    ],
                    retcode = 1,
                    stdout = "[]",
                ),
            ],
        ),
        "exited with code 1, but reported 0 findings",
    )
