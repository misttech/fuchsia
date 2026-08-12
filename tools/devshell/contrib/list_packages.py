# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

#### CATEGORY=Build
### List which packages are built.

import argparse
import json
import os
import re
from collections.abc import Callable, Iterable, Iterator
from typing import Any


# Print all the packages in sorted order, one per line.
def print_packages(packages: Iterable[str]) -> None:
    for p in sorted(packages):
        print(p)


# Extracts the list of package names that are accepted by filter_ from a
# decoded package list manifest.
def extract_packages_from_listing(
    manifest_data: dict[str, Any],
    filter_: Callable[[str], bool],
    build_dir: str,
) -> Iterator[str]:
    packages: list[str] = []
    for manifest in manifest_data["content"]["manifests"]:
        manifest_path = os.path.join(build_dir, manifest)
        with open(manifest_path) as f:
            packages.append(json.load(f)["package"]["name"])
    return filter(filter_, packages)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="""
list-packages lists the packages that the build is aware of. These are
packages that can be rebuilt and/or pushed to a device.
Note: list-packages DOES NOT list all packages that *could* be built, only
those that are included in the current build configuration.
""",
        epilog="""
See https://fuchsia.dev/fuchsia-src/development/build/software_assembly/build_configuration
for more information about using these package sets.
""",
    )
    parser.add_argument(
        "pattern",
        nargs="?",
        help="list only packages that full match this regular expression",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()

    # If a custom regex for package names is provided, use that to filter
    # results; otherwise, return all results
    if args.pattern:
        regex = re.compile(args.pattern)
        filter_ = lambda s: bool(regex.fullmatch(s))
    else:
        filter_ = lambda s: True

    build_dir = os.environ.get("FUCHSIA_BUILD_DIR")
    if not build_dir:
        raise RuntimeError(
            'Environment variable "FUCHSIA_BUILD_DIR" is not set.'
        )

    manifest_list_path = os.path.join(build_dir, "all_package_manifests.list")
    if not os.path.exists(manifest_list_path):
        raise RuntimeError(
            f"'{manifest_list_path}' not found. Run 'fx build' or 'fx build updates' to assemble package manifests."
        )

    with open(manifest_list_path) as f:
        packages = extract_packages_from_listing(
            json.load(f), filter_, build_dir=build_dir
        )
    print_packages(packages)


if __name__ == "__main__":
    main()
