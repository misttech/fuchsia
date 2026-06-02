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


def read_json_file(file: Path):
    with open(file) as f:
        return json.load(f)


def write_json_file(file: Path, contents) -> None:
    if file.exists() and read_json_file(file) == contents:
        return
    with open(file, "w") as f:
        json.dump(contents, f, indent=2, sort_keys=True)


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

    output = original

    def apply_edit(key, action, value):
        if action == "unset":
            assert value is None
            if key in output:
                del output[key]
            return
        assert value is not None, f"Bad edit: f{key=}, f{action=}, f{value=}"
        if action == "set":
            output[key] = value
            return
        assert action == "append", f"Bad edit: f{key=}, f{action=}, f{value=}"
        if key == "features":
            append_features(value)
        else:
            output[key] += value

    for edit in edits:
        apply_edit(edit["key"], edit["action"], edit.get("value"))

    output["features"] = ",".join(features)

    write_json_file(args.output, output)

    return 0


if __name__ == "__main__":
    sys.exit(main())
