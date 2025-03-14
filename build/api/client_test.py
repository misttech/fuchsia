#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, List

_SCRIPT_DIR = Path(__file__).parent
_BUILD_API_SCRIPT = _SCRIPT_DIR / "client"

# While these values are also defined in ninja_artifacts.py, redefine
# them here to avoid an import statement, as this regression test
# should not depend on implementation details of the client.py
# script.
_NINJA_BUILD_PLAN_DEPS_FILE = "build.ninja.d"
_NINJA_LAST_BUILD_TARGETS_FILE = "last_ninja_build_targets.txt"
_NINJA_LAST_BUILD_SUCCESS_FILE = "last_ninja_build_success.stamp"


def _write_file(path: Path, content: str):
    path.write_text(content)


def _write_json(path: Path, content: Any):
    with path.open("w") as f:
        json.dump(content, f, sort_keys=True)


class ClientTest(unittest.TestCase):
    def setUp(self):
        self._temp_dir = tempfile.TemporaryDirectory()
        self._top_dir = Path(self._temp_dir.name)

        self._build_gn_path = self._top_dir / "BUILD.gn"
        self._build_gn_path.write_text("# EMPTY\n")

        self._build_dir = self._top_dir / "out" / "build_dir"
        self._build_dir.mkdir(parents=True)

        self._build_ninja_d_path = self._build_dir / _NINJA_BUILD_PLAN_DEPS_FILE
        self._build_ninja_d_path.write_text(
            "build.ninja.stamp: ../../BUILD.gn\n"
        )

        self._last_build_success_path = (
            self._build_dir / _NINJA_LAST_BUILD_SUCCESS_FILE
        )
        self._last_targets_path = (
            self._build_dir / _NINJA_LAST_BUILD_TARGETS_FILE
        )
        self._build_ninja_path = self._build_dir / "build.ninja"

        # The build_api_client_info file maps each module name to its .json file.
        with (self._build_dir / "build_api_client_info").open("wt") as f:
            f.write(
                """args=args.json
build_info=build_info.json
tests=tests.json
"""
            )

        # The $BUILD_DIR/args.json file is necessary to extract the
        # target cpu value.
        self._args_json = json.dumps({"target_cpu": "aRm64"})

        # Fake tests.json with a single entry.
        self._tests_json = json.dumps(
            [
                {
                    "environments": [
                        {
                            "dimensions": {
                                "cpu": "y64",
                                "os": "Linux",
                            }
                        }
                    ],
                    "test": {
                        "cpu": "y64",
                        "label": "//some/test:target(//build/toolchain:host_y64)",
                        "name": "host_y64/obj/some/test/target_test.sh",
                        "os": "linux",
                        "path": "host_y64/obj/some/test/target_test.sh",
                        "runtime_deps": "host_y64/gen/some/test/target_test.deps.json",
                    },
                },
            ]
        )

        # Fake build_info.json
        self._build_info_json = json.dumps(
            {
                "configurations": [
                    {
                        "board": "y64",
                        "product": "core",
                    },
                ],
                "version": "",
            },
        )

        _write_file(self._build_dir / "args.json", self._args_json)
        _write_file(self._build_dir / "tests.json", self._tests_json)
        _write_file(self._build_dir / "build_info.json", self._build_info_json)

        # Fake Ninja outputs.
        self._ninja_outputs = {
            "//foo:foo": [
                "obj/foo.stamp",
            ],
            "//bar:bar": [
                "obj/bar.output",
                "obj/bar.stamp",
            ],
            "//src:lib": [
                "obj/src/lib.cc.o",
            ],
            "//src:bin": [
                "obj/src/main.cc.o",
                "obj/src/program",
            ],
            "//tools:hammer(//build/toolchain:host_y64)": [
                "host_y64/exe.unstripped/hammer",
                "host_y64/hammer",
                "host_y64/obj/tools/hammer.cc.o",
            ],
            "//some/test:target(//build/toolchain:host_y64)": [
                "host_y64/obj/some/test/target_test.sh",
                "host_y64/gen/some/test/target_test.deps.json",
            ],
        }
        _write_json(self._build_dir / "ninja_outputs.json", self._ninja_outputs)

    def tearDown(self):
        self._temp_dir.cleanup()

    def run_raw_client(
        self, args: List[str | Path]
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                _BUILD_API_SCRIPT,
            ]
            + [str(a) for a in args],
            cwd=self._build_dir,
            text=True,
            capture_output=True,
        )

    def assert_raw_outputs(
        self,
        raw_ret: subprocess.CompletedProcess,
        expected_out: str,
        expected_err: str = "",
        expected_status: int = 0,
        msg: str = "",
    ):
        if not expected_err and raw_ret.stderr:
            print(f"ERROR: {raw_ret.stderr}", file=sys.stderr)
        self.assertEqual(expected_err, raw_ret.stderr, msg=msg)
        self.assertEqual(expected_out, raw_ret.stdout, msg=msg)
        self.assertEqual(expected_status, raw_ret.returncode, msg=msg)

    def run_client(self, args: List[str | Path]) -> subprocess.CompletedProcess:
        return self.run_raw_client(
            [
                "--build-dir",
                str(self._build_dir),
                "--host-tag=linux-y64",
            ]
            + args
        )

    def assert_output(
        self,
        args: List[str | Path],
        expected_out: str,
        expected_err: str = "",
        expected_status: int = 0,
        msg: str = "",
    ):
        return self.assert_raw_outputs(
            self.run_client(args),
            expected_out,
            expected_err,
            expected_status,
            msg="'%s' command" % " ".join(str(a) for a in args),
        )

    def assert_error(
        self,
        args: List[str | Path],
        expected_err: str,
        msg: str = "",
    ):
        self.assert_output(args, "", expected_err, expected_status=1, msg=msg)

    def test_list(self):
        self.assert_output(["list"], "args\nbuild_info\ntests\n")

    def test_print(self):
        MODULES = {
            "args": self._args_json + "\n",
            "tests": self._tests_json + "\n",
            "build_info": self._build_info_json + "\n",
        }
        for module, expected in MODULES.items():
            self.assert_output(["print", module], expected)

    def test_print_all(self):
        expected = {
            "args": {
                "file": "args.json",
                "json": json.loads(self._args_json),
            },
            "build_info": {
                "file": "build_info.json",
                "json": json.loads(self._build_info_json),
            },
            "tests": {
                "file": "tests.json",
                "json": json.loads(self._tests_json),
            },
        }
        self.assert_output(["print_all"], json.dumps(expected) + "\n")
        self.assert_output(
            ["print_all", "--pretty"], json.dumps(expected, indent=2) + "\n"
        )

    def test_print_all(self):
        expected = {
            "args": {
                "file": "args.json",
                "json": json.loads(self._args_json),
            },
            "build_info": {
                "file": "build_info.json",
                "json": json.loads(self._build_info_json),
            },
            "tests": {
                "file": "tests.json",
                "json": json.loads(self._tests_json),
            },
        }
        self.assert_output(["print_all"], json.dumps(expected) + "\n")
        self.assert_output(
            ["print_all", "--pretty"], json.dumps(expected, indent=2) + "\n"
        )

    def test_ninja_path_to_gn_label(self):
        # Test each Ninja path individually.
        for label, paths in self._ninja_outputs.items():
            for path in paths:
                self.assert_output(
                    ["ninja_path_to_gn_label", path], f"{label}\n"
                )

        # Test each set of Ninja output paths per label.
        for label, paths in self._ninja_outputs.items():
            self.assert_output(["ninja_path_to_gn_label"] + paths, f"{label}\n")

        # Test a single invocation with all Ninja paths, which must return the set of all labels,
        # deduplicated.
        all_paths = set()
        for paths in self._ninja_outputs.values():
            all_paths.update(paths)
        all_labels = sorted(set(self._ninja_outputs.keys()))
        expected = "\n".join(all_labels) + "\n"
        self.assert_output(
            ["ninja_path_to_gn_label"] + sorted(all_paths), expected
        )

        # Test unknown Ninja path
        self.assert_error(
            ["ninja_path_to_gn_label", "obj/unknown/path"],
            "ERROR: Unknown Ninja target path: obj/unknown/path\n",
        )

    def test_gn_labels_to_ninja_paths(self):
        # Test each label individually.
        for label, paths in self._ninja_outputs.items():
            expected = "\n".join(sorted(paths)) + "\n"
            self.assert_output(["gn_label_to_ninja_paths", label], expected)

        # Test all labels at the same time
        all_paths = set()
        for paths in self._ninja_outputs.values():
            all_paths.update(paths)
        expected = "\n".join(sorted(all_paths)) + "\n"
        self.assert_output(
            ["gn_label_to_ninja_paths"] + list(self._ninja_outputs.keys()),
            expected,
        )

        # Test unknown GN label
        self.assert_error(
            ["gn_label_to_ninja_paths", "//unknown:label"],
            "ERROR: Unknown GN label (not in the configured graph): //unknown:label\n",
        )

        # Test unknown GN label
        self.assert_output(
            [
                "gn_label_to_ninja_paths",
                "--allow-unknown",
                "unknown_path",
                "unknown:label",
            ],
            "unknown:label\nunknown_path\n",
        )

        # Test that labels are properly qualified before looking into the database.
        self.assert_output(
            [
                "gn_label_to_ninja_paths",
                "//bar(//build/toolchain/fuchsia:aRm64)",
            ],
            "obj/bar.output\nobj/bar.stamp\n",
        )

        # Test that --allow_unknown does not pass unknown GN labels or absolute file paths.
        self.assert_error(
            [
                "gn_label_to_ninja_paths",
                "--allow-unknown",
                "//unknown:label",
            ],
            "ERROR: Unknown GN label (not in the configured graph): //unknown:label\n",
        )

        self.assert_error(
            [
                "gn_label_to_ninja_paths",
                "--allow-unknown",
                "/unknown/path",
            ],
            "ERROR: Absolute path is not a valid GN label or Ninja path: /unknown/path\n",
        )

    def test_fx_build_args_to_labels(self):
        _TEST_CASES = [
            (["--args", "//aa"], ["//aa:aa"]),
            (
                ["--args", "--host", "//foo/bar"],
                ["//foo/bar:bar(//build/toolchain:host_y64)"],
            ),
            (["--args", "--fuchsia", "//:foo"], ["//:foo"]),
            (
                [
                    "--args",
                    "--host",
                    "//first",
                    "//second",
                    "--fuchsia",
                    "//third",
                    "//fourth",
                    "--fidl",
                    "//fifth",
                ],
                [
                    "//first:first(//build/toolchain:host_y64)",
                    "//second:second(//build/toolchain:host_y64)",
                    "//third:third",
                    "//fourth:fourth",
                    "//fifth:fifth(//build/fidl:fidling)",
                ],
            ),
            (
                [
                    "--allow-unknown",
                    "--args",
                    "first_path",
                    "second_path",
                ],
                ["first_path", "second_path"],
            ),
            (
                [
                    "--allow-unknown",
                    "--args",
                    "//unknown",
                    "//other:unknown",
                ],
                ["//unknown:unknown", "//other:unknown"],
            ),
        ]
        for args, expected_list in _TEST_CASES:
            expected_out = "\n".join(expected_list) + "\n"
            self.assert_output(["fx_build_args_to_labels"] + args, expected_out)

        _WARNING_CASES = [
            (
                ["host_y64/hammer"],
                ["//tools:hammer(//build/toolchain:host_y64)"],
                "WARNING: Use '--host //tools:hammer' instead of Ninja path 'host_y64/hammer'\n",
            ),
        ]
        for args, expected_list, expected_err in _WARNING_CASES:
            expected_out = "\n".join(expected_list) + "\n"
            self.assert_output(
                ["fx_build_args_to_labels", "--args"] + args,
                expected_out,
                expected_err=expected_err,
                expected_status=0,
            )

        _ERROR_CASES = [
            (
                ["host_y64/unknown"],
                "ERROR: Unknown Ninja path: host_y64/unknown\n",
            ),
        ]
        self.maxDiff = 1000
        for args, expected_err in _ERROR_CASES:
            self.assert_error(
                ["fx_build_args_to_labels", "--args"] + args,
                expected_err=expected_err,
            )

    def test_last_ninja_artifacts(self):
        self._build_ninja_path.write_text(
            """
rule copy
  command = cp -f $in $out

build out1: copy input1
build out2: copy out1
build out3: copy out1
build $:default: phony out1
build all: phony out1 out2 out3
"""
        )

        def assert_last_ninja_artifacts_output(expected):
            self.assert_raw_outputs(
                self.run_raw_client(
                    [
                        "--build-dir",
                        str(self._build_dir),
                        "--fuchsia-dir",
                        str(_SCRIPT_DIR.parent.parent.resolve()),
                        "last_ninja_artifacts",
                    ]
                ),
                expected,
            )

        # Verify that if the file doesn't exist, then the result should
        # correspond to the :default target.
        assert not self._last_targets_path.exists()
        assert_last_ninja_artifacts_output("input1\nout1\n")

        # Change the list of targets.
        self._last_targets_path.write_text("all")
        assert_last_ninja_artifacts_output("input1\nout1\nout2\nout3\n")


if __name__ == "__main__":
    unittest.main()
