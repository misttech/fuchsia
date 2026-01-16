#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Create a bazelrc file that contains unique values per invocation
based on environment variables such as uuids and paths to temporary sockets.

The generated bazelrc file is closely related to
template.remote_services.bazelrc in that it amends existing configuration
definitions.

This script is intended to work for both developer (fx) and infra builds.
"""

import os
import sys
from typing import Iterable

_SCRIPT_DIR = os.path.dirname(__file__)

_SPONGE_LINK = "http://sponge/invocations/"
_RESULTSTORE_LINK = "http://go/fxbtx/"


def sibling_builds_link(link_template: str, key: str, id: str) -> str:
    """Return a URL of a search query for finding related invocations."""
    return f"{link_template}?q={key}:{id}"


def parent_build_link(bbid: str) -> str:
    # Builds launched directly by the 'led' tool look different
    # from other infra builds.
    if "/led/" in bbid:
        return f"http://go/lucibuild/{bbid}/+/build.proto"
    else:
        return f"http://go/bbid/{bbid}"


def metadata_option(key: str, value: str) -> str:
    """Returns the equivalent bazel option for passing metadata."""
    return f"--build_metadata={key}={value}"


def build_config_option(config: str, options: str) -> str:
    """Returns a bazelrc command that maps options into a config definition."""
    return f"build:{config} {options}"


def metadata_bazelrc(env: dict[str, str]) -> Iterable[str]:
    uuid = env.get("FX_BUILD_UUID")
    bbid = env.get("BUILDBUCKET_ID")
    if not uuid and not bbid:
        raise KeyError(
            "Expecting either FX_BUILD_UUID or BUILDBUCKET_ID in environment, but got neither."
        )

    # Establish build metadata for linking related related invocations.
    if uuid:
        # There is no parent-build-link for fx-build (yet).
        # Once ninja+resultstore is integrated, then there will be
        # a single top-level build invocation we can point to.
        yield build_config_option(
            "sponge",
            metadata_option(
                "SIBLING_BUILDS_LINK",
                sibling_builds_link(_SPONGE_LINK, "FX_BUILD_UUID", uuid),
            ),
        )
        yield build_config_option(
            "resultstore",
            metadata_option(
                "SIBLING_BUILDS_LINK",
                sibling_builds_link(_RESULTSTORE_LINK, "FX_BUILD_UUID", uuid),
            ),
        )

    elif bbid:
        yield build_config_option(
            "_bes_common",
            metadata_option("PARENT_BUILD_LINK", parent_build_link(bbid)),
        )
        yield build_config_option(
            "sponge_infra",
            metadata_option(
                "SIBLING_BUILDS_LINK",
                sibling_builds_link(_SPONGE_LINK, "BUILDBUCKET_ID", bbid),
            ),
        )
        yield build_config_option(
            "resultstore_infra",
            metadata_option(
                "SIBLING_BUILDS_LINK",
                sibling_builds_link(_RESULTSTORE_LINK, "BUILDBUCKET_ID", bbid),
            ),
        )


def service_proxies_bazelrc(env: dict[str, str]) -> Iterable[str]:
    # Redirect traffic to proxies (infra only).
    # Remote service configuration.
    rbe_socket_path = env.get("BAZEL_rbe_socket_path")
    if rbe_socket_path:
        yield build_config_option(
            "_remote_common", f"--remote_proxy=unix://{rbe_socket_path}"
        )
    # sponge and resultstore are mutually exclusive, so only one of the
    # following two --bes_proxy options will ever apply.
    sponge_socket_path = env.get("BAZEL_sponge_socket_path")
    if sponge_socket_path:
        yield build_config_option(
            "sponge_infra", f"--bes_proxy=unix://{sponge_socket_path}"
        )

    resultstore_socket_path = env.get("BAZEL_resultstore_socket_path")
    if resultstore_socket_path:
        yield build_config_option(
            "resultstore_infra", f"--bes_proxy=unix://{resultstore_socket_path}"
        )


def generate_bazelrc(env: dict[str, str]) -> Iterable[str]:
    header = [
        "# This bazelrc file contains ephemeral options that are not intended",
        "# to persist across build invocations.",
        "# AUTO-GENERATED - DO NOT EDIT!",
    ]
    yield from header
    yield from metadata_bazelrc(env)
    yield from service_proxies_bazelrc(env)


def main():
    for line in generate_bazelrc(os.environ):
        print(line)


if __name__ == "__main__":
    sys.exit(main())
