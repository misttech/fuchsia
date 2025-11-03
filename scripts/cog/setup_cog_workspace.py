#!/usr/bin/env -S python3 -B
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

import prebuilts
import snapshotter


def log_warn(message: str) -> None:
    """Prints a warning message."""
    print(f"WARNING: {message}")


def log_info(message: str) -> None:
    """Prints an information message."""
    print(f"INFO: {message}")


def get_workspace_name(workspace_dir: str) -> str:
    """Returns the workspace name from the workspace directory.

    Note: This method assumes that the workspace directory is a direct child of
    the cog workspace directory, e.g. /google/cog/cloud/<user>/<workspace_name>.
    It is up to the caller to ensure this is the case.

    Args:
        workspace_dir: The workspace directory.

    Returns:
        The workspace name.
    """
    return os.path.basename(workspace_dir)


def _ensure_cartfs_workspace_directory(workspace_name: str) -> str | None:
    """Ensures the cartfs workspace directory exists."""
    cartfs_mount = _find_cartfs_mount_point()
    if not cartfs_mount:
        log_warn("Could not find cartfs mount point.")
        return None

    cartfs_workspace_dir = os.path.join(cartfs_mount, workspace_name)
    if not os.path.exists(cartfs_workspace_dir):
        log_info(f"Creating cartfs workspace directory: {cartfs_workspace_dir}")
        os.makedirs(cartfs_workspace_dir)

    return cartfs_workspace_dir


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


def create_prebuilt_symlink(
    cartfs_mount_point: str, workspace_prebuilt_path: str, workspace_name: str
) -> None:
    """Creates a symlink to the cartfs prebuilt directory from the workspace.

    This method will check to see if a directory exists in the cartfs mountpoint
    at <mount_point>/<workspace_name>/prebuilt and create it if not. It will
    then create a symlink from the workspace/prebuilt to the newly created
    directory. If the symlink already exists it should check to see if it is
    still valid and if not it will repair the broken link.
    """
    cartfs_prebuilt_dir = os.path.join(
        cartfs_mount_point, workspace_name, "prebuilt"
    )

    # Create the target directory if it doesn't exist.
    os.makedirs(cartfs_prebuilt_dir, exist_ok=True)

    if os.path.islink(workspace_prebuilt_path):
        # Symlink exists. Check if it's correct and not broken.
        if os.readlink(
            workspace_prebuilt_path
        ) == cartfs_prebuilt_dir and os.path.exists(workspace_prebuilt_path):
            print(f"Symlink {workspace_prebuilt_path} is already correct.")
            return

        log_warn(
            f"Symlink at {workspace_prebuilt_path} is incorrect or broken. Repairing."
        )
        os.remove(workspace_prebuilt_path)

    elif os.path.exists(workspace_prebuilt_path):
        log_warn(
            f"{workspace_prebuilt_path} exists and is not a symlink. "
            "Cannot create prebuilt symlink."
        )
        return

    # Create the symlink if it doesn't exist or was removed.
    print(
        f"Creating symlink from {workspace_prebuilt_path} to {cartfs_prebuilt_dir}"
    )
    os.symlink(cartfs_prebuilt_dir, workspace_prebuilt_path)


def _verify_opted_in() -> None:
    """Verifies that the user has opted in to using this script."""
    if not os.environ.get("FUCHSIA_ALLOW_SETUP_COG_WORKSPACE"):
        log_warn(
            "This script is highly experimental and not yet ready for use."
        )
        print(
            "To acknowledge this and proceed, please set the environment variable:"
        )
        print("  export FUCHSIA_ALLOW_SETUP_COG_WORKSPACE=1")
        sys.exit(1)


def get_repo_name() -> str:
    """Returns the repo name from the workspace directory."""
    # TODO: chaselatta - Figure out how we want to get the
    return "fuchsia"


def main() -> None:
    """Main function to set up the cog workspace."""
    _verify_opted_in()

    workspace_dir = find_cog_workspace_directory()
    if not workspace_dir:
        log_warn(
            "Could not find workspace directory. Please run this script from a directory matching the pattern /google/cog/cloud/<user>/<workspace_name>."
        )
        sys.exit(1)

    log_info(f"Workspace dir: {workspace_dir}")

    cartfs_mount = _find_cartfs_mount_point()
    if cartfs_mount:
        log_info(f"Found cartfs mount point: {cartfs_mount}")
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

    workspace_name = get_workspace_name(workspace_dir)

    cartfs_workspace_dir = _ensure_cartfs_workspace_directory(workspace_name)
    if not cartfs_workspace_dir:
        log_warn(
            "Could not create cartfs workspace directory. Please run this script from a directory matching the pattern /google/cog/cloud/<user>/<workspace_name>."
        )
        sys.exit(1)

    log_info(f"Cartfs workspace dir: {cartfs_workspace_dir}")

    prebuilts_manager = prebuilts.Prebuilts(
        cartfs_workspace_dir, workspace_dir, workspace_name, get_repo_name()
    )
    if not prebuilts_manager.is_jiri_bootstrapped():
        prebuilts_manager.bootstrap_jiri()

    prebuilts_manager.fetch_prebuilts()
    prebuilts_manager.create_symlinks()

    snapshotter_manager = snapshotter.Snapshotter(
        cartfs_mount, workspace_dir, workspace_name, get_repo_name()
    )
    previous_prebuilt_dir = snapshotter_manager.find_previous_instance(
        "prebuilt"
    )
    if previous_prebuilt_dir:
        print(f"Previous prebuilt dir: {previous_prebuilt_dir}")
        # snapshotter_manager.snapshot_directory_from(
        #    previous_prebuilt_dir, "prebuilt"
        # )


if __name__ == "__main__":
    main()
