#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for converting Bazel aspect outputs to rust-project.json format."""

import json
import os
import subprocess
import sys
import typing as T
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import build_utils

# Set this to True to debug operations locally in this script.
_DEBUG = False

# Arguments to pass to Bazel to suppress CLI outputs.
_SILENT_BAZEL_ARGS = [
    "--ui_event_filters=-info,-warning",
    "--noshow_loading_progress",
    "--noshow_progress",
    "--show_result=0",
]

# Aspect to use for building/querying rust-analyzer related data from Rust Bazel targets.
_RUST_ANALYZER_ASPECT = "@rules_rust//rust:defs.bzl%rust_analyzer_aspect"


class CrateSpecSource(T.TypedDict, total=False):
    """Source file information for a crate, from the aspect."""

    exclude_dirs: list[str]
    include_dirs: list[str]


class CrateSpecBuild(T.TypedDict, total=False):
    """Build information for a crate, from the aspect."""

    label: str
    build_file: str


class CrateSpec(T.TypedDict, total=False):
    """Raw crate specification as output by the rust_analyzer_aspect."""

    aliases: dict[str, str]
    crate_id: str
    display_name: str
    edition: str
    root_module: str
    is_workspace_member: bool
    deps: list[str]  # list of crate_ids
    proc_macro_dylib_path: T.Optional[str]
    source: T.Optional[CrateSpecSource]
    cfg: list[str]
    env: dict[str, str]
    target: str
    crate_type: str  # bin, rlib, lib, dylib, cdylib, staticlib, proc-macro
    is_test: bool
    build: T.Optional[CrateSpecBuild]


# See rust-project.json format at
# https://rust-analyzer.github.io/book/non_cargo_based_projects.html


class Dependency(T.TypedDict, total=False):
    """Represents a dependency in the rust-project.json format."""

    crate: int  # Index in the final crates array
    name: str


class Source(T.TypedDict, total=False):
    """Source file information in the rust-project.json format."""

    include_dirs: list[str]
    exclude_dirs: list[str]


class Build(T.TypedDict, total=False):
    """Build information in the rust-project.json format."""

    label: str
    build_file: str
    target_kind: str  # bin, lib, test


class Crate(T.TypedDict, total=False):
    """Represents a crate in the rust-project.json format."""

    display_name: T.Optional[str]
    root_module: str
    edition: str
    deps: list[Dependency]  # This will be empty in this function
    is_workspace_member: T.Optional[bool]
    source: T.Optional[Source]
    cfg: list[str]
    target: T.Optional[str]
    env: T.Optional[T.Dict[str, str]]
    is_proc_macro: bool
    proc_macro_dylib_path: T.Optional[str]
    build: T.Optional[Build]


