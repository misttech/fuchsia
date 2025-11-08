#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import collections.abc
import json
import os
import subprocess
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

_SCRIPT_DIR = Path(__file__).parent
_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent
_BUILD_API_SCRIPT = _SCRIPT_DIR / "client"

# While these values are also defined in ninja_artifacts.py, redefine
# them here to avoid an import statement, as this regression test
# should not depend on implementation details of the client.py
# script.
_NINJA_BUILD_PLAN_DEPS_FILE = "build.ninja.d"
_NINJA_LAST_BUILD_TARGETS_FILE = "last_ninja_build_targets.txt"
_NINJA_LAST_BUILD_SUCCESS_FILE = "last_ninja_build_success.stamp"

CommandResult: T.TypeAlias = subprocess.CompletedProcess[str]

CommandArguments: T.TypeAlias = collections.abc.Sequence[str | Path]


def _write_file(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def _write_json(path: Path, content: T.Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        json.dump(content, f, sort_keys=True)


class ClientTestBase(unittest.TestCase):
    def setUp(self) -> None:
        """Common setup for all test classes."""
        self._temp_dir = tempfile.TemporaryDirectory()
        self._top_dir = Path(self._temp_dir.name)

        self._build_gn_path = self._top_dir / "BUILD.gn"
        self._build_gn_path.write_text("# EMPTY\n")

        self._build_dir = self._top_dir / "out" / "build_dir"
        self._build_dir.mkdir(parents=True)

        self._build_ninja_d_path = self._build_dir / _NINJA_BUILD_PLAN_DEPS_FILE
        _write_file(
            self._build_ninja_d_path, "build.ninja.stamp: ../../BUILD.gn\n"
        )

        # A fake host tag used to verify that the command used the --host-tag value
        # properly, instead of picking the real one by mistake.
        self._host_tag = "linux-y64"

        # Compute the real host tag to locate the Ninja binary.
        real_host_arch = os.uname().machine
        real_host_arch = {
            "x86_64": "x64",
            "aarch64": "arm64",
        }.get(real_host_arch, real_host_arch)

        real_host_tag = f"{sys.platform}-{real_host_arch}"

        real_ninja_path = (
            _FUCHSIA_DIR / f"prebuilt/third_party/ninja/{real_host_tag}/ninja"
        )
        assert (
            real_ninja_path.exists()
        ), f"Missing Ninja binary: {real_ninja_path}"

        # Create symlink to real Ninja binary here.
        self._ninja_path = (
            self._top_dir / f"prebuilt/third_party/ninja/{self._host_tag}/ninja"
        )
        self._ninja_path.parent.mkdir(parents=True)
        self._ninja_path.symlink_to(real_ninja_path)

        self._last_build_success_path = (
            self._build_dir / _NINJA_LAST_BUILD_SUCCESS_FILE
        )
        self._last_targets_path = (
            self._build_dir / _NINJA_LAST_BUILD_TARGETS_FILE
        )
        self._build_ninja_path = self._build_dir / "build.ninja"

        # The build_api_client_info file maps each module name to its .json file.
        _write_file(
            self._build_dir / "build_api_client_info",
            "\n".join(
                [
                    "args=args.json",
                    "build_info=build_info.json",
                    "debug_symbols=debug_symbols.json",
                    "tests=tests.json",
                ]
            ),
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

        # Fake debug symbols
        self._debug_symbols_json = json.dumps(
            [
                {
                    "cpu": "x64",
                    "debug": "obj/src/foo/lib_shared/libfoo.so.unstripped",
                    "elf_build_id": "00000000000000001",
                    "label": "//src/foo:lib_shared",
                    "os": "fuchsia",
                },
                {
                    "cpu": "x64",
                    "debug": "../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug",
                    "label": "//prebuilt/foo:symbol_file",
                    "os": "fuchsia",
                },
                {
                    "cpu": "x64",
                    "debug": "obj/src/bar/binary.unstripped",
                    "elf_build_id_file": "obj/src/bar/binary.elf_build_id",
                    "label": "//src/bar:binary",
                    "os": "fuchsia",
                },
                {
                    "cpu": "x64",
                    "debug": "obj/src/zoo/binary.unstripped",
                    "label": "//src/zoo:binary",
                    "os": "fuchsia",
                },
            ]
        )
        _write_file(
            self._build_dir / "debug_symbols.json", self._debug_symbols_json
        )
        _write_file(
            self._build_dir / "obj/src/bar/binary.elf_build_id",
            "build_id_for_bar",
        )

    def tearDown(self) -> None:
        """Common cleanup for all test classes."""
        self._temp_dir.cleanup()

    def run_client(self, args: CommandArguments) -> CommandResult:
        """Run a //build/api/client command and return results after capturing output as text.

        This runs the command in the test's build directory to mimic calls from Ninja actions.

        Args:
            args: The command name followed by its optional arguments.
        Returns:
            A CommandResult value.
        """
        return subprocess.run(
            [
                str(a)
                for a in [
                    _BUILD_API_SCRIPT,
                    "--fuchsia-dir",
                    self._top_dir,
                    "--build-dir",
                    self._build_dir,
                    f"--host-tag={self._host_tag}",
                    *args,
                ]
            ],
            cwd=self._build_dir,
            text=True,
            capture_output=True,
        )

    def assert_command_result(
        self,
        ret: CommandResult,
        expected_out: str,
        expected_err: str = "",
        expected_status: int = 0,
        msg: str = "",
    ) -> None:
        """Assert the result of executing a given command with run_client().

        Args:
            raw_ret: A CommandResult value from run_client().
            expected_out: The expected stdout.
            expected_err: The expected stderr, defaults to an empty string.
            expected_status: The expected status code, defaults to 0.
            msg: Optional message printed in case of assertion failure. If
               empty (the default), the command arguments will be used instead
        """
        if not expected_err and ret.stderr:
            print(f"ERROR: {ret.stderr}", file=sys.stderr)
        self.assertEqual(expected_err, ret.stderr, msg=msg)
        self.assertEqual(expected_out, ret.stdout, msg=msg)
        self.assertEqual(expected_status, ret.returncode, msg=msg)

    def assert_output(
        self,
        args: CommandArguments,
        expected_out: str,
        expected_err: str = "",
        expected_status: int = 0,
        msg: str = "",
    ) -> None:
        """Run a command through run_client() then call assert_command_result() on its result.

        Args:
            args: A sequence of command arguments.
            expected_out: The expected stdout.
            expected_err: The expected stderr, defaults to an empty string.
            expected_status: The expected status code, defaults to 0.
            msg: Optional message printed in case of failure.
        """
        return self.assert_command_result(
            self.run_client(args),
            expected_out,
            expected_err,
            expected_status,
            msg="'%s' command" % " ".join(str(a) for a in args)
            + (": " + msg if msg else ""),
        )

    def assert_error(
        self,
        args: CommandArguments,
        expected_err: str,
        msg: str = "",
    ) -> None:
        """Run a command through run_client() and expect it to fail with no output

        This also assumes that the status code is 1.

        Args:
            args: A sequence of command arguments.
            expected_err: The expected stderr.
            msg: Optional message printed in case of failure.
        """
        self.assert_output(args, "", expected_err, expected_status=1, msg=msg)


class ClientTest(ClientTestBase):
    def test_list(self) -> None:
        self.assert_output(["list"], "args\nbuild_info\ndebug_symbols\ntests\n")

    def test_print(self) -> None:
        MODULES = {
            "args": self._args_json + "\n",
            "tests": self._tests_json + "\n",
            "build_info": self._build_info_json + "\n",
        }
        for module, expected in MODULES.items():
            self.assert_output(["print", module], expected)

    def test_print_all(self) -> None:
        expected = {
            "args": {
                "file": "args.json",
                "json": json.loads(self._args_json),
            },
            "build_info": {
                "file": "build_info.json",
                "json": json.loads(self._build_info_json),
            },
            "debug_symbols": {
                "file": "debug_symbols.json",
                "json": json.loads(self._debug_symbols_json),
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

    def test_print_debug_symbols(self) -> None:
        self.maxDiff = None
        expected = [
            {
                "cpu": "x64",
                "debug": "obj/src/foo/lib_shared/libfoo.so.unstripped",
                "elf_build_id": "00000000000000001",
                "label": "//src/foo:lib_shared",
                "os": "fuchsia",
            },
            {
                "cpu": "x64",
                "debug": "../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug",
                "label": "//prebuilt/foo:symbol_file",
                "os": "fuchsia",
            },
            {
                "cpu": "x64",
                "debug": "obj/src/bar/binary.unstripped",
                "elf_build_id_file": "obj/src/bar/binary.elf_build_id",
                "label": "//src/bar:binary",
                "os": "fuchsia",
            },
            {
                "cpu": "x64",
                "debug": "obj/src/zoo/binary.unstripped",
                "label": "//src/zoo:binary",
                "os": "fuchsia",
            },
        ]
        self.assert_output(["print_debug_symbols"], json.dumps(expected) + "\n")
        self.assert_output(
            ["print_debug_symbols", "--pretty"],
            json.dumps(expected, indent=2) + "\n",
        )

    def test_print_debug_symbols_with_build_id_resolution(self) -> None:
        self.maxDiff = None
        expected = [
            {
                "cpu": "x64",
                "debug": "obj/src/foo/lib_shared/libfoo.so.unstripped",
                "elf_build_id": "00000000000000001",
                "label": "//src/foo:lib_shared",
                "os": "fuchsia",
            },
            {
                "cpu": "x64",
                "debug": "../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug",
                "elf_build_id": "aabbbbbbbbbbb",
                "label": "//prebuilt/foo:symbol_file",
                "os": "fuchsia",
            },
            {
                "cpu": "x64",
                "debug": "obj/src/bar/binary.unstripped",
                "elf_build_id": "build_id_for_bar",
                "elf_build_id_file": "obj/src/bar/binary.elf_build_id",
                "label": "//src/bar:binary",
                "os": "fuchsia",
            },
            # NOTE: Because of the --test-mode flag used below, the build-id value for the
            # file obj/src/zoo/binary.unstripped is just its file name. This avoids creating
            # a fake ELF file in the test build directory.
            {
                "cpu": "x64",
                "debug": "obj/src/zoo/binary.unstripped",
                "elf_build_id": "binary.unstripped",
                "label": "//src/zoo:binary",
                "os": "fuchsia",
            },
        ]
        self.assert_output(
            ["print_debug_symbols", "--resolve-build-ids", "--test-mode"],
            json.dumps(expected) + "\n",
        )
        self.assert_output(
            [
                "print_debug_symbols",
                "--resolve-build-ids",
                "--test-mode",
                "--pretty",
            ],
            json.dumps(expected, indent=2) + "\n",
        )

    def test_ninja_path_to_gn_label(self) -> None:
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

    def test_ninja_target_to_gn_labels(self) -> None:
        # Test each Ninja path basename individually. This works because
        # the only two file paths with the same name are produced by the same GN
        # label.
        for label, paths in self._ninja_outputs.items():
            for target_name in list({os.path.basename(p) for p in paths}):
                self.assert_output(
                    ["ninja_target_to_gn_labels", target_name], f"{label}\n"
                )

        # Test unknown Ninja target name
        self.assert_output(["ninja_target_to_gn_labels", "unknown_target"], "")

        # Test malformed Ninja target name
        self.assert_error(
            ["ninja_target_to_gn_labels", "some/path"],
            "ERROR: Malformed Ninja target file name: some/path\n",
        )

        # Update the ninja_outputs.json file to include a second label
        # that generates a target with the name "hammer", then check that
        # the command returns a list with two labels.
        self._ninja_outputs["//secondary:hammer_target"] = ["other/hammer"]
        _write_json(self._build_dir / "ninja_outputs.json", self._ninja_outputs)

        self.assert_output(
            ["ninja_target_to_gn_labels", "hammer"],
            "//secondary:hammer_target\n"
            + "//tools:hammer(//build/toolchain:host_y64)\n",
        )

    def test_gn_labels_to_ninja_paths(self) -> None:
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

    def test_fx_build_args_to_labels(self) -> None:
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
                [
                    "--args",
                    "host_y64/hammer",
                ],
                ["//tools:hammer(//build/toolchain:host_y64)"],
                "WARNING: Use '--host //tools:hammer' instead of Ninja path 'host_y64/hammer'\n",
            ),
            (
                [
                    "--allow-targets",
                    "--args",
                    "hammer",
                ],
                ["//tools:hammer(//build/toolchain:host_y64)"],
                "WARNING: Use '--host //tools:hammer' instead of Ninja target 'hammer'\n",
            ),
        ]
        for args, expected_list, expected_err in _WARNING_CASES:
            expected_out = "\n".join(expected_list) + "\n"
            self.assert_output(
                ["fx_build_args_to_labels"] + args,
                expected_out,
                expected_err=expected_err,
                expected_status=0,
            )

        _ERROR_CASES = [
            (
                [
                    "--args",
                    "host_y64/unknown",
                ],
                "ERROR: Unknown Ninja path: host_y64/unknown\n",
            ),
            (
                [
                    "--allow-targets",
                    "--args",
                    "first_path",
                    "second/path",
                ],
                "ERROR: Unknown Ninja target: first_path\n"
                + "ERROR: Unknown Ninja path: second/path\n",
            ),
        ]
        self.maxDiff = 1000
        for args, expected_err in _ERROR_CASES:
            self.assert_error(
                ["fx_build_args_to_labels"] + args,
                expected_err=expected_err,
            )

    def test_last_ninja_artifacts(self) -> None:
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

        def assert_last_ninja_artifacts_output(expected: str) -> None:
            self.assert_output(["last_ninja_artifacts"], expected)

        # Verify that if the file doesn't exist, then the result should
        # correspond to the :default target.
        assert not self._last_targets_path.exists()
        assert_last_ninja_artifacts_output("out1\n")

        # Change the list of targets.
        self._last_targets_path.write_text("all")
        assert_last_ninja_artifacts_output("out1\nout2\nout3\n")

    def test_export_last_build_debug_symbols(self) -> None:
        self.maxDiff = None

        self._build_ninja_path.write_text(
            """
rule whatever
  command = ignored

build obj/src/foo/lib_shared/libfoo.so.unstripped obj/src/bar/binary.unstripped obj/src/zoo/binary.unstripped: whatever ../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug

build $:default: phony obj/src/foo/lib_shared/libfoo.so.unstripped
"""
        )

        # Create a fake dump_syms tool that simply prints the path of
        # the input debug symbol file.
        dump_syms = self._top_dir / "dump_syms"
        dump_syms.write_text(
            f"""#!{sys.executable}
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("-r", action="store_true")
parser.add_argument("-n")
parser.add_argument("-o")
parser.add_argument("debug_symbol_file")

args = parser.parse_args()

print(args.debug_symbol_file)
"""
        )
        dump_syms.chmod(0o755)

        gsymutil = self._top_dir / "gsymutil"
        gsymutil.write_text(
            f"""#!{sys.executable}
import argparse
import os

parser = argparse.ArgumentParser()
parser.add_argument("--convert", required=True, help="Input debug binary")
parser.add_argument("--out-file", required=True, help="Output file path")
args = parser.parse_args()

os.makedirs(os.path.dirname(args.out_file), exist_ok=True)
with open(args.out_file, "wt") as f:
    f.write(args.convert)
    f.write("\\n")
"""
        )
        gsymutil.chmod(0o755)

        export_dir = self._top_dir / "exported_debug_symbols"

        expected_err = """MISSING build-id FOR {'cpu': 'x64', 'debug': 'obj/src/zoo/binary.unstripped', 'label': '//src/zoo:binary', 'os': 'fuchsia'}
"""

        expected_out = f"""Creating {export_dir}/build-ids.json
Creating {export_dir}/build-ids.txt
Creating 3 symlinks in {export_dir}
Generating 3 breakpad symbols in {export_dir}
  - Creating .build-id/00/000000000000001.sym FROM obj/src/foo/lib_shared/libfoo.so.unstripped
  - Creating .build-id/aa/bbbbbbbbbbb.sym FROM ../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug
  - Creating .build-id/bu/ild_id_for_bar.sym FROM obj/src/bar/binary.unstripped
Generating 3 GSYM symbols in {export_dir}
  - Creating .build-id/00/000000000000001.gsym FROM obj/src/foo/lib_shared/libfoo.so.unstripped
  - Creating .build-id/aa/bbbbbbbbbbb.gsym FROM ../../prebuilt/.build-id/aa/bbbbbbbbbbb.debug
  - Creating .build-id/bu/ild_id_for_bar.gsym FROM obj/src/bar/binary.unstripped
Done!
"""
        self.assert_output(
            [
                "export_last_build_debug_symbols",
                f"--output-dir={export_dir}",
                "--with-breakpad-symbols",
                f"--dump_syms={dump_syms}",
                "--with-gsym-symbols",
                f"--gsymutil={gsymutil}",
            ],
            expected_out,
            expected_err,
        )


class ShouldFileChangesTriggerBuildClientTest(ClientTestBase):
    def setUp(self) -> None:
        super().setUp()
        self._build_ninja_d_path.write_text(
            "build.ninja.stamp: ../../BUILD.gn host_x64/ignore.me ../../src/template.gni\n"
        )

        # The build_api_client_info file maps each module name to its .json file.
        _write_file(
            self._build_dir / "build_api_client_info",
            """args=args.json
build_info=build_info.json
""",
        )

        # Ninja build plan that matches the following diagram
        #
        #    //foo.cc    //foo.h   //bar.h  //bar.cc
        #       |         |   |        |        |
        #       |         |   |        |        |
        #       ===========   ===================
        #            |                 |
        #       obj/foo.cc.o        obj/bar.cc.o
        #
        #
        #    //src/lib.cc
        #         |
        #         |
        #       =====
        #         |
        #    obj/src/lib.cc.o    //src/main.cc
        #         |                    |
        #         |                    |
        #         ======================
        #            |              |
        #      obj/src/program   obj/src/min.cc.o
        #            |
        #            |
        #           ===
        #            |
        #         :default
        #
        self._build_ninja_path.write_text(
            r"""
rule compile
    command = touch $out

rule link
    command = touch $out

build obj/foo.cc.o: compile ../../foo.cc ../../foo.h

build obj/bar.cc.o: compile ../../bar.cc ../../bar.h ../../foo.h

build obj/src/lib.cc.o: compile ../../src/lib.cc

build obj/src/main.cc.o obj/src/program: link ../../src/main.cc obj/src/lib.cc.o

build $:default: phony obj/src/program

default $:default
"""
        )

        # Fake Ninja outputs matching the build plan above.
        self._ninja_outputs = {
            "//:foo": [
                "obj/foo.cc.o",
            ],
            "//:bar": [
                "obj/bar.cc.o",
            ],
            "//src:lib": [
                "obj/src/lib.cc.o",
            ],
            "//src:bin": [
                "obj/src/main.cc.o",
                "obj/src/program",
            ],
        }
        _write_json(self._build_dir / "ninja_outputs.json", self._ninja_outputs)

        self._files_list_path = self._top_dir / "files_list.txt"

    def write_files_list(self, files: list[str]) -> None:
        self._files_list_path.write_text("\n".join(files))

    def _test_changed_files(
        self, changed_files: list[str], expected_out: str
    ) -> None:
        self.write_files_list(changed_files)
        self.assert_output(
            args=[
                "should_file_changes_trigger_build",
                f"--files-list={self._files_list_path}",
            ],
            expected_out=expected_out,
            msg=f"for changed files {changed_files}",
        )

    def test_no_changes_needed(self) -> None:
        TEST_CASES = (
            # No changed files at all.
            [],
            # Changed files are sources that are not inputs in the current build plan.
            ["src/other.cc"],
            # Chagned files are build files that are used not used by the current GN graph.
            ["other/BUILD.gn", "other/template.gni"],
        )

        for changed_files in TEST_CASES:
            self._test_changed_files(changed_files, "NO\n")

    def test_build_file_changes(self) -> None:
        TEST_CASES = (
            # Main build file changed.
            ["BUILD.gn"],
            # Unrelated and main build file changed.
            ["other/BUILD.gn", "BUILD.gn"],
            # Main .gni file changed.
            ["src/template.gni"],
            # Unrelated and main .gni file changed.
            ["other/template.gni", "src/template.gni"],
            # Unrelated and main build and .gni files changed.
            [
                "BUILD.gn",
                "other/BUILD.gn",
                "src/template.gni",
                "other/template.gni",
            ],
        )
        for changed_files in TEST_CASES:
            self._test_changed_files(
                changed_files, "YES: GN build graph changed.\n"
            )

    def test_source_file_changes(self) -> None:
        TEST_CASES = (
            # Sources that are dependencies of :default should trigger a rebuild.
            (["src/lib.cc"], "YES: Sources updated for target: :default\n"),
            # Sources that are not dependencies of :default should not trigger a rebuild.
            (["bar.cc"], "NO\n"),
            # Sources that are inputs for different targets.
            (
                ["bar.cc", "src/main.cc"],
                "YES: Sources updated for target: :default\n",
            ),
        )
        for changed_files, expected_out in TEST_CASES:
            self._test_changed_files(changed_files, expected_out)

        # Now make 'foo' and 'bar' the last build's targets.
        self._last_targets_path.write_text("obj/foo.cc.o obj/bar.cc.o\n")

        TEST_CASES = (
            # Sources that are dependencies of foo or bar should trigger a rebuild.
            (["bar.cc"], "YES: Sources updated for target: obj/bar.cc.o\n"),
            (["foo.cc"], "YES: Sources updated for target: obj/foo.cc.o\n"),
            (["foo.cc", "bar.cc"], "YES: Sources updated for 2 targets.\n"),
            (["foo.h"], "YES: Sources updated for 2 targets.\n"),
            # Sources that are not dependencies of foo or bar should not trigger a rebuild.
            (["src/lib.cc"], "NO\n"),
            # Changed to build files and sources should report build change only.
            (["BUILD.gn", "bar.cc"], "YES: GN build graph changed.\n"),
        )
        for changed_files, expected_out in TEST_CASES:
            self._test_changed_files(changed_files, expected_out)


if __name__ == "__main__":
    unittest.main()
