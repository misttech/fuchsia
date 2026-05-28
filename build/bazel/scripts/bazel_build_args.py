# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Core logic for expanding and resolving Bazel actions' command lines."""

import dataclasses
import json
import os
import shlex
import sys
from typing import Any

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import build_utils


@dataclasses.dataclass
class ExpandedAction:
    """Models a Bazel action with its fully expanded command-line."""

    action: str  # Action's description
    target: str  # Bazel target label
    configuration: str  # Bazel configuration mnemonic (e.g. k8-fastbuild)
    mnemonic: str  # The action's mnemonic
    args: list[str]  # The command line arguments
    env_vars: list[str] = dataclasses.field(
        default_factory=list
    )  # A list of VARNAME=value strings. This is kept as a list, instead
    # of a dictionary to be as close as possible from the original
    # command-line. This helps debugging problematic cases when multiple
    # entries assign to the same key (e.g. ["FOO=1", "FOO=2"]).
    warnings: list[str] = dataclasses.field(
        default_factory=list
    )  # Warnings encountered during expansion.

    def to_dict(self) -> dict[str, Any]:
        """Serializes the expanded action into a dictionary for JSON output."""
        res = {
            "action": self.action,
            "target": self.target,
            "configuration": self.configuration,
            "mnemonic": self.mnemonic,
            "args": self.args,
        }
        if self.env_vars:
            res["environment"] = self.env_vars
        if self.warnings:
            res["warnings"] = self.warnings
        return res


def get_bazel_expanded_actions(
    bazel_launcher: build_utils.BazelLauncher,
    bazel_execroot: str,
    bazel_target: str,
    config_args: list[str],
    filter_mnemonics: None | list[str] = None,
) -> list[ExpandedAction]:
    """Return the expanded command-line of all actions used to build a given Bazel target.

    Args:
        bazel_launcher: A BazelLauncher used to run bazel commands.
        bazel_execroot: Absolute path to the Bazel execroot.
        bazel_target: Bazel target label to get the actions for.
        config_args: Additional configuration arguments to pass to Bazel, such as --config=NAME.
        filter_mnemonics: Optional list of mnemonics to filter by. If provided only actions
            whose mnemonic match these values will be included in the returned list.

    Returns:
        A list of ExpandedAction objects with the expanded command lines.

    Raises:
        RuntimeError if invoking Bazel failed.
    """
    # 1. Run a Bazel aquery to get the list of actions with their non-expanded command lines
    #    and map of response files.
    aquery_args = config_args + ["--output=jsonproto", bazel_target]
    ret = bazel_launcher.run_query("aquery", aquery_args, ignore_errors=False)
    if ret.returncode != 0:
        raise RuntimeError(
            f"ERROR: aquery failed with exit code {ret.returncode}\n\n{ret.stderr}\n"
        )

    aquery_result = parse_aquery_output(ret.stdout, filter_mnemonics)

    execroot = os.path.realpath(bazel_execroot)

    # 2. Perform Expansion
    expanded_actions: list[ExpandedAction] = []
    for action in aquery_result.actions:
        expanded_result = expand_args_from_disk(
            action.args, action.env_vars, execroot
        )

        # Skip actions that don't have any arguments. These are not useful to inspect.
        # This corresponds to Bazel's creation of symlink trees and runfiles manifests.
        if not expanded_result.expanded_args:
            continue

        expanded_actions.append(
            ExpandedAction(
                action=action.name,
                target=action.target,
                configuration=action.config,
                mnemonic=action.mnemonic,
                args=expanded_result.expanded_args,
                env_vars=expanded_result.env_vars,
                warnings=expanded_result.warnings,
            )
        )
    return expanded_actions


##############################################################################################
##############################################################################################
#####
#####    A Q U E R Y   P A R S I N G
#####


def _normalize_path(path: str) -> str:
    """Safely strips shell pwd prefixes from Bazel paths to prevent false positives."""
    if path.startswith("${pwd}/"):
        return path[len("${pwd}/") :]
    return path


@dataclasses.dataclass(frozen=True)
class ParsedAction:
    """Models a single parsed action from aquery."""

    name: str
    target: str
    config: str
    mnemonic: str
    args: list[str]
    env_vars: dict[str, str]


