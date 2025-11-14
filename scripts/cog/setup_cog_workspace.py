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
import sys

import cartfs
import cartfs_out_directory
import prebuilts
import workspace
from util import log_info, log_warn


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


def _ensure_cartfs_workspace_directory(
    workspace_name: str, cartfs_mount: str
) -> str:
    """Ensures the cartfs workspace directory exists."""
    cartfs_workspace_dir = os.path.join(cartfs_mount, workspace_name)
    if not os.path.exists(cartfs_workspace_dir):
        log_info(f"Creating cartfs workspace directory: {cartfs_workspace_dir}")
        os.makedirs(cartfs_workspace_dir)

    return cartfs_workspace_dir


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


def prepare_workspace_instance() -> workspace.Workspace | None:
    """Prepares a workspace instance."""
    try:
        workspace_instance = workspace.Workspace.create()
        log_info(f"Found workspace dir: {workspace_instance.workspace_dir}")
        log_info(
            f"Found cartfs mount point: {workspace_instance.cartfs_instance.mount_point}"
        )
        log_info(f"the repository name is: {workspace_instance.repo_name}")

        if (
            existing_cartfs_workspace_dir := workspace_instance.cartfs_workspace_dir
        ):
            log_info(
                f"Workspace is already linked to cartfs: {existing_cartfs_workspace_dir}"
            )
        else:
            log_info(
                "Workspace is not linked to cartfs. Attempting to Snapshot from previous instance."
            )
            cartfs_workspace_dir = (
                workspace_instance.snapshot_from_previous_instance()
            )
            if not cartfs_workspace_dir:
                log_info(
                    "Unable to snapshot from previous instance. Creating new"
                    " cartfs workspace directory."
                )
                cartfs_workspace_dir = (
                    workspace_instance.create_empty_cartfs_workspace_directory()
                )
            workspace_instance.link_to_cartfs(cartfs_workspace_dir)

        return workspace_instance
    except cartfs.CartfsError as e:
        log_warn(str(e))

    return None


def main() -> None:
    """Main function to set up the cog workspace."""
    _verify_opted_in()
    workspace_instance = prepare_workspace_instance()
    if not workspace_instance:
        log_warn("Could not create workspace instance.")
        sys.exit(1)

    # TODO: Move this logic into the workspace instance.
    prebuilts_manager = prebuilts.Prebuilts(
        str(workspace_instance.cartfs_workspace_dir),
        str(workspace_instance.workspace_dir),
        workspace_instance.workspace_name,
        workspace_instance.repo_name,
    )
    if not prebuilts_manager.is_jiri_bootstrapped():
        prebuilts_manager.bootstrap_jiri()

    prebuilts_manager.cartfs_structure_initialization()
    prebuilts_manager.fetch_prebuilts()
    prebuilts_manager.create_symlinks()

    # Install/update cartfs-backed out directory.
    cartfs_out_directory.CartfsOutDirectory(
        cog_workspace_dir=workspace_instance.workspace_dir
        / workspace_instance.repo_name,
        cartfs_workspace_dir=workspace_instance.cartfs_workspace_dir,
    ).apply()


if __name__ == "__main__":
    main()
