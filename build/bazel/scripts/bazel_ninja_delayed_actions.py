#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"Run a set of Ninja-delayed Bazel actions"

import argparse
import dataclasses
import json
import os
import sys
import typing as T
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import bazel_action_impl
import build_utils
from bazel_action_file_copy_utils import write_file_if_changed
from bazel_action_utils import (
    BazelGlobalArguments,
    BazelTargetInfo,
    BazelTargetInfosMap,
    update_gn_targets_symlink,
)
from workspace_utils import (
    BazelPackageAndTargetToGnInputsEntriesMap,
    BazelTargetGnInputsEntriesMap,
    GeneratedWorkspaceFiles,
    GnTargetsDirectoryManifestEntry,
    record_gn_targets_dir_from_entries,
)

_MODULES_DIR = os.path.join(_SCRIPT_DIR, "../../python/modules")
sys.path.insert(0, _MODULES_DIR)
from depfile import DepFile

# Set this to True to debug operations locally in this script.
# IMPORTANT: Setting this to True will result in Ninja timeouts in CQ
# due to the stdout/stderr logs being too large.
_DEBUG = False

# Set this to True to enable debug printing of the action's timing profiles
_DEBUG_TIME_PROFILE = _DEBUG


def main() -> int:
    time_profile = build_utils.TimeProfile()
    parser = argparse.ArgumentParser(description=__doc__)

    ##
    # Options for directory that the build is running in.
    parser.add_argument(
        "--build-dir",
        type=Path,
        help="Specify Ninja build directory (defaults to current directory)",
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=Path,
        help="Specify Fuchsia source directory (defaults to auto-detected)",
    )

    parser.add_argument(
        "--delayed-actions-request",
        type=Path,
        required=True,
        help="Path to a json file describing the set of actions Ninja needs run.",
    )

    parser.add_argument(
        "--delayed-actions-response",
        type=Path,
        required=True,
        help="Path to a json file to write describing the status of the action.",
    )

    args = parser.parse_args()

    time_profile.start("load_config", "Load the configuration files.")

    try:
        bazel_paths = build_utils.BazelPaths.new(
            args.fuchsia_dir, args.build_dir
        )
    except ValueError as e:
        parser.error(str(e))

    # Load the extra global settings configured via GN global args
    global_bazel_args = BazelGlobalArguments.create_from_build_dir(
        bazel_paths.ninja_build_dir
    )

    # load the BazelTargetInfos so that we can find which targets need to be built in order
    # to build the requested outputs.
    bazel_target_infos = BazelTargetInfosMap.create_from_build_dir(
        bazel_paths.ninja_build_dir
    )

    time_profile.start("query_cache", "loading Bazel query cache")
    query_cache = build_utils.BazelQueryCache(
        bazel_paths.workspace / "fuchsia_build_generated/bazel_query_cache"
    )

    time_profile.start(
        "find_targets_for_outputs",
        "Find the Bazel targets that create the given ninja outputs",
    )

    ninja_request = DelayedActionsRequest.from_json(
        args.delayed_actions_request.read_text()
    )

    targets_by_platform: dict[str, dict[str, BazelTargetInfo]] = {}
    target_request_map: dict[str, DelayedAction] = {}
    for action in ninja_request.actions:
        for output in action.ninja_outputs:
            target = bazel_target_infos.get_target(output)

            if target:
                if target.bazel_target not in target_request_map:
                    target_request_map[target.bazel_target] = action
                else:
                    if action != target_request_map[target.bazel_target]:
                        parser.error(
                            f"Bazel target {target.bazel_target} is requested by multiple actions: "
                            + f"{action} and {target_request_map[target.bazel_target]}"
                        )

                platform_targets = targets_by_platform.setdefault(
                    target.bazel_platform_label, {}
                )
                platform_targets[target.bazel_target] = target

            elif is_debugging_output(output):
                # These files are listed as ninja outputs, but aren't actually outputs of Bazel.
                pass
            else:
                parser.error(f"Can't find a Bazel target for output: {output}")

    print()
    print("Bazel targets to build:")
    for platform, targets in sorted(targets_by_platform.items()):
        print(f"  {platform}")
        for target in sorted(targets):
            print(f"    {target}")
    print()

    bazel_action_runner = bazel_action_impl.BazelActionRunner(
        bazel_paths,
        global_bazel_args,
        query_cache,
    )
    # This will raise an exception on failure.
    try:
        for platform_label, platform_targets in targets_by_platform.items():
            time_profile.start("merging_bazel_target_infos")

            bazel_target_infos = list(platform_targets.values())
            platform_config = bazel_target_infos[0].bazel_platform_config

            (
                outputs,
                gn_target_manifests,
            ) = bazel_action_impl.merge_target_info_outputs(bazel_target_infos)

            gn_target_manifest_entries = merge_gn_target_manifests(
                gn_target_manifests
            )

            gn_targets_dir = (
                bazel_paths.ninja_build_dir
                / "build/bazel/ninja_delayed_action.gn_targets"
            )

            # The path for this file can't be `all_licenses.spdx.json` because the
            # `update_gn_targets_symlink()` function symlinks that path to this file, which creates
            # a symbolic link to itself.
            licenses_file = gn_targets_dir / "placeholder_licenses.spdx.json"
            licenses_file.parent.mkdir(parents=True, exist_ok=True)
            licenses_file.write_text(
                "This is a placeholder file - It should always be overwritten by Ninja during a build"
            )

            time_profile.start("generate_gn_targets_dir")
            generated = GeneratedWorkspaceFiles()
            record_gn_targets_dir_from_entries(
                generated,
                bazel_paths.ninja_build_dir,
                gn_target_manifest_entries,
                licenses_file,
            )
            generated.write(gn_targets_dir)

            update_gn_targets_symlink(
                bazel_paths, gn_targets_dir, check_license_timestamps=True
            )

            action_result = bazel_action_runner.run(
                command="build",
                platform_config=platform_config,
                platform_label=platform_label,
                targets=[
                    target_info.bazel_target
                    for target_info in bazel_target_infos
                ],
                outputs=outputs,
                extra_outputs=bazel_action_impl.BazelExtraOutputs(),
                time_profile=time_profile,
            )

            # Update the depfiles data
            for target, sources in action_result.source_files.items():
                # Locate the action request for this target.
                action = target_request_map[target]

                # Construct a depfile for it.
                depfile = DepFile(action.ninja_outputs[0])

                # With all outputs that are in the request
                for output in action.ninja_outputs[1:]:
                    depfile.add_output(output)

                # And all the sources for the target.
                depfile.update(sources)

                # And then write out the depfile
                with open(action.ninja_depfile, "w") as f:
                    depfile.write_to(f)

        rc = 0

    except bazel_action_impl.BazelActionError:
        rc = 1

    time_profile.stop()
    if _DEBUG_TIME_PROFILE:
        time_profile.print(0.001)

    response = DelayedActionsResponse(ninja_request.request_id, rc, "")
    write_file_if_changed(args.delayed_actions_response, response.to_json())

    # Done!
    return rc


