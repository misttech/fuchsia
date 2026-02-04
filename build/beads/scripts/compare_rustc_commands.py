#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import pathlib
import shlex
import sys
import tempfile
import typing as T

import gn_runner
import shell_utils

# Enable debug logging.
_DEBUG = False

# Root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

# Path to the default Ninja binary.
_DEFAULT_NINJA_PATH = (
    _FUCHSIA_DIR / "prebuilt/third_party/ninja/linux-x64/ninja"
)

sys.path.insert(0, str(_FUCHSIA_DIR / "build/api"))
import ninja_artifacts

sys.path.insert(0, str(_FUCHSIA_DIR / "build/bazel/scripts"))
import build_utils

# List of args to ignore when comparing GN and Bazel commands.
_ARGS_TO_IGNORE = (
    # Dependency directories and externs are provided by response files in GN,
    # so omit them.
    # This should be OK since missing these args would cause compilation to fail.
    "--extern",
    "-L",
    "-Ldependency",
    "@shell:",
    # Ignore --emit flags for now. In GN they are used to emit dep-info and
    # rmeta files, which Bazel doesn't need for now.
    "--emit=",
    "-Zdep-info-omit-d-target",
    # This is the default value, which Bazel sets explicitly, and GN omits.
    "--error-format=human",
    # TODO(https://fxbug.dev/477167250): Propagate debug_info to Bazel and
    # remove this.
    "-Cdebug-assertions=",
    "-Cdebuginfo=",
    # TODO(https://fxbug.dev/478707341): LTO and thinlto seems to cause
    # inconsistency here, figure out how to match the config between GN and
    # Bazel.
    "-Cembed-bitcode=",
    "-Ccodegen-units=16",
    # TODO(https://fxbug.dev/478707341): Figure out why the default value of
    # -Cstrip is different between GN and Bazel.
    "-Cstrip=",
    # TODO(https://fxbug.dev/478707341): Figure out why the default value of
    # -Copt-level is different between GN and Bazel.
    "-Copt-level=",
    # TODO(https://fxbug.dev/478707341): Figure out how to set the following args in Bazel and remove this.
    "--cfg=__rust_toolchain=",
    "-Cmetadata=",
    "@rust_api_level_cfg_flags.txt",
    "RUST_BACKTRACE=1",
    # TODO(https://fxbug.dev/478707341): Figure out the root causes of link-arg inconsistencies.
    "-Clink-arg=",
    "-Clink-args=",
)

# Argument prefixes that need to be converted for consistency between GN and Bazel.
_ARGS_PREFIXES_TO_CONVERT = {
    "--codegen": "-C",
    "--allow": "-A",
    "--deny": "-D",
    "--warn": "-W",
}


def debug(s: str):
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


def try_get_build_dir() -> T.Optional[pathlib.Path]:
    """
    Try to get the build directory using `fx get-build-dir`.

    Returns:
        The build directory if it can be found, None otherwise.
    """
    fx_build_dir_file = _FUCHSIA_DIR / ".fx-build-dir"
    if not fx_build_dir_file.exists():
        return None
    return pathlib.Path(fx_build_dir_file.read_text().strip())


def query_ninja_command(build_dir: pathlib.Path, gn_label: str) -> list[str]:
    """
    Fetch the command line of the rustc command for the given GN label.

    This function uses `fx gn desc` to get the output files of the GN label.
    Then it uses `fx ninja -t commands` to get the command line of the rustc
    command.

    Args:
        build_dir: The path to the build directory.
        gn_label: The GN label to fetch the command line for.

    Returns:
        A list of strings representing the command line of the rustc command.
    """
    debug(f"Fetching Ninja command for GN label {gn_label}...")

    debug(
        f"Running gn desc for target {gn_label} in build directory {build_dir}"
    )
    gn = gn_runner.GnRunner(build_dir)
    desc_output = gn.run_and_extract_output(["desc", gn_label, "outputs"])
    outputs = desc_output.strip().splitlines()
    if not outputs:
        debug(f"No outputs found for GN label {gn_label}.")
        return []

    cmd_raw = ""
    relative_outputs = [
        os.path.relpath(output[2:], build_dir) for output in outputs
    ]

    ninja_runner = ninja_artifacts.NinjaRunner(_DEFAULT_NINJA_PATH, build_dir)
    for candidate in relative_outputs:
        try:
            ninja_cmd = ["-t", "commands", "-s", candidate]
            debug(f"Running ninja command with args: {ninja_cmd}")
            ninja_cmd_output = ninja_runner.run_and_extract_output(ninja_cmd)
            out = ninja_cmd_output.strip()
            if out:
                cmd_raw = out
                selected_output = candidate
                break
        except Exception as e:
            debug(f"Error running ninja -t commands for {candidate}: {e}")
            continue

    if not cmd_raw:
        debug("Failed to retrieve ninja command for any output.")
        return []

    debug(
        f"Selected GN output artifact: {selected_output} to retrieve Ninja command"
    )
    return shlex.split(cmd_raw)


