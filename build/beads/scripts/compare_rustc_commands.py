#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import pathlib
import shlex
import shutil
import subprocess
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
_DEFAULT_NINJA_BIN = _FUCHSIA_DIR / "prebuilt/third_party/ninja/linux-x64/ninja"

# Path to the default GN binary.
_DEFAULT_GN_BIN = _FUCHSIA_DIR / "prebuilt/third_party/gn/linux-x64/gn"

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
    "-Zshell-argfiles",
    "@shell:",
    # Ignore --emit flags for now. In GN they are used to emit dep-info and
    # rmeta files, which Bazel doesn't need for now.
    "--emit=",
    "-Zdep-info-omit-d-target",
    # This is the default value, which Bazel sets explicitly, and GN omits.
    "--error-format=human",
    # GN and Bazel writes outputs to different locations, so ignore output flags.
    "--out-dir=",
    "-o=",
    # Bazel sets sysroot to the Rust toolchain in bazel-out, while GN omits this.
    "--sysroot=",
    # Bazel sets this for determinism purposes, and GN omits it.
    "--remap-path-prefix=",
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
    # TODO(https://fxbug.dev/478707341): Figure out how to make remote flags
    # consistent between GN and Bazel.
    "--remote-flag=",
    # TODO(https://fxbug.dev/478707341): GN uses clang++ and Bazel uses clang.
    # Figure out where this discrepancy comes from and remove this.
    "-Clinker=",
    # TODO(https://fxbug.dev/478707341): Bazel adds this for some binaries.
    # Figure out why and determine if it's OK to ignore.
    "-Cextra-filename=",
)

# Argument prefixes that need to be converted for consistency between GN and Bazel.
_ARGS_PREFIXES_TO_CONVERT = {
    "--codegen": "-C",
    "--allow": "-A",
    "--deny": "-D",
    "--warn": "-W",
}


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


def query_ninja_command(
    gn: gn_runner.GnRunner,
    ninja_runner: ninja_artifacts.NinjaRunner,
    gn_label: str,
    fuchsia_dir: pathlib.Path,
) -> list[str]:
    """
    Fetch the command line of the rustc command for the given GN label.

    This function uses `fx gn desc` to get the output files of the GN label.
    Then it uses `fx ninja -t commands` to get the command line of the rustc
    command.

    Args:
        gn: The GnRunner instance to use.
        ninja_runner: The NinjaRunner instance to use.
        gn_label: The GN label to fetch the command line for.

    Returns:
        A list of strings representing the command line of the rustc command.
    """
    debug(f"Fetching Ninja command for GN label {gn_label}...")

    debug(
        f"Running gn desc for target {gn_label} in build directory {gn.build_dir}"
    )
    desc_output = gn.run_and_extract_output(["desc", gn_label, "outputs"])
    outputs = desc_output.strip().splitlines()
    if not outputs:
        debug(f"No outputs found for GN label {gn_label}.")
        return []

    debug(f"outputs from GN desc: {outputs}")
    # Outputs are in format `//out/dir/foo/bar`. Need to strip the `//out/dir` part.
    # Build root is `out/dir`.
    build_root_relpath = os.path.relpath(gn.build_dir, fuchsia_dir)
    relative_outputs = [
        os.path.relpath(output[2:], build_root_relpath) for output in outputs
    ]

    cmd_raw = ""
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
            print(f"ERROR: running ninja -t commands for {candidate}: {e}")
            continue

    if not cmd_raw:
        debug("Failed to retrieve ninja command for any output.")
        return []

    debug(
        f"Selected GN output artifact: {selected_output} to retrieve Ninja command"
    )
    return shlex.split(cmd_raw)


def query_bazel_command(
    bazel_launcher: build_utils.BazelLauncher, bazel_label: str
) -> list[str]:
    """
    Query Bazel for the command line of the rustc command for the given Bazel
    label.

    Args:
        bazel_launcher: The BazelLauncher instance to use.
        bazel_label: The Bazel label to fetch the command line for.

    Returns:
        A list of strings representing the command line of the rustc command.
    """
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

    if not arg.startswith("-"):
        # This is likely a path, e.g. not a `--arg=val` argument, so try to
        # apply path-related normalization.

        # Normalize paths to the `rustc` compiler.
        if os.path.basename(arg) == "rustc":
            return "rustc"

        # Ignore Bazel-specific paths for prebuilt rust toolchain lib.
        if arg.startswith("bazel-out") and "fuchsia_prebuilt_rust" in arg:
            return ""

        # Strip `../../` prefixes, which is how GN/Ninja locates sources.
        if arg.startswith("../../"):
            return arg[6:]

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
        "--ninja_bin",
        type=pathlib.Path,
        default=_DEFAULT_NINJA_BIN,
        help="Path to ninja binary",
    )
    parser.add_argument(
        "--gn_bin",
        type=pathlib.Path,
        default=_DEFAULT_GN_BIN,
        help="Path to gn binary",
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
    parser.add_argument(
        "--temp_dir", type=pathlib.Path, help="Temporary directory path"
    )

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    build_dir = args.build_dir or try_get_build_dir(args.fuchsia_dir)
    if not build_dir:
        print(
            "Error: Could not determine build directory. Please provide --build_dir."
        )
        return 1

    debug(f"Fuchsia Dir: {args.fuchsia_dir}")
    debug(f"Build Dir: {build_dir}")
    debug(f"GN Path: {args.gn_bin}")
    debug(f"Ninja Path: {args.ninja_bin}")
    debug(f"GN Label: {args.gn_label}")
    debug(f"Bazel Label: {args.bazel_label}")

    gn = gn_runner.GnRunner(build_dir, args.gn_bin)
    ninja_runner = ninja_artifacts.NinjaRunner(args.ninja_bin, build_dir)
    gn_cmd_list = query_ninja_command(
        gn, ninja_runner, args.gn_label, args.fuchsia_dir
    )
    gn_cmd = shell_utils.ShellCommand(" ".join(gn_cmd_list))

    bazel_paths = build_utils.BazelPaths(args.fuchsia_dir, build_dir)
    bazel_launcher = build_utils.BazelLauncher(bazel_paths.launcher)
    bazel_cmd_list = query_bazel_command(bazel_launcher, args.bazel_label)
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

    # Fixups to the GN rustc command where it uses `--arg val` instead of
    # `--arg=val`, which differ from Bazel. These need to be done before
    # tokenizing the command line.
    gn_rustc_cmd_replaced = (
        str(gn_rustc_cmd)
        .replace("--target ", "--target=")
        .replace("-o ", "-o=")
    )

    normalized_gn_args = sorted(
        set(normalize_rustc_arg(a) for a in shlex.split(gn_rustc_cmd_replaced))
    )
    normalized_bazel_args = sorted(
        set(normalize_rustc_arg(a) for a in shlex.split(str(bazel_rustc_cmd)))
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
