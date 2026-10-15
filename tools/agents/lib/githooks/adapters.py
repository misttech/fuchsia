# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia platform hook action adapters.

Integrates Fuchsia platform formatting and commit message checking
with the generic git_staging engine.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path

from agents.lib import git_staging
from agents.lib.git_staging.engine import run_cmd
from agents.lib.githooks.reporters import ConsoleReporter
from agents.lib.paths import find_fuchsia_dir

DEFAULT_FORMATTABLE_EXTENSIONS: tuple[str, ...] = (
    ".py",
    ".md",
    ".c",
    ".cc",
    ".cpp",
    ".h",
    ".hh",
    ".hpp",
    ".rs",
    ".go",
    ".fidl",
    ".gn",
    ".gni",
    ".json",
    ".json5",
    ".cml",
    ".proto",
    ".ts",
)

# Linters that are skipped on every commit (both human and agent).
# - `clang_tidy` takes ~10-20s per C++ file, does not emit auto-fixes in shac,
#   and only emits non-blocking warnings.
# - `python_format` takes ~1.1s per Python file and already runs during the
#   preceding `fx format-code` (`shac fmt`) pass.
ALWAYS_SKIPPED_LINTERS: tuple[str, ...] = ("clang_tidy", "python_format")

# Linters that are prohibitively slow to run on every interactive commit.
# `check_licenses` can take over a minute when it is triggered, so human
# commits skip it and rely on CQ. Agents still run it so that automated
# changes are CQ-clean before upload.
HUMAN_SKIPPED_LINTERS: tuple[str, ...] = ("check_licenses",)


@dataclass(frozen=True)
class HookContext(git_staging.ActionContext):
    """Action context for Fuchsia git hooks including agent execution flag."""

    is_agent: bool = False
    reporter: ConsoleReporter = field(default_factory=ConsoleReporter)


@dataclass(frozen=True)
class HookAction:
    """Action executed on staged or checked repository files.

    Attributes:
        name: Identifier for the action.
        action_fn: Callable[[HookContext, Sequence[str]], bool]
        is_mutating: Whether the action modifies files on disk.
        extensions: File extensions to filter for (None matches all files).
        remediation_cmd_fn: Optional callable to generate remediation commands.
    """

    name: str
    action_fn: Callable[[HookContext, Sequence[str]], bool]
    is_mutating: bool
    extensions: Sequence[str] | None = None
    remediation_cmd_fn: Callable[[Sequence[str]], Sequence[str]] | None = None


def _resolve_fx_cmd(context: HookContext, purpose: str) -> str | None:
    """Resolves the `fx` executable, reporting to the console when missing.

    Prefers the in-tree `scripts/fx` over any `fx` on `PATH` so hooks always
    run the tooling belonging to the checkout being committed to.

    Args:
        context: HookContext with repo_root, is_agent, and reporter.
        purpose: Description of the skipped work, used in the warning message.

    Returns:
        Path to the `fx` executable, or None if it could not be resolved.
    """
    try:
        candidate = find_fuchsia_dir(context.repo_root) / "scripts" / "fx"
        if candidate.is_file():
            return str(candidate)
    except RuntimeError:
        pass

    if shutil.which("fx"):
        return "fx"

    if context.is_agent:
        context.reporter.on_error(
            "`fx` command not found on PATH or in scripts/fx in agent environment."
        )
    else:
        context.reporter.on_warning(
            f"`fx` not found on PATH or in scripts/fx. Skipping {purpose}."
        )
    return None


def _run_reporting_errors(
    context: HookContext,
    cmd: Sequence[str],
) -> subprocess.CompletedProcess[str] | None:
    """Runs a command, reporting its combined output as an error on failure.

    Args:
        context: HookContext with repo_root and reporter.
        cmd: Command and arguments to execute from the repository root.

    Returns:
        The completed process on success, or None if the command failed.
    """
    res = run_cmd(cmd, cwd=context.repo_root)
    if res.returncode == 0:
        return res

    output = "\n".join(filter(None, [res.stdout.strip(), res.stderr.strip()]))
    if not output:
        output = f"Command {cmd[0]} failed with exit code {res.returncode}"
    context.reporter.on_error(output)
    return None


