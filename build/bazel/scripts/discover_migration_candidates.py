#!/usr/bin/env fuchsia-vendored-python

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import pathlib
import re
import sys

# Fields that are considered "standard" and easy to convert.
_STANDARD_FIELDS = {
    "deps",
    "edition",
    "inputs",
    "name",
    "output_name",
    "source_root",
    "sources",
    "test_deps",
    "testonly",
    "visibility",
    "with_unit_tests",
}

# Default complexity scores for non-simple deps.
_DEFAULT_DEP_COMPLEXITY_SCORE = 1

# Default complexity scores for non-standard fields.
_DEFAULT_FIELD_COMPLEXITY_SCORE = 1

# Fields that are considered "complex" and require more attention.
#
# This dictionary maps field names to their complexity score.
_COMPLEX_FIELDS = {
    "configs": 2,
    "rustc_flags": 2,
    "forward_variables_from": 2,
}


def find_gn_files(start_dir: pathlib.Path, exclude_dirs: list[str]):
    """Find all BUILD.gn files in the specified directory and its subdirectories."""
    return [
        path
        for path in start_dir.rglob("BUILD.gn")
        if path.is_file()
        and not any(
            path.is_relative_to(excluded_dir) for excluded_dir in exclude_dirs
        )
    ]


def end_pos_for_target(content: str, start_pos: int) -> int:
    """Find the matching closing brace for a target."""
    brace_count = 1
    end_pos = start_pos

    while brace_count > 0 and end_pos < len(content):
        if content[end_pos] == "{":
            brace_count += 1
        elif content[end_pos] == "}":
            brace_count -= 1
        end_pos += 1

    if brace_count != 0:
        raise ValueError("Unmatched braces in BUILD.gn file")

    return end_pos


def deps_from_target_body(target_body: str) -> list[str]:
    """Extract deps from a GN target body."""
    deps = []
    for deps_match in re.finditer(
        r"^\s*(deps|public_deps|data)\s*(?:\+=|=)\s*\[(.*?)\]",
        target_body,
        re.MULTILINE | re.DOTALL,  # To handle multi-line deps.
    ):
        deps_content = deps_match.group(2)
        dep_pattern = re.compile(r'"([^"]+)"')
        deps.extend(dep_pattern.findall(deps_content))
    return deps


def fields_from_target_body(target_body: str) -> list[str]:
    """Extract fields from a GN target body."""
    fields = set()
    for field_match in re.finditer(
        r"^\s*([a-z_]+)\s*\+?=",
        target_body,
        re.MULTILINE,
    ):
        fields.add(field_match.group(1))
    return list(fields)


def targets_from_gn_file(
    filepath: pathlib.Path, target_types: list[str]
) -> list[dict]:
    """Parse a BUILD.gn file and extract information about targets in input target_types.

    This function implements a simple regex-based parser for BUILD.gn files.
    It looks for `target_type(name) { ... }` patterns and extracts dependencies
    and fields from the target body.
    """
    content = filepath.read_text()

    target_type_pattern = "|".join(target_types)
    # This regex looks for any of the input target_types followed by a name
    # and an opening brace.
    target_re = re.compile(
        r'((?:{}))\s*\(\s*"([^"]+)"\s*\)\s*\{{'.format(target_type_pattern)
    )

    targets = []
    for match in target_re.finditer(content):
        start_pos = match.end()
        try:
            end_pos = end_pos_for_target(content, start_pos)
        except ValueError as e:
            print(
                f"WARNING: Error parsing {filepath} after position {start_pos}: {e}",
                file=sys.stderr,
            )
            continue

        target_body = content[start_pos : end_pos - 1]
        targets.append(
            {
                "type": match.group(1),
                "name": match.group(2),
                "path": filepath,
                "deps": deps_from_target_body(target_body),
                "fields": fields_from_target_body(target_body),
            }
        )

    return targets


def is_simple_dep(dep: str) -> bool:
    """
    Determine if a dependency is simple and can be easily mapped to Bazel.
    """

    # We have generated Bazel targets for third-party Rust and Go libraries.
    # These dependencies are simple and can be easily mapped to Bazel.
    return dep.startswith("//third_party/rust_crates") or dep.startswith(
        "//third_party/golibs"
    )


