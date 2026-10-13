#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Checks affected files for direct //prebuilt references outside allowed rules."""

from __future__ import annotations

import argparse
import dataclasses
import json
import pathlib
import subprocess
import sys
from collections.abc import Iterable, Mapping, Sequence

import languages

ALLOWLIST_RELATIVE_PATH = (
    "build/prebuilt/on_demand_migration/shac/direct_prebuilt_allowlist.json"
)


@dataclasses.dataclass(frozen=True)
class Finding:
    """Single SHAC violation finding for an unmigrated direct prebuilt path.

    Attributes:
        filepath: Repository-relative path of the file with the violation.
        line: 1-indexed line number of the direct prebuilt reference.
        col: 1-indexed column number of the direct prebuilt reference.
        message: Actionable remediation guidance tailored to the file type.
        level: Finding severity level (defaults to `"error"`).
    """

    filepath: str
    line: int
    col: int
    message: str
    level: str = "error"


def is_checked_file(path: str) -> bool:
    """Returns True if `path` should be checked for direct prebuilt references."""
    return languages.match_language_family(path) is not None


def find_direct_prebuilt_lines(
    path: str, content: str
) -> Sequence[tuple[int, int]]:
    """Returns 1-indexed `(line, col)` pairs for direct prebuilt references.

    Args:
        path: Repository-relative file path determining syntax rules to apply.
        content: Full text contents of the file to inspect.

    Returns:
        A sequence of 1-indexed `(line_num, col_num)` pairs for each unmigrated
        direct prebuilt reference found in `content`.
    """
    if family := languages.match_language_family(path):
        return family.find_matches(content)
    return ()


def _remediation_message(family: languages.LanguageFamily) -> str:
    """Returns the full remediation message for `family`."""
    return (
        "Direct //prebuilt reference is not allowed "
        f"(exceeds baseline in {ALLOWLIST_RELATIVE_PATH}). "
        f"{family.remediation_suffix}"
    )


def _load_allowlist(allowlist_path: pathlib.Path) -> Mapping[str, int]:
    """Loads per-file baseline counts, or returns an empty mapping if absent."""
    try:
        return json.loads(allowlist_path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}


def _scan_file(
    root: pathlib.Path, relative_path: str
) -> tuple[languages.LanguageFamily, Sequence[tuple[int, int]]] | None:
    """Matches language family and scans `relative_path` under `root`."""
    family = languages.match_language_family(relative_path)
    if family is None:
        return None
    try:
        content = (root / relative_path).read_text(
            encoding="utf-8", errors="ignore"
        )
    except OSError:
        return None
    return family, family.find_matches(content)


def check_files(
    root: pathlib.Path,
    paths: Iterable[str],
    allowlist_path: pathlib.Path,
) -> Sequence[Finding]:
    """Checks candidate files under `root` against `allowlist_path`.

    Args:
        root: Repository root directory containing candidate files.
        paths: Repository-relative file paths to inspect.
        allowlist_path: Path to the JSON file mapping relative file paths to
            allowed baseline reference counts.

    Returns:
        A sequence of `Finding` objects for all direct prebuilt references
        exceeding each file's recorded baseline allowance.
    """
    allowlist = _load_allowlist(allowlist_path)
    return tuple(
        Finding(
            filepath=relative_path,
            line=line_num,
            col=col_num,
            message=_remediation_message(family),
        )
        for relative_path in paths
        if (scanned := _scan_file(root, relative_path)) is not None
        for family, matches in (scanned,)
        for line_num, col_num in matches[allowlist.get(relative_path, 0) :]
    )


