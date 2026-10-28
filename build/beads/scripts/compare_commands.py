#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import typing as T

from build.bazel.scripts import build_utils
from build.beads.scripts import compare_utils
from build.beads.scripts.compare_utils import (
    CompareCommandsQuery,
    ConfiguredBazelLabel,
)

# Enable debug logging.
_DEBUG = False

# Root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

# Path to the default Ninja binary.
_DEFAULT_NINJA_BIN = _FUCHSIA_DIR / "prebuilt/third_party/ninja/linux-x64/ninja"


def debug(s: T.Any) -> None:
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare GN and Bazel build commands for rustc."
    )

    build_utils.BuildPaths.add_parser_arguments(parser)

    parser.add_argument(
        "--gn_label", required=True, help="GN Rust target label"
    )
    parser.add_argument(
        "--bazel_label", required=True, help="Bazel Rust target label"
    )
    parser.add_argument(
        "--bazel_config",
        default=compare_utils.BAZEL_CONFIG_HOST,
        help=f"Bazel configuration (defaults to {compare_utils.BAZEL_CONFIG_HOST})",
        choices=compare_utils.VALID_BAZEL_CONFIGS,
    )
    parser.add_argument(
        "--action_type",
        default="rustc",
        help="Action type (defaults to rustc)",
        choices=compare_utils.VALID_ACTION_TYPES,
    )
    parser.add_argument(
        "--read_response_files",
        action="store_true",
        default=False,
        help="Read response files directly from the Bazel execroot instead of querying Starlark providers.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output",
    )
    parser.add_argument(
        "--temp_dir", type=pathlib.Path, help="Temporary directory path"
    )

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    try:
        paths = build_utils.BuildPaths.from_parser_args(args)
    except ValueError as e:
        parser.error(str(e))

    debug(f"Fuchsia Dir: {paths.fuchsia_dir}")
    debug(f"Build Dir: {paths.build_dir}")
    debug(f"GN Label: {args.gn_label}")
    debug(f"Bazel Label: {args.bazel_label}")
    debug(f"Bazel Config: {args.bazel_config}")
    debug(f"Action type: {args.action_type}")

    bazel_paths = build_utils.BazelPaths(paths.fuchsia_dir, paths.build_dir)

    target_queries = [
        CompareCommandsQuery(
            gn=args.gn_label,
            bazel=ConfiguredBazelLabel(args.bazel_label, args.bazel_config),
            action_type=args.action_type,
        ),
    ]

    results = compare_utils.compare_gn_and_bazel_commands_for(
        target_queries,
        bazel_paths,
        read_response_files=args.read_response_files,
        debug=debug,
    )

    assert (
        len(results) == 1
    ), f"Unexpected results length (expected 1): {len(results)}"

    result = results[0]
    if result.error:
        print(f"ERROR: {result.error}", file=sys.stderr)
        return 1

    temp_dir = tempfile.mkdtemp(
        prefix="compare_commands_",
        dir=args.temp_dir,
    )
    gn_file = os.path.join(temp_dir, "normalized_gn_args.txt")
    bazel_file = os.path.join(temp_dir, "normalized_bazel_args.txt")
    with open(gn_file, "w") as f:
        f.write("\n".join(result.normalized_gn_args) + "\n")
    with open(bazel_file, "w") as f:
        f.write("\n".join(result.normalized_bazel_args) + "\n")

    debug(f"Comparing normalized args with command:")
    debug(f"diff -u {gn_file} {bazel_file}")
    ret = subprocess.run(["diff", "-u", gn_file, bazel_file])

    # Preserve temporary results if verbose mode or a temp dir is specified.
    # In these modes, the user may want to inspect the temporary files.
    if not (_DEBUG or args.temp_dir):
        shutil.rmtree(temp_dir, ignore_errors=True)

    return ret.returncode


if __name__ == "__main__":
    sys.exit(main())
