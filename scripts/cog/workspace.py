# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import re
from typing import Callable

import cartfs
import snapshotter


class WorkspaceError(Exception):
    """Base exception for Cartfs errors."""


class UserNotFoundError(WorkspaceError):
    """Raised when the user is not found."""


class NotInCogWorkspaceError(WorkspaceError):
    """Raised when the current directory is not within a Cog workspace."""


class CannotFindRepoNameError(WorkspaceError):
    """Raised when the repo name cannot be found."""


CARTFS_SYMLINK_NAME: str = "cart-fs-dir"
REPO_NAME_FILE_NAME: str = ".repo-name"


class Workspace:
    """A class to encapsulate a Cog workspace and an associated Cartfs workspace."""

    def __init__(
        self,
        workspace_dir: str,
        repo_name: str,
        workspace_name: str,
        cartfs_workspace_dir: str | None,
        cartfs_instance: cartfs.Cartfs,
    ):
        """Initializes a Workspace instance.

        Note: This constructor should not be called directly. Instead, use the
        `create` class method to create an instance.
        """
        self.workspace_dir = workspace_dir
        self.repo_name = repo_name
        self.workspace_name = workspace_name
        self.cartfs_instance = cartfs_instance
        self.cartfs_workspace_dir = cartfs_workspace_dir

    @staticmethod
    def _find_cog_workspace_directory(
        start_dir: str, user: str, cog_mount_point: str = "/google/cog/cloud"
    ) -> str | None:
        """Finds the workspace directory from the given directory."""
        path_prefix = f"{cog_mount_point}/{re.escape(user)}"
        path_pattern = re.compile(f"^{path_prefix}/[^/]+$")

        current_dir = start_dir
        while current_dir != "/":
            match = path_pattern.match(current_dir)
            if match:
                return match.group(0)
            else:
                current_dir = os.path.dirname(current_dir)
        return None

    @classmethod
    def create(cls, cog_mount_point: str = "/google/cog/cloud") -> "Workspace":
        """Creates a Workspace instance after verifying its state.

        Raises:
            UserNotFoundError: If we cannot find the current user.
            NotInCogWorkspaceError: If the current directory is not within a Cog workspace.
                cannot be found.
            CannotFindRepoNameError: If the repo name cannot be found.
            CartfsError: If cartfs is not installed or running.

        Returns:
            A new Workspace instance.
        """
        user = os.environ.get("USER")
        if not user:
            raise UserNotFoundError(
                "Expected $USER environment variable to be set."
            )

        current_dir = os.getcwd()

        workspace_dir = cls._find_cog_workspace_directory(
            current_dir, user, cog_mount_point
        )
        if not workspace_dir:
            raise NotInCogWorkspaceError(
                f"Current directory is not within a Cog workspace: {current_dir}"
            )

        # Note: this will raise a CartfsError if cartfs is not installed or
        # running.
        cartfs_instance = cartfs.Cartfs.create()

        repo_name = cls._get_repo_name_from_path(workspace_dir, current_dir)
        if not repo_name:
            raise CannotFindRepoNameError(
                "Could not find repo name from the path."
            )

        workspace_name = os.path.basename(workspace_dir)

        cartfs_workspace_dir = cls.get_linked_cartfs_workspace_directory(
            workspace_dir, repo_name
        )

        return cls(
            workspace_dir,
            repo_name,
            workspace_name,
            cartfs_workspace_dir,
            cartfs_instance,
        )

    @staticmethod
    def _get_repo_name_from_path(workspace_dir: str, path: str) -> str | None:
        """Finds the repo name from a given path.

        The repo name is the first path element that is common between the
        workspace directory and the given path, after the workspace directory.

        Args:
            path: The path to find the repo name from.

        Returns:
            The repo name if found, None otherwise.
        """
        if not path.startswith(workspace_dir):
            return None

        relative_path = os.path.relpath(path, workspace_dir)
        if relative_path == ".":
            return None

        return relative_path.split(os.sep)[0]

    @staticmethod
    def get_linked_cartfs_workspace_directory(
        workspace_dir: str, repo_name: str
    ) -> str | None:
        """Returns the cartfs workspace directory if the workspace is linked to cartfs.

        A workspace is linked to cartfs if there is a symlink in the workspace
        directory that points to a cartfs directory and a .repo-name file that
        matches the repo name.
        """
        repo_dir = os.path.join(workspace_dir, repo_name)
        symlink_path = os.path.join(repo_dir, CARTFS_SYMLINK_NAME)
        if not os.path.islink(symlink_path):
            print(f"symlink_path is not a link: {symlink_path}")
            return None

        target_path = os.readlink(symlink_path)
        if not os.path.isabs(target_path):
            # Handles relative symlinks. The target is relative to the directory
            # containing the symlink.
            target_path = os.path.join(repo_dir, target_path)

        if not os.path.isdir(target_path):
            return None

        repo_name_file_path = os.path.join(target_path, REPO_NAME_FILE_NAME)
        if not os.path.exists(repo_name_file_path):
            return None

        with open(repo_name_file_path, "r") as f:
            content = f.read().strip()
            if content != repo_name:
                print(f"names don't match {content} != {repo_name}")
                return None

        return target_path

    def snapshot_from_previous_instance(
        self,
        snapshot_function: Callable[
            [str, str, str], None
        ] = snapshotter.snapshot_workspace,
    ) -> str | None:
        """Snapshots the workspace from the most recent cartfs directory."""
        previous_cartfs_instance = self._find_previous_instance()
        if not previous_cartfs_instance:
            return None

        suggested_directory_name = (
            self.cartfs_instance.suggest_cartfs_directory_name(
                self.workspace_name
            )
        )
        try:
            snapshot_function(
                os.path.basename(previous_cartfs_instance),
                suggested_directory_name,
                self.cartfs_instance.mount_point,
            )
        except ValueError as e:
            print(f"Error during snapshotting: {e}")
            return None

        return os.path.join(
            self.cartfs_instance.mount_point, suggested_directory_name
        )

    def create_empty_cartfs_workspace_directory(self) -> str:
        """Creates an empty cartfs workspace directory."""
        suggested_directory_name = (
            self.cartfs_instance.suggest_cartfs_directory_name(
                self.workspace_name
            )
        )
        cartfs_workspace_dir = os.path.join(
            self.cartfs_instance.mount_point, suggested_directory_name
        )
        # It is ok to use exist_ok here because the suggested directory name
        # is generated by cartfs, and there should not be a directory with
        # the same name in the cartfs mount point.
        os.makedirs(cartfs_workspace_dir, exist_ok=True)

        # Write the name of the repository in cartfs
        with open(
            os.path.join(cartfs_workspace_dir, REPO_NAME_FILE_NAME), "w"
        ) as f:
            f.write(self.repo_name)

        return cartfs_workspace_dir

    def link_to_cartfs(self, cartfs_workspace_dir: str) -> None:
        """Links the workspace to the given cartfs directory.

        This method creates a symlink in the workspace directory that points to a
        cartfs directory, and that directory contains a .cog-path file which
        contains the path to this workspace.

        Args:
            cartfs_workspace_dir: The path to the cartfs directory to link to.
        """
        repo_dir = os.path.join(self.workspace_dir, self.repo_name)
        symlink_path = os.path.join(repo_dir, CARTFS_SYMLINK_NAME)

        # Create an absolute symlink from the repo directory to the cartfs
        # workspace directory. If a symlink already exists, remove it first.
        if os.path.islink(symlink_path):
            os.remove(symlink_path)
        os.symlink(cartfs_workspace_dir, symlink_path)

        self.cartfs_workspace_dir = cartfs_workspace_dir

    def _find_previous_instance(self) -> str | None:
        """Finds the most recent cartfs directory for the same repo.

        This method iterates through all directories in the cartfs mount point,
        looking for directories that are linked to a workspace with the same repo
        name as the current one. It then returns the path to the one with the
        most recent modification time.

        Returns:
            The path to the newest directory found, or None if no instances are
            found.
        """
        mount_point = self.cartfs_instance.mount_point
        if not mount_point or not os.path.isdir(mount_point):
            return None

        candidates = set()
        for entry in os.listdir(mount_point):
            entry_path = os.path.join(mount_point, entry)
            if not os.path.isdir(entry_path):
                continue

            repo_name_file_path = os.path.join(entry_path, REPO_NAME_FILE_NAME)
            if not os.path.isfile(repo_name_file_path):
                continue

            try:
                with open(repo_name_file_path, "r") as f:
                    repo_name = f.read().strip()
            except OSError:
                continue

            if not repo_name:
                continue

            # Check if it's for the same repo.
            if repo_name != self.repo_name:
                continue

            candidates.add(entry_path)

        newest_candidate = None
        newest_mtime = -1.0
        for candidate in candidates:
            try:
                mtime = os.stat(candidate).st_mtime
                if mtime > newest_mtime:
                    newest_mtime = mtime
                    newest_candidate = candidate
            except FileNotFoundError:
                # The directory was deleted between listing and stat-ing.
                continue
        return newest_candidate