def _candidate_repo_files(repo_dir: pathlib.Path) -> Sequence[str]:
    """Returns candidate tracked paths in `repo_dir` that may reference prebuilts."""
    tracked = subprocess.check_output(
        ["git", "-C", str(repo_dir), "ls-files"], text=True
    ).splitlines()
    if len(tracked) < 500:
        return tracked

    completed = subprocess.run(
        [
            "git",
            "-C",
            str(repo_dir),
            "grep",
            "-I",
            "-l",
            "-E",
            r"(prebuilts?/|PREBUILT_)",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    candidates = set(completed.stdout.splitlines())
    staged_lines = subprocess.check_output(
        ["git", "-C", str(repo_dir), "ls-files", "-s"], text=True
    ).splitlines()
    for staged_line in staged_lines:
        if staged_line.startswith("120000 ") and "\t" in staged_line:
            candidates.add(staged_line.split("\t", 1)[1])
    return sorted(candidates)


def _scan_git_repo(repo_dir: pathlib.Path) -> Mapping[str, int]:
    """Scans tracked checked files in `repo_dir` and returns nonzero match counts."""
    counts = {}
    for relative_path in _candidate_repo_files(repo_dir):
        if (scanned := _scan_file(repo_dir, relative_path)) is None:
            continue
        _, matches = scanned
        if line_count := len(matches):
            counts[relative_path] = line_count
    return counts


def update_allowlists(fuchsia_dir: pathlib.Path) -> None:
    """Regenerates allowlists for both `fuchsia.git` and `vendor/google`.

    Args:
        fuchsia_dir: Root directory of the Fuchsia checkout.

    Raises:
        subprocess.CalledProcessError: If listing tracked git files fails.
        OSError: If writing the allowlist JSON file(s) fails.
    """
    fuchsia_counts = _scan_git_repo(fuchsia_dir)
    root_allowlist = fuchsia_dir / ALLOWLIST_RELATIVE_PATH
    root_allowlist.parent.mkdir(parents=True, exist_ok=True)
    root_allowlist.write_text(
        json.dumps(fuchsia_counts, indent=4, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    vendor_dir = fuchsia_dir / "vendor" / "google"
    if vendor_dir.is_dir():
        vendor_counts = _scan_git_repo(vendor_dir)
        vendor_allowlist = vendor_dir / ALLOWLIST_RELATIVE_PATH
        vendor_allowlist.parent.mkdir(parents=True, exist_ok=True)
        vendor_allowlist.write_text(
            json.dumps(vendor_counts, indent=4, sort_keys=True) + "\n",
            encoding="utf-8",
        )


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entrypoint for checking files or regenerating direct-prebuilt allowlists.

    Args:
        argv: Command-line arguments to parse, or `None` to read `sys.argv[1:]`.

    Returns:
        Exit status code (`0` when compliant or updated, `1` when violations exist).
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        default=None,
        help="Repository root directory.",
    )
    parser.add_argument(
        "--allowlist",
        type=pathlib.Path,
        default=None,
        help="Optional override path to the allowlist JSON file.",
    )
    parser.add_argument(
        "--update-allowlists",
        action="store_true",
        help="Regenerate allowlist JSON files in fuchsia and vendor/google.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output findings as JSON.",
    )
    parser.add_argument(
        "--files-from",
        type=pathlib.Path,
        default=None,
        help="Optional newline-separated file listing relative paths to inspect.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Repository-relative file paths to inspect.",
    )
    args = parser.parse_args(argv)

    if args.update_allowlists:
        fuchsia_root = args.root or pathlib.Path(__file__).resolve().parents[5]
        update_allowlists(fuchsia_root)
        return 0

    repo_root = args.root or pathlib.Path.cwd()
    allowlist_path = args.allowlist or (repo_root / ALLOWLIST_RELATIVE_PATH)
    candidate_files: Sequence[str] = args.files
    if args.files_from is not None:
        candidate_files = (
            *args.files_from.read_text(encoding="utf-8").splitlines(),
            *args.files,
        )
    findings = check_files(repo_root, candidate_files, allowlist_path)
    if args.json:
        print(json.dumps([dataclasses.asdict(finding) for finding in findings]))
    else:
        for finding in findings:
            print(
                f"{finding.filepath}:{finding.line}:{finding.col}: "
                f"{finding.level}: {finding.message}"
            )
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
