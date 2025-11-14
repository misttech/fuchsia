# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
from pathlib import Path
from typing import Callable

import cartfs
import snapshotter


class WorkspaceError(Exception):
    """Base exception for Cartfs errors."""


class NotInCogWorkspaceError(WorkspaceError):
    """Raised when the current directory is not within a Cog workspace."""


class CannotFindRepoNameError(WorkspaceError):
    """Raised when the repo name cannot be found."""


CARTFS_SYMLINK_NAME: str = "cartfs-dir"
COG_METADATA_FILE_NAME: str = ".cog.json"


class CogMetadata:
    """Represents the metadata stored in the .cog.json file."""

    def __init__(self, workspace_name: str, repo_name: str):
        """Initializes CogMetadata.

        Args:
            workspace_name: The name of the cog workspace.
            repo_name: The name of the repository within the workspace.
        """
        self.workspace_name = workspace_name
        self.repo_name = repo_name

    def to_dict(self) -> dict[str, str]:
        """Returns a dictionary representation of the metadata."""
        return {
            "workspace_name": self.workspace_name,
            "repo_name": self.repo_name,
        }

    @classmethod
    def from_file(cls, path: str | Path) -> "CogMetadata | None":
        """Loads metadata from a .cog.json file.

        Args:
            path: The full path to the .cog.json file.

        Returns:
            A CogMetadata instance if the file is valid, otherwise None.
        """
        path = Path(path)
        if not path.exists():
            return None
        try:
            with path.open("r") as f:
                data = json.load(f)
            return cls(
                workspace_name=data["workspace_name"],
                repo_name=data["repo_name"],
            )
        except (
            OSError,
            json.JSONDecodeError,
            KeyError,
        ) as e:
            print(f"Warning: Could not read or parse {path}: {e}")
            return None

    def write(self, directory: str | Path) -> None:
        """Writes the metadata to a JSON file in the given directory."""
        path = Path(directory) / COG_METADATA_FILE_NAME
        with path.open("w") as f:
            json.dump(self.to_dict(), f, indent=4)