def convert_crate_specs_to_rust_project_crates(
    crate_specs: list[CrateSpec],
) -> list[Crate]:
    """
    Converts a list of CrateSpec dictionaries to a list of Crate dictionaries.

    This function takes the raw crate specifications output by the Bazel aspect
    and transforms them into the format expected by rust-analyzer in the
    rust-project.json file. It resolves crate dependencies by their `crate_id`
    and replaces them with an index into the final list of crates.

    Args:
        crate_specs: A list of dictionaries, where each dictionary represents
            a crate's metadata as produced by the rust_analyzer_aspect.

    Returns:
        A list of dictionaries, formatted according to the Crate T.TypedDict,
        suitable for inclusion in the rust-project.json 'crates' array.
    """
    crate_id_to_index: dict[str, int] = {
        spec["crate_id"]: i for i, spec in enumerate(crate_specs)
    }
    crate_id_to_spec: dict[str, CrateSpec] = {
        spec["crate_id"]: spec for spec in crate_specs
    }

    result_crates: list[Crate] = []

    for crate_spec in crate_specs:
        target_kind = "lib"
        crate_type = crate_spec.get("crate_type", "rlib")
        is_test = crate_spec.get("is_test", False)
        if crate_type == "bin":
            target_kind = "test" if is_test else "bin"

        source: T.Optional[Source] = None
        spec_source = crate_spec.get("source")
        if spec_source:
            include_dirs = spec_source.get("include_dirs", [])
            exclude_dirs = spec_source.get("exclude_dirs", [])
            # If both include_dirs and exclude_dirs are empty, we don't include the optional source
            # field in the final rust-project.json, so rust-analyzer can use root_module to
            # determine the crate's source files.
            if include_dirs or exclude_dirs:
                source = {
                    "include_dirs": include_dirs,
                    "exclude_dirs": exclude_dirs,
                }

        build: T.Optional[Build] = None
        spec_build = crate_spec.get("build")
        if spec_build:
            build = {
                "label": spec_build.get("label", ""),
                "build_file": spec_build.get("build_file", ""),
                "target_kind": target_kind,
            }

        deps: list[Dependency] = []
        for dep_id in crate_spec.get("deps", []):
            if dep_id not in crate_id_to_index:
                if _DEBUG:
                    print(
                        f"Warning: Dependency '{dep_id}' not found for crate '{crate_spec.get('crate_id')}'",
                        file=sys.stderr,
                    )
                continue
            dep_index = crate_id_to_index[dep_id]
            dep_spec = crate_id_to_spec[dep_id]
            deps.append({"crate": dep_index, "name": dep_spec["display_name"]})

        result_crate = {
            "crate_id": crate_id_to_index[crate_spec["crate_id"]],
            "display_name": crate_spec["display_name"],
            "root_module": crate_spec["root_module"],
            "edition": crate_spec["edition"],
            "deps": deps,
            "is_workspace_member": crate_spec["is_workspace_member"],
            "cfg": crate_spec["cfg"],
            "target": crate_spec["target"],
            "env": crate_spec["env"],
            "is_proc_macro": (
                crate_spec.get("proc_macro_dylib_path") is not None
            ),
            "proc_macro_dylib_path": crate_spec.get("proc_macro_dylib_path"),
            "build": build,
        }
        if source:
            result_crate["source"] = source
        result_crates.append(result_crate)

    return result_crates


def substitute_tokens(text: str, bazel_paths: build_utils.BazelPaths) -> str:
    """
    Replaces placeholder tokens in a string with actual paths from BazelPaths.

    The following substitutions are made:
        __WORKSPACE__: Fuchsia source root directory. Note this is intentionally not the Bazel
            workspace root directory, so editors can correctly map source files to their locations
            in the Fuchsia source tree. These files are symlinked to our synthesized Bazel
            workspace.
        ${pwd}: Bazel execution root directory.
        __EXEC_ROOT__: Bazel execution root directory.
        __OUTPUT_BASE__: Bazel output base directory.

    The substitutions are based on the output from rules_rust.
    See more details in https://github.com/bazelbuild/rules_rust/blob/6b4edd077776d719fc3bb4f891f92e782e68fdaa/tools/rust_analyzer/lib.rs#L157

    Args:
        text: The string containing tokens to be replaced.
        bazel_paths: A BazelPaths object containing the relevant paths.

    Returns:
        The string with all tokens substituted.
    """
    return (
        text.replace("__WORKSPACE__", str(bazel_paths.fuchsia_dir))
        .replace("${pwd}", str(bazel_paths.execroot))
        .replace("__EXEC_ROOT__", str(bazel_paths.execroot))
        .replace("__OUTPUT_BASE__", str(bazel_paths.output_base))
    )


def load_crate_spec_from_json(
    file_path: Path, bazel_paths: build_utils.BazelPaths
) -> CrateSpec:
    """
    Loads a CrateSpec dictionary from a JSON file, performing token substitutions.

    Args:
        file_path: Path to the .rust_analyzer_crate_spec.json file.
        bazel_paths: A BazelPaths object containing the relevant paths.

    Returns:
        A CrateSpec dictionary.
    """
    return json.loads(substitute_tokens(file_path.read_text(), bazel_paths))


