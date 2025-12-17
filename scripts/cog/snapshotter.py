# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import shutil
import subprocess
from pathlib import Path

import logger


def snapshot_workspace(
    workspace_to_snapshot_from: Path,
    workspace_to_snapshot_to: Path,
    cartfs_mount_point: Path,
    use_local_mock_cartfs: bool,
) -> None:
    """Snapshots a workspace.

    This method copies a workspace to a new workspace using cartfs and
    cogfsd RPCs.

    Args:
        workspace_to_snapshot_from: The name of the workspace to snapshot from.
        workspace_to_snapshot_to: The name of the new workspace to create.
        cartfs_mount_point: The path to the cartfs mount point.
    """
    from_path = cartfs_mount_point / workspace_to_snapshot_from
    if not from_path.is_dir():
        raise ValueError(
            f"Source workspace directory {from_path} does not exist or is not a directory."
        )

    to_path = cartfs_mount_point / workspace_to_snapshot_to
    if to_path.exists():
        raise ValueError(
            f"Target workspace directory {to_path} already exists."
        )

    logger.log_info(
        f"Snapshotting workspace '{workspace_to_snapshot_from}' to '{workspace_to_snapshot_to}'"
    )

    from_path_rel_base = workspace_to_snapshot_from
    to_path_rel_base = workspace_to_snapshot_to
    copy_subdirs = [
        "prebuilt",
        ".cipd",
        ".jiri_manifest",
        ".jiri_root",
    ]

    if use_local_mock_cartfs:
        for subdir in copy_subdirs:
            from_path_rel = from_path_rel_base / subdir
            to_path_rel = to_path_rel_base / subdir
            if from_path_rel.is_dir():
                shutil.copytree(
                    from_path_rel,
                    to_path_rel,
                    dirs_exist_ok=True,
                    symlinks=True,
                )
            else:
                shutil.copyfile(from_path_rel, to_path_rel)
    else:
        # Placeholders for endpoint and RPC names.
        cartfs_endpoint = "127.0.0.1:65001"
        cartfs_rpc_copy_directory = "cartfs.Cartfs.CopyDirectory"
        cogfsd_endpoint = f"unix:///google/cog/status/uds/{os.getuid()}"
        cogfsd_rpc_forkmtimes = "devtools_srcfs.CogLocalRpcService.ForkMtimes"

        try:
            # We need to make the directory first because cartfs.CopyDirectory
            # does not update the directory immediately and a subsequent write
            # will fail. If we create the directory first, we can avoid this issue
            # and still correctly snapshot the workspace.
            to_path.mkdir(parents=True, exist_ok=True)

            for subdir in copy_subdirs:
                from_path_rel = from_path_rel_base / subdir
                to_path_rel = to_path_rel_base / subdir

                logger.log_info(
                    f"Copying from {from_path_rel} to {to_path_rel}"
                )
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

            logger.log_info(
                f"Forking mtimes from {workspace_to_snapshot_from} to {workspace_to_snapshot_to}"
            )
            subprocess.run(
                [
                    "grpc_cli",
                    "call",
                    cogfsd_endpoint,
                    cogfsd_rpc_forkmtimes,
                    "\n".join(
                        [
                            f'source_workspace: "{workspace_to_snapshot_from}"',
                            f'target_workspace: "{workspace_to_snapshot_to}"',
                        ]
                    ),
                    "--channel_creds_type=insecure",
                ],
                check=True,
                capture_output=True,
                text=True,
            )

        except FileNotFoundError:
            logger.log_error(
                "Error: grpc_cli not found. Please ensure it is in your PATH."
            )
            raise
        except subprocess.CalledProcessError as e:
            logger.log_error(f"Error during snapshotting via grpc_cli: {e}")
            logger.log_error(f"stdout: {e.stdout}")
            logger.log_error(f"stderr: {e.stderr}")
            raise