def format_code_action(
    context: HookContext,
    files: Sequence[str],
) -> bool:
    """Executes fx format-code on target files.

    Args:
        context: HookContext with repo_root, check_only, is_agent, and reporter.
        files: Sequence of file paths relative to repo_root.

    Returns:
        True if formatting succeeded or fail-opened, False otherwise.
    """
    if not files:
        return True

    fx_cmd = _resolve_fx_cmd(context, "code formatting")
    if fx_cmd is None:
        return not context.is_agent

    cmd = [fx_cmd, "format-code", f"--files={','.join(files)}"]
    if _run_reporting_errors(context, cmd) is None:
        return False

    if context.check_only:
        diff_res = run_cmd(
            ["git", "diff", "--quiet", "--", *files],
            cwd=context.repo_root,
        )
        if diff_res.returncode != 0:
            return False

    return True


def lint_code_action(
    context: HookContext,
    files: Sequence[str],
) -> bool:
    """Executes fx lint on target files.

    Outside of check-only runs the linters are first invoked with `--fix` so
    auto-fixable findings are applied in place and re-staged by the pipeline.
    That pass is advisory only: `fx lint --fix` delegates to `shac fix`, which
    skips formatter checks and exits 0 even when it leaves findings behind. A
    second check-mode pass is what actually gates the commit.

    Partially staged files are linted in check-only mode under stash isolation
    to avoid clobbering unstaged hunks.

    `ALWAYS_SKIPPED_LINTERS` (e.g. `clang_tidy`) are skipped on all commits.
    Human commits additionally skip the linters in `HUMAN_SKIPPED_LINTERS` to
    keep interactive commit latency low, while agents run all remaining linters
    so automated changes match what CQ enforces.

    Args:
        context: HookContext with repo_root, check_only, is_agent, and reporter.
        files: Sequence of file paths relative to repo_root.

    Returns:
        True if lint checks passed, False otherwise.
    """
    if not files:
        return True

    fx_cmd = _resolve_fx_cmd(context, "lint checks")
    if fx_cmd is None:
        return not context.is_agent

    skipped = (
        ALWAYS_SKIPPED_LINTERS
        if context.is_agent
        else (*HUMAN_SKIPPED_LINTERS, *ALWAYS_SKIPPED_LINTERS)
    )
    base_cmd = [fx_cmd, "lint", f"--skip={','.join(skipped)}"]
    files_arg = f"--files={','.join(files)}"

    if not context.check_only:
        # Advisory pass: apply whatever `shac fix` can repair. Its exit code is
        # deliberately ignored because it reports success even when findings
        # remain; the check pass below is the real gate.
        run_cmd([*base_cmd, "--fix", files_arg], cwd=context.repo_root)

    return _run_reporting_errors(context, [*base_cmd, files_arg]) is not None


DEFAULT_PRE_COMMIT_ACTIONS: tuple[HookAction, ...] = (
    HookAction(
        name="fx_format_code",
        action_fn=format_code_action,
        is_mutating=True,
        extensions=DEFAULT_FORMATTABLE_EXTENSIONS,
        remediation_cmd_fn=lambda files: [
            f"fx format-code --files={','.join(files)}"
        ],
    ),
    HookAction(
        name="fx_lint",
        action_fn=lint_code_action,
        is_mutating=True,
        extensions=DEFAULT_FORMATTABLE_EXTENSIONS,
        remediation_cmd_fn=lambda files: [f"fx lint --files={','.join(files)}"],
    ),
)


def commit_msg_action(
    context: HookContext,
    files: Sequence[str],
) -> bool:
    """Executes commit_msg_checker.py on a commit message file.

    Args:
        context: HookContext with repo_root, is_agent, and reporter.
        files: Sequence of file paths containing the commit message file.

    Returns:
        True if commit message check passed, False otherwise.
    """
    if not files:
        return True

    reporter = context.reporter
    is_agent = context.is_agent

    msg_file = Path(files[0])
    try:
        fuchsia_dir = find_fuchsia_dir(context.repo_root)
    except RuntimeError:
        fuchsia_dir = context.repo_root

    checker_script = fuchsia_dir / "scripts" / "shac" / "commit_msg_checker.py"
    if not checker_script.is_file():
        if is_agent:
            reporter.on_error(
                f"Commit message checker script not found: {checker_script}"
            )
            return False
        reporter.on_warning(
            f"Commit message checker script not found: {checker_script}. Skipping check."
        )
        return True

    python_bin = fuchsia_dir / "scripts" / "fuchsia-vendored-python"
    py_exec = str(python_bin) if python_bin.is_file() else sys.executable

    cmd = [py_exec, str(checker_script), "--message-file", str(msg_file)]
    if is_agent:
        cmd.append("--strict")

    res = _run_reporting_errors(context, cmd)
    if res is None:
        return False

    # In non-strict mode (human developer), surface advisory warnings if any
    output = res.stdout.strip()
    if output and not is_agent:
        reporter.on_warning(output)

    return True