def build_rust_analyzer_aspect(
    bazel_paths: build_utils.BazelPaths,
    configured_args: list[str],
    targets: list[str],
) -> None:
    """
    Runs bazel build with the rust_analyzer_aspect to generate crate spec files.

    Args:
        bazel_paths: A BazelPaths object containing the relevant paths.
        configured_args: Additional arguments to pass to Bazel.
        targets: A list of Bazel targets to build the aspect for.
    """
    command = (
        [
            str(bazel_paths.launcher),
            "build",
        ]
        + ([] if _DEBUG else _SILENT_BAZEL_ARGS)
        + configured_args
        + [
            "--norun_validations",
            f"--aspects={_RUST_ANALYZER_ASPECT}",
            "--output_groups=rust_analyzer_crate_spec,rust_generated_srcs,rust_analyzer_proc_macro_dylib,rust_analyzer_src",
        ]
        + targets
    )

    if _DEBUG:
        print("Running Bazel build:", " ".join(command), file=sys.stderr)

    subprocess.check_call(command, cwd=bazel_paths.workspace)


def aquery_rust_analyzer_outputs(
    bazel_paths: build_utils.BazelPaths,
    configured_args: list[str],
    targets: list[str],
) -> list[Path]:
    """
    Runs bazel aquery to find the paths of the generated .rust_analyzer_crate_spec.json files.

    Args:
        bazel_paths: A BazelPaths object containing the relevant paths.
        configured_args: Additional arguments to pass to Bazel.
        targets: A list of Bazel targets to query.

    Returns:
        A list of Paths to the generated .rust_analyzer_crate_spec.json files.
    """
    target_pattern = "+".join(targets)
    # Exclude build scripts and their dependencies from the query result.
    # These dependencies are not always built, and they are unlikely to be useful to rust-analyzer.
    query = f"""outputs(".*\\.rust_analyzer_crate_spec\\.json",
        let all_deps = deps({target_pattern}) in
        let build_scripts = filter(".*_bs$", $all_deps) in
        let build_script_deps = deps($build_scripts) in
        let to_exclude = $build_scripts + $build_script_deps in
        $all_deps except $to_exclude
    )"""

    command = (
        [
            str(bazel_paths.launcher),
            "aquery",
        ]
        + ([] if _DEBUG else _SILENT_BAZEL_ARGS)
        + configured_args
        + [
            "--include_aspects",
            "--include_artifacts",
            f"--aspects={_RUST_ANALYZER_ASPECT}",
            "--output_groups=rust_analyzer_crate_spec",
            query,
            "--output=jsonproto",
        ]
    )

    if _DEBUG:
        print("Running Bazel aquery:", " ".join(command), file=sys.stderr)

    result = subprocess.check_output(command, cwd=bazel_paths.workspace)

    aquery_output = json.loads(result)
    with open("/tmp/debug.json", "w") as f:
        json.dump(aquery_output, f, indent=2)

    artifacts = {a["id"]: a for a in aquery_output.get("artifacts", [])}
    path_fragments = {
        pf["id"]: pf for pf in aquery_output.get("pathFragments", [])
    }

    def get_path(fragment_id: int) -> Path:
        """
        Recursively reconstructs the full path from path fragments.

        Implementation of this function is coupled with the output format of `bazel aquery`.

        Args:
            fragment_id: The ID of the path fragment to start from.

        Returns:
            The reconstructed Path object.
        """
        fragment = path_fragments[fragment_id]
        if "parentId" in fragment:
            return get_path(fragment["parentId"]) / fragment["label"]
        return Path(fragment["label"])

    output_files = []
    for action in aquery_output.get("actions", []):
        for output_id in action.get("outputIds", []):
            if output_id in artifacts:
                artifact = artifacts[output_id]
                p = get_path(artifact["pathFragmentId"])
                output_files.append(bazel_paths.workspace / p)

    return output_files


