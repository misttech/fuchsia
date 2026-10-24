#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generates an ephemeral bazelrc for a single bazel invocation."""

import argparse
import os
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path

_DEFAULT_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent.parent

from build.bazel.scripts import bazel_invocation as bazel_inv
from build.resultstore import related_invocations as rel_inv
from build.resultstore import rs_profile

build_config_option = bazel_inv.build_config_option


def _metadata_option(key: str, value: str) -> str:
    """Returns the equivalent bazel option for passing metadata."""
    return f"--build_metadata={key}={value}"


def _metadata_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates BES metadata bazelrc lines from environment variables."""
    uuid = env.get(rel_inv.ENV_FX_BUILD_UUID)
    bbid = env.get(rel_inv.ENV_BUILDBUCKET_ID)
    if not uuid and not bbid:
        # Out-of-build invocation that we don't care to record.
        return

    # Resolve RS_results_url from active profile if not in environment
    effective_env = dict(env)
    if rel_inv.ENV_RESULTS_URL not in effective_env:
        try:
            profile = rs_profile.load(_DEFAULT_FUCHSIA_DIR, env)
            effective_env[rel_inv.ENV_RESULTS_URL] = profile.results_url
        except Exception:
            pass

    # Use related_invocations to resolve the complete build hierarchy.
    linkage = rel_inv.resolve(effective_env)
    for k, v in sorted(linkage.metadata.items()):
        opt = _metadata_option(k, v)
        yield build_config_option("_bes_common", opt)


def _service_proxies_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates bazelrc lines for remote execution and BES proxy sockets."""
    yield from bazel_inv.generate_service_proxies_bazelrc(env)
    yield from _jiri_prebuilt_bazelrc(env)


def _jiri_prebuilt_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Configure sandbox mounts for on-demand Jiri/CIPD prebuilts."""
    env_fuchsia_dir = env.get("FUCHSIA_DIR")
    fuchsia_dir = (
        Path(env_fuchsia_dir).resolve()
        if env_fuchsia_dir
        else _DEFAULT_FUCHSIA_DIR
    )
    for subdir in (".jiri_root", "prebuilt"):
        dir_path = fuchsia_dir / subdir
        if dir_path.is_dir():
            yield f"build --sandbox_add_mount_pair={dir_path}"
            yield f"build --sandbox_writable_path={dir_path}"

    # TODO(https://fxbug.dev/570715338): Mount the Jiri proxy socket directory
    # and forward its environment variable once jiri proxy is implemented.


def _auth_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates bazelrc lines for authentication (e.g. gcertauth)."""
    rbe_auth = env.get("FX_BUILD_RBE_AUTH_TYPE", "")
    rs_auth = env.get("FX_BUILD_RS_AUTH_TYPE", "")
    yield from bazel_inv.generate_auth_bazelrc(rbe_auth, rs_auth)


def generate_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    header = [
        "# This bazelrc file contains ephemeral options that are not intended",
        "# to persist across build invocations.",
        "# AUTO-GENERATED - DO NOT EDIT!",
    ]
    yield from header

    yield from _metadata_bazelrc(env)
    yield from _auth_bazelrc(env)
    yield from _service_proxies_bazelrc(env)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args(argv)
    for line in generate_bazelrc(os.environ):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
