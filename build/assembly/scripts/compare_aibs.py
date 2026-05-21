#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import filecmp
import json
import sys
from pathlib import Path

import serialization
from assembly.assembly_input_bundle import AssemblyInputBundle


def compare_json(obj1: object, obj2: object, path: str = "") -> list[str]:
    """Recursively compares two JSON objects and returns a list of differences."""
    diffs: list[str] = []

    if type(obj1) != type(obj2):
        diffs.append(f"Type mismatch at {path}: {type(obj1)} != {type(obj2)}")
        return diffs

    if isinstance(obj1, dict):
        assert isinstance(obj2, dict)
        keys1 = set(obj1.keys())
        keys2 = set(obj2.keys())

        missing_in_2 = keys1 - keys2
        added_in_2 = keys2 - keys1

        for k in missing_in_2:
            diffs.append(f"Missing field in second object at {path}: {k}")
        for k in added_in_2:
            diffs.append(f"Added field in second object at {path}: {k}")

        for k in keys1 & keys2:
            diffs.extend(compare_json(obj1[k], obj2[k], f"{path}/{k}"))

    elif isinstance(obj1, list):
        assert isinstance(obj2, list)
        if len(obj1) != len(obj2):
            diffs.append(
                f"List length mismatch at {path}: {len(obj1)} != {len(obj2)}"
            )

        for i in range(min(len(obj1), len(obj2))):
            diffs.extend(compare_json(obj1[i], obj2[i], f"{path}[{i}]"))

        if len(obj1) > len(obj2):
            for i in range(len(obj2), len(obj1)):
                diffs.append(
                    f"Extra item in first list at {path}[{i}]: {obj1[i]}"
                )
        elif len(obj2) > len(obj1):
            for i in range(len(obj1), len(obj2)):
                diffs.append(
                    f"Extra item in second list at {path}[{i}]: {obj2[i]}"
                )

    elif isinstance(obj1, set):
        assert isinstance(obj2, set)
        missing_in_2 = obj1 - obj2
        added_in_2 = obj2 - obj1

        for k in missing_in_2:
            diffs.append(f"Missing item in second set at {path}: {k}")
        for k in added_in_2:
            diffs.append(f"Added item in second set at {path}: {k}")

    else:
        if obj1 != obj2:
            diffs.append(f"Value mismatch at {path}: {obj1} != {obj2}")

    return diffs


def validate_aib(
    aib_name: str, dir1: Path, dir2: Path, compare_contents: bool
) -> bool:
    """Validates that corresponding AIBs are the same. Returns True if valid, False if diffs found."""
    if compare_contents:
        diffs = []
        files1 = sorted(
            [p.relative_to(dir1) for p in dir1.rglob("*") if p.is_file()]
        )
        files2 = sorted(
            [p.relative_to(dir2) for p in dir2.rglob("*") if p.is_file()]
        )

        missing_in_2 = set(files1) - set(files2)
        added_in_2 = set(files2) - set(files1)

        for f in missing_in_2:
            diffs.append(f"Missing file in second directory: {f}")
        for f in added_in_2:
            diffs.append(f"Extra file in second directory: {f}")

        for f in set(files1) & set(files2):
            if f.name == "assembly_config.json":
                try:
                    with (
                        open(dir1 / f) as file1,
                        open(dir2 / f) as file2,
                    ):
                        bundle1 = serialization.json_load(
                            AssemblyInputBundle, file1
                        )
                        bundle2 = serialization.json_load(
                            AssemblyInputBundle, file2
                        )
                    cfg1 = serialization.instance_to_dict(bundle1)
                    cfg2 = serialization.instance_to_dict(bundle2)
                    cfg_diffs = compare_json(cfg1, cfg2)
                    diffs.extend(cfg_diffs)
                except Exception as e:
                    diffs.append(
                        f"Failed to semantically compare assembly_config.json: {e}"
                    )
            else:
                if not filecmp.cmp(dir1 / f, dir2 / f, shallow=False):
                    diffs.append(f"File content mismatch: {f}")

        if diffs:
            print(
                f"\n\nDirectory content differences found for: '{aib_name}'",
                file=sys.stderr,
            )
            for d in diffs:
                print(f"  {d}", file=sys.stderr)
            print("\n\n", file=sys.stderr)
            return False
        return True

    # Tier 2 & 3: Semantic check for JSON files
    cfg1_path = dir1 / "assembly_config.json"
    cfg2_path = dir2 / "assembly_config.json"

    if cfg1_path.exists() and cfg2_path.exists():
        if filecmp.cmp(cfg1_path, cfg2_path):
            return True

        try:
            with open(cfg1_path) as file1:
                bundle1 = serialization.json_load(AssemblyInputBundle, file1)
            with open(cfg2_path) as file2:
                bundle2 = serialization.json_load(AssemblyInputBundle, file2)

            if bundle1 == bundle2:
                print(
                    f"WARNING: assembly_config.json files for {aib_name} are semantically identical but the AIBs themselves are not strictly identical.",
                    file=sys.stderr,
                )
                return True

            cfg1 = serialization.instance_to_dict(bundle1)
            cfg2 = serialization.instance_to_dict(bundle2)

            diffs = compare_json(cfg1, cfg2)
            if not diffs:
                return True

            print(
                f"\n\nassembly_config.json differences found for AIB:  '{aib_name}'",
                file=sys.stderr,
            )
            for d in diffs:
                print(f"  {d}", file=sys.stderr)
            print("\n\n", file=sys.stderr)
            return False
        except json.JSONDecodeError as e:
            print(f"Error decoding JSON: {e}", file=sys.stderr)
            return False
    else:
        print(
            "assembly_config.json missing in one of the directories.",
            file=sys.stderr,
        )
        return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare two AIB directories.")
    parser.add_argument(
        "dir1",
        type=Path,
        help="Path to first AIB directory (GN)",
    )
    parser.add_argument(
        "dir2",
        type=Path,
        help="Path to second AIB directory (Bazel)",
    )
    parser.add_argument(
        "--compare-contents",
        action="store_true",
        help="Recursively compare all files in the directories",
    )
    parser.add_argument(
        "--stamp",
        type=argparse.FileType("w"),
        help="Stamp file to write upon successful completion",
    )
    args = parser.parse_args()

    if not args.dir1.is_dir():
        print(f"Error: {args.dir1} is not a directory", file=sys.stderr)
        return 1
    if not args.dir2.is_dir():
        print(f"Error: {args.dir2} is not a directory", file=sys.stderr)
        return 1

    if args.dir1.name != args.dir2.name:
        print(
            f"Error: Directory names differ: {args.dir1.name} != {args.dir2.name}",
            file=sys.stderr,
        )

    aib_name = args.dir1.name

    if not validate_aib(aib_name, args.dir1, args.dir2, args.compare_contents):
        return 1

    if args.stamp:
        with args.stamp as stamp:
            stamp.write("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
