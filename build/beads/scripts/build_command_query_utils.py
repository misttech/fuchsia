# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import pathlib
import sys
import typing as T

import gn_runner

# Root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

sys.path.insert(0, str(_FUCHSIA_DIR / "build/api"))
import ninja_artifacts

sys.path.insert(0, str(_FUCHSIA_DIR / "build/bazel/scripts"))
import build_utils

_DEBUG = False


def set_debug(enabled: bool) -> None:
    global _DEBUG
    _DEBUG = enabled


def debug(s: T.Any) -> None:
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


def query_gn_outputs(
    gn: gn_runner.GnRunner, gn_label_pattern: str
) -> dict[str, str]:
    """
    Query GN for outputs matching the given label pattern.

    Args:
        gn: The GnRunner instance to use.
        gn_label_pattern: The GN label pattern to query for.

    Returns:
        A dictionary mapping GN labels to their corresponding output files.
        GN labels with no outputs are NOT included in the final output.
    """
    debug(f"Querying GN outputs using gn desc for {gn_label_pattern}...")
    return {}


def query_ninja_commands(
    gn: gn_runner.GnRunner,
    ninja_runner: ninja_artifacts.NinjaRunner,
    gn_labels: list[str],
    fuchsia_dir: pathlib.Path,
) -> dict[str, str]:
    """
    Fetch the command line of the rustc command for the given GN labels.

    Args:
        gn: The GnRunner instance to use.
        ninja_runner: The NinjaRunner instance to use.
        gn_labels: The GN labels to fetch the command line for.
        fuchsia_dir: Path to the Fuchsia source tree.

    Returns:
        A dictionary mapping GN labels to their corresponding rustc command lines.
    """
    debug(f"Fetching Ninja commands for GN labels {gn_labels}...")
    return {
        gn_label: query_ninja_command(gn, ninja_runner, gn_label, fuchsia_dir)
        for gn_label in gn_labels
    }


def query_ninja_command(
    gn: gn_runner.GnRunner,
    ninja_runner: ninja_artifacts.NinjaRunner,
    gn_label: str,
    fuchsia_dir: pathlib.Path,
) -> str:
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
        A string representing the command line of the rustc command.
    """
    debug(f"Fetching Ninja command for GN label {gn_label}...")

    debug(
        f"Running gn desc for target {gn_label} in build directory {gn.build_dir}"
    )
    desc_output = gn.run_and_extract_output(["desc", gn_label, "outputs"])
    outputs = desc_output.strip().splitlines()
    if not outputs:
        debug(f"No outputs found for GN label {gn_label}.")
        return ""

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
        return ""

    debug(
        f"Selected GN output artifact: {selected_output} to retrieve Ninja command"
    )
    return cmd_raw


def query_bazel_command(
    bazel_launcher: build_utils.BazelLauncher, bazel_label: str
) -> str:
    """
    Query Bazel for the command line of the rustc command for the given Bazel
    label.

    Args:
        bazel_launcher: The BazelLauncher instance to use.
        bazel_label: The Bazel label to fetch the command line for.

    Returns:
        A string representing the command line of the rustc command.
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
        return ""

    try:
        aquery_json = json.loads(res.stdout)
    except json.JSONDecodeError as e:
        debug(f"Error parsing Bazel aquery JSON: {e}")
        return ""

    actions = aquery_json.get("actions", [])
    if not actions:
        debug(f"No actions found in Bazel aquery for {bazel_label}")
        return ""

    # Assume the first action is the relevant action for this target.
    return " ".join(actions[0].get("arguments", []))