def generate_rust_project_json_crates(
    bazel_paths: build_utils.BazelPaths,
    bazel_args: list[str],
    targets: list[str],
) -> list[dict[str, T.Any]]:
    """
    Generates the content for a rust-project.json file for the given targets.

    This function orchestrates the process:
    1.  Runs `bazel build` with the rust_analyzer_aspect to produce crate spec files.
    2.  Runs `bazel aquery` to find the paths of the generated spec files.
    3.  Loads and parses each spec file, substituting tokens.
    4.  Converts the loaded CrateSpecs into the final rust-project format.

    Args:
        bazel_paths: A BazelPaths object containing the relevant paths.
        bazel_args: Additional arguments to pass to Bazel commands.
        targets: A list of Bazel targets to include in the rust-project.json.

    Returns:
        A list of dictionaries suitable for dumping to the `crates` key of a rust-project.json file.
        Returns an empty list if no output files are found.
    """
    build_rust_analyzer_aspect(bazel_paths, bazel_args, targets)
    output_files = aquery_rust_analyzer_outputs(
        bazel_paths, bazel_args, targets
    )

    if not output_files:
        return []

    crate_specs = [
        load_crate_spec_from_json(file_path, bazel_paths)
        for file_path in output_files
    ]
    return convert_crate_specs_to_rust_project_crates(crate_specs)


def merge_rust_project_jsons(
    base_json: dict[str, T.Any], jsons_to_merge: list[dict[str, T.Any]]
) -> dict[str, T.Any]:
    """
    Merges multiple rust-project.json structures into a base one.

    Deduplicates crates based on (root_module, target) pair.
    Renumbers crate IDs in merged JSONs to avoid conflicts with the base JSON.

    NOTE: Input dictionaries are modified in-place to avoid copying for efficiency reasons. If this
    is a problem, make a copy at the call site.

    Args:
        base_json: The base rust-project.json dict.
        jsons_to_merge: A list of rust-project.json dicts to merge into the base.

    Returns:
        The base rust-project.json dict with merged crates.
    """
    merged_json = base_json
    base_crates = merged_json["crates"]

    # Create a lookup set for existing crates in the base JSON for deduplication.
    # Also map (root_module, target) to the base crate_id for dependency resolution.
    existing_crates = {}
    base_max_crate_id = -1
    for crate in base_crates:
        root_module = crate["root_module"]
        target = crate["target"]
        crate_id = crate["crate_id"]

        existing_crates[(root_module, target)] = crate_id
        base_max_crate_id = max(base_max_crate_id, crate_id)

    current_offset = base_max_crate_id + 1

    for json_to_merge in jsons_to_merge:
        crates_to_merge = json_to_merge["crates"]
        current_json_max_id = -1

        # First pass: Identify crates to keep and build a remapping table for this JSON.
        id_remap = {}
        crates_to_add = []

        for crate in crates_to_merge:
            root_module = crate["root_module"]
            target = crate["target"]
            crate_id = crate["crate_id"]

            current_json_max_id = max(current_json_max_id, crate_id)

            if (root_module, target) in existing_crates:
                # If it's a duplicate, map its local ID to the existing base ID.
                id_remap[crate_id] = existing_crates[(root_module, target)]
                continue

            # It's a new crate, needs remapping.
            new_id = crate_id + current_offset
            id_remap[crate_id] = new_id
            # Add to existing_crates so subsequent JSONs also deduplicate against this one.
            existing_crates[(root_module, target)] = new_id

            crates_to_add.append(crate)

        # Second pass: Apply remapping and add to merged list.
        for crate in crates_to_add:
            crate["crate_id"] = id_remap[crate["crate_id"]]

            # Remap dependencies
            new_deps = []
            for dep in crate.get("deps", []):
                original_dep_crate = dep["crate"]

                if original_dep_crate in id_remap:
                    dep["crate"] = id_remap[original_dep_crate]
                else:
                    # If dependency isn't found, simply add the current offset. Although this likely
                    # means the crate is invalid.
                    if _DEBUG:
                        print(
                            f"Warning: Dependency {original_dep_crate} not found in remapping.",
                            file=sys.stderr,
                        )
                    dep["crate"] = original_dep_crate + current_offset

                new_deps.append(dep)

            if new_deps:
                crate["deps"] = new_deps
            merged_json["crates"].append(crate)

        # Update offset for the next JSON file to merge.
        current_offset += current_json_max_id + 1

    return merged_json


