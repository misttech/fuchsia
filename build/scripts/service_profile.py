#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generic, schema-agnostic service profile management.

This module provides a reusable, protocol-based framework for managing build service
configuration profiles (e.g. Remote Build Execution, ResultStore, Siso).

A service profile system allows developers and automation to switch between different
infrastructure environments (e.g., production, development, untrusted sandbox)
without code modifications or manual environment juggling.

Architecture & Concepts:
-------------------------
1. ServiceProfile (Protocol):
   A data class representing a validated configuration for a specific build service.
   It must implement `from_dict()` for deserialization and `to_env_dict()` for
   serializing its parameters to environment variables for child processes.

2. ServiceProfileSpec:
   An immutable configuration specification defining filesystem paths, environment
   variable names, and CLI descriptions for a given service.

3. ServiceProfileManager:
   The orchestration engine that resolves active profile paths, loads and parses
   configurations, persists developer profile selections into a single combined
   JSON file in `.fx/config/`, and provides standard CLI subcommands (`list`,
   `select`, `active`, `run`).

Resolution Precedence:
----------------------
When resolving the active profile, `ServiceProfileManager` checks sources in this order:
  1. Environment Variable Override (`FX_BUILD_<SERVICE>_PROFILE`):
     Highest precedence, allowing per-command or CI job overrides.
  2. Single Active On-Disk Configuration (`.fx/config/<service>_profile.json`):
     Set persistently by running `fx <service> profile select <name>`.
     Contains both `"profile_name"` and the full copied profile dictionary.
  3. Default Source Tree Profile (`build/<service>/profiles/default.json`):
     Fallback when no sticky on-disk profile has been selected yet.

Example Usage:
--------------
```python
import dataclasses
from pathlib import Path
from build.scripts import service_profile

# 1. Define the service-specific profile schema satisfying ServiceProfile
@dataclasses.dataclass
class SisoProfile:
    siso_project: str
    auth_type: str = "google"

    @classmethod
    def from_dict(cls, data: service_profile.JSONObject) -> "SisoProfile":
        return cls(
            siso_project=str(data["siso_project"]),
            auth_type=str(data.get("auth_type", "google")),
        )

    def to_env_dict(self) -> dict[str, str]:
        return {
            "SISO_PROJECT": self.siso_project,
            "SISO_AUTH": self.auth_type,
        }

# 2. Define the static filesystem and environment specification
_SPEC = service_profile.ServiceProfileSpec(
    service_name="siso",
    env_profile_var="FX_BUILD_SISO_PROFILE",
    profiles_relpath=Path("build/siso/profiles"),
    config_relpath=Path(".fx/config/siso_profile.json"),
    description="Manage Siso configuration profiles.",
)

# 3. Instantiate the manager and export public interfaces
_MANAGER = service_profile.ServiceProfileManager(SisoProfile, _SPEC)

ProfilePaths = service_profile.ProfilePaths
resolve_paths = _MANAGER.resolve_paths
select = _MANAGER.select
load = _MANAGER.load
main = _MANAGER.main
```
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import sys
import typing
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Generic, TypeVar


class ServiceProfile(typing.Protocol):
    """Protocol defining the structural interface expected of service profile instances.

    Any profile class managed by `ServiceProfileManager` must implement both
    deserialization (`from_dict`) and subprocess environment serialization (`to_env_dict`).
    No inheritance is required; compliance is verified structurally via duck typing.
    """

    @classmethod
    def from_dict(cls, data: JSONObject) -> typing.Self:
        """Deserializes a profile instance from a raw JSON dictionary.

        Args:
            data: Raw dictionary loaded from a profile JSON file.

        Returns:
            A validated instance of the profile class.

        Raises:
            ValueError: If required fields are missing or values are malformed.
        """
        ...

    def to_env_dict(self) -> dict[str, str]:
        """Serializes profile properties to standard environment variables.

        Used by `run` subcommands and child build tools to inject service parameters
        into subprocess execution environments.

        Returns:
            Dictionary mapping environment variable names to string values.
        """
        ...


T = TypeVar("T", bound=ServiceProfile)

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]

DEFAULT_PROFILE_NAME = "default"
PROFILE_NAME_KEY = "profile_name"


