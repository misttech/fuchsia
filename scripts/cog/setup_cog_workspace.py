#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""This script is used to set up a cog-based workspace for Fuchsia development."""

import argparse
import logging
import os
import sys

import logger
import preflight
import workspace


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Set up a cog-based workspace for Fuchsia development."
    )
    parser.add_argument(
        "--no-snapshot",
        dest="snapshot",
        action="store_false",
        help="Disable snapshotting and initialize this workspace from scratch.",
    )
    parser.add_argument(
        "-l",
        "--local-mock-cartfs",
        dest="use_local_mock_cartfs",
        action="store_true",
        help="Use a local mock cartfs directory located at ~/.mock_cartfs.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="count",
        default=0,
        help="Increase verbosity level (-v for INFO, -vv for DEBUG).",
    )
    parser.add_argument(
        "--enable-status-updates",
        action="store_true",
        help="Enable status updates.",
    )
    parser.add_argument(
        "--color",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Enable or disable color output.",
    )
    return parser.parse_args()


def main() -> int:
    """Main function to set up the cog workspace."""
    args = _parse_args()

    if not args.color:
        os.environ["NO_COLOR"] = "1"

    if args.verbose == 0:
        log_level = logging.WARNING
    elif args.verbose == 1:
        log_level = logging.INFO
    else:
        log_level = logging.DEBUG

    logger.init_logger(
        level=log_level,
        colors=args.color,
        enable_status_updates=args.enable_status_updates,
    )

    if preflight.check_all(
        skip_cartfs_checks=args.use_local_mock_cartfs,
        require_grpc_cli=args.snapshot,
    ):
        logger.log_info("All preflight checks passed!")
    else:
        return 1

    logger.emit_status("Creating workspace instance...")
    ws = workspace.Workspace(use_local_mock_cartfs=args.use_local_mock_cartfs)

    logger.log_debug(f"Found repository: {ws.repo_dir}")
    logger.log_debug(f"CartFS mount point: {ws.cartfs_instance.mount_point}")

    with ws.lock():
        if ws.has_cartfs_dir and ws.is_checkout_uptodate():
            logger.log_warn("No work to do, workspace is already bootstrapped.")
            return 0

        if ws.has_cartfs_dir:
            logger.log_info(
                f"Workspace is already linked to cartfs: {ws.cartfs_dir}"
            )
        else:
            logger.log_info("Workspace is not linked to cartfs.")
            ws.init_cartfs_workspace(args.snapshot)

        if ws.is_checkout_uptodate():
            logger.log_info(
                "CartFS checkout is up to date, skipping cartfs checkout update."
            )
        else:
            ws.checkout_cartfs_to_cog_revisions()
    return 0


if __name__ == "__main__":
    sys.exit(main())