@dataclasses.dataclass(frozen=True)
class ParsedAqueryResult:
    """The result of parsing aquery outputs.

    This provides:
      - A list of ParsedAction values corresponding to all non-empty actions returned
        by the aquery command.
    """

    actions: list[ParsedAction]

    @staticmethod
    def new_empty() -> "ParsedAqueryResult":
        """Create a new ParsedAqueryResult with empty values."""
        return ParsedAqueryResult([])


def parse_aquery_output(
    aquery_output: str,
    filter_mnemonics: list[str] | None = None,
) -> ParsedAqueryResult:
    """Parses raw aquery output and extracts commands and response file mappings.

    Args:
        aquery_output: The raw string output from a 'bazel aquery --output=jsonproto' command.
        filter_mnemonics: Optional list of mnemonic names to filter actions early.

    Returns:
        A ParsedAqueryResult value.
    """
    # The following parses the output of `bazel aquery --format=jsonproto` which should
    # emit a JSON object that corresponds to the ActionGraphContainer schema from
    # https://github.com/bazelbuild/bazel/blob/7278be3f9b0c26842ecb8225f0215c1e4aede5a9/src/main/protobuf/analysis_v2.proto#L189

    # Filter out the output from unexpected garbage, for example our main_build.py
    # wrapper still appends "Lock acquired, proceeding with build." to stdout when performing
    # `fx bazel aquery --config=host --output=jsonproto //build/bazel/host_tests/cc_tests/... 2>/dev/null`
    start_idx = aquery_output.find("{")
    end_idx = aquery_output.rfind("}")
    if start_idx < 0 or end_idx < 0 or start_idx > end_idx:
        return ParsedAqueryResult.new_empty()

    try:
        data = json.loads(aquery_output[start_idx : end_idx + 1])

        # data["targets"] is a list of Target dictionaries. Build a map associating
        # target unique ids to their label.
        targets_map = {t["id"]: t["label"] for t in data.get("targets", [])}

        # data["configuration"] is a list of Configuration dicts. Build a map
        # association configuration unique ids to the "mnemonic" which is
        # something like "k8-fastbuild" (not related to action mnemonics).
        configs_map = {
            c["id"]: c["mnemonic"] for c in data.get("configuration", [])
        }

    except Exception as e:
        print(f"ERROR: Failed to parse aquery jsonproto: {e}", file=sys.stderr)
        return ParsedAqueryResult.new_empty()

    actions: list[ParsedAction] = []

    raw_actions = data.get("actions", [])
    for action in raw_actions:
        action_mnemonic = action.get("mnemonic", "Unknown Mnemonic")
        if filter_mnemonics and action_mnemonic not in filter_mnemonics:
            continue

        target_id = action.get("targetId", 0)
        action_target = targets_map.get(target_id, "Unknown Target")

        config_id = action.get("configurationId", 0)
        action_config = configs_map.get(config_id, "Unknown Configuration")

        cmd_args = action.get("arguments", [])
        action_name = f"action '{action_mnemonic} on {action_target}'"

        env_vars: dict[str, str] = {
            item["key"]: item["value"]
            for item in action.get("environmentVariables", [])
            # Sometimes there is no value at all, ignore such entries.
            if "value" in item
        }

        actions.append(
            ParsedAction(
                name=action_name,
                target=action_target,
                config=action_config,
                mnemonic=action_mnemonic,
                args=cmd_args,
                env_vars=env_vars,
            )
        )

    return ParsedAqueryResult(actions=actions)


##############################################################################################
##############################################################################################
#####
#####    A R G U M E N T   E X P A N S I O N
#####


@dataclasses.dataclass(frozen=True)
class ArgumentsExpansionResult:
    """Models the result of expanding command line arguments dynamically."""

    expanded_args: list[str]  # Expanded command-line arguments.
    env_vars: list[str]  # A list of "VARNAME=value" strings.
    warnings: list[str]  # Warnings encountered during expansion.