def merge_gn_target_manifests(
    manifests: list[Path],
) -> BazelPackageAndTargetToGnInputsEntriesMap:
    manifest_entries_package_map = BazelPackageAndTargetToGnInputsEntriesMap()
    for manifest_path in manifests:
        with open(manifest_path) as f:
            for entry_json in json.load(f):
                entry = GnTargetsDirectoryManifestEntry.from_json_value(
                    entry_json
                )

                bazel_package = entry.bazel_package
                name_map = manifest_entries_package_map.setdefault(
                    bazel_package, BazelTargetGnInputsEntriesMap()
                )

                bazel_name = entry.bazel_name
                found_entry = name_map.setdefault(bazel_name, entry)
                if found_entry != entry:
                    raise ValueError(
                        f"Found duplicate GN target entry for //{bazel_package}:{bazel_name}:  {found_entry.generator_label} vs {entry.generator_label}"
                    )
    return manifest_entries_package_map


@dataclasses.dataclass
class DelayedAction(object):
    action_id: int
    command: str
    description: str
    ninja_outputs: list[str]
    ninja_depfile: Path


@dataclasses.dataclass
class DelayedActionsRequest(object):
    request_id: int
    actions: list[DelayedAction]

    @classmethod
    def from_json(cls, raw: str) -> "DelayedActionsRequest":
        """Parse the json Ninja uses to describe a batch of delayed action requests.

        This parses the request from ninja which has the following schema:

            "version": Required. Integer. Must be 2
            "request_id": Required. Integer. Must be in 1..INT32_MAX range.
            "actions": Required. Array of objects. Each one with:

                "action_id": Required. Integer. Must be in 1..INT32_MAX range and
                    correspond to the action's index in the request.
                "command": Required. String. Command to run as a single string.
                "description": Optional. String. Command description from GN.
                "ninja_outputs": Optional. Array of Ninja output path strings,
                    relative to the build directory.

            "build_metadata": Optional. Object of key-value string pairs.
        """
        parsed: dict[str, T.Any] = json.loads(raw)
        assert parsed["version"] == 2

        return DelayedActionsRequest(
            request_id=int(parsed["request_id"]),
            actions=[
                DelayedAction(
                    action_id=a["action_id"],
                    command=a["command"],
                    description=a["description"],
                    ninja_outputs=a["ninja_outputs"],
                    ninja_depfile=Path(a["ninja_depfile"]),
                )
                for a in parsed["actions"]
            ],
        )


@dataclasses.dataclass
class DelayedActionsResponse(object):
    request_id: int
    status: int
    output: str

    def to_json(self) -> str:
        """Convert the response in the json format expected by Ninja.

        This converts the response into the schema expected by Ninja.
        """
        as_dict = dataclasses.asdict(self)
        as_dict["version"] = 1
        return json.dumps(as_dict, indent=2)


# These are the suffixes of files we use to debug Bazel actions, and
# they are listed as outputs in the DelayedActionsRequest, but aren't
# outputs from Bazel, that are in the BazelTargetInfos.
_DEBUGGING_OUTPUT_SUFFIXES = [
    "bazel_command.sh",
    "bazel_explain.txt",
    "debug_symbols.json",
    "bazel_action_timings.json",
    "bazel_events.log.json",
    "rust-project.json",
]


def is_debugging_output(output: str) -> bool:
    """Return whether the given output is one of our debugging outputs.

    This is used to filter out debugging outputs from the list of outputs
    passed to Bazel.
    """
    return any(output.endswith(suffix) for suffix in _DEBUGGING_OUTPUT_SUFFIXES)


if __name__ == "__main__":
    rc = main()
    sys.exit(rc)