@dataclasses.dataclass(frozen=True)
class ServiceProfileSpec:
    """Static configuration and metadata defining a service's profile system.

    Attributes:
        service_name: Canonical identifier for the service (e.g. "rbe", "resultstore").
            Used in help messages and user-facing CLI status text.
        env_profile_var: Environment variable name used for dynamic profile name overrides
            (e.g., "FX_BUILD_RBE_PROFILE", "FX_BUILD_RS_PROFILE").
        profiles_relpath: Path relative to the Fuchsia checkout root where available profile
            template JSON files reside (e.g., `Path("build/rbe/profiles")`).
        config_relpath: Path relative to the Fuchsia checkout root where the active selected
            profile JSON is stored (e.g., `Path(".fx/config/rbe_profile.json")`).
        description: Description of the service profile tool displayed in CLI help output.
    """

    service_name: str
    env_profile_var: str
    profiles_relpath: Path
    config_relpath: Path
    description: str = ""


@dataclasses.dataclass
class ProfilePaths:
    """Encapsulates the resolved on-disk paths and metadata for an active profile.

    Attributes:
        active_name: The resolved active profile name (e.g., "default", "untrusted").
        active_json: Absolute or workspace-relative path to the active profile JSON file.
        available_json: Sorted list of all available profile template files in the
            service's profiles directory.
    """

    active_name: str
    active_json: Path
    available_json: list[Path]

    @property
    def active_inputs(self) -> list[Path]:
        """Returns the on-disk configuration files that define the active profile.

        Suitable for build system dependency tracking (e.g., Ninja or Bazel inputs).
        Because the active profile is stored in a single combined JSON file, this
        always contains exactly one file path.
        """
        return [self.active_json]


