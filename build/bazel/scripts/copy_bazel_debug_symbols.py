# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Copies debug symbols from bazel-out to the Ninja build directory's .build-id/."""

import argparse
import sys
from pathlib import Path

from build.api.debug_symbols import DebugSymbolsManifestParser
from build.bazel.scripts.bazel_action_impl import (
    copy_debug_symbols_to_build_dir,
)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Copy debug symbols from bazel-out to the Ninja build directory's .build-id/"
    )
    parser.add_argument("build_dir", type=str, help="The build directory.")
    args = parser.parse_args()
    build_dir = Path(args.build_dir).resolve()
    # LINT.IfChange(bazel_device_tests_debug_symbols_json)
    manifest_files = [
        build_dir / "bazel_host_tests.debug_symbols.json",
        build_dir / "bazel_device_tests.debug_symbols.json",
    ]
    # LINT.ThenChange(//build/bazel/scripts/bazel_tests_utils.py:bazel_device_tests_debug_symbols_json)
    existing_manifests = [f for f in manifest_files if f.exists()]
    if not existing_manifests:
        print("Manifest file not found", file=sys.stderr)
        return 1

    manifest_parser = DebugSymbolsManifestParser(build_dir)
    manifest_parser.enable_build_id_resolution()
    for manifest_file in existing_manifests:
        try:
            manifest_parser.parse_manifest_file(manifest_file)
        except Exception as e:
            print(
                f"Error parsing debug symbols manifest file {manifest_file}: {e}",
                file=sys.stderr,
            )
            return 1

    if not manifest_parser.entries:
        return 0

    copy_debug_symbols_to_build_dir(build_dir, manifest_parser.entries)
    return 0


if __name__ == "__main__":
    sys.exit(main())
