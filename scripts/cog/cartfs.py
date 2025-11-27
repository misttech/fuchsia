# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import shutil
import subprocess
from pathlib import Path

import logger


class CartfsError(Exception):
    """Base exception for Cartfs errors."""


class CartfsNotInstalledError(CartfsError):
    """Raised when cartfs is not installed."""


class CartfsNotRunningError(CartfsError):
    """Raised when cartfs is not running."""


class Cartfs:
    """A class to interact with a cartfs filesystem."""

    def __init__(self, mount_point: Path, use_local_mock_cartfs: bool):
        """Initializes a Cartfs instance.

        Note: This constructor should not be called directly. Instead, use the
        `create` class method to create an instance.
        """
        self.mount_point = mount_point
        self.use_local_mock_cartfs = use_local_mock_cartfs

    @staticmethod
    def _is_installed(use_local_mock_cartfs: bool) -> bool:
        """Checks if cartfs is installed."""
        if use_local_mock_cartfs:
            return True
        return shutil.which("cartfs") is not None

    @staticmethod
    def _find_mount_point(use_local_mock_cartfs: bool) -> Path | None:
        """Finds the mount point for cartfs.

        Returns:
            The mount point if found, None otherwise.
        """
        if use_local_mock_cartfs:
            local_mock_cartfs_mount_point = Path.home() / "mock_cartfs"
            local_mock_cartfs_mount_point.mkdir(parents=True, exist_ok=True)
            return local_mock_cartfs_mount_point

        try:
            cartfs_uid_process = subprocess.run(
                ["id", "-u", "cartfs"],
                capture_output=True,
                text=True,
                check=True,
            )
            cartfs_uid = cartfs_uid_process.stdout.strip()
        except (subprocess.CalledProcessError, FileNotFoundError):
            logger.log_warn(
                "Unable to find uid for cartfs. It is likely cartfs is not running or the user does not have permission"
            )
            # This is the case where cartfs is not running or user does not have permissions.
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
            return Path(output.splitlines()[0])
        except (subprocess.CalledProcessError, FileNotFoundError):
            # findmnt is not found or failed. This is unexpected on a gLinux host.
            # But we can treat it as cartfs not running.
            logger.log_warn(
                "Unable to find cartfs mount. Either findmnt failed or cartfs is not running."
            )
            return None

    @classmethod
    def create(cls, use_local_mock_cartfs: bool = False) -> "Cartfs":
        """Creates a Cartfs instance after verifying its state.

        Raises:
            CartfsNotInstalledError: If cartfs is not installed.
            CartfsNotRunningError: If cartfs is not running or the mount point
                cannot be found.

        Returns:
            A new Cartfs instance.
        """
        if not cls._is_installed(use_local_mock_cartfs):
            raise CartfsNotInstalledError(
                "cartfs is not installed. Please follow instructions at go/cartfs to install."
            )

        mount_point = cls._find_mount_point(use_local_mock_cartfs)
        if not mount_point:
            raise CartfsNotRunningError(
                "cartfs is installed but not running. Please start cartfs to continue."
            )

        return cls(mount_point, use_local_mock_cartfs)

    def suggest_cartfs_directory_name(self, workspace_name: str) -> Path:
        """Suggests a directory name within the cartfs mount point.

        Args:
            workspace_name: The base name for the workspace directory.

        Returns:
            A path to a directory that does not yet exist.
        """
        path = self.mount_point / workspace_name
        counter = 1
        while path.exists():
            path = self.mount_point / f"{workspace_name}-{counter}"
            counter += 1
        return path
