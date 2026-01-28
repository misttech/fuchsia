#!/usr/bin/env fuchsia-vendored-python

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import pathlib
import re
import sys

# Debug flag to enable verbose output.
_DEBUG = False

# The root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

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
_DEFAULT_DEP_COMPLEXITY = 1

# Default complexity scores for non-standard fields.
_DEFAULT_FIELD_COMPLEXITY = 1

# Fields that are considered "complex" and require more attention.
#
# This dictionary maps field names to their complexity score.
_COMPLEX_FIELDS = {
    "configs": 2,
    "rustc_flags": 2,
    "forward_variables_from": 2,
}


def debug(msg: str):
    if _DEBUG:
        print(msg, file=sys.stderr)


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


class ComplexityCalculator:
    def __init__(self, root_dir: pathlib.Path):
        self._root_dir = root_dir.resolve()

    def _is_third_party_target(self, label: str) -> bool:
        """Check if a target is a third-party target.

        We have generated Bazel targets for third-party Rust and Go libraries.
        These dependencies are simple and can be easily mapped to Bazel.
        """
        if not self._is_fully_qualified_label(label):
            raise ValueError(
                f"Invalid label: {label}, only fully-qualified labels are supported"
            )

        return label.startswith(
            "//third_party/rust_crates"
        ) or label.startswith("//third_party/golibs")

    def _is_bazel_target(self, label: str) -> bool:
        if not self._is_fully_qualified_label(label):
            raise ValueError(
                f"Invalid label: {label}, only fully-qualified labels are supported"
            )

        """Check if a target is already in a BUILD.bazel file in the same directory."""
        path_part, target_name = label.split(":")
        path_dir = self._root_dir / path_part.lstrip("/")
        bazel_file = path_dir / "BUILD.bazel"
        if not bazel_file.exists():
            return False

        content = bazel_file.read_text()
        return (
            f'name = "{target_name}"' in content
            or f'name="{target_name}"' in content
        )

    def _is_fully_qualified_label(self, label: str) -> bool:
        return label.startswith("//") and ":" in label

    def _to_fully_qualified_label(
        self, dir_path: pathlib.Path, label: str
    ) -> str:
        if self._is_fully_qualified_label(label):
            return label

        if ":" in label:
            label_path, target_name = label.split(":")
            target_path = dir_path / label_path
        else:
            target_name = pathlib.Path(label).name
            target_path = dir_path / label
        return f"//{target_path}:{target_name}"

    def complexity_for_dep(self, dep_label: str) -> int:
        """Calculate complexity for a dependency."""
        if not dep_label.startswith("//"):
            raise ValueError(
                f"Invalid dep label: {dep_label}, only fully-qualified labels are supported"
            )

        return (
            0
            if (
                self._is_third_party_target(dep_label)
                or self._is_bazel_target(dep_label)
            )
            else 1
        )

    def complexity_for_target(self, target: dict) -> int:
        """Calculate complexity for a target taking its dependencies and fields into account."""
        filepath = target["path"]
        target_name = target["name"]

        dir_path = filepath.parent
        # If target itself is in Bazel already (e.g. bazel2gn targets), complexity is 0.
        if self._is_bazel_target(f"//{dir_path}:{target_name}"):
            return 0

        dep_score = 0
        for dep in target["deps"]:
            dep_score += self.complexity_for_dep(
                self._to_fully_qualified_label(dir_path, dep)
            )

        non_standard_fields = [
            f for f in target["fields"] if f not in _STANDARD_FIELDS
        ]
        field_score = sum(
            _COMPLEX_FIELDS.get(f, _DEFAULT_FIELD_COMPLEXITY)
            for f in non_standard_fields
        )

        return dep_score + field_score

    def complexity_for_file(
        self, gn_file: pathlib.Path, target_types: list[str]
    ) -> dict:
        targets = targets_from_gn_file(gn_file, target_types)

        file_result = {
            "path": gn_file,
            "targets": [],
            "total_complexity": 0,
            "total_targets": 0,
        }

        for target in targets:
            complexity = self.complexity_for_target(target)

            target_info = {
                "name": target["name"],
                "type": target["type"],
                "complexity": complexity,
                "deps": target["deps"],
                "fields": target["fields"],
                # Add extra info for display if needed
                "non_standard_fields": [
                    f for f in target["fields"] if f not in _STANDARD_FIELDS
                ],
            }

            file_result["targets"].append(target_info)
            file_result["total_complexity"] += complexity
            file_result["total_targets"] += 1

        return file_result


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
            print(f"       Complexity: {target['complexity']}")
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
    parser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        default=_FUCHSIA_DIR,
        help="Path to the Fuchsia directory",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Enable verbose output",
    )
    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    debug(
        f"Searching for GN files in {args.directory}, excluding {args.exclude_dirs}"
    )
    gn_files = find_gn_files(args.directory, args.exclude_dirs)
    debug(f"Found {len(gn_files)} BUILD.gn files.")

    calculator = ComplexityCalculator(args.fuchsia_dir)
    file_results = []
    for gn_file in gn_files:
        file_result = calculator.complexity_for_file(gn_file, args.target_types)
        # Only add files that have targets
        if file_result["total_targets"] > 0:
            file_results.append(file_result)

    # Sort by total complexity
    file_results.sort(key=lambda x: (x["total_complexity"], x["total_targets"]))
    print_results(file_results, args.top)

    return 0


if __name__ == "__main__":
    sys.exit(main())
