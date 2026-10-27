#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors
#
# Use of this source code is governed by a MIT-style
# license that can be found in the LICENSE file or at
# https://opensource.org/licenses/MIT

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any


def read_json_file(file: Path) -> Any:
    with open(file) as f:
        return json.load(f)


def write_json_file(file: Path, contents: Any) -> None:
    if file.exists() and read_json_file(file) == contents:
        return
    with open(file, "w") as f:
        json.dump(contents, f, indent=2, sort_keys=True)


def erase[T](full: list[T], sublist: list[T]) -> list[T]:
    if sublist:
        for i in range(len(full) - len(sublist) + 1):
            if full[i : i + len(sublist)] == sublist:
                return erase(full[0:i] + full[i + len(sublist) :], sublist)
    return full


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--rustc",
        metavar="PATH",
        type=Path,
        help="Path to the rustc binary",
        required=True,
    )
    parser.add_argument(
        "--output",
        metavar="PATH",
        type=Path,
        action="append",
        help="Path of the new custom target JSON file to write",
        required=True,
    )
    parser.add_argument(
        "--edits",
        metavar="PATH",
        type=Path,
        help="Path to a JSON file of edits to make",
        required=True,
    )
    args = parser.parse_args()

    # The JSON is a list whose first element is the target string.
    # Each remaining element is a dict describing an edit to make.
    edits = read_json_file(args.edits)
    target = edits.pop(0)

    rustc_cmd = [
        f"{args.rustc}",
        f"--target={target}",
        "-Zunstable-options",
        "--print",
        "target-spec-json",
    ]
    with subprocess.Popen(rustc_cmd, stdout=subprocess.PIPE) as proc:
        assert proc.stdout is not None
        original = json.load(proc.stdout)
        if proc.wait() != 0:
            raise subprocess.CalledProcessError(proc.returncode, rustc_cmd)

    # The "features" key is handled specially with its own action.
    original_features = original.get("features", "").split(",")
    features = original_features

    def is_toggle(feature: str) -> bool:
        return feature.startswith("-") or feature.startswith("+")

    def append_features(new_features: list[str]) -> None:
        nonlocal features
        for new_feature in new_features:

            def overridden(old_feature: str) -> bool:
                return (
                    is_toggle(old_feature)
                    and old_feature[1:] == new_feature[1:]
                )

            if is_toggle(new_feature):
                features = [
                    old_feature
                    for old_feature in features
                    if not overridden(old_feature)
                ]
                # If the original list had +foo and that was removed, do not
                # also add -foo since its presence in the original list means
                # it was not in the default set.
                if (
                    new_feature.startswith("-")
                    and ("+" + new_feature[1:]) in original_features
                    # TODO(https://fxbug.dev/518918403): rustc complains it
                    # wants to see -neon even when we had to remove +neon to
                    # get back to the apparent baseline.
                    and new_feature != "-neon"
                ):
                    continue
            features.append(new_feature)

    def append_sanitizers(
        output: dict[str, Any], new_sanitizers: list[str]
    ) -> None:
        sanitizers = output.setdefault("default-sanitizers", [])
        supported = output.setdefault("supported-sanitizers", [])
        for entry in new_sanitizers:
            assert is_toggle(entry), f"Bad default-sanitizers edit: {entry=}"
            name = entry[1:]
            if name in sanitizers:
                sanitizers.remove(name)
            if entry.startswith("+"):
                sanitizers.append(name)
                if name not in supported:
                    supported.append(name)

    def apply_edits(
        output: dict[str, Any], edits: list[dict[str, Any]]
    ) -> dict[str, Any]:
        def apply_edit(key: str, action: str, value: Any) -> None:
            if action == "unset":
                assert value is None
                if key in output:
                    del output[key]
                return
            assert value is not None, f"Bad edit: {key=}, {action=}, {value=}"
            if action == "set":
                output[key] = value
                return
            if action == "edit":
                output[key] = apply_edits(output.get(key, {}), value)
                return
            if action == "erase":
                output[key] = erase(output[key], value)
                return
            if action == "extend":
                output[key].extend(value)
                return
            assert action == "append", f"Bad edit: {key=}, {action=}, {value=}"
            if key == "features":
                append_features(value)
            elif key == "default-sanitizers":
                append_sanitizers(output, value)
            else:
                output[key].append(value)

        for edit in edits:
            apply_edit(edit["key"], edit["action"], edit.get("value"))
        return output

    output = apply_edits(original, edits)
    output["features"] = ",".join(features)

    write_json_file(args.output[0], output)

    # The main file wasn't touched if it didn't change.  But it doesn't hurt to
    # unconditionally recreate the hard links, since they just come back as the
    # very same unchanged file.
    for extra_output in args.output[1:]:
        extra_output.unlink(missing_ok=True)
        extra_output.hardlink_to(args.output[0])

    return 0


if __name__ == "__main__":
    sys.exit(main())
