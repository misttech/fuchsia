# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Filesystem paths and directory discovery for AI coding agents."""

from __future__ import annotations

import os
import pathlib

__all__ = [
    "find_checkout_git_repos",
    "find_config_dirs",
    "find_fuchsia_dir",
    "find_permission_dirs",
]


def _search_upwards_for_root(path: pathlib.Path) -> pathlib.Path | None:
    current = path.resolve()
    while True:
        if (current / ".jiri_root").is_dir() or (
            current / ".fx-root"
        ).is_file():
            return current
        if current == current.parent:
            return None
        current = current.parent


def find_fuchsia_dir(start_dir: pathlib.Path | None = None) -> pathlib.Path:
    """Locate the Fuchsia source root directory."""
    if start_dir is not None:
        root = _search_upwards_for_root(start_dir)
        if root is not None:
            return root
        raise RuntimeError(
            f"Could not locate Fuchsia root directory from start directory: {start_dir}"
        )

    env_dir = os.environ.get("FUCHSIA_DIR")
    if env_dir:
        candidate = pathlib.Path(env_dir).resolve()
        if candidate.is_dir() and (
            (candidate / ".jiri_root").is_dir()
            or (candidate / ".fx-root").is_file()
        ):
            return candidate

    root = _search_upwards_for_root(pathlib.Path(__file__).parent)
    if root is not None:
        return root

    raise RuntimeError(
        "Could not locate Fuchsia root directory. Run within a Fuchsia source checkout or set FUCHSIA_DIR."
    )


def find_checkout_git_repos(fuchsia_dir: pathlib.Path) -> list[pathlib.Path]:
    """Find all active Git repositories within the checkout (root, vendor/*, and integration)."""
    repos: list[pathlib.Path] = []
    if (fuchsia_dir / ".git").exists():
        repos.append(fuchsia_dir)

    vendor_dir = fuchsia_dir / "vendor"
    if vendor_dir.is_dir():
        for vendor_child in sorted(vendor_dir.iterdir()):
            if vendor_child.is_dir() and (vendor_child / ".git").exists():
                repos.append(vendor_child)

    integration_dir = fuchsia_dir / "integration"
    if (integration_dir / ".git").exists():
        repos.append(integration_dir)

    return repos


def find_config_dirs(fuchsia_dir: pathlib.Path) -> list[pathlib.Path]:
    """Find all agent config directories (public root + vendor extensions)."""
    candidates: list[pathlib.Path] = []
    root_cfg = fuchsia_dir / ".agents" / "config"
    if root_cfg.is_dir():
        candidates.append(root_cfg)
    vendor_dir = fuchsia_dir / "vendor"
    # Note: Assumes standard single-level vendor layout (vendor/<name>/.agents/config).
    # If nested vendor repositories are introduced in the future, recursive search or manifest
    # discovery can be considered.
    if vendor_dir.is_dir():
        for vendor_child in sorted(vendor_dir.iterdir()):
            if vendor_child.is_dir():
                cfg_dir = vendor_child / ".agents" / "config"
                if cfg_dir.is_dir():
                    candidates.append(cfg_dir)
    return candidates


def find_permission_dirs(fuchsia_dir: pathlib.Path) -> list[pathlib.Path]:
    """Find all permission config directories (public root + vendor extensions)."""
    candidates: list[pathlib.Path] = []
    for cfg_dir in find_config_dirs(fuchsia_dir):
        perm_dir = cfg_dir / "permissions"
        if perm_dir.is_dir():
            candidates.append(perm_dir)
    return candidates
