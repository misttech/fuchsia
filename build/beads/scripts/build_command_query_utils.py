# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import pathlib
import sys
import typing as T
from itertools import zip_longest

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


def query_ninja_commands(
    ninja_runner: ninja_artifacts.NinjaRunner,
    ninja_outputs_path: pathlib.Path,
    gn_labels: list[str],
) -> dict[str, str]:
    """
    Fetch the ninja build commands for the given GN labels.

    Args:
        ninja_runner: The NinjaRunner instance to use.
        ninja_outputs_path: Path to the ninja outputs JSON file, it stores a mapping from GN labels
            to Ninja outputs.
        gn_labels: The GN labels to fetch the command line for.

    Returns:
        A dictionary mapping GN labels to their corresponding Ninja build commands.
    """
    debug(f"Querying Ninja commands for GN labels {gn_labels}...")

    if not gn_labels:
        return {}

    with ninja_outputs_path.open("r") as f:
        ninja_outputs: dict[str, list[str]] = json.load(f)

    all_outputs = []
    for gn_label in gn_labels:
        outputs = ninja_outputs.get(gn_label)
        if not outputs:
            raise ValueError(
                f"Could not find outputs for label {gn_label} in {ninja_outputs_path}"
            )
        all_outputs.append((gn_label, outputs))
    debug(f"Found Ninja outputs: {all_outputs}")

    outputs_for_query = [
        output for _, outputs in all_outputs for output in outputs
    ]
    ninja_cmd = ["-t", "commands", "-s"] + outputs_for_query
    debug(f"Running ninja command with args: {ninja_cmd}")

    ninja_cmd_output = ninja_runner.run_and_extract_output(ninja_cmd)
    commands = ninja_cmd_output.strip().splitlines()

    # On successful runs of `ninja -t commands -s`, each GN label should have a single Ninja command
    # associated with it.
    #
    # We check that this is true by checking that each command contains the expected output.
    results = {}
    for (gn_label, outputs), cmd in zip_longest(all_outputs, commands):
        # This happens if the ninja query returned a shorter output than expected.
        if not cmd:
            raise ValueError(
                f"Could not find command for label: {gn_label}, no command returned from Ninja query"
            )

        # Confirm that the command is associated with the expected output.
        for output in outputs:
            if output in cmd:
                results[gn_label] = cmd
                break

        if gn_label not in results:
            raise ValueError(
                f"Could not find command for label: {gn_label}, no matching output found in Ninja command"
            )

    return results


def query_ninja_command(
    ninja_runner: ninja_artifacts.NinjaRunner,
    ninja_outputs_path: pathlib.Path,
    gn_label: str,
) -> str:
    """
    Fetch the Ninja build command for the given GN label.

    Args:
        ninja_runner: The NinjaRunner instance to use.
        ninja_outputs_path: Path to the ninja outputs JSON file.
        gn_label: The GN label to fetch the command line for.

    Returns:
        A string representing the Ninja build command for the given GN label.
    """
    debug(f"Fetching Ninja command for GN label {gn_label}...")
    return query_ninja_commands(ninja_runner, ninja_outputs_path, [gn_label])[
        gn_label
    ]


def query_bazel_commands(
    bazel_launcher: build_utils.BazelLauncher, bazel_labels: list[str]
) -> dict[str, str]:
    """
    Query Bazel for the command lines of the rustc commands for the given Bazel labels.

    Args:
        bazel_launcher: The BazelLauncher instance to use.
        bazel_labels: The Bazel labels to fetch the command lines for.

    Returns:
        A dictionary mapping Bazel labels to their corresponding rustc command lines.
    """
    if not bazel_labels:
        return {}

    query_targets = " + ".join(bazel_labels)
    query_args = [
        "--config=host",
        "--config=quiet",
        "--output=jsonproto",
        f'mnemonic("Rustc", {query_targets})',
    ]
    debug(
        f"Fetching Bazel commands for {bazel_labels} using aquery with args: {query_args}"
    )
    res = bazel_launcher.run_query(
        "aquery",
        query_args,
        ignore_errors=False,
    )
    if res.returncode != 0:
        debug(f"Bazel aquery stdout:\n{res.stdout}")
        debug(f"Bazel aquery stderr:\n{res.stderr}")
        raise ValueError(
            f"Failed to run bazel aquery for labels, return code: {res.returncode}"
        )

    aquery_json = json.loads(res.stdout)
    targets = aquery_json.get("targets", [])
    target_id_to_label = {
        target.get("id"): target.get("label") for target in targets
    }

    results: dict[str, str] = {}
    actions = aquery_json.get("actions", [])
    for action in actions:
        target_id = action.get("targetId")
        if not target_id:
            continue

        label = target_id_to_label.get(target_id)
        if not label:
            # Not a label we know about, skip.
            continue
        arguments = " ".join(action.get("arguments", []))
        if not arguments:
            # Skip actions that don't have any arguments.
            continue
        results.setdefault(label, arguments)

    missing_labels = [label for label in bazel_labels if label not in results]
    if missing_labels:
        raise ValueError(f"Could not find command for labels: {missing_labels}")

    return results


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
    debug(f"Fetching Bazel command for Bazel label {bazel_label}...")
    return query_bazel_commands(bazel_launcher, [bazel_label])[bazel_label]