class ServiceProfileManager(Generic[T]):
    """Protocol-based, schema-agnostic manager for service configuration profiles.

    Coordinates path resolution, template instantiation, single-file `.fx/config/`
    persistence, and CLI subcommands.

    Generic Parameter:
        T: The profile data class, bounded by `ServiceProfile`. Inferred automatically
           from the `profile_cls` constructor argument.
    """

    def __init__(
        self,
        profile_cls: type[T],
        spec: ServiceProfileSpec,
    ) -> None:
        """Initializes a new ServiceProfileManager.

        Args:
            profile_cls: Concrete class implementing `ServiceProfile`.
            spec: Immutable specification defining paths and metadata.
        """
        self.profile_cls = profile_cls
        self.spec = spec

    def resolve_paths(
        self, fuchsia_dir: Path, environ: Mapping[str, str]
    ) -> ProfilePaths:
        """Resolves the active profile name, active JSON path, and available profiles.

        Follows standard precedence:
          1. Environment variable override (`self.spec.env_profile_var`).
          2. Single active on-disk configuration file (`self.spec.config_relpath`).
          3. Fallback default profile template (`self.spec.profiles_relpath / 'default.json'`).

        Args:
            fuchsia_dir: Root directory of the Fuchsia source checkout.
            environ: Current execution environment dictionary.

        Returns:
            A `ProfilePaths` instance containing resolved paths and available templates.
        """
        profiles_dir = fuchsia_dir / self.spec.profiles_relpath
        config_path = fuchsia_dir / self.spec.config_relpath

        # LINT.IfChange(service_profile_resolution)
        # 1. Environment variable override has highest precedence
        env_override = environ.get(self.spec.env_profile_var)
        if env_override:
            profile_file = profiles_dir / f"{env_override}.json"
            if not profile_file.is_file():
                raise FileNotFoundError(
                    f"Profile '{env_override}' specified by {self.spec.env_profile_var} does not exist at {profile_file}"
                )
            active_json = profile_file
            active_name = env_override
        # 2. Single active on-disk configuration file
        elif config_path.is_file():
            try:
                data = json.loads(config_path.read_text(encoding="utf-8"))
                active_name = (
                    data.get(PROFILE_NAME_KEY, DEFAULT_PROFILE_NAME)
                    if isinstance(data, dict)
                    else DEFAULT_PROFILE_NAME
                )
            except (json.JSONDecodeError, OSError):
                active_name = DEFAULT_PROFILE_NAME
            active_json = config_path
        # 3. Fallback to default profile template in source tree
        else:
            active_name = DEFAULT_PROFILE_NAME
            active_json = profiles_dir / f"{DEFAULT_PROFILE_NAME}.json"
        # LINT.ThenChange(//build/bazel_sdk/bazel_rules_fuchsia/common/fuchsia_platform_build.bzl:service_profile_resolution)

        available_json: list[Path] = []
        if profiles_dir.is_dir():
            available_json = sorted(profiles_dir.glob("*.json"))

        return ProfilePaths(
            active_name=active_name,
            active_json=active_json,
            available_json=available_json,
        )

    def read_active_name(
        self, fuchsia_dir: Path, environ: Mapping[str, str]
    ) -> str:
        """Returns the active profile name."""
        return self.resolve_paths(fuchsia_dir, environ).active_name

    def select(self, fuchsia_dir: Path, name: str) -> None:
        """Selects a profile by copying its template and recording the name into the single JSON config.

        Args:
            fuchsia_dir: Root directory of the Fuchsia source checkout.
            name: Profile name to select (must exist as `<name>.json` in profiles directory).

        Raises:
            FileNotFoundError: If the requested profile template does not exist.
            ValueError: If the profile template contains invalid JSON.
        """
        template_path = (
            fuchsia_dir / self.spec.profiles_relpath / f"{name}.json"
        )
        if not template_path.is_file():
            raise FileNotFoundError(
                f"Profile '{name}' does not exist at {template_path}"
            )

        try:
            data = json.loads(template_path.read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                raise ValueError(
                    f"Profile '{name}' must contain a JSON dictionary object."
                )
        except json.JSONDecodeError as e:
            raise ValueError(
                f"Failed to parse profile JSON at {template_path}: {e}"
            )

        # Record the profile name directly into the single config file
        data[PROFILE_NAME_KEY] = name

        config_path = fuchsia_dir / self.spec.config_relpath
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(json.dumps(data, indent=2) + "\n")

    def load(
        self,
        fuchsia_dir: Path,
        environ: Mapping[str, str],
        name: str | None = None,
    ) -> T:
        """Loads and parses the active profile or an explicitly named profile template.

        Args:
            fuchsia_dir: Root directory of the Fuchsia source checkout.
            environ: Current execution environment dictionary.
            name: Optional explicit profile name. If provided, loads `<name>.json`
                from the profiles template directory directly. Otherwise, loads
                the resolved active profile JSON.

        Returns:
            A validated instance of `self.profile_cls`.

        Raises:
            FileNotFoundError: If the target profile file does not exist.
            ValueError: If profile parsing fails or required fields are invalid.
        """
        if name:
            template_path = (
                fuchsia_dir / self.spec.profiles_relpath / f"{name}.json"
            )
            if not template_path.is_file():
                raise FileNotFoundError(
                    f"Profile '{name}' does not exist at {template_path}"
                )
            target_path = template_path
        else:
            paths = self.resolve_paths(fuchsia_dir, environ)
            target_path = paths.active_json
            if not target_path.is_file():
                raise FileNotFoundError(
                    f"Profile file does not exist at {target_path}. "
                    f"Run '{self.spec.service_name} profile select default' to initialize."
                )

        try:
            data = json.loads(target_path.read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                raise ValueError("Profile content must be a JSON object.")
            if PROFILE_NAME_KEY not in data:
                data[PROFILE_NAME_KEY] = target_path.stem
        except json.JSONDecodeError as e:
            raise ValueError(
                f"Failed to read/parse {self.spec.service_name} profile at {target_path}: {e}"
            )

        return self.profile_cls.from_dict(data)

    def _cmd_list(
        self, args: argparse.Namespace, environ: Mapping[str, str]
    ) -> int:
        paths = self.resolve_paths(args.fuchsia_dir, environ)
        for p in paths.available_json:
            print(p.stem)
            if args.verbose:
                content = p.read_text(encoding="utf-8").strip()
                for line in content.splitlines():
                    print(f"  {line}")
        return 0

    def _cmd_select(
        self, args: argparse.Namespace, environ: Mapping[str, str]
    ) -> int:
        if not args.name or "/" in args.name or "\\" in args.name:
            print(
                f"Error: Invalid profile name '{args.name}'.", file=sys.stderr
            )
            return 1

        display_name = (
            "ResultStore"
            if self.spec.service_name == "resultstore"
            else self.spec.service_name.upper()
        )

        try:
            self.select(args.fuchsia_dir, args.name)
        except FileNotFoundError:
            print(
                f"Error: Unknown {display_name} profile '{args.name}'.",
                file=sys.stderr,
            )
            return 1
        except ValueError as e:
            print(
                f"Error: Selected {display_name} profile '{args.name}' is invalid: {e}",
                file=sys.stderr,
            )
            return 1

        print(f"{display_name} profile set to: {args.name}")
        return 0

    def _cmd_active(
        self, args: argparse.Namespace, environ: Mapping[str, str]
    ) -> int:
        paths = self.resolve_paths(args.fuchsia_dir, environ)
        print(paths.active_name)
        if args.verbose and paths.active_json.is_file():
            content = paths.active_json.read_text(encoding="utf-8").strip()
            for line in content.splitlines():
                print(f"  {line}")
        return 0

    def _cmd_run(
        self, args: argparse.Namespace, environ: Mapping[str, str]
    ) -> int:
        prof = self.load(args.fuchsia_dir, environ, name=args.profile)
        new_env = dict(environ)
        new_env.update(prof.to_env_dict())

        cmd = args.cmd_args
        if cmd and cmd[0] == "--":
            cmd = cmd[1:]

        if not cmd:
            print("Error: No command specified to run.", file=sys.stderr)
            return 1

        os.execvpe(cmd[0], cmd, new_env)
        return 0

    def _main_argparser(
        self, default_fuchsia_dir: Path | None = None
    ) -> argparse.ArgumentParser:
        """Constructs and returns the argument parser for the CLI."""
        description = (
            self.spec.description
            or f"Manage {self.spec.service_name} configuration profiles."
        )
        parser = argparse.ArgumentParser(description=description)
        if default_fuchsia_dir is None:
            default_fuchsia_dir = Path(__file__).resolve().parent.parent.parent
        parser.add_argument(
            "--fuchsia-dir",
            type=Path,
            default=default_fuchsia_dir,
            help="Fuchsia checkout root.",
        )

        subparsers = parser.add_subparsers(dest="subcommand", required=False)

        # list
        parser_list = subparsers.add_parser(
            "list", help=f"List available {self.spec.service_name} profiles."
        )
        parser_list.add_argument(
            "-v",
            "--verbose",
            action="store_true",
            help="Show detailed configuration for each profile.",
        )
        parser_list.set_defaults(func=self._cmd_list)

        # select
        parser_select = subparsers.add_parser(
            "select",
            help=f"Select a {self.spec.service_name} profile as the active profile.",
        )
        parser_select.add_argument(
            "name",
            type=str,
            help="Profile name to select.",
        )
        parser_select.set_defaults(func=self._cmd_select)

        # active
        parser_active = subparsers.add_parser(
            "active",
            help=f"Print the active {self.spec.service_name} profile name.",
        )
        parser_active.add_argument(
            "-v",
            "--verbose",
            action="store_true",
            help="Show detailed configuration of the active profile.",
        )
        parser_active.set_defaults(func=self._cmd_active)

        # run
        parser_run = subparsers.add_parser(
            "run",
            help=f"Run a command under the context of a {self.spec.service_name} profile.",
        )
        parser_run.add_argument(
            "-p",
            "--profile",
            type=str,
            help="Profile name to load context from (falls back to active profile).",
        )
        parser_run.add_argument(
            "cmd_args",
            nargs=argparse.REMAINDER,
            help="Command and arguments to execute.",
        )
        parser_run.set_defaults(func=self._cmd_run)

        return parser

    def main(self, argv: Sequence[str], environ: Mapping[str, str]) -> int:
        """Standard CLI entry point implementing list, select, active, run subcommands."""
        default_fuchsia_dir = (
            Path(environ["FUCHSIA_DIR"]) if "FUCHSIA_DIR" in environ else None
        )
        parser = self._main_argparser(default_fuchsia_dir=default_fuchsia_dir)
        args = parser.parse_args(argv)
        if not args.subcommand:
            args.verbose = False
            return self._cmd_active(args, environ)

        return args.func(args, environ)
