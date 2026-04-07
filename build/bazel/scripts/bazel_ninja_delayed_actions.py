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

from bazel_action_file_copy_utils import write_file_if_changed
from workspace_utils import (
    BazelPackageAndTargetToGnInputsEntriesMap,
    BazelTargetGNInputsEntriesMap,
    GeneratedWorkspaceFiles,
    record_gn_targets_dir_from_entries,
)

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import bazel_action_impl
import build_utils
from bazel_action_utils import (
    BazelGlobalArguments,
    BazelTargetInfo,
    BazelTargetInfosMap,
    update_gn_targets_symlink,
)

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

    ninja_outputs: list[str] = []
    ninja_request = DelayedActionsRequest.from_json(
        args.delayed_actions_request.read_text()
    )
    for action in ninja_request.actions:
        ninja_outputs.extend(action.ninja_outputs)

    targets_by_platform: dict[str, dict[str, BazelTargetInfo]] = {}
    for output in ninja_outputs:
        target = bazel_target_infos.get_target(output)

        if target:
            platform_targets = targets_by_platform.setdefault(
                target.bazel_platform_label, {}
            )
            platform_targets[target.bazel_target] = target
            # TODO: track all the outputs of the found Bazel actions so we don't look them up multiple times.
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
        # This tracks the contents of the depfiles that will need to be written.
        depfiles: dict[str, tuple[set[str], set[str]]] = {}

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
                info = platform_targets[target]

                depfile_entry = depfiles.setdefault(
                    info.ninja_depfile, (set(), set())
                )
                depfile_entry[1].update(sources)

                target_outputs: list[str] = []
                target_outputs.extend([c.ninja_path for c in info.copy_outputs])
                for d in info.directory_outputs:
                    target_outputs.extend(d.tracked_file_ninja_paths)
                target_outputs.extend(
                    [p.archive_path for p in info.package_outputs]
                )
                target_outputs.extend(
                    [f.ninja_path for f in info.final_symlink_outputs]
                )

                depfile_entry[0].update(target_outputs)

        for depfile, (outputs, sources) in depfiles.items():
            depfile_content = "%s: %s\n" % (
                " ".join(sorted(outputs)),
                " ".join(sorted(sources)),
            )

            print(f"writing depfile: {depfile}")
            write_file_if_changed(depfile, depfile_content)

        rc = 0

    except bazel_action_impl.BazelActionError:
        rc = 1

    time_profile.stop()
    if _DEBUG_TIME_PROFILE:
        time_profile.print(0.001)

    response = DelayedActionsResponse(ninja_request.request_id, rc)
    write_file_if_changed(args.delayed_actions_response, response.to_json())

    # Done!
    return rc


def merge_gn_target_manifests(
    manifests: T.Sequence[Path],
) -> BazelPackageAndTargetToGnInputsEntriesMap:
    manifest_entries_package_map = BazelPackageAndTargetToGnInputsEntriesMap()
    for manifest_path in manifests:
        with open(manifest_path) as f:
            for entry in json.load(f):
                bazel_package = entry["bazel_package"]
                name_map = manifest_entries_package_map.setdefault(
                    bazel_package, BazelTargetGNInputsEntriesMap()
                )

                bazel_name = entry["bazel_name"]
                found_entry = name_map.setdefault(bazel_name, entry)
                if found_entry != entry:
                    raise ValueError(
                        f"Found duplicate GN target entry for //{bazel_package}:{bazel_name}:  {found_entry['generator']} vs {entry['generator']}"
                    )
    return manifest_entries_package_map


@dataclasses.dataclass
class DelayedAction(object):
    action_id: int
    command: str
    description: str
    ninja_outputs: list[str]


@dataclasses.dataclass
class DelayedActionsRequest(object):
    request_id: int
    actions: list[DelayedAction]
    build_metadata: dict[str, str]

    @classmethod
    def from_json(cls, raw: str) -> "DelayedActionsRequest":
        """Parse the json Ninja uses to describe a batch of delayed action requests.

        This parses the request from ninja which has the following schema:

            "version": Required. Integer. Must be 1
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
        assert parsed["version"] == 1

        return DelayedActionsRequest(
            request_id=int(parsed["request_id"]),
            actions=[
                DelayedAction(
                    action_id=a["action_id"],
                    command=a["command"],
                    description=a["description"],
                    ninja_outputs=a["ninja_outputs"],
                )
                for a in parsed["actions"]
            ],
            build_metadata=parsed["build_metadata"],
        )


@dataclasses.dataclass
class DelayedActionsResponse(object):
    request_id: int
    status: int

    def to_json(self) -> str:
        """Convert the response in the json format expected by Ninja.

        This converts the response into the schema expected by Ninja.
        """
        as_dict = dataclasses.asdict(self)
        as_dict["version"] = 1
        return json.dumps(as_dict, indent=2)


if __name__ == "__main__":
    rc = main()
    sys.exit(rc)
