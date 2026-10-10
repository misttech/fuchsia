#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs static analysis checks on modified files via SHAC."""

import argparse
import pathlib
import sys
from collections.abc import Collection, Sequence

import shac_runner

_SCRIPT_DIR = pathlib.Path(__file__).resolve().parent


def _parse_args(
    argv: Sequence[str],
) -> tuple[argparse.Namespace, Sequence[str]]:
    """Parses command-line arguments for `fx lint`.

    Args:
        argv: Command-line arguments.

    Returns:
        A tuple of `(parsed_args, extra_shac_args)`.
    """
    parser = argparse.ArgumentParser(
        prog="fx lint",
        description="Runs static analysis checks on modified files via SHAC.",
    )
    parser.add_argument(
        "--fix",
        action="store_true",
        help="Apply automated fixes emitted by checks (runs 'shac fix').",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Run checks across all tracked files in the repository.",
    )
    parser.add_argument(
        "--only",
        action="append",
        default=[],
        help="Run only the specified check(s) (comma-separated or repeated).",
    )
    parser.add_argument(
        "--skip",
        action="append",
        default=[],
        help="Skip the specified check(s) (comma-separated or repeated).",
    )
    # TODO(https://fxbug.dev/562256780): Remove after updating skills to use --only=commit_msg.
    parser.add_argument(
        "--commit-msg",
        action="store_true",
        help="Lint commit message (alias for --only=commit_msg).",
    )
    parser.add_argument(
        "--files",
        dest="explicit_files",
        action="append",
        default=[],
        help="Comma-separated list of specific files to lint.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print verbose command execution output.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print shac commands without executing.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Specific files to lint (defaults to git modified files).",
    )

    if "--" in argv:
        sep_index = argv.index("--")
        args, extra_args = parser.parse_known_args(argv[:sep_index])
        return args, (*extra_args, *argv[sep_index + 1 :])

    args, extra_args = parser.parse_known_args(argv)
    return args, tuple(extra_args)


def main(argv: Sequence[str]) -> None:
    """Parses CLI arguments for `fx lint` and runs SHAC checks.

    Args:
        argv: Command-line arguments.
    """
    args, extra_args = _parse_args(argv)
    fuchsia_dir, build_dir, cwd = shac_runner.resolve_workspace_paths(
        _SCRIPT_DIR.parents[1]
    )

    only_checks = shac_runner.split_comma_separated(args.only)
    if args.commit_msg and "commit_msg" not in only_checks:
        only_checks = (*only_checks, "commit_msg")
    skip_checks = shac_runner.split_comma_separated(args.skip)

    explicit_files: Collection[str] | None
    if args.files or args.explicit_files:
        explicit_files = (
            *args.files,
            *shac_runner.split_comma_separated(args.explicit_files),
        )
    else:
        explicit_files = None

    subcommand = "fix" if args.fix else "check"
    shac_runner.run_shac(
        subcommand,
        fuchsia_dir,
        build_dir,
        cwd,
        all_files=args.all,
        only_checks=only_checks,
        skip_checks=skip_checks,
        explicit_files=explicit_files,
        extra_args=extra_args,
        verbose=args.verbose,
        dry_run=args.dry_run,
    )


if __name__ == "__main__":
    main(sys.argv[1:])
