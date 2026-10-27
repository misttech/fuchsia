#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"Run a set of Ninja-delayed Bazel actions"

import argparse
import dataclasses
import datetime
import json
import os
import sys
import typing as T
from pathlib import Path

# LINT.IfChange(imports)
_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import bazel_action_impl
import bazel_compdb_utils
import bazel_rust_analyzer_utils
import build_utils
from bazel_action_file_copy_utils import write_file_if_changed
from bazel_action_utils import (
    BazelGlobalArguments,
    BazelTargetInfo,
    BazelTargetInfosMap,
    update_gn_targets_symlink,
)
from workspace_utils import (
    GeneratedWorkspaceFiles,
    merge_gn_target_manifests,
    record_gn_targets_dir_from_entries,
)

_MODULES_DIR = os.path.join(_SCRIPT_DIR, "../../python/modules")
sys.path.insert(0, _MODULES_DIR)
from depfile import DepFile

# LINT.ThenChange(//build/bazel/bazel_action.gni:delayed_action_imports, //build/bazel/scripts/BUILD.gn:delayed_action_imports)

# Set this to True to debug operations locally in this script.
# IMPORTANT: Setting this to True will result in Ninja timeouts in CQ
# due to the stdout/stderr logs being too large.
_DEBUG = False

# Set this to True to enable debug printing of the action's timing profiles
_DEBUG_TIME_PROFILE = _DEBUG


@dataclasses.dataclass(frozen=True, order=True)
class TargetWithPlatform:
    target: str
    platform: str


def read_extra_bazel_targets(path: Path) -> list[str]:
    """Reads the Bazel labels listed in an `extra_bazel_targets_file`.

    The file is written by a GN `generated_file()`, one label per line, and is
    empty when the metadata walk that feeds it collected nothing.

    Raises:
        ValueError: if `path` does not exist on disk.
    """
    if not path.exists():
        raise ValueError(
            f"extra_bazel_targets_file {path} does not exist. It must be "
            "written at `gn gen` time (e.g. via `generated_file()`), not by "
            "a build-time Ninja action."
        )
    return [
        line.strip() for line in path.read_text().splitlines() if line.strip()
    ]


def validate_extra_bazel_targets(
    extra_targets_by_owner: dict[TargetWithPlatform, list[str]],
    bazel_target_infos_map: BazelTargetInfosMap,
) -> None:
    """Checks that no Bazel label is claimed by two actions on one platform.

    Nothing keys off an extra target the way the stamp file keys off
    `bazel_target`, so an overlap would silently give one label two owners:
    results for it would be attributed to the wrong action, or to several.

    Raises:
        ValueError: if a label is both declared as some action's
            `bazel_target` and named by another action's extra targets file,
            or is named by two actions' extra targets files.
    """
    seen: dict[TargetWithPlatform, TargetWithPlatform] = {}
    for owner, extra_targets in sorted(extra_targets_by_owner.items()):
        for extra_target in extra_targets:
            extra_with_platform = TargetWithPlatform(
                extra_target, owner.platform
            )
            if bazel_target_infos_map.get_info(extra_target, owner.platform):
                raise ValueError(
                    f"Bazel target {extra_target} is both declared by a "
                    f"bazel_action() and named by {owner.target}'s "
                    f"extra_bazel_targets_file, for platform {owner.platform}."
                )
            previous_owner = seen.setdefault(extra_with_platform, owner)
            if previous_owner != owner:
                raise ValueError(
                    f"Bazel target {extra_target} is named by the "
                    f"extra_bazel_targets_file of both {previous_owner.target} "
                    f"and {owner.target}, for platform {owner.platform}."
                )


