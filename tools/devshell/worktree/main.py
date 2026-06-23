#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys

from subcommands import add as add_cmd
from subcommands import list as list_cmd
from subcommands import locate as locate_cmd
from subcommands import pool_add as pool_add_cmd
from subcommands import pool_list as pool_list_cmd
from subcommands import pool_remove as pool_remove_cmd
from subcommands import remove as remove_cmd
from worktree_pool import WorktreePool


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="fx worktree",
        description="Manage Fuchsia worktrees for parallel development.",
        epilog=(
            "Worktree Pool Lifecycle & Command Comparison:\n"
            "  Unlike standard git worktrees, 'fx worktree' maintains a pool of reusable checkouts\n"
            "  with 2 distinct states: free (available) and leased (in active use).\n"
            "\n"
            "  • Physical Disk Management (pool add / pool remove):\n"
            "      pool add       Create a new physical checkout on disk under .jiri_root/worktrees/<name>\n"
            "                     and register it in the pool as 'free'.\n"
            "      pool remove    Permanently delete a worktree directory and its build artifacts from disk.\n"
            "\n"
            "  • Active Task Allocation (add / remove):\n"
            "      add            Temporarily allocate a 'free' worktree for active use (e.g. by an AI agent),\n"
            "                     marking it 'leased' to prevent concurrent modifications by other tasks.\n"
            "      remove         End active use of a worktree, restore backed-up GN build arguments, and return\n"
            "                     the checkout to the pool as 'free'."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    # Subcommand group 'pool'
    parser_pool = subparsers.add_parser(
        "pool",
        help="Manage physical worktree checkouts in the pool",
        description="Administrative commands for provisioning and maintaining physical checkouts on disk.",
    )
    pool_subparsers = parser_pool.add_subparsers(
        dest="pool_subcommand", required=True
    )

    parser_pool_list = pool_subparsers.add_parser(
        "list",
        help="List all physical worktrees in the pool",
        description="List all physical worktrees in the pool along with state and physical paths.",
    )

    parser_pool_add = pool_subparsers.add_parser(
        "add",
        help="Add a new physical worktree checkout to the pool",
    )
    parser_pool_add.add_argument(
        "name", nargs="?", help="Optional physical name"
    )
    parser_pool_add.add_argument("--set", action="append", help="Run 'fx set'")

    parser_pool_remove = pool_subparsers.add_parser(
        "remove",
        help="Remove a physical worktree from the pool",
    )
    parser_pool_remove.add_argument("name", help="Name of physical worktree")
    parser_pool_remove.add_argument(
        "--force", action="store_true", help="Force removal"
    )

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
        help="List all active worktrees",
        description="List all active worktrees and their git branches.",
    )

    # Subcommand 'add'
    parser_add = subparsers.add_parser(
        "add",
        help="Add an active worktree checkout for development",
    )
    parser_add.add_argument("name", help="Name of worktree / branch")
    parser_add.add_argument(
        "--sync", action="store_true", help="Sync after adding"
    )
    parser_add.add_argument(
        "--pool-name", help="Specific pool slot to allocate"
    )
    parser_add.add_argument("--json", action="store_true", help="Output JSON")

    # Subcommand 'remove'
    parser_remove = subparsers.add_parser(
        "remove",
        help="Remove an active worktree and return it to the pool",
    )
    parser_remove.add_argument("name", help="Name of worktree to remove")

    args = parser.parse_args()
    pool = WorktreePool()

    try:
        if args.subcommand == "pool":
            if args.pool_subcommand == "list":
                pool_list_cmd.run(pool)
            elif args.pool_subcommand == "add":
                pool_add_cmd.run(args, pool)
            elif args.pool_subcommand == "remove":
                pool_remove_cmd.run(args, pool)
        elif args.subcommand == "locate":
            locate_cmd.run(args, pool)
        elif args.subcommand == "list":
            list_cmd.run(args, pool)
        elif args.subcommand == "add":
            add_cmd.run(args, pool)
        elif args.subcommand == "remove":
            remove_cmd.run(args, pool)
        else:
            print(f"Unknown subcommand: {args.subcommand}", file=sys.stderr)
            sys.exit(1)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