def expand_args_from_disk(
    args: list[str],
    env_vars: dict[str, str],
    execroot: str,
) -> ArgumentsExpansionResult:
    """Recursively expand standard response files and Rust env files from disk iteratively.

    This functions takes a list of command-line arguments and will expand response files
    that appear in the input to generate a final version. It only works correctly if
    these files are in the execroot, hence must be called after building the target
    to give correct results.

    Args:
        args: The raw command line arguments list to expand.
        env_vars: The dictionary of environment variables to use for expansion.
        execroot: Absolute path to Bazel's execution root.

    Returns:
        An ArgumentsExpansionResult dataclass containing expanded arguments, environment
        variable overrides, and warnings.
    """
    import collections

    expanded: list[str] = []
    env_list: list[str] = [f"{k}={v}" for k, v in env_vars.items()]
    warnings: list[str] = []

    # The implementation below uses a single double-ended queue to parse
    # the arguments recursively. In particular, it recognizes two special
    # types of input:
    #
    #  - '@<path>': as a response file usage. The code will read the
    #    content of <path>, shell-unquote it, and inject its content into
    #    the queue.
    #
    #  -'--rust-env', '<path>': as a Rust environment file use. The code
    #   will read <path> which should be a series of NAME=value definitions,
    #   one per line, which will be recorded in env_list in the order they
    #   appear (no deduplication is performed).
    #
    # The operation is recursive (a response file can contain other @<path>
    # arguments) and will properly flatten the final result, however, path
    # cycles can exist, and the loop below will detect them and generate
    # a warning then one is detected, and will also keep the corresponding
    # final '@<path>' argument in the output to help debugging.
    #
    # Each item in the queue is either a string, modelling an incoming
    #
    # string argument, or a tuple[str, str] which will be ('POP_MARKER', abs_path)
    # to model the end of a file scropt. These items are used to pop paths
    # from the stack used for cycle detection.
    #
    # A few example will illustrate how this works, consider this input:
    #
    #  queue = [ "foo", "@path1", "bar" ]      output = []            paths = []
    #
    # The first loop iteration see "foo" and just pulls it to send to the output.
    #
    #  queue = [ "@path1", "bar" ]             output = [ "foo" ]     paths = []
    #
    # The second loop iteration will see the "@<path>" format, and will
    # push 'path1' to the `paths` stack, read the content of that file
    # into a list of unquoted string arguments, followed by a POP_MARKER item,
    # the following assumed 'path1' contained "qux", "@path2":
    #
    #  queue  = [ "qux", "@path2", (POP_MARKER, "path1"), "bar" ]
    #  output = [ "foo" ]
    #  paths  = [ "path1" ]
    #
    # The third loop iteration moves 'qux' to the output, and the fourth one
    # will see the new "@path2" statement:
    #
    #  queue  = [ "@path2", (POP_MARKER, "path1"), "bar" ]
    #  output = [ "foo", "qux" ]
    #  paths  = [ "path1" ]
    #
    # the 'path2' will pushed to the top of the stak, its content read, unquoted
    # and prepended at the start of the queue, followed by its POP_MARKER item:
    #
    #  queue  = [ "zoo", (POP_MARKER, "path2"), (POP_MARKER, "path1"), "bar" ]
    #  output = [ "foo", "qux" ]
    #  paths  = [ "path1", "path2" ]
    #
    # From there, 'zoo' is moved to the output. The first POP_MARKER simply
    # removes 'path2' from the top of the `paths` stack, the second one
    # removed "path1", then "bar" is appended to the output:
    #
    #  queue  = []
    #  output = [ "foo", "qux", "zoo", "bar" ]
    #  paths  = []
    #
    # The look also contain special logic to handle "--env-file", "<path>",
    # even when this crosses a file scope.
    #
    work_queue: collections.deque[str | tuple[str, str]] = collections.deque(
        args
    )

    # Active circular dependency path stack (mimics the recursive call stack)
    active_path: list[str] = []

    def detect_path_cycle(item: str, abs_path: str) -> bool:
        """Return true if a path cycle is detected."""
        if abs_path not in active_path:
            return False

        cycle_idx = active_path.index(abs_path)
        cycle_path = [
            os.path.relpath(p, execroot) for p in active_path[cycle_idx:]
        ] + [os.path.relpath(abs_path, execroot)]
        cycle_str = " -> ".join(cycle_path)
        warnings.append(f"Cycle detected in response file {item}: {cycle_str}")
        return True

    while work_queue:
        item = work_queue.popleft()

        # Handle pop marker to unwind active path scope
        if isinstance(item, tuple) and item[0] == "POP_MARKER":
            pop_path = item[1]

            # Consistency checks for safety.
            assert (
                active_path
            ), f"Unexpected POP_MARKER for {item[1]} with empty stack!"
            assert (
                active_path[-1] == pop_path
            ), f"Unexpected POP_MARKER for {item[1]}, expected {active_path[-1]}"
            active_path.pop()
            continue

        assert isinstance(item, str)

        # Handle --env-file <path> for Rust actions
        if item == "--env-file":
            # If this is the final --env-file, just append it as-is to the output.
            # The command-line is likely broken, but should be reported.
            if not work_queue:
                append(item)
                continue

            env_path = work_queue.popleft()

            # If the --env-file is followed by a POP_MARKER, swap them in the
            # queue and loop again to simplify processing.
            if not isinstance(env_path, str):
                # --env-file POP_MARKER ... => POP_MARKER --env-file ...
                work_queue.appendleft(item)
                work_queue, appendleft(env_path)
                continue

            assert isinstance(env_path, str)  # for mypy
            normalized_path = _normalize_path(env_path)
            abs_path = os.path.join(execroot, normalized_path)

            # Tricky case: '@path1' where path1 contains '--env-file path1'
            # This is a path cycle, keep the --env-file <path> in the output
            # for debugging after the warning (which will include the cycle).
            if detect_path_cycle(env_path, abs_path):
                expanded += [item, env_path]
                continue

            if os.path.isfile(abs_path):
                try:
                    with open(abs_path, "r") as f:
                        envs = [line.strip() for line in f if line.strip()]
                        env_list.extend(envs)
                except Exception as e:
                    warnings.append(
                        f"Failed to read env file {env_path} from disk: {e}"
                    )
                    expanded += [
                        item,
                        env_path,
                    ]  # Keep --env-file <path> in output.
            else:
                warnings.append(
                    f"Rust environment file {env_path} was not found on disk. "
                    f"This script requires build outputs to expand arguments. "
                    f"Please build the target first (using 'fx build') and re-run this script."
                )
                expanded += [
                    item,
                    env_path,
                ]  # Keep --env-file <path> in output.
            continue

        # Handle @path response files recursively
        if item.startswith("@"):
            path = item[1:]
            normalized_path = _normalize_path(path)
            abs_path = os.path.join(execroot, normalized_path)

            # Detect path cycles, keep the last @path in the output for debugging
            if detect_path_cycle(item, abs_path):
                expanded.append(item)
                continue

            if os.path.isfile(abs_path):
                try:
                    # Read the response file, assume content is shell quoted
                    # as that's what most tools expect (though there is no
                    # standard for that), and that it can contain multiple
                    # arguments per line.
                    with open(abs_path, "r") as f:
                        nested_args = []
                        for line in f:
                            line = line.strip()
                            if line:
                                nested_args.extend(shlex.split(line))

                    # Prepend POP_MARKER and nested args in original order
                    work_queue.appendleft(("POP_MARKER", abs_path))
                    for nested_arg in reversed(nested_args):
                        work_queue.appendleft(nested_arg)

                    # Save path in stack.
                    active_path.append(abs_path)
                    continue
                except Exception as e:
                    # Something went wrong, keep @path for debugging.
                    warnings.append(
                        f"Failed to read parameter file {item} from disk: {e}"
                    )
            else:
                warnings.append(
                    f"Parameter file {item} was not found on disk. "
                    f"This script requires build outputs to expand arguments. "
                    f"Please build the target first (using 'fx build') and re-run this script."
                )
        # Either a normal item, or something went wrong in the @path
        # case, and we keep it for debugging the issue.
        expanded.append(item)

    return ArgumentsExpansionResult(
        expanded_args=expanded, env_vars=env_list, warnings=warnings
    )
