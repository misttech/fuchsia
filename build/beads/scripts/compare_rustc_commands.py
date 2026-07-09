#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import typing as T

import build_command_query_utils
import normalize_rustc_args
import shell_utils

# Enable debug logging.
_DEBUG = False

# Root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

# Path to the default Ninja binary.
_DEFAULT_NINJA_BIN = _FUCHSIA_DIR / "prebuilt/third_party/ninja/linux-x64/ninja"

sys.path.insert(0, str(_FUCHSIA_DIR / "build/api"))
import ninja_artifacts

sys.path.insert(0, str(_FUCHSIA_DIR / "build/bazel/scripts"))
import build_utils


def debug(s: T.Any) -> None:
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


def try_get_build_dir(fuchsia_dir: pathlib.Path) -> T.Optional[pathlib.Path]:
    """
    Try to get the build directory using `fx get-build-dir`.

    Args:
        fuchsia_dir: Path to the Fuchsia source tree.

    Returns:
        The build directory if it can be found, None otherwise.
    """
    fx_build_dir_file = fuchsia_dir / ".fx-build-dir"
    if not fx_build_dir_file.exists():
        return None
    return pathlib.Path(fx_build_dir_file.read_text().strip())


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare GN and Bazel build commands for rustc."
    )
    parser.add_argument(
        "--fuchsia_dir",
        type=pathlib.Path,
        default=_FUCHSIA_DIR,
        help="Path to Fuchsia directory",
    )
    parser.add_argument(
        "--build_dir",
        type=pathlib.Path,
        help="Path to GN build directory (e.g. out/default)",
    )
    parser.add_argument(
        "--ninja_outputs_path",
        type=pathlib.Path,
        help="Path to ninja outputs JSON file",
    )
    parser.add_argument(
        "--ninja_bin",
        type=pathlib.Path,
        default=_DEFAULT_NINJA_BIN,
        help="Path to ninja binary",
    )
    parser.add_argument(
        "--gn_label", required=True, help="GN Rust target label"
    )
    parser.add_argument(
        "--bazel_label", required=True, help="Bazel Rust target label"
    )
    parser.add_argument(
        "--read_response_files",
        action="store_true",
        default=False,
        help="Read response files directly from the Bazel execroot instead of querying Starlark providers.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output",
    )
    parser.add_argument(
        "--temp_dir", type=pathlib.Path, help="Temporary directory path"
    )

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose
    build_command_query_utils.set_debug(args.verbose)

    build_dir = args.build_dir or try_get_build_dir(args.fuchsia_dir)
    if not build_dir:
        print(
            "Error: Could not determine build directory. Please provide --build_dir."
        )
        return 1
    ninja_outputs_path = args.ninja_outputs_path or (
        build_dir / "ninja_outputs.json"
    )

    debug(f"Fuchsia Dir: {args.fuchsia_dir}")
    debug(f"Build Dir: {build_dir}")
    debug(f"Ninja outputs path: {ninja_outputs_path}")
    debug(f"Ninja Path: {args.ninja_bin}")
    debug(f"GN Label: {args.gn_label}")
    debug(f"Bazel Label: {args.bazel_label}")

    ninja_runner = ninja_artifacts.NinjaRunner(args.ninja_bin, build_dir)
    gn_cmd_raw = build_command_query_utils.query_ninja_command(
        ninja_runner, ninja_outputs_path, args.gn_label
    )
    gn_cmd = shell_utils.ShellCommand(gn_cmd_raw)

    bazel_paths = build_utils.BazelPaths(args.fuchsia_dir, build_dir)
    bazel_launcher = build_utils.BazelLauncher(bazel_paths.launcher)
    bazel_cmd_raw = build_command_query_utils.query_bazel_command(
        bazel_launcher,
        bazel_paths.execroot,
        args.bazel_label,
        read_response_files=args.read_response_files,
    )
    bazel_cmd = shell_utils.ShellCommand(bazel_cmd_raw)

    gn_rustc_cmd = shell_utils.find_command_with_tool(gn_cmd.split(), "rustc")
    bazel_rustc_cmd = shell_utils.find_command_with_tool(
        bazel_cmd.split(), "rustc"
    )

    debug("====== GN Command ======")
    debug(gn_rustc_cmd)
    debug("====== Bazel Command ======")
    debug(bazel_rustc_cmd)

    if not gn_rustc_cmd or not bazel_rustc_cmd:
        print("Failed to get GN or Bazel rustc command.")
        return 1

    normalized_gn_args = normalize_rustc_args.normalize_rustc_cmd(
        str(gn_rustc_cmd)
    )
    normalized_bazel_args = normalize_rustc_args.normalize_rustc_cmd(
        str(bazel_rustc_cmd)
    )

    temp_dir = tempfile.mkdtemp(
        prefix="compare_rustc_commands_",
        dir=args.temp_dir,
    )
    gn_file = os.path.join(temp_dir, "normalized_gn_args.txt")
    bazel_file = os.path.join(temp_dir, "normalized_bazel_args.txt")
    with open(gn_file, "w") as f:
        f.write("\n".join(normalized_gn_args) + "\n")
    with open(bazel_file, "w") as f:
        f.write("\n".join(normalized_bazel_args) + "\n")

    debug(f"Comparing normalized args with command:")
    debug(f"diff -u {gn_file} {bazel_file}")
    result = subprocess.run(["diff", "-u", gn_file, bazel_file])

    # Preserve temporary results if verbose mode or a temp dir is specified.
    # In these modes, the user may want to inspect the temporary files.
    if not (_DEBUG or args.temp_dir):
        shutil.rmtree(temp_dir, ignore_errors=True)

    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
