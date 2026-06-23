#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys

from subcommands import add as add_cmd
from subcommands import lease as lease_cmd
from subcommands import list as list_cmd
from subcommands import locate as locate_cmd
from subcommands import release as release_cmd
from subcommands import remove as remove_cmd
from worktree_registry import WorktreeRegistry


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="fx worktree",
        description="Manage Fuchsia worktrees for parallel development.",
        epilog=(
            "Worktree Pool Lifecycle & Command Comparison:\n"
            "  Unlike standard git worktrees, 'fx worktree' maintains a pool of reusable checkouts\n"
            "  with 2 distinct states: free (available) and leased (in active use).\n"
            "\n"
            "  • Physical Disk Management (add / remove):\n"
            "      add            Create a new physical checkout on disk under .jiri_root/worktrees/<name>\n"
            "                     and register it in the pool as 'free'.\n"
            "      remove         Permanently delete a worktree directory and its build artifacts from disk.\n"
            "\n"
            "  • Active Task Allocation (lease / release):\n"
            "      lease          Temporarily claim a 'free' worktree for active use (e.g. by an AI agent),\n"
            "                     marking it 'leased' to prevent concurrent modifications by other tasks.\n"
            "      release        End an active lease, restore backed-up GN build arguments, and return\n"
            "                     the worktree to the pool as 'free'."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    # Subcommand 'locate'
    parser_locate = subparsers.add_parser(
        "locate",
        help="Print the absolute path to a worktree directory",
        description="Locate a worktree by name and print its absolute path on disk.",
    )
    parser_locate.add_argument("name", help="Name of the worktree")

    # Subcommand 'list'
    subparsers.add_parser(
        "list",
        help="List all worktrees in the pool with their state, lease info, and branch",
        description="List all worktrees in the pool along with their current state (free, leased), lease details, sync status, and git branch.",
    )

    # Subcommand 'add'
    parser_add = subparsers.add_parser(
        "add",
        help="Create a new physical worktree checkout on disk and add it to the pool as 'free'",
        description=(
            "Create a new physical Jiri worktree checkout on disk under .jiri_root/worktrees/<name>\n"
            "and register it in the worktree pool in the 'free' state.\n"
            "\n"
            "Optionally runs 'fx set' to initialize build directories.\n"
            "\n"
            "Contrast with:\n"
            "  • remove: Permanently deletes a worktree directory from disk.\n"
            "  • lease: Temporarily allocates an already existing 'free' worktree for a task."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser_add.add_argument(
        "name",
        nargs="?",
        help="Name of the worktree to create (optional, auto-generated if omitted)",
    )
    parser_add.add_argument(
        "--set",
        action="append",
        help="Run 'fx set' with these arguments in the new worktree (can be specified multiple times)",
    )

    # Subcommand 'remove'
    parser_remove = subparsers.add_parser(
        "remove",
        help="Permanently delete a worktree directory and its build artifacts from disk",
        description=(
            "Permanently delete a worktree checkout and all of its build directories from disk.\n"
            "\n"
            "By default, this command will error if the worktree is currently leased.\n"
            "\n"
            "Contrast with:\n"
            "  • add: Creates a new physical worktree on disk.\n"
            "  • release: Ends an active lease on a worktree and returns it to the pool (keeps files intact)."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser_remove.add_argument("name", help="Name of the worktree to remove")
    parser_remove.add_argument(
        "--force",
        action="store_true",
        help="Force removal even if the worktree is currently leased",
    )

    # Subcommand 'lease'
    parser_lease = subparsers.add_parser(
        "lease",
        help="Temporarily claim a free worktree for an active task, marking it 'leased'",
        description=(
            "Temporarily allocate a 'free' worktree for active use (e.g., by an AI agent or developer),\n"
            "changing its state to 'leased' to prevent concurrent modifications by other tasks.\n"
            "\n"
            "When leased, existing GN build arguments are backed up so they can be restored upon release.\n"
            "\n"
            "Contrast with:\n"
            "  • release: Ends the active lease and returns the worktree to 'free' in the pool.\n"
            "  • add: Creates a brand new worktree checkout on disk."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser_lease.add_argument(
        "name", nargs="?", help="Name of the worktree to lease"
    )
    parser_lease.add_argument(
        "--json",
        action="store_true",
        help="Output lease details in JSON format",
    )
    parser_lease.add_argument(
        "--sync",
        action="store_true",
        help="Sync the worktree (jiri worktree sync) after leasing",
    )
    parser_lease.add_argument(
        "--task-id",
        help="Metadata identifying the agent/task leasing the worktree; automatically creates and checks out git branch 'feat/<task-id>'",
    )
    parser_lease.add_argument(
        "--any", action="store_true", help="Lease any free worktree"
    )

    # Subcommand 'release'
    parser_release = subparsers.add_parser(
        "release",
        help="End active use of a leased worktree and return it to the pool as 'free'",
        description=(
            "Release an actively 'leased' worktree back to the worktree pool.\n"
            "\n"
            "This deletes the temporary lease tracking record, restores backed-up GN build arguments,\n"
            "and changes the worktree's state back to 'free' so it can be leased by new tasks.\n"
            "\n"
            "Contrast with:\n"
            "  • lease: Claims a free worktree for an active task.\n"
            "  • remove: Permanently deletes the worktree directory from disk."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser_release.add_argument(
        "name", help="Name of the leased worktree to release"
    )

    args = parser.parse_args()
    registry = WorktreeRegistry()

    try:
        if args.subcommand == "locate":
            locate_cmd.run(args, registry)
        elif args.subcommand == "list":
            list_cmd.run(args, registry)
        elif args.subcommand == "add":
            add_cmd.run(args, registry)
        elif args.subcommand == "remove":
            remove_cmd.run(args, registry)
        elif args.subcommand == "lease":
            lease_cmd.run(args, registry)
        elif args.subcommand == "release":
            release_cmd.run(args, registry)
        else:
            print(f"Unknown subcommand: {args.subcommand}", file=sys.stderr)
            sys.exit(1)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
