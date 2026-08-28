# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Setup subcommand handler for configuring AI coding agent permissions."""

from __future__ import annotations

import argparse
import pathlib

from agents.lib import (
    config,
    permissions,
    services,
    state,
)

DEFAULT_PROFILE = "local-changes"


def register_subcommand(
    subparsers: argparse._SubParsersAction[argparse.ArgumentParser],
) -> argparse.ArgumentParser:
    """Register the setup subcommand with the top-level argument parser."""
    parser = subparsers.add_parser(
        "setup",
        help="Configure AI coding agent permissions and profiles.",
        description="Configure AI coding agent permissions and profiles.",
    )
    add_arguments(parser)
    parser.set_defaults(func=run)
    return parser


def add_arguments(parser: argparse.ArgumentParser) -> None:
    """Add arguments for setup command to parser."""
    parser.add_argument(
        "-p",
        "--profile",
        choices=list(permissions.PROFILE_DEFINITIONS.keys()),
        help="Permission profile flavor (read-only, local-changes, external-changes, full-access). Defaults to local-changes.",
    )
    parser.add_argument(
        "--status",
        action="store_true",
        help="Display current agent configuration status, active profile, and rule counts.",
    )
    parser.add_argument(
        "--rollback",
        nargs="?",
        const=1,
        type=int,
        default=None,
        metavar="N",
        help="Roll back configuration to N setups ago (default: 1).",
    )
    parser.add_argument(
        "--reset",
        action="store_true",
        help="Purge all Fuchsia-managed rules from configuration while preserving user custom rules.",
    )
    parser.add_argument(
        "--state-dir",
        type=pathlib.Path,
        default=None,
        help="Custom path to state directory (default: ~/.local/share/Fuchsia/agents/setup).",
    )
    parser.add_argument(
        "-a",
        "--allow",
        action="append",
        default=[],
        help="Additional grant rule to ALLOW",
    )
    parser.add_argument(
        "-d",
        "--deny",
        action="append",
        default=[],
        help="Additional grant rule to DENY",
    )
    parser.add_argument(
        "-k",
        "--ask",
        action="append",
        default=[],
        help="Additional grant rule to ASK",
    )
    parser.add_argument(
        "--allow-list",
        action="append",
        default=[],
        type=pathlib.Path,
        help="Extra allowed list file",
    )
    parser.add_argument(
        "--deny-list",
        action="append",
        default=[],
        type=pathlib.Path,
        help="Extra denied list file",
    )
    parser.add_argument(
        "--ask-list",
        action="append",
        default=[],
        type=pathlib.Path,
        help="Extra ask list file",
    )
    parser.add_argument(
        "--config",
        type=pathlib.Path,
        default=None,
        help="Custom path to output Gemini configuration file (default: ~/.gemini/config/config.json)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print changes without modifying config or services.",
    )


def _restart_daemons(fuchsia_dir: pathlib.Path, dry_run: bool) -> None:
    """Discover and restart managed agent daemon services."""
    daemon_services = services.find_daemon_services(fuchsia_dir)
    services.restart_daemons(
        service_names=daemon_services,
        dry_run=dry_run,
    )


def run(args: argparse.Namespace) -> int:
    """Execute setup with parsed arguments."""
    fuchsia_dir = permissions.find_fuchsia_dir()
    state_dir = args.state_dir or state.get_default_state_dir()
    config_path = args.config or config.get_default_config_path()

    if args.status:
        status_text = state.format_status(
            config_path=config_path,
            state_path=state_dir / "state.json",
        )
        print(status_text)
        return 0

    if args.rollback is not None:
        steps = args.rollback
        success = state.rollback(
            config_path=config_path,
            state_path=state_dir / "state.json",
            backups_dir=state_dir / "backups",
            steps=steps,
            dry_run=args.dry_run,
        )
        if success:
            _restart_daemons(fuchsia_dir, args.dry_run)
        return 0 if success else 1

    if args.reset:
        success = state.reset(
            config_path=config_path,
            state_path=state_dir / "state.json",
            backups_dir=state_dir / "backups",
            dry_run=args.dry_run,
        )
        if success:
            _restart_daemons(fuchsia_dir, args.dry_run)
        return 0 if success else 1

    journal = state.load_state(state_dir / "state.json")

    selected_profile = args.profile
    has_explicit_rules = bool(
        args.allow
        or args.deny
        or args.ask
        or args.allow_list
        or args.deny_list
        or args.ask_list
    )

    if not selected_profile:
        if journal.active_profile:
            selected_profile = journal.active_profile
        elif not has_explicit_rules:
            selected_profile = DEFAULT_PROFILE

    allow_grants: list[str] = []
    deny_grants: list[str] = []
    ask_grants: list[str] = []

    if selected_profile:
        print(f"\nApplying permission profile: [{selected_profile}]")
        grants = permissions.load_profile_grants(fuchsia_dir, selected_profile)
        allow_grants.extend(grants.allow)
        deny_grants.extend(grants.deny)
        ask_grants.extend(grants.ask)

    for cmd in args.allow or []:
        allow_grants.extend(permissions.expand_command_variants(cmd))
    for f in args.allow_list or []:
        allow_grants.extend(permissions.read_command_list_file(f))

    for cmd in args.deny or []:
        deny_grants.extend(permissions.expand_command_variants(cmd))
    for f in args.deny_list or []:
        deny_grants.extend(permissions.read_command_list_file(f))

    for cmd in args.ask or []:
        ask_grants.extend(permissions.expand_command_variants(cmd))
    for f in args.ask_list or []:
        ask_grants.extend(permissions.read_command_list_file(f))

    success = config.apply_grants(
        config_path=config_path,
        allow=allow_grants,
        deny=deny_grants,
        ask=ask_grants,
        dry_run=args.dry_run,
        state_dir=state_dir,
        selected_profile=selected_profile or "",
    )
    if not success:
        return 1

    _restart_daemons(fuchsia_dir, args.dry_run)
    return 0