class Workspace:
    """A class to encapsulate a Cog workspace and an associated Cartfs workspace."""

    def __init__(
        self,
        workspace_dir: str | Path,
        repo_name: str,
        workspace_name: str,
        cartfs_workspace_dir: str | Path | None,
        cartfs_instance: cartfs.Cartfs,
    ):
        """Initializes a Workspace instance.

        Note: This constructor should not be called directly. Instead, use the
        `create` class method to create an instance.
        """
        self.workspace_dir = Path(workspace_dir)
        self.repo_name = repo_name
        self.workspace_name = workspace_name
        self.cartfs_instance = cartfs_instance
        self.cartfs_workspace_dir = (
            Path(cartfs_workspace_dir) if cartfs_workspace_dir else None
        )

    @staticmethod
    def _find_cog_workspace_directory(
        start_dir: str | Path,
    ) -> Path | None:
        """Finds the root cog workspace directory by traversing up from a start path.

        This function looks for a directory which contains a .citc directory.

        Args:
            start_dir: The directory to start searching upwards from.
            cog_mount_point: The base path for cog workspaces.

        Returns:
            The absolute path to the workspace directory if found, otherwise None.
        """
        start_dir = Path(start_dir)
        for ancestor in [start_dir] + list(start_dir.parents):
            if (ancestor / ".citc").is_dir():
                return ancestor
        return None

    @classmethod
    def create(cls) -> "Workspace":
        """Creates a Workspace instance after verifying its state.

        Raises:
            NotInCogWorkspaceError: If the current directory is not within a Cog workspace.
                cannot be found.
            CannotFindRepoNameError: If the repo name cannot be found.
            CartfsError: If cartfs is not installed or running.

        Returns:
            A new Workspace instance.
        """
        current_dir = Path.cwd()

        workspace_dir = cls._find_cog_workspace_directory(current_dir)
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

        workspace_name = workspace_dir.name

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
    def _get_repo_name_from_path(
        workspace_dir: str | Path, path: str | Path
    ) -> str | None:
        """Finds the repo name from a given path.

        The repo name is the first path element that is common between the
        workspace directory and the given path, after the workspace directory.

        Args:
            path: The path to find the repo name from.

        Returns:
            The repo name if found, None otherwise.
        """
        path = Path(path)
        workspace_dir = Path(workspace_dir)

        try:
            relative_path = path.relative_to(workspace_dir)
        except:
            return None

        if relative_path == Path("."):
            return None

        return relative_path.parts[0]

    @staticmethod
    def get_linked_cartfs_workspace_directory(
        workspace_dir: str | Path, repo_name: str
    ) -> Path | None:
        """Gets the linked cartfs directory for a specific repo in a cog workspace.

        A workspace is considered linked if a symlink named `cartfs-dir` exists
        inside the specified repository directory, pointing to a valid cartfs
        directory. This target cartfs directory must contain a `.cog.json` file
        with a matching `repo_name` and `workspace_name`.

        Args:
            workspace_dir: The absolute path to the cog workspace.
            repo_name: The name of the repository within the workspace.

        Returns:
            The absolute path to the linked cartfs directory if found and valid,
            otherwise None.
        """
        workspace_dir = Path(workspace_dir)
        repo_dir = workspace_dir / repo_name
        symlink_path = repo_dir / CARTFS_SYMLINK_NAME
        if not symlink_path.is_symlink():
            print(f"symlink_path is not a link: {symlink_path}")
            return None

        target_path = symlink_path.readlink()
        if not target_path.is_absolute():
            # Handles relative symlinks. The target is relative to the directory
            # containing the symlink.
            target_path = repo_dir / target_path

        if not target_path.is_dir():
            return None

        metadata = CogMetadata.from_file(target_path / COG_METADATA_FILE_NAME)
        if not metadata:
            return None

        if (
            metadata.repo_name != repo_name
            or metadata.workspace_name != workspace_dir.name
        ):
            return None

        return target_path

    def snapshot_from_previous_instance(
        self,
        snapshot_function: Callable[
            [str, str, str], None
        ] = snapshotter.snapshot_workspace,
    ) -> Path | None:
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
                previous_cartfs_instance.name,
                suggested_directory_name,
                self.cartfs_instance.mount_point,
            )
        except ValueError as e:
            print(f"Error during snapshotting: {e}")
            return None

        return Path(self.cartfs_instance.mount_point) / suggested_directory_name

    def create_empty_cartfs_workspace_directory(self) -> Path:
        """Creates a new, empty directory in the cartfs mount for this workspace.

        This method generates a unique directory name based on the workspace name,
        creates the directory, and writes a `.cog.json` metadata file into it.

        Returns:
            The absolute path to the newly created cartfs directory.
        """
        suggested_directory_name = (
            self.cartfs_instance.suggest_cartfs_directory_name(
                self.workspace_name
            )
        )
        cartfs_workspace_dir = (
            Path(self.cartfs_instance.mount_point) / suggested_directory_name
        )
        # It is ok to use exist_ok here because the suggested directory name
        # is generated by cartfs, and there should not be a directory with
        # the same name in the cartfs mount point.
        cartfs_workspace_dir.mkdir(exist_ok=True)

        # Write the metadata file in cartfs
        metadata = CogMetadata(
            workspace_name=self.workspace_name, repo_name=self.repo_name
        )
        metadata.write(cartfs_workspace_dir)

        return cartfs_workspace_dir

    def link_to_cartfs(self, cartfs_workspace_dir: str | Path) -> None:
        """Links the cog workspace to a cartfs directory.

        This creates a symlink named `cartfs-dir` inside the repository
        directory of the cog workspace. This symlink points to the specified
        cartfs directory, establishing the link between them. If a symlink
        already exists, it will be replaced.

        Additionally, it writes a `.cog.json` metadata file into the cartfs
        directory.

        Args:
            cartfs_workspace_dir: The absolute path to the target cartfs directory.
        """
        cartfs_workspace_dir = Path(cartfs_workspace_dir)

        symlink_path = self.workspace_dir / self.repo_name / CARTFS_SYMLINK_NAME

        # Create an absolute symlink from the repo directory to the cartfs
        # workspace directory. If a symlink already exists, remove it first.
        if symlink_path.is_symlink():
            symlink_path.unlink()
        symlink_path.symlink_to(cartfs_workspace_dir)

        metadata = CogMetadata(
            workspace_name=self.workspace_name, repo_name=self.repo_name
        )
        metadata.write(cartfs_workspace_dir)

        self.cartfs_workspace_dir = cartfs_workspace_dir

    def _find_previous_instance(self) -> Path | None:
        """Finds the most recent cartfs directory for the same repo.

        This method iterates through all directories in the cartfs mount point,
        looking for directories that are linked to a workspace with the same repo
        name as the current one. It then returns the path to the one with the
        most recent modification time.

        Returns:
            The path to the newest directory found, or None if no instances are
            found.
        """
        mount_point = Path(self.cartfs_instance.mount_point)
        if not mount_point or not mount_point.is_dir():
            return None

        candidates = set()
        for entry in mount_point.iterdir():
            entry_path = mount_point / entry
            if not entry_path.is_dir():
                continue

            metadata = CogMetadata.from_file(
                entry_path / COG_METADATA_FILE_NAME
            )
            if not metadata:
                continue

            repo_name = metadata.repo_name

            # Check if it's for the same repo.
            if repo_name != self.repo_name:
                continue

            candidates.add(entry_path)

        newest_candidate = None
        newest_mtime = -1.0
        for candidate in candidates:
            try:
                mtime = candidate.stat().st_mtime
                if mtime > newest_mtime:
                    newest_mtime = mtime
                    newest_candidate = candidate
            except FileNotFoundError:
                # The directory was deleted between listing and stat-ing.
                continue
        return newest_candidate