def compute_sources_by_owner(
    source_files: dict[str, list[str]],
    requested_targets: T.Iterable[str],
    extra_targets_by_owner: dict[TargetWithPlatform, list[str]],
    platform_label: str,
) -> dict[TargetWithPlatform, list[str]]:
    """Maps each requested Bazel target to the source files it is accountable for.

    Bazel expands the `test_suite()` labels named by an
    `extra_bazel_targets_file` into their member tests, so it reports results
    for targets that no action declared. Labels that appear verbatim in an
    action's extra targets list (such as the `test_suite()` label itself in
    `buildfiles_genquery`) are attributed directly to that action; expanded
    member targets, for which Bazel does not report the originating command
    line suite, are attributed to every action in the batch that supplied extra
    targets so the depfile over-approximates rather than drops inputs.

    Every requested target appears in the result even when it has no sources,
    because the caller writes stamp files while iterating and an action whose
    targets report nothing - an empty list of test suites, say - still owes
    Ninja the stamp it declared as an output.

    Raises:
        ValueError: if Bazel reported a target that nothing can account for.
    """
    sources_by_owner: dict[TargetWithPlatform, list[str]] = {
        TargetWithPlatform(target, platform_label): []
        for target in requested_targets
    }
    extra_target_to_owner_map: dict[TargetWithPlatform, TargetWithPlatform] = {
        TargetWithPlatform(extra_target, platform_label): owner
        for owner, extra_targets in extra_targets_by_owner.items()
        if owner in sources_by_owner
        for extra_target in extra_targets
    }
    fallback_owners = [
        owner for owner in extra_targets_by_owner if owner in sources_by_owner
    ]
    for target, sources in source_files.items():
        target_with_platform = TargetWithPlatform(target, platform_label)
        if target_with_platform in sources_by_owner:
            owners: T.Sequence[TargetWithPlatform] = [target_with_platform]
        elif target_with_platform in extra_target_to_owner_map:
            owners = [extra_target_to_owner_map[target_with_platform]]
        else:
            owners = fallback_owners
        if not owners:
            raise ValueError(
                f"Bazel reported results for target {target} on platform "
                f"{platform_label}, which no bazel_action() requested and no "
                "extra_bazel_targets_file could have expanded to."
            )
        for owner in owners:
            sources_by_owner[owner].extend(sources)
    return sources_by_owner


