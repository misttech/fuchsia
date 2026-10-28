#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities and options generator for Bazel build invocations."""

from collections.abc import Iterable, Mapping
from pathlib import Path

# Bazel configuration names matching template.remote_services.bazelrc.
# LINT.IfChange(bazel_remote_services_configs)
CONFIG_GCERTAUTH = "gcertauth"
CONFIG_REMOTE_COMMON = "_remote_common"
CONFIG_REMOTE = "remote"
CONFIG_REMOTE_CACHE_ONLY = "remote_cache_only"
CONFIG_RESULTSTORE_COMMON = "_resultstore"
CONFIG_RESULTSTORE = "resultstore"
CONFIG_RESULTSTORE_INFRA = "resultstore_infra"
CONFIG_SPONGE_INFRA = "sponge_infra"
# LINT.ThenChange(//build/bazel/templates/template.remote_services.bazelrc:bazel_remote_services_configs)

VALID_RESULTSTORE_CONFIGS = frozenset(
    {CONFIG_RESULTSTORE, CONFIG_RESULTSTORE_INFRA}
)

# Sockets and environment variable names used across orchestrator and wrappers.
# Consumed in main_build.py, generate_invocation_bazelrc.py, wrapper.bazel.sh,
# and bazel_launcher.sh.
ENV_BAZEL_RBE_SOCKET = "FX_INTERNAL_BAZEL_RBE_SOCKET_PATH"
ENV_BAZEL_RESULTSTORE_SOCKET = "FX_INTERNAL_BAZEL_RESULTSTORE_SOCKET_PATH"

ENV_RESULTSTORE_BAZEL = "FX_INTERNAL_RESULTSTORE_BAZEL"


def socket_env_vars(
    rbe_socket: Path | str | None = None,
    resultstore_socket: Path | str | None = None,
) -> dict[str, str]:
    """Generates the environment variables mapping CLI proxy sockets."""
    env: dict[str, str] = {}
    if rbe_socket:
        env[ENV_BAZEL_RBE_SOCKET] = str(rbe_socket)
    if resultstore_socket:
        env[ENV_BAZEL_RESULTSTORE_SOCKET] = str(resultstore_socket)
    return env


def _enabled_bazel_resultstore(resultstore_mode: str) -> bool:
    """Returns True if ResultStore reporting is enabled for Bazel."""
    return resultstore_mode in ("all", "bazel")


def resultstore_bazel_config(
    resultstore_mode: str,
    resultstore_auth_type: str,
    in_infra: bool = False,
) -> str | None:
    """Determines the Bazel ResultStore configuration mode name.

    Args:
        resultstore_mode: ResultStore reporting scope. Expected values:
          - "all": Publish both Ninja and Bazel events.
          - "bazel": Publish Bazel events only.
          - "ninja": Publish Ninja events only.
          - "none": ResultStore reporting is disabled.
        resultstore_auth_type: Authentication mechanism. Expected values:
          - "loas": Google corp LOAS authentication via credential helper.
          - "oauth": Developer OAuth token (ADC / gcloud).
          - "machine": Machine-to-machine service credentials.
          - "none": No authentication.
        in_infra: True if executing within infrastructure (e.g. Buildbucket bot).

    Returns:
        One of:
          - "resultstore": Local developer mode using LOAS and interactive auth.
          - "resultstore_infra": Machine/infra mode using GCE VM credentials.
          - None: ResultStore reporting is disabled for Bazel (mapped to "0" by
            callers like main_build.py to satisfy downstream wrapper.bazel.sh and
            devshell test conventions).
    """
    if not _enabled_bazel_resultstore(resultstore_mode):
        return None
    if resultstore_auth_type == "loas" and not in_infra:
        return CONFIG_RESULTSTORE
    return CONFIG_RESULTSTORE_INFRA


def _needs_gcertauth(
    rbe_auth_type: str,
    resultstore_auth_type: str,
) -> bool:
    """Returns True if Bazel should use the LOAS gcertauth credential helper."""
    return rbe_auth_type == "loas" or resultstore_auth_type == "loas"


def build_config_option(config: str, option: str) -> str:
    """Formats a bazelrc command that maps an option into a config definition."""
    return f"build:{config} {option}"


def generate_service_proxies_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates bazelrc lines for remote execution and BES proxy sockets."""
    rbe_socket = env.get(ENV_BAZEL_RBE_SOCKET)
    if rbe_socket:
        yield build_config_option(
            CONFIG_REMOTE_COMMON, f"--remote_proxy=unix://{rbe_socket}"
        )

    rs_socket = env.get(ENV_BAZEL_RESULTSTORE_SOCKET)
    if rs_socket:
        yield build_config_option(
            CONFIG_RESULTSTORE_COMMON, f"--bes_proxy=unix://{rs_socket}"
        )


def generate_auth_bazelrc(
    rbe_auth_type: str,
    resultstore_auth_type: str,
) -> Iterable[str]:
    """Generates bazelrc lines for authentication.

    Args:
        rbe_auth_type: Remote build execution auth mode ("loas", "oauth",
          "machine", or "none").
        resultstore_auth_type: ResultStore auth mode ("loas", "oauth",
          "machine", or "none").
    """
    if _needs_gcertauth(rbe_auth_type, resultstore_auth_type):
        yield build_config_option(
            CONFIG_REMOTE_COMMON, f"--config={CONFIG_GCERTAUTH}"
        )
