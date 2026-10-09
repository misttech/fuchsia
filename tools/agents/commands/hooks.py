# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Hooks subcommand handler for managing Fuchsia git hooks."""

from __future__ import annotations

import argparse
import pathlib
import sys

from agents.lib import githooks, paths


def _format_hook_count(count: int) -> str:
    return f"{count} Git hook{'s' if count != 1 else ''}"


def _report_hook_results(
    count: int,
    failed: list[tuple[pathlib.Path, str]],
    action_verb: str,
    past_verb: str,
    dry_run: bool,
    has_repos: bool = True,
) -> None:
    prefix = "[DRY RUN] " if dry_run else ""
    if count:
        verb = f"Would {action_verb}" if dry_run else past_verb
        msg = _format_hook_count(count)
        print(f"{prefix}{verb} {msg} across checkout.")
    elif not has_repos:
        print(f"{prefix}No Git repositories found across checkout.")
    elif not failed:
        if action_verb == "remove":
            print(f"{prefix}No Git hooks found to remove.")
        else:
            print(f"{prefix}No Git hooks configured.")
    if failed:
        for repo, err in failed:
            print(
                f"Warning: Failed to {action_verb} Git hooks in {repo}: {err}",
                file=sys.stderr,
            )


def run_enable(args: argparse.Namespace) -> int:
    """Install and configure Git hooks across checkout repositories."""
    fuchsia_dir = paths.find_fuchsia_dir()
    installed = githooks.install_git_hooks(fuchsia_dir, dry_run=args.dry_run)
    has_repos = bool(paths.find_checkout_git_repos(fuchsia_dir))
    _report_hook_results(
        count=len(installed.modified),
        failed=installed.failed,
        action_verb="configure",
        past_verb="Configured",
        dry_run=args.dry_run,
        has_repos=has_repos,
    )
    return 1 if installed.failed else 0


def run_disable(args: argparse.Namespace) -> int:
    """Uninstall Git hooks across checkout repositories."""
    fuchsia_dir = paths.find_fuchsia_dir()
    uninstalled = githooks.uninstall_git_hooks(
        fuchsia_dir, dry_run=args.dry_run
    )
    has_repos = bool(paths.find_checkout_git_repos(fuchsia_dir))
    _report_hook_results(
        count=len(uninstalled.modified),
        failed=uninstalled.failed,
        action_verb="remove",
        past_verb="Removed",
        dry_run=args.dry_run,
        has_repos=has_repos,
    )
    return 1 if uninstalled.failed else 0


def run_status(args: argparse.Namespace) -> int:
    """Display current Git hooks configuration status."""
    fuchsia_dir = paths.find_fuchsia_dir()
    hook_status = githooks.get_git_hooks_status(fuchsia_dir)
    repo_label = (
        "repository" if hook_status.total_repos == 1 else "repositories"
    )
    print(
        f"Git hooks: Configured across {hook_status.configured_repos}/{hook_status.total_repos} {repo_label}."
    )
    return 0


def _hooks_help(parser: argparse.ArgumentParser) -> int:
    parser.print_help(sys.stderr)
    return 1


def register_subcommand(
    subparsers: argparse._SubParsersAction[argparse.ArgumentParser],
) -> argparse.ArgumentParser:
    """Register the hooks subcommand group with the top-level argument parser."""
    parser = subparsers.add_parser(
        "hooks",
        help="Manage Fuchsia git hooks for AI coding agents.",
        description="Manage Fuchsia git hooks for AI coding agents.",
    )
    hooks_subparsers = parser.add_subparsers(
        dest="hooks_subcommand",
        metavar="COMMAND",
        help="Hook command to execute (enable, disable, status).",
    )

    enable_parser = hooks_subparsers.add_parser(
        "enable",
        help="Install and configure Git hooks across checkout repositories.",
        description="Install and configure Git hooks across checkout repositories.",
    )
    enable_parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print changes without modifying files.",
    )
    enable_parser.set_defaults(func=run_enable)

    disable_parser = hooks_subparsers.add_parser(
        "disable",
        help="Uninstall Git hooks across checkout repositories.",
        description="Uninstall Git hooks across checkout repositories.",
    )
    disable_parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print changes without modifying files.",
    )
    disable_parser.set_defaults(func=run_disable)

    status_parser = hooks_subparsers.add_parser(
        "status",
        help="Display current Git hooks configuration status.",
        description="Display current Git hooks configuration status.",
    )
    status_parser.set_defaults(func=run_status)

    parser.set_defaults(func=lambda args: _hooks_help(parser))
    return parser