def should_update_stamp(
    stamp_exists: bool,
    declared_outputs: T.Sequence[str],
    updated_outputs: T.AbstractSet[Path],
    has_extra_targets: bool,
) -> bool:
    """Returns whether a delayed action's stamp file must be rewritten.

    The stamp is normally left untouched when none of the action's declared
    outputs changed, so that Ninja's restat can prune downstream dependents.

    Args:
        stamp_exists: Whether the stamp file already exists.
        declared_outputs: The action's Ninja outputs, excluding the stamp.
        updated_outputs: The outputs that Bazel rewrote in this build.
        has_extra_targets: Whether the action also builds the targets listed
            in an `extra_bazel_targets_file`.
    """
    if not stamp_exists:
        return True
    # Targets from an `extra_bazel_targets_file` (e.g. Bazel test packages)
    # write outputs that no Ninja edge declares, so their changes are
    # invisible here. Downstream actions that read those outputs through
    # depfiles (e.g. `//build/bazel/target_tests:debug_symbols` and the
    # amber-files publish) only rerun if the stamp changes, so assume they
    # changed.
    if has_extra_targets:
        return True
    return any(Path(output) in updated_outputs for output in declared_outputs)


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
    bazel_target_infos_map = BazelTargetInfosMap.create_from_build_dir(
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

    # This is a nested map, first by platform, then by bazel target label, to the BazelTargetInfo
    # struct used to define the outputs for the Bazel targets that we need to build.
    targets_by_platform: dict[str, dict[str, BazelTargetInfo]] = {}

    # This is a map from a bazel target (with platform label) to the DelayedAction from ninja that's
    # requesting its outputs to be built, along with the stamp file that needs to be created so that
    # ninja can tell that the action was performed.
    target_request_map: dict[
        TargetWithPlatform, tuple[DelayedAction, Path]
    ] = {}

    for action in ninja_request.actions:
        for output in action.ninja_outputs:
            target = bazel_target_infos_map.get_target(output)

            if target:
                # In order to line up the results with the action IDs (and to setup the depfile
                # infos) we need to know which action is responsible for which target, which
                # means that we can't have multiple actions creating the same output file.
                target_with_platform = TargetWithPlatform(
                    target.bazel_target,
                    target.bazel_platform_label,
                )
                existing_action, _ = target_request_map.setdefault(
                    target_with_platform, (action, Path(target.stamp_path))
                )
                if existing_action is not action:
                    parser.error(
                        f"Bazel target {target.bazel_target} is requested by multiple actions: "
                        + f"{action.action_id} and {existing_action.action_id}"
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

    # Read the extra command line targets of every declared bazel_action(),
    # not just of the ones in this batch, so that the validation below cannot
    # depend on which actions Ninja happened to batch together.
    extra_targets_by_owner: dict[TargetWithPlatform, list[str]] = {
        TargetWithPlatform(
            target_info.bazel_target, target_info.bazel_platform_label
        ): read_extra_bazel_targets(
            bazel_paths.ninja_build_dir / target_info.extra_bazel_targets_file
        )
        for target_info in bazel_target_infos_map.all_infos()
        if target_info.extra_bazel_targets_file
    }
    try:
        validate_extra_bazel_targets(
            extra_targets_by_owner, bazel_target_infos_map
        )
    except ValueError as e:
        parser.error(str(e))

    if _DEBUG:
        print()
        print("Bazel targets to build:")
        for platform, platform_targets in sorted(targets_by_platform.items()):
            print()
            print(f"Using platform: {platform}")
            for target_label in sorted(platform_targets):
                print(f"    {target_label}")
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

            print(
                f"Building {len(platform_targets)} targets using {platform_label}"
            )

            platform_target_infos = list(platform_targets.values())
            platform_config = platform_target_infos[0].bazel_platform_config

            (
                outputs,
                gn_target_manifests,
            ) = bazel_action_impl.merge_target_info_outputs(
                platform_target_infos
            )

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

            # Extra targets are attached to an individual action rather than
            # unioned over the batch, so they are only built when Ninja
            # actually asked for that action's outputs.
            #
            # All targets in `extra_bazel_targets_file` are built under this
            # action's platform (`--config=<platform_config>`). The generator of
            # `extra_bazel_targets_file` (e.g. `//:bazel_host_test_suites`) is
            # responsible for only listing targets intended for that platform.
            # Note that when an extra target is a `test_suite()`, Bazel expands
            # it and silently skips any member tests whose
            # `target_compatible_with` constraints do not match the platform
            # (see https://fxbug.dev/540003943).
            targets: list[str] = []
            extra_targets: list[str] = []
            for target_info in platform_target_infos:
                targets.append(target_info.bazel_target)
                if target_info.extra_bazel_targets_file:
                    extra_targets += extra_targets_by_owner[
                        TargetWithPlatform(
                            target_info.bazel_target, platform_label
                        )
                    ]

            # Deduplicate targets, because passing the same target twice makes
            # Bazel report it twice.
            targets = list(dict.fromkeys(targets + extra_targets))

            action_result = bazel_action_runner.run(
                command="build",
                platform_config=platform_config,
                platform_label=platform_label,
                targets=targets,
                outputs=outputs,
                time_profile=time_profile,
            )

            if global_bazel_args.auto_refresh_compdb:
                compdb_file = (
                    bazel_paths.ninja_build_dir / "compile_commands.json"
                )
                time_profile.start(
                    "generate_compdb",
                    "Generate {}".format(compdb_file),
                )
                compile_commands: list[dict[str, T.Any]] = []
                if compdb_file.exists() and compdb_file.stat().st_size > 0:
                    with open(compdb_file, "r") as f:
                        compile_commands = json.load(f)
                compile_commands.extend(
                    bazel_compdb_utils.compdb_for_labels(
                        bazel_paths.ninja_build_dir,
                        str(bazel_paths.launcher),
                        action_result.configured_args,
                        # Query the targets in `action_result.source_files`
                        # rather than the requested `bazel_target` labels so
                        # that all targets found by the source-collection
                        # aspect, including member tests expanded from command
                        # line `test_suite()`s, are included in the compilation
                        # database (`bazel aquery` finds no actions under a
                        # `test_suite()` label itself).
                        list(action_result.source_files.keys()),
                    )
                )
                write_file_if_changed(
                    compdb_file,
                    json.dumps(
                        bazel_compdb_utils.dedupe(compile_commands), indent=2
                    ),
                )

            # If any of the targets that were built are flagged as requiring the updating of the
            # the rust_project.json file, then do so now.
            if any(
                target_info.update_rust_project
                for target_info in platform_target_infos
            ):
                rust_project_file = (
                    bazel_paths.ninja_build_dir / "rust-project.json"
                )
                time_profile.start(
                    "generate_rust_project_json",
                    "Generate {}".format(rust_project_file),
                )
                _sysroot_src_subdir = Path("lib/rustlib/src/rust/library")
                rust_sysroot = global_bazel_args.rust_sysroot

                base_rust_project: dict[str, T.Any] = {}
                if (
                    rust_project_file.exists()
                    and rust_project_file.stat().st_size > 0
                ):
                    with open(rust_project_file, "r") as f:
                        base_rust_project = json.load(f)

                if "sysroot" not in base_rust_project:
                    base_rust_project["sysroot"] = str(
                        rust_sysroot.resolve().absolute()
                    )
                if "sysroot_src" not in base_rust_project:
                    base_rust_project["sysroot_src"] = str(
                        rust_sysroot.resolve().absolute() / _sysroot_src_subdir
                    )
                if "crates" not in base_rust_project:
                    base_rust_project["crates"] = []

                new_rust_project = {
                    "sysroot": str(rust_sysroot.resolve().absolute()),
                    "sysroot_src": str(
                        rust_sysroot.resolve().absolute() / _sysroot_src_subdir
                    ),
                    "crates": action_result.rust_crates,
                }
                merged_rust_project = (
                    bazel_rust_analyzer_utils.merge_rust_project_jsons(
                        base_rust_project, [new_rust_project]
                    )
                )
                write_file_if_changed(
                    rust_project_file,
                    json.dumps(merged_rust_project, indent=2),
                )

            # Update the depfiles data and the stamp file
            time_profile.start("update_depfile_and_stampfiles")
            updated_outputs = set(action_result.output_files)

            try:
                sources_by_owner = compute_sources_by_owner(
                    action_result.source_files,
                    platform_targets.keys(),
                    extra_targets_by_owner,
                    platform_label,
                )
            except ValueError as e:
                raise bazel_action_impl.BazelActionError(str(e)) from e

            for owner, sources in sources_by_owner.items():
                # Locate the action request and stamp path for this target.
                action, stamp_path = target_request_map[owner]
                target_info = platform_targets[owner.target]

                if target_info.debug_symbols_manifest:
                    debug_manifest_path = (
                        bazel_paths.ninja_build_dir
                        / target_info.debug_symbols_manifest
                    )
                    owner_debug_symbols = action_result.debug_symbols.get(
                        owner.target, []
                    )
                    if write_file_if_changed(
                        debug_manifest_path,
                        json.dumps(owner_debug_symbols, indent=2),
                    ):
                        updated_outputs.add(
                            Path(target_info.debug_symbols_manifest)
                        )

                # Construct a depfile for it.
                depfile = DepFile(action.ninja_outputs[0])

                # With all outputs that are in the request
                for output in action.ninja_outputs[1:]:
                    depfile.add_output(output)

                # And all the sources for the target.
                for source in sources:
                    # Don't include our @gn_targets generated BUILD.bazel files as
                    # inputs for the depfile, because they're created on the fly.
                    if not (
                        source.startswith(
                            "build/bazel/ninja_delayed_action.gn_targets"
                        )
                        and source.endswith("BUILD.bazel")
                    ):
                        depfile.add_input(source)

                # The file listing extra command line targets is written by
                # `gn gen`. GN won't accept a generated file in `inputs` unless
                # the action depends on the target that generates it, and for
                # the host test suites that dependency would be a cycle
                # (`//:bazel_host_test_suites` walks `//:host_tests`, which
                # depends on this action). Record the file in the depfile
                # instead so the action still reruns when its contents change.
                extra_targets_file = target_info.extra_bazel_targets_file
                if extra_targets_file:
                    depfile.add_input(extra_targets_file)

                # And then write out the depfile
                with open(action.ninja_depfile, "w") as f:
                    depfile.write_to(f)

                if should_update_stamp(
                    stamp_exists=stamp_path.exists(),
                    declared_outputs=action.ninja_outputs[1:],
                    updated_outputs=updated_outputs,
                    has_extra_targets=bool(extra_targets_file),
                ):
                    timestamp = datetime.datetime.now().timestamp()
                    stamp_path.parent.mkdir(parents=True, exist_ok=True)
                    if stamp_path.exists():
                        stamp_path.unlink()
                    with open(stamp_path, "w") as f:
                        f.write(f"{timestamp}\n")

        rc = 0

    except bazel_action_impl.BazelActionError as e:
        rc = 1
        print(str(e), file=sys.stderr)

    time_profile.stop()
    if _DEBUG_TIME_PROFILE:
        time_profile.print(0.001)

    response = DelayedActionsResponse(ninja_request.request_id, rc, "")
    write_file_if_changed(args.delayed_actions_response, response.to_json())

    # Done!  (Don't return the 'rc' from above, that's for the action itself,
    # here we need to return 0 to tell Ninja that the script exited successfully.)
    return 0


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
