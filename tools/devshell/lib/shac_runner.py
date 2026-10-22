# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common SHAC execution helpers for fx lint and fx format-code."""

import os
import pathlib
import shutil
import subprocess
import sys
from collections.abc import Collection, Sequence

import scm


def _resolve_shac_binary() -> pathlib.Path:
    """Locates the prebuilt shac binary from PREBUILT_SHAC or PATH.

    Returns:
        Path to the shac executable.

    Raises:
        FileNotFoundError: If PREBUILT_SHAC points to a non-existent file or
            shac cannot be found on PATH.
    """
    if prebuilt_shac := os.environ.get("PREBUILT_SHAC"):
        candidate = pathlib.Path(prebuilt_shac)
        if candidate.is_file():
            return candidate
        raise FileNotFoundError(
            f"shac binary not found at PREBUILT_SHAC={prebuilt_shac}"
        )
    if (which_shac := shutil.which("shac")) is not None:
        return pathlib.Path(which_shac)
    raise FileNotFoundError("shac binary not found.")


def _validate_explicit_files(
    explicit_files: Collection[str],
    fuchsia_dir: pathlib.Path,
    cwd: pathlib.Path,
) -> Sequence[str]:
    """Validates explicit file paths and prints an error for missing files."""
    valid_files = []
    for raw_path in explicit_files:
        stripped_path = raw_path.strip()
        if not stripped_path:
            continue
        absolute_path = scm.canonicalize_path(stripped_path, cwd, fuchsia_dir)
        if not absolute_path.is_file():
            print(f"ERROR: no such file: {absolute_path}", file=sys.stderr)
            continue
        valid_files.append(stripped_path)
    return tuple(valid_files)


def split_comma_separated(
    entries: Collection[str],
) -> Sequence[str]:
    """Splits repeatable comma-separated CLI flag entries into stripped items.

    Args:
        entries: Collection of raw CLI flag values (each possibly
            comma-separated).

    Returns:
        A sequence of non-empty stripped strings in order of appearance.
    """
    return tuple(
        stripped
        for entry in entries
        for item in entry.split(",")
        if (stripped := item.strip())
    )


def resolve_workspace_paths(
    default_fuchsia_dir: pathlib.Path,
) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
    """Resolves FUCHSIA_DIR, FUCHSIA_BUILD_DIR, and current working directory.

    Args:
        default_fuchsia_dir: Fallback Fuchsia root directory when FUCHSIA_DIR
            is not set in the environment.

    Returns:
        A tuple of (fuchsia_dir, build_dir, cwd) as resolved absolute paths.
    """
    if (fuchsia_dir_env := os.environ.get("FUCHSIA_DIR")) is not None:
        fuchsia_dir = pathlib.Path(fuchsia_dir_env).resolve()
    else:
        fuchsia_dir = default_fuchsia_dir.resolve()

    if (
        fuchsia_build_dir_env := os.environ.get("FUCHSIA_BUILD_DIR")
    ) is not None:
        build_dir = pathlib.Path(fuchsia_build_dir_env).resolve()
    else:
        build_dir = fuchsia_dir / "out" / "default"

    cwd = pathlib.Path.cwd().resolve()
    return (fuchsia_dir, build_dir, cwd)


def _build_shac_command(
    shac_binary: pathlib.Path,
    subcommand: str,
    group: scm.RepoGroup,
    fuchsia_dir: pathlib.Path,
    build_dir: pathlib.Path,
    *,
    all_files: bool,
    only_checks: Collection[str],
    skip_checks: Collection[str],
    extra_args: Collection[str],
    verbose: bool,
) -> Sequence[str]:
    """Builds the shac command-line invocation for a single repository group."""
    relative_fuchsia_dir = os.path.relpath(fuchsia_dir, group.repo_root)
    relative_build_dir = os.path.relpath(build_dir, fuchsia_dir)

    command = [
        str(shac_binary),
        subcommand,
        "-C",
        str(group.repo_root),
        f"--var=fuchsia_dir={relative_fuchsia_dir}",
        f"--var=fuchsia_build_dir={relative_build_dir}",
    ]
    if all_files:
        command.append("--all")
    if verbose:
        command.append("--verbose")
    else:
        command.append("--quiet")
    if only_checks:
        command.append(f"--only={','.join(only_checks)}")
    if skip_checks:
        command.append(f"--skip={','.join(skip_checks)}")
    if extra_args:
        command.extend(extra_args)
    if group.files:
        command.extend(group.files)
    return tuple(command)


def run_shac(
    subcommand: str,
    fuchsia_dir: pathlib.Path,
    build_dir: pathlib.Path,
    cwd: pathlib.Path,
    *,
    all_files: bool = False,
    only_checks: Collection[str] = (),
    skip_checks: Collection[str] = (),
    explicit_files: Collection[str] | None = None,
    extra_args: Collection[str] = (),
    verbose: bool = False,
    dry_run: bool = False,
) -> None:
    """Runs a shac subcommand ('check', 'fix', or 'fmt') across repository groups.

    Args:
        subcommand: The SHAC subcommand to execute ('check', 'fix', or 'fmt').
        fuchsia_dir: Absolute path to the Fuchsia checkout root.
        build_dir: Path to the active Fuchsia build directory.
        cwd: Current working directory for resolving relative paths.
        all_files: If True, passes '--all' to SHAC to check all tracked files.
        only_checks: Collection of check names to pass via '--only'.
        skip_checks: Collection of check names to pass via '--skip'.
        explicit_files: Optional collection of explicit file paths to check, or
            None to run across all active repositories without file filtering.
            If an empty collection (or if all specified files are missing),
            returns without invoking SHAC.
        extra_args: Collection of extra CLI arguments forwarded to SHAC.
        verbose: If True, passes '--verbose' to SHAC and logs command lines;
            otherwise passes '--quiet' to SHAC.
        dry_run: If True, prints the SHAC command lines without executing them.

    Raises:
        subprocess.CalledProcessError: If `shac` or `git` exits with a non-zero
            status code.
        FileNotFoundError: If the `shac` binary cannot be found or if a target
            path requires invoking `git` and the `git` executable cannot be
            found.
    """
    valid_files = None
    if explicit_files is not None:
        valid_files = _validate_explicit_files(explicit_files, fuchsia_dir, cwd)
        if not valid_files:
            return

    shac_binary = _resolve_shac_binary()

    shac_env = os.environ | {"FUCHSIA_DIR": str(fuchsia_dir)}

    repo_groups = scm.get_repo_groups(
        fuchsia_dir,
        cwd,
        explicit_files=valid_files,
    )

    for group in repo_groups:
        if not group.has_shac:
            continue

        command = _build_shac_command(
            shac_binary,
            subcommand,
            group,
            fuchsia_dir,
            build_dir,
            all_files=all_files,
            only_checks=only_checks,
            skip_checks=skip_checks,
            extra_args=extra_args,
            verbose=verbose,
        )

        if verbose:
            print(f"Running: {' '.join(command)}")

        if dry_run:
            print(f"[dry-run] Would execute: {' '.join(command)}")
            continue

        subprocess.run(command, cwd=group.repo_root, env=shac_env, check=True)