def query_bazel_command(build_dir: pathlib.Path, bazel_label: str) -> list[str]:
    bazel_paths = build_utils.BazelPaths(_FUCHSIA_DIR, build_dir)
    bazel_launcher = build_utils.BazelLauncher(bazel_paths.launcher)
    query_args = [
        "--config=host",
        "--config=quiet",
        "--output=jsonproto",
        f'mnemonic("Rustc", {bazel_label})',
    ]
    debug(
        f"Fetching Bazel command for {bazel_label} using aquery with args: {query_args}"
    )
    res = bazel_launcher.run_query(
        "aquery",
        query_args,
        ignore_errors=False,
    )
    if res.returncode != 0:
        debug(
            f"Failed to run bazel aquery for {bazel_label}, return code: {res.returncode}"
        )
        debug(f"Stdout:\n{res.stdout}")
        debug(f"Stderr:\n{res.stderr}")
        return []

    try:
        aquery_json = json.loads(res.stdout)
    except json.JSONDecodeError as e:
        debug(f"Error parsing Bazel aquery JSON: {e}")
        return []

    actions = aquery_json.get("actions", [])
    if not actions:
        debug(f"No actions found in Bazel aquery for {bazel_label}")
        return []

    # Assume the first action is the relevant action for this target.
    return actions[0].get("arguments", [])


def normalize_rustc_arg(arg: str) -> str:
    """Normalize a single rustc argument.

    This function normalizes arguments by:
    - Omitting certain arguments that are not relevant to the comparison.
    - Converting some flags to a more common format.

    Args:
        arg: The argument to normalize.

    Returns:
        The normalized argument.
    """
    # Convert to the same flag format.
    if arg.startswith(tuple(_ARGS_PREFIXES_TO_CONVERT.keys())):
        opt, _, val = arg.partition("=")
        opt_new = _ARGS_PREFIXES_TO_CONVERT[opt]
        arg = f"{opt_new}{val}"

    if arg.startswith(_ARGS_TO_IGNORE):
        return ""

    if arg.startswith("-Clinker="):
        parts = arg.split("=", maxsplit=1)
        base_linker_name = os.path.basename(parts[1])
        return f"-Clinker={base_linker_name}"

    return arg


def rindex(l: list[str], value: str) -> int:
    """Find the last index of a value in a list."""
    for i in range(len(l) - 1, -1, -1):
        if l[i] == value:
            return i
    raise ValueError(f"Value {value} not found in list.")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare GN and Bazel build commands for rustc."
    )
    parser.add_argument(
        "--build_dir",
        type=pathlib.Path,
        help="Path to GN build directory (e.g. out/default)",
    )
    parser.add_argument(
        "--gn_label", required=True, help="GN Rust target label"
    )
    parser.add_argument(
        "--bazel_label", required=True, help="Bazel Rust target label"
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output",
    )

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    build_dir = args.build_dir or try_get_build_dir()
    if not build_dir:
        print(
            "Error: Could not determine build directory. Please provide --build_dir."
        )
        return 1

    debug(f"Build Dir: {build_dir}")
    debug(f"GN Label: {args.gn_label}")
    debug(f"Bazel Label: {args.bazel_label}")

    gn_cmd_list = query_ninja_command(build_dir, args.gn_label)
    gn_cmd = shell_utils.ShellCommand(" ".join(gn_cmd_list))

    bazel_cmd_list = query_bazel_command(build_dir, args.bazel_label)
    bazel_cmd = shell_utils.ShellCommand(" ".join(bazel_cmd_list))

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

    normalized_gn_args = sorted(
        set(normalize_rustc_arg(a) for a in str(gn_rustc_cmd).split())
    )
    normalized_bazel_args = sorted(
        set(normalize_rustc_arg(a) for a in str(bazel_rustc_cmd).split())
    )

    temp_dir = tempfile.mkdtemp(prefix="compare_rustc_commands_")
    with open(os.path.join(temp_dir, "normalized_gn_args.txt"), "w") as f:
        f.write("\n".join(normalized_gn_args))
    with open(os.path.join(temp_dir, "normalized_bazel_args.txt"), "w") as f:
        f.write("\n".join(normalized_bazel_args))
    print(f"Wrote args to {temp_dir}, to compare commands, run:")
    print(
        f"diff -y {temp_dir}/normalized_gn_args.txt {temp_dir}/normalized_bazel_args.txt"
    )

    return 0


if __name__ == "__main__":
    sys.exit(main())
