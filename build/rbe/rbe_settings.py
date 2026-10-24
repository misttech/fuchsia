# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""RBE settings model and loader."""

import dataclasses
import json
from pathlib import Path
from typing import Any

from build.python.modules.serialization import serialization


# LINT.IfChange(RbeSettings)
@dataclasses.dataclass
class RbeSettings:
    """Strongly-typed representation of RBE and ResultStore build settings.

    All fields are strictly required and synchronized with build/rbe/BUILD.gn.
    """

    bazel_enable: bool
    bazel_exec_strategy: str
    bazel_download_outputs: str
    cxx_download_objects: bool
    cxx_enable: bool
    cxx_exec_strategy: str
    cxx_minimalist_wrapper: bool
    link_download_unstripped_outputs: bool
    link_enable: bool
    link_exec_strategy: str
    rust_download_rlibs: bool
    rust_download_unstripped_binaries: bool
    rust_enable: bool
    rust_exec_strategy: str
    needs_reproxy: bool
    needs_auth: bool
    # LINT.ThenChange(//build/rbe/BUILD.gn:RbeSettings)

    @property
    def rbe_enabled(self) -> bool:
        """True if remote execution is active for any compiler or build engine."""
        return any(
            (
                self.cxx_enable,
                self.rust_enable,
                self.link_enable,
                self.bazel_enable,
            )
        )

    @property
    def rs_enabled(self) -> bool:
        """True if ResultStore/BES event uploading is active."""
        return self.rbe_enabled or self.needs_auth

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "RbeSettings":
        """Creates an RbeSettings instance from a raw dictionary."""
        try:
            return serialization.instance_from_dict(cls, data)
        except (KeyError, TypeError) as e:
            raise KeyError(
                f"Validation failed when parsing RbeSettings: {e}. "
                "This structure must remain strictly synchronized with build/rbe/BUILD.gn."
            )


_SETTINGS_FILE = "rbe_settings.json"


def _load_from_path(rbe_settings_path: Path) -> RbeSettings:
    """Loads and parses RbeSettings from a specific JSON path."""
    try:
        profile_data = json.loads(rbe_settings_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        raise ValueError(
            f"Failed to read/parse RBE settings at {rbe_settings_path}: {e}"
        )
    return RbeSettings.from_dict(profile_data.get("final", {}))


def load(build_dir: Path) -> RbeSettings:
    """Loads and parses RbeSettings from the standard RBE settings file in a build directory."""
    return _load_from_path(build_dir / _SETTINGS_FILE)


def exists(build_dir: Path) -> bool:
    """Returns True if the RBE settings file exists in the specified build directory."""
    return (build_dir / _SETTINGS_FILE).is_file()


def fake(**overrides: Any) -> RbeSettings:
    """Returns a valid in-memory RbeSettings instance for unit tests."""
    defaults: dict[str, Any] = {
        "bazel_enable": False,
        "bazel_exec_strategy": "",
        "bazel_download_outputs": "all",
        "cxx_download_objects": True,
        "cxx_enable": False,
        "cxx_exec_strategy": "",
        "cxx_minimalist_wrapper": True,
        "link_download_unstripped_outputs": True,
        "link_enable": False,
        "link_exec_strategy": "",
        "rust_download_rlibs": True,
        "rust_download_unstripped_binaries": True,
        "rust_enable": False,
        "rust_exec_strategy": "",
        "needs_reproxy": False,
        "needs_auth": False,
    }
    unknown_keys = set(overrides) - set(defaults)
    if unknown_keys:
        raise ValueError(
            f"Unknown RbeSettings field(s) in overrides: {sorted(unknown_keys)}"
        )
    defaults.update(overrides)
    return RbeSettings.from_dict(defaults)