def complexity_scores_by_file(targets: list[dict]) -> list[dict]:
    """
    Analyze the given list of targets and return a dictionary of files and their associated targets.
    """

    # TODO(https://fxbug.dev/470222143): Improve the scoring mechanism.

    files = {}
    for target in targets:
        filepath = target["path"]
        if filepath not in files:
            files[filepath] = {
                "path": filepath,
                "targets": [],
                "total_complexity": 0,
                "total_targets": 0,
            }

        simple_deps = [dep for dep in target["deps"] if is_simple_dep(dep)]
        non_simple_deps = [
            dep for dep in target["deps"] if not is_simple_dep(dep)
        ]

        non_standard_fields = [
            f for f in target["fields"] if f not in _STANDARD_FIELDS
        ]

        # Complex score is the sum of the number of non-third-party deps and
        # the complexity of non-standard fields.
        #
        # TODO(https://fxbug.dev/470222143): Further filter out deps that already exist in Bazel.
        _dep_score = len(non_simple_deps) * _DEFAULT_DEP_COMPLEXITY_SCORE
        _field_score = sum(
            _COMPLEX_FIELDS.get(f, _DEFAULT_FIELD_COMPLEXITY_SCORE)
            for f in non_standard_fields
        )
        # TODO(https://fxbug.dev/470222143): Improve the scoring mechanism.
        complexity_score = _dep_score + _field_score

        target_info = {
            "name": target["name"],
            "type": target["type"],
            "simple_deps": simple_deps,
            "non_simple_deps": non_simple_deps,
            "non_standard_fields": non_standard_fields,
            "complexity_score": complexity_score,
        }

        files[filepath]["targets"].append(target_info)
        files[filepath]["total_complexity"] += complexity_score
        files[filepath]["total_targets"] += 1

    # Convert to list and sort by total complexity.
    result_files = list(files.values())
    result_files.sort(key=lambda x: (x["total_complexity"], x["total_targets"]))
    return result_files


def bazel_targets_in_dir(directory: pathlib.Path) -> set[str]:
    """Get all Bazel target names from input directory."""
    bazel_file = directory / "BUILD.bazel"
    if not bazel_file.exists():
        return set()

    content = bazel_file.read_text()

    target_names = {
        match.group(1)
        for match in re.finditer(r'name\s*=\s*"([^"]+)"', content)
    }
    return target_names


def print_results(file_results: list[dict], top: int):
    """Print the results of the analysis in a tabular format."""
    print("\nTop Candidates for Migration (Grouped by File):")
    print("=" * 80)
    for i, file_res in enumerate(file_results[:top]):
        print(f"{i+1}. {file_res['path']}")
        print(f"   Total Complexity: {file_res['total_complexity']}")
        print(f"   Targets ({file_res['total_targets']}):")
        for target in file_res["targets"]:
            print(f"     - {target['name']} ({target['type']})")
            print(f"       Score: {target['complexity_score']}")
            if target["non_simple_deps"]:
                print(f"       Non-simple deps: {target['non_simple_deps']}")
            if target["non_standard_fields"]:
                print(
                    f"       Non-standard fields: {target['non_standard_fields']}"
                )
        print("-" * 80)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Analyze targets in a directory."
    )
    parser.add_argument(
        "--directory",
        type=pathlib.Path,
        required=True,
        help="Directory to search for BUILD.gn files",
    )
    parser.add_argument(
        "--exclude-dirs",
        type=str,
        nargs="*",
        default=[],
        help="Directories to exclude when searching for BUILD.gn files",
    )
    parser.add_argument(
        "--target-types",
        type=str,
        nargs="+",
        required=True,
        help="Target types to look for when searching for BUILD.gn files",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=10,
        help="Number of top candidates to show",
    )
    args = parser.parse_args()

    print(
        f"Searching for GN files in {args.directory}, excluding {args.exclude_dirs}"
    )
    gn_files = find_gn_files(args.directory, args.exclude_dirs)
    print(f"Found {len(gn_files)} BUILD.gn files.")

    all_targets = []
    for gn_file in gn_files:
        targets = targets_from_gn_file(gn_file, args.target_types)

        # Filter targets that already exist in Bazel
        bazel_targets = bazel_targets_in_dir(gn_file.parent)
        filtered_targets = []
        for t in targets:
            if t["name"] not in bazel_targets:
                filtered_targets.append(t)

        all_targets.extend(filtered_targets)

    print(
        f"Found {len(all_targets)} targets of target types {args.target_types} (excluding targets that already exist in Bazel)."
    )

    file_results = complexity_scores_by_file(all_targets)
    print_results(file_results, args.top)

    return 0


if __name__ == "__main__":
    sys.exit(main())
