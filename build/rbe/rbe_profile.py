#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""RBE profile dataclass, loaders, and subcommand CLI."""

import dataclasses
import json
import os
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

# Ensure the fuchsia root is in sys.path so we can import build python modules
_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.python.modules.serialization import serialization
from build.scripts import service_profile

# Environment variables used for serialization and child process communication.
# RBE_instance is read directly by reclient's reproxy tool to select the RBE endpoint.
_ENV_RBE_INSTANCE = "RBE_instance"
_ENV_EXEC_ROOT = "FX_BUILD_INTERNAL_RBE_REMOTE_EXEC_ROOT"
_ENV_WORKER_POOLS = "FX_BUILD_INTERNAL_RBE_WORKER_POOLS"
_ENV_AUTH_TYPE = "FX_BUILD_INTERNAL_RBE_AUTH_TYPE"
_ENV_PROFILE_NAME = "FX_BUILD_RBE_PROFILE"

# Path relative to fuchsia directory for RBE profiles directory
_PROFILES_DIR = Path("build/rbe/profiles")

# Path relative to fuchsia directory for the active sticky RBE profile JSON config.
_CONFIG_PATH = Path(".fx/config/rbe_profile.json")

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]


@dataclasses.dataclass
class RbeProfile:
    """Represents a validated Remote Build Execution (RBE) profile configuration.

    Attributes:
        rbe_instance: The fully-qualified RBE instance name. Must match the
            standard format: `projects/{project_id}/instances/{instance_name}`
            (e.g., `projects/rbe-fuchsia-prod/instances/default`).
        remote_exec_root: The root path of the execution workspace on remote
            workers (typically `/b/f/w`).
        worker_pools: A dictionary mapping abstract worker pool names (e.g.,
            "default", "large") to the concrete RBE platform pool constraints
            (e.g., "Pool=default", "Pool=large_worker") used by this instance.
        auth_type: Authentication mechanism ("google" or "none").
    """

    rbe_instance: str
    remote_exec_root: Path
    worker_pools: dict[str, str]
    profile_name: str
    auth_type: str = "google"

    def to_env_dict(self) -> dict[str, str]:
        """Serializes properties to standard environment variables for subprocesses."""
        return {
            _ENV_RBE_INSTANCE: self.rbe_instance,
            _ENV_EXEC_ROOT: str(self.remote_exec_root),
            _ENV_WORKER_POOLS: json.dumps(self.worker_pools),
            _ENV_AUTH_TYPE: self.auth_type,
            _ENV_PROFILE_NAME: self.profile_name,
        }

    @classmethod
    def from_dict(cls, data: JSONObject) -> "RbeProfile":
        """Creates an RbeProfile instance from a raw dictionary."""
        try:
            return serialization.instance_from_dict(cls, data)
        except (KeyError, TypeError) as e:
            raise ValueError(f"Validation failed when parsing RbeProfile: {e}.")


def load(
    fuchsia_dir: Path,
    environ: Mapping[str, str],
    name: str | None = None,
) -> RbeProfile:
    """Loads the active RBE profile, prioritizing env variables over on-disk files."""
    if _ENV_RBE_INSTANCE in environ and name is None:
        # Fast path for subprocesses: reconstruct profile directly from environment variables without disk I/O.
        rbe_instance = environ[_ENV_RBE_INSTANCE]
        exec_root_str = environ.get(_ENV_EXEC_ROOT)
        if not exec_root_str or not exec_root_str.startswith("/"):
            raise ValueError(
                f"{_ENV_EXEC_ROOT} must be an absolute path in environment, got '{exec_root_str}'"
            )
        worker_pools_raw = environ.get(_ENV_WORKER_POOLS, "{}")
        try:
            worker_pools = json.loads(worker_pools_raw)
        except json.JSONDecodeError as e:
            raise ValueError(
                f"Failed to decode worker pools JSON from environment: {e}"
            )
        auth_type = environ.get(_ENV_AUTH_TYPE, "google")
        profile_name = environ.get(_ENV_PROFILE_NAME, "default")
        return RbeProfile(
            rbe_instance=rbe_instance,
            remote_exec_root=Path(exec_root_str),
            worker_pools=worker_pools,
            profile_name=profile_name,
            auth_type=auth_type,
        )

    try:
        return _MANAGER.load(fuchsia_dir, environ, name=name)
    except FileNotFoundError as e:
        raise ValueError(f"Unknown RBE profile '{name}': {e}")


def fake(**overrides: Any) -> RbeProfile:
    """Factory helper generating valid in-memory RbeProfile instances for tests."""
    defaults: dict[str, Any] = {
        "rbe_instance": "projects/fake-project/instances/fake-instance",
        "remote_exec_root": Path("/fake/root"),
        "worker_pools": {"default": "Pool=default"},
        "profile_name": "fake-profile",
        "auth_type": "fake-auth",
    }
    valid_keys = {
        "rbe_instance",
        "remote_exec_root",
        "worker_pools",
        "profile_name",
        "auth_type",
    }
    for k in overrides:
        if k not in valid_keys:
            raise ValueError(
                f"Unknown override '{k}'. Expected one of {valid_keys}"
            )
    defaults.update(overrides)
    if isinstance(defaults["remote_exec_root"], str):
        defaults["remote_exec_root"] = Path(defaults["remote_exec_root"])
    return RbeProfile(**defaults)


_SPEC = service_profile.ServiceProfileSpec(
    service_name="rbe",
    env_profile_var=_ENV_PROFILE_NAME,
    profiles_relpath=_PROFILES_DIR,
    config_relpath=_CONFIG_PATH,
    description="Manage RBE (Remote Build Execution) configuration profiles.",
)

_MANAGER = service_profile.ServiceProfileManager(RbeProfile, _SPEC)

resolve_paths = _MANAGER.resolve_paths
main = _MANAGER.main


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
