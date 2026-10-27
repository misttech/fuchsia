#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""CLI entrypoint for AST-based Bazel lint checks."""

import argparse
import importlib
import json
import sys
from pathlib import Path

from allowlist import Allowlist, AllowlistError, load_allowlist
from framework import (
    REGISTRY,
    BazelCheck,
    Finding,
    run_check_on_contents,
)

_DEFAULT_ALLOWLISTS_DIR = Path(__file__).resolve().parent / "allowlists"

# LINT.IfChange(exit_codes)
EXIT_OK = 0
EXIT_ERROR = 2
# A unique non-standard exit code for findings avoids colliding with standard
# exit codes such as 1 (uncaught Python exceptions) or 2 (argparse / runtime
# errors).
EXIT_FINDINGS = 42
# LINT.ThenChange(//scripts/shac/bazel.star:exit_codes)


def _resolve_allowlist_path(
    check_name: str,
    *,
    root: Path | None,
    allowlists_dir: Path | None,
) -> Path:
    """Resolves the allowlist JSON file path for `check_name`."""
    filename = f"{check_name}.json"
    if allowlists_dir is not None:
        return allowlists_dir / filename
    if root is not None:
        return (
            root / "scripts" / "shac" / "bazel_lint" / "allowlists" / filename
        )
    return _DEFAULT_ALLOWLISTS_DIR / filename


def _check_file(
    filepath: Path | str,
    *,
    check: BazelCheck,
    root: Path | None,
    allowlist: Allowlist | None,
) -> list[Finding]:
    """Checks a single file on disk against `check`."""
    path = Path(filepath)
    if not path.is_absolute() and root:
        actual_path = root / path
    else:
        actual_path = path

    if not actual_path.is_file():
        raise FileNotFoundError(f"File not found: {actual_path}")

    content = actual_path.read_bytes().decode("utf-8", errors="replace")

    if root and actual_path.is_relative_to(root):
        rel_path = actual_path.relative_to(root).as_posix()
    else:
        rel_path = path.as_posix()

    return run_check_on_contents(
        check,
        content,
        rel_path,
        allowlist=allowlist,
    )


def main(argv: list[str] | None = None) -> int:
    """CLI entrypoint."""

    # Import the checks package to trigger @register_check decorators before
    # constructing the argument parser and populating the --check choices.
    importlib.import_module("checks")

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        required=True,
        choices=sorted(REGISTRY.keys()),
        help="Name of the registered Bazel lint check to run.",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="Repository root directory against which relative paths are resolved.",
    )
    parser.add_argument(
        "--allowlists-dir",
        type=Path,
        default=None,
        help="Directory containing per-check allowlist JSON files.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output findings as a JSON array.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        type=Path,
        help="Files to check.",
    )
    args = parser.parse_args(argv)

    check = REGISTRY[args.check]
    root = args.root.resolve() if args.root else None
    all_findings: list[Finding] = []

    try:
        allowlist_path = _resolve_allowlist_path(
            check.name, root=root, allowlists_dir=args.allowlists_dir
        )
        if check.supports_allowlist:
            allowlist: Allowlist | None = load_allowlist(
                allowlist_path,
                supports_targets_allowlist=check.supports_targets_allowlist,
            )
        elif allowlist_path.is_file():
            raise AllowlistError(
                f"{allowlist_path}: allowlist file is not permitted for check "
                f"'{check.name}' (supports_allowlist = False)"
            )
        else:
            allowlist = None
        for file_path in args.files:
            all_findings.extend(
                _check_file(
                    file_path,
                    check=check,
                    root=root,
                    allowlist=allowlist,
                )
            )
    except Exception as e:
        print(f"Error checking files: {e}", file=sys.stderr)
        return EXIT_ERROR

    if args.json:
        print(json.dumps(all_findings, indent=2))
    else:
        for f in all_findings:
            line = f.get("line", 1)
            col = f.get("col", 1)
            print(
                f"{f['filepath']}:{line}:{col}: [{f['level']}] {f['message']}"
            )

    return EXIT_FINDINGS if all_findings else EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
