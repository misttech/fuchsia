#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""This script is used to set up a cog-based workspace for Fuchsia development.
It is currently highly experimental and not guaranteed to work.
"""

import os
import re
import shutil
import subprocess
import sys


def log_warn(message: str) -> None:
    """Prints a warning message."""
    print(f"WARNING: {message}")


def _workspace_base_path() -> str | None:
    """Returns the base path for the workspace."""
    user: str | None = os.environ.get("USER")
    if not user:
        return None
    return f"/google/cog/cloud/{user}"


def _find_cartfs_mount_point() -> str | None:
    """Finds the mount point for cartfs.

    Returns:
        The mount point if found, None otherwise.
    """
    try:
        cartfs_uid_process = subprocess.run(
            ["id", "-u", "cartfs"], capture_output=True, text=True, check=True
        )
        cartfs_uid = cartfs_uid_process.stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        log_warn("Could not determine UID for cartfs. Is cartfs running?")
        return None

    try:
        findmnt_process = subprocess.run(
            [
                "findmnt",
                "-n",
                "-t",
                "fuse",
                "-O",
                f"user_id={cartfs_uid}",
                "-o",
                "TARGET",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        output = findmnt_process.stdout.strip()
        if not output:
            return None
        # Return the first mount point found.
        return output.splitlines()[0]
    except (subprocess.CalledProcessError, FileNotFoundError):
        log_warn("findmnt command not found or failed to execute.")
        return None


def _is_cartfs_installed() -> bool:
    """Checks if cartfs is installed."""
    return shutil.which("cartfs") is not None


def find_cog_workspace_directory() -> str | None:
    """Finds the workspace directory from the current working directory.

    Returns:
        The workspace directory if found, None otherwise.
    """
    user: str | None = os.environ.get("USER")
    if not user:
        return None

    path_prefix = f"/google/cog/cloud/{re.escape(user)}"
    path_pattern = re.compile(f"^{path_prefix}/[^/]+$")

    current_dir = os.getcwd()
    while current_dir != "/":
        match = path_pattern.match(current_dir)
        if match:
            return match.group(0)
        else:
            current_dir = os.path.dirname(current_dir)
    return None


def main() -> None:
    """Main function to set up the cog workspace."""
    # This script is experimental and not ready for general use.
    # To enable, set the FUCHSIA_ALLOW_SETUP_COG_WORKSPACE environment variable.
    if not os.environ.get("FUCHSIA_ALLOW_SETUP_COG_WORKSPACE"):
        log_warn(
            "This script is highly experimental and not yet ready for use."
        )
        print(
            "To acknowledge this and proceed, please set the environment variable:"
        )
        print("  export FUCHSIA_ALLOW_SETUP_COG_WORKSPACE=1")
        sys.exit(1)

    workspace_dir = find_cog_workspace_directory()
    if not workspace_dir:
        log_warn(
            "Could not find workspace directory. Please run this script from a directory matching the pattern /google/cog/cloud/<user>/<workspace_name>."
        )
        sys.exit(1)

    print(f"Workspace dir: {workspace_dir}")

    cartfs_mount = _find_cartfs_mount_point()
    if cartfs_mount:
        print(f"Found cartfs mount point: {cartfs_mount}")
    else:
        log_warn("Could not find cartfs mount point.")
        if not _is_cartfs_installed():
            log_warn(
                "cartfs is not installed. Please follow instructions at go/cartfs to install."
            )
            sys.exit(1)
        else:
            log_warn(
                "cartfs is installed but not running. Please start cartfs to continue."
            )
            sys.exit(1)


if __name__ == "__main__":
    main()