def find_crate_for_file(
    file_path: Path, crates: list[Crate]
) -> T.Optional[Crate]:
    """
    Finds the crate that contains the given Rust file.

    This function is best-effort, it returns the crate with a longest
    root_module that is a parent of the file_path. If no crate is found, it
    returns None.

    Args:
        file_path: The path to the Rust file.
        crates: A list of Crate dictionaries.

    Returns:
        The Crate dictionary that contains the file, or None if not found.
    """
    found = None
    for crate in crates:
        is_candidate = False
        root_module = Path(crate["root_module"])
        is_candidate = file_path.is_relative_to(root_module.parent)

        source = crate.get("source")
        if source and not is_candidate:
            for include_dir in source.get("include_dirs", []):
                if file_path.is_relative_to(include_dir):
                    is_candidate = True
                    break

        if source and is_candidate:
            for exclude_dir in source.get("exclude_dirs", []):
                if file_path.is_relative_to(exclude_dir):
                    is_candidate = False
                    break

        if not is_candidate:
            continue

        if not found or len(crate["root_module"]) > len(found["root_module"]):
            found = crate

    return found


def get_crate_and_dependencies(
    crate: Crate, crates: list[Crate]
) -> list[Crate]:
    """
    Returns a list including the input crate and all its recursive dependencies.

    Args:
        crate: The starting crate.
        crates: The full list of crates, used to resolve dependency indices.

    Returns:
        A list of Crate dictionaries, starting with the input crate, followed
        by its dependencies in breadth-first order.
    """
    result = [crate]
    visited = {crate["crate_id"]}

    # Index crates by crate_id for quick lookup, just in case the crate_id
    # used by crates are not guaranteed to be the same as their indices in
    # the list.
    crate_id_to_crate = {c["crate_id"]: c for c in crates}

    if crate["crate_id"] not in crate_id_to_crate:
        raise ValueError(f"Crate {crate['crate_id']} not found in crate list.")

    # Make a copy of the deps list to avoid modifying the original crate.
    q = list(crate.get("deps", []))
    while q:
        dep = q.pop(0)
        dep_crate_id = dep["crate"]
        if dep_crate_id in visited:
            continue

        if dep_crate_id not in crate_id_to_crate:
            raise ValueError(
                f"Dependency crate {dep_crate_id} not found in crate list."
            )

        visited.add(dep_crate_id)
        dep_crate = crate_id_to_crate[dep_crate_id]
        result.append(dep_crate)
        q.extend(dep_crate.get("deps", []))

    # Remap crate_ids to be contiguous 0-based indices, as required by
    # rust-analyzer. rust-analyzer uses the index in the 'crates' array to
    # resolve dependencies, ignoring the 'crate_id' field in the crate object
    # itself.
    old_id_to_new_index = {c["crate_id"]: i for i, c in enumerate(result)}
    remapped_result = []

    for i, c in enumerate(result):
        # Shallow copy to avoid modifying the input crates.
        remapped_crate = c.copy()
        remapped_crate["crate_id"] = old_id_to_new_index[c["crate_id"]]

        remapped_deps = []
        for dep in c.get("deps", []):
            old_id = dep["crate"]
            assert old_id in old_id_to_new_index
            remapped_deps.append(
                {
                    "crate": old_id_to_new_index[old_id],
                    "name": dep["name"],
                }
            )
        remapped_crate["deps"] = remapped_deps
        remapped_result.append(remapped_crate)

    return remapped_result
