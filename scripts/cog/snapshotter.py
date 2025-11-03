# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import os
import subprocess
import sys
from typing import Optional


class Snapshotter:
    """A class to manage snapshotting for a cog workspace."""

    def __init__(
        self,
        cartfs_mount_point: str,
        workspace_dir: str,
        workspace_name: str,
        repo_name: str,
    ):
        self.cartfs_mount_point = cartfs_mount_point
        self.workspace_dir = workspace_dir
        self.workspace_name = workspace_name
        self.repo_name = repo_name

    def find_previous_instance(self, directory_name: str) -> Optional[str]:
        """Finds the most recent instance of a directory in the cartfs mount point.

        This method iterates through all directories in the cartfs_mount_point,
        looking for a subdirectory with the given directory_name. It then
        returns the path to the one with the most recent modification time.

        Args:
            directory_name: The name of the directory to search for.

        Returns:
            The path to the newest directory found, or None if no instances are found.
        """
        # NOTE: This is a temporary solution to get the snapshotter working.
        # The code here makes a lot of assumptions and does not check for things
        # like the directories still containing valid data or even coming from
        # the same repo.

        candidates = set()
        try:
            for entry in os.listdir(self.cartfs_mount_point):
                entry_path = os.path.join(self.cartfs_mount_point, entry)
                if not os.path.isdir(entry_path):
                    continue

                # Exclude the current workspace directory from the search for
                # previous instances.
                if entry_path == self.workspace_dir:
                    continue

                target_dir = os.path.join(entry_path, directory_name)
                if os.path.isdir(target_dir):
                    candidates.add(target_dir)
        except FileNotFoundError:
            return None

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

    def snapshot_directory_from(
        self, source_directory_path: Optional[str], directory_name: str
    ) -> None:
        """Snapshots a directory from a previous instance.

        This method copies a directory from a previous instance to the current
        workspace using cartfs and cogfsd RPCs.

        Args:
            source_directory_path: The path to the previous directory to
                copy from (e.g., result of find_previous_instance). If None,
                a message is printed and nothing is done.
            directory_name: The name of the directory to snapshot (e.g., "out/default").
        """
        if not source_directory_path:
            print(
                "No suitable build outputs matching the same repository found."
            )
            print("Starting from a fresh build output directory.")
            return

        # Derive source workspace name from the path.
        # e.g. /path/to/cartfs/ws1/out/default -> ws1
        real_source_path = os.path.realpath(source_directory_path)
        real_mount_point = os.path.realpath(self.cartfs_mount_point)
        if not real_source_path.startswith(real_mount_point):
            raise ValueError(
                f"Source path {source_directory_path} is not inside the cartfs mount point {self.cartfs_mount_point}"
            )

        relative_path = os.path.relpath(
            source_directory_path, self.cartfs_mount_point
        )
        source_workspace_name = relative_path.split(os.sep)[0]

        print(
            f"Found suitable directory from workspace '{source_workspace_name}'"
        )

        copy_from = source_directory_path
        link_destination = os.path.join(
            self.cartfs_mount_point, self.workspace_name, directory_name
        )

        print(f"Copying content from {copy_from} to {link_destination}")

        # Placeholders for endpoint and RPC names.
        cartfs_endpoint = "127.0.0.1:65001"
        cartfs_rpc_copy_directory = "cartfs.Cartfs.CopyDirectory"
        cogfsd_endpoint = f"unix:///google/cog/status/uds/{os.getuid()}"
        cogfsd_rpc_forkmtimes = "devtools_srcfs.CogLocalRpcService.ForkMtimes"

        from_path_rel = relative_path
        to_path_rel = os.path.relpath(link_destination, self.cartfs_mount_point)

        try:
            # We should use the gRPC client library instead of shelling out to
            # grpc_cli. However, we might not be able to do this since we don't
            # have access to third_party libraries at the time of using this
            # script.
            # TODO: check if the directory exists first since this will fail if
            # it already exists.
            print(f"Copying from {from_path_rel} to {to_path_rel}")
            subprocess.run(
                [
                    "grpc_cli",
                    "call",
                    cartfs_endpoint,
                    cartfs_rpc_copy_directory,
                    f'from_path: "{from_path_rel}"\nto_path: "{to_path_rel}"',
                    "--channel_creds_type=insecure",
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            print(
                f"Forking mtimes from {source_workspace_name} to {self.workspace_name}"
            )
            subprocess.run(
                [
                    "grpc_cli",
                    "call",
                    cogfsd_endpoint,
                    cogfsd_rpc_forkmtimes,
                    "\n".join(
                        [
                            f'source_workspace: "{source_workspace_name}"',
                            f'target_workspace: "{self.workspace_name}"',
                        ]
                    ),
                    "--channel_creds_type=insecure",
                ],
                check=True,
                capture_output=True,
                text=True,
            )

        except FileNotFoundError:
            print(
                "Error: grpc_cli not found. Please ensure it is in your PATH.",
                file=sys.stderr,
            )
            raise
        except subprocess.CalledProcessError as e:
            print(
                f"Error during snapshotting via grpc_cli: {e}", file=sys.stderr
            )
            print(f"stdout: {e.stdout}", file=sys.stderr)
            print(f"stderr: {e.stderr}", file=sys.stderr)
            raise
