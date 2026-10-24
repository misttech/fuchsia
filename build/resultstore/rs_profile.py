#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""ResultStore profile dataclass, loaders, and subcommand CLI."""

import dataclasses
import os
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from build.python.modules.serialization import serialization
from build.scripts import service_profile

# Environment variables used for serialization and child process communication.
# RS_* variables directly influence rsclient's rsproxy tool (names cannot be changed).
_ENV_RS_INSTANCE = "RS_rs_instance"
_ENV_CAS_INSTANCE = "RS_cas_instance"
_ENV_RESULTS_URL = "RS_results_url"

# FX_BUILD_INTERNAL_* variables are internal to Fuchsia build orchestration scripts.
_ENV_AUTH_TYPE = "FX_BUILD_INTERNAL_RS_AUTH_TYPE"
_ENV_PROFILE_NAME = "FX_BUILD_RS_PROFILE"

# Path relative to fuchsia directory for ResultStore profiles directory
_PROFILES_DIR = Path("build/resultstore/profiles")

# Path relative to fuchsia directory for the active sticky ResultStore profile JSON config.
_CONFIG_PATH = Path(".fx/config/rs_profile.json")

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]


@dataclasses.dataclass
class RsProfile:
    """Represents a validated ResultStore profile configuration."""

    rs_instance: str
    cas_instance: str
    results_url: str
    profile_name: str
    auth_type: str = "google"

    def __post_init__(self) -> None:
        self.results_url = self.results_url.rstrip("/")

    @property
    def rs_project(self) -> str:
        """Extracts the project ID from rs_instance, or empty string if not found."""
        if self.rs_instance.startswith("projects/"):
            parts = self.rs_instance.split("/")
            if len(parts) >= 2:
                return parts[1]
        return ""

    def to_env_dict(self) -> dict[str, str]:
        """Serializes properties to standard environment variables for subprocesses."""
        return {
            _ENV_RS_INSTANCE: self.rs_instance,
            _ENV_CAS_INSTANCE: self.cas_instance,
            _ENV_RESULTS_URL: self.results_url,
            _ENV_AUTH_TYPE: self.auth_type,
            _ENV_PROFILE_NAME: self.profile_name,
        }

    @classmethod
    def from_dict(cls, data: JSONObject) -> "RsProfile":
        """Creates an RsProfile instance from a raw dictionary."""
        try:
            return serialization.instance_from_dict(cls, data)
        except (KeyError, TypeError) as e:
            raise ValueError(f"Validation failed when parsing RsProfile: {e}.")

    @classmethod
    def from_env_dict(cls, environ: Mapping[str, str]) -> "RsProfile":
        """Reconstructs an RsProfile instance from environment variables."""
        if _ENV_RS_INSTANCE not in environ:
            raise ValueError(
                f"{_ENV_RS_INSTANCE} must be present in environment"
            )
        rs_instance = environ[_ENV_RS_INSTANCE]
        cas_instance = environ.get(_ENV_CAS_INSTANCE, "")
        results_url = environ.get(_ENV_RESULTS_URL, "")
        if not cas_instance:
            raise ValueError(
                f"{_ENV_CAS_INSTANCE} must be present in environment"
            )
        if not results_url:
            raise ValueError(
                f"{_ENV_RESULTS_URL} must be present in environment"
            )
        auth_type = environ.get(_ENV_AUTH_TYPE, "google")
        profile_name = environ.get(_ENV_PROFILE_NAME, "default")
        return cls(
            rs_instance=rs_instance,
            cas_instance=cas_instance,
            results_url=results_url,
            profile_name=profile_name,
            auth_type=auth_type,
        )


def load(
    fuchsia_dir: Path,
    environ: Mapping[str, str],
    name: str | None = None,
) -> RsProfile:
    """Loads the active ResultStore profile, prioritizing env variables over on-disk files."""
    if _ENV_RS_INSTANCE in environ and name is None:
        return RsProfile.from_env_dict(environ)

    try:
        return _MANAGER.load(fuchsia_dir, environ, name=name)
    except FileNotFoundError as e:
        raise ValueError(f"Unknown ResultStore profile '{name}': {e}")


def fake(**overrides: Any) -> RsProfile:
    """Factory helper generating valid in-memory RsProfile instances for tests."""
    defaults: dict[str, Any] = {
        "rs_instance": "projects/fake-project/instances/fake-rs",
        "cas_instance": "projects/fake-project/instances/fake-cas",
        "results_url": "http://fake-results-url",
        "profile_name": "fake-profile",
        "auth_type": "fake-auth",
    }
    valid_keys = {
        "rs_instance",
        "cas_instance",
        "results_url",
        "profile_name",
        "auth_type",
    }
    for k in overrides:
        if k not in valid_keys:
            raise ValueError(
                f"Unknown override '{k}'. Expected one of {valid_keys}"
            )
    defaults.update(overrides)
    return RsProfile(**defaults)


_SPEC = service_profile.ServiceProfileSpec(
    service_name="resultstore",
    env_profile_var=_ENV_PROFILE_NAME,
    profiles_relpath=_PROFILES_DIR,
    config_relpath=_CONFIG_PATH,
    description="Manage ResultStore configuration profiles.",
)

_MANAGER = service_profile.ServiceProfileManager(RsProfile, _SPEC)

resolve_paths = _MANAGER.resolve_paths
main = _MANAGER.main


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
