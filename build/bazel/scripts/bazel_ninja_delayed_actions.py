#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"Run a set of Ninja-delayed Bazel actions"

import argparse
import json
import os
import sys
import typing as T
from pathlib import Path

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

    ##
    # The set of output paths that are needed to be built
    parser.add_argument(
        "--outputs",
        default=[],
        nargs="+",
        help="list of bazel outputs needed by Ninja.",
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
        args.build_dir
    )

    # load the BazelTargetInfos so that we can find which targets need to be built in order
    # to build the requested outputs.
    bazel_target_infos = BazelTargetInfosMap.create_from_build_dir(
        args.build_dir
    )

    time_profile.start("query_cache", "loading Bazel query cache")
    query_cache = build_utils.BazelQueryCache(
        bazel_paths.workspace / "fuchsia_build_generated/bazel_query_cache"
    )

    time_profile.start(
        "find_targets_for_outputs",
        "Find the Bazel targets that create the given ninja outputs",
    )

    targets_by_platform: dict[str, dict[str, BazelTargetInfo]] = {}
    for output in args.outputs:
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
    except bazel_action_impl.BazelActionError:
        return 1

    time_profile.stop()
    if _DEBUG_TIME_PROFILE:
        time_profile.print(0.001)

    # Done!
    return 0


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


if __name__ == "__main__":
    try:
        rc = main()
    except bazel_action_impl.BazelActionError:
        # Convert these exceptions into an error rc instead of printing a stack trace.
        rc = 1
    sys.exit(rc)
