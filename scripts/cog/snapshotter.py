# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import subprocess
import sys


def snapshot_workspace(
    workspace_to_snapshot_from: str,
    workspace_to_snapshot_to: str,
    cartfs_mount_point: str,
) -> None:
    """Snapshots a workspace.

    This method copies a workspace to a new workspace using cartfs and
    cogfsd RPCs.

    Args:
        workspace_to_snapshot_from: The name of the workspace to snapshot from.
        workspace_to_snapshot_to: The name of the new workspace to create.
        cartfs_mount_point: The path to the cartfs mount point.
    """
    from_path = os.path.join(cartfs_mount_point, workspace_to_snapshot_from)
    if not os.path.isdir(from_path):
        raise ValueError(
            f"Source workspace directory {from_path} does not exist or is not a directory."
        )

    to_path = os.path.join(cartfs_mount_point, workspace_to_snapshot_to)
    if os.path.exists(to_path):
        raise ValueError(
            f"Target workspace directory {to_path} already exists."
        )

    print(
        f"Snapshotting workspace '{workspace_to_snapshot_from}' to '{workspace_to_snapshot_to}'"
    )

    # Placeholders for endpoint and RPC names.
    cartfs_endpoint = "127.0.0.1:65001"
    cartfs_rpc_copy_directory = "cartfs.Cartfs.CopyDirectory"
    cogfsd_endpoint = f"unix:///google/cog/status/uds/{os.getuid()}"
    cogfsd_rpc_forkmtimes = "devtools_srcfs.CogLocalRpcService.ForkMtimes"

    from_path_rel = workspace_to_snapshot_from
    to_path_rel = workspace_to_snapshot_to

    try:
        # We need to make the directory first because cartfs.CopyDirectory
        # does not update the directory immediately and a subsequent write
        # will fail. If we create the directory first, we can avoid this issue
        # and still correctly snapshot the workspace.
        os.makedirs(to_path, exist_ok=True)

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
        print(
            "Error: grpc_cli not found. Please ensure it is in your PATH.",
            file=sys.stderr,
        )
        raise
    except subprocess.CalledProcessError as e:
        print(
            f"Error during snapshotting via grpc_cli: {e}",
            file=sys.stderr,
        )
        print(f"stdout: {e.stdout}", file=sys.stderr)
        print(f"stderr: {e.stderr}", file=sys.stderr)
        raise
