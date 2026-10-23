# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Declarative CLI command schemas for PreToolUse policy evaluation.

`CommandSchema` already handles arbitrary unmodeled commands, subcommands,
short-flag bundles (`-abc`), and `--flag=val` / `--flag val` matching via
`CommandMatch.positionals` and `CommandMatch.unknown_flags`.

Explicit schemas in this module are strictly scoped to the two cases that
generic token matching cannot infer on its own for the rules in
`//.agents/config/permissions/`:
1. **Global prefix flags** on `git`, `fx`, `ffx`, and `jiri` (e.g. `git -C dir`,
   `fx -t dev`, `ffx -c k=v`, `jiri -j 8`) so value-taking global flags are
   consumed before identifying the target subcommand.
2. **Flag aliases and value-taking sibling flags** on commands whose flags are
   explicitly inspected by built-in permission policies (`git clean`,
   `git reset`, `git push`, `git config`, `rm`, and `sed`).
"""

from __future__ import annotations

from agents.lib.agent_hooks.engine.parser import CommandSchema, Flag, Subcommand


class GitSchema(CommandSchema):
    """Schema for `git` global flags and policy-gated subcommands.

    Backs `git clean -f`, `git reset --hard`, `git push --force`, and
    `git push --force-with-lease` in `never_allow.txt`, plus `git push` and
    `git config --global/--system/--file/-f` in `external_changes.txt`.
    """

    prog = "git"
    global_flags = [
        Flag(("-C",)),
        Flag(("-c",)),
        Flag(("--git-dir",)),
        Flag(("--work-tree",)),
        Flag(("--namespace",)),
        Flag(("--config-env",)),
        Flag(("--exec-path",), nargs="?"),
        Flag(("-p", "--paginate"), action="store_true"),
        Flag(("-P", "--no-pager"), action="store_true"),
        Flag(("--no-replace-objects",), action="store_true"),
        Flag(("--no-optional-locks",), action="store_true"),
        Flag(("--bare",), action="store_true"),
    ]
    subcommands = [
        Subcommand(
            "clean",
            flags=[
                Flag(("-f", "--force"), action="store_true"),
                Flag(("-d",), action="store_true"),
                Flag(("-n", "--dry-run"), action="store_true"),
                Flag(("-q", "--quiet"), action="store_true"),
                Flag(("-x",), action="store_true"),
                Flag(("-X",), action="store_true"),
                Flag(("-e", "--exclude"), action="append"),
            ],
        ),
        Subcommand(
            "reset",
            flags=[
                Flag(("--hard",), action="store_true"),
                Flag(("--soft",), action="store_true"),
                Flag(("--mixed",), action="store_true"),
                Flag(("--merge",), action="store_true"),
                Flag(("--keep",), action="store_true"),
                Flag(("-q", "--quiet"), action="store_true"),
            ],
        ),
        Subcommand(
            "push",
            flags=[
                Flag(("-f", "--force"), action="store_true"),
                Flag(("--force-with-lease",), action="append", nargs="?"),
                Flag(("--force-if-includes",), action="store_true"),
                Flag(("-d", "--delete"), action="store_true"),
                Flag(("-n", "--dry-run"), action="store_true"),
                Flag(("-u", "--set-upstream"), action="store_true"),
                Flag(("-q", "--quiet"), action="store_true"),
                Flag(("-v", "--verbose"), action="store_true"),
                Flag(("--repo",)),
                Flag(("-o", "--push-option"), action="append"),
            ],
        ),
        Subcommand(
            "config",
            flags=[
                Flag(("--global",), action="store_true"),
                Flag(("--system",), action="store_true"),
                Flag(("--local",), action="store_true"),
                Flag(("--worktree",), action="store_true"),
                Flag(("-f", "--file")),
                Flag(("--blob",)),
                Flag(("-l", "--list"), action="store_true"),
                Flag(("--get",), action="store_true"),
                Flag(("--get-all",), action="store_true"),
                Flag(("--get-regexp",), action="store_true"),
                Flag(("--unset",), action="store_true"),
            ],
        ),
    ]


class FxSchema(CommandSchema):
    """Schema for `fx` global prefix flags (`FX_GLOBAL_FLAGS_PATTERN`).

    Subcommands are intentionally omitted so all `fx <subcommand>` invocations
    (e.g. `fx flash`, `fx ota`, `fx clean`, or `fx ffx`/`fx jiri` delegation)
    are matched uniformly via `positionals` after stripping leading `fx` flags.
    """

    prog = "fx"
    strict_global_prefix = True
    global_flags = [
        Flag(("--dir",)),
        Flag(("-t", "--target")),
        Flag(("-i",), action="store_true"),
        Flag(("-x",), action="store_true"),
        Flag(("-xx",), action="store_true"),
        Flag(("--invoker",)),
        Flag(("--enable",)),
        Flag(("--disable",)),
    ]


class FfxSchema(CommandSchema):
    """Schema for `ffx` global flags (`FFX_GLOBAL_FLAGS_PATTERN`).

    Consumes global options (`-t`, `-c`, `-e`, `--isolate-dir`, etc.) so
    subcommands like `ffx target flash` and `ffx target reboot` in
    `device_ops.txt` resolve as leading `positionals`.
    """

    prog = "ffx"
    global_flags = [
        Flag(("--help",), action="store_true"),
        Flag(("-c", "--config")),
        Flag(("-e", "--env")),
        Flag(("--env-root",)),
        Flag(("--machine",)),
        Flag(("--schema",), action="store_true"),
        Flag(("--stamp",)),
        Flag(("-t", "--target")),
        Flag(("--timeout",)),
        Flag(("-l", "--log-level")),
        Flag(("--isolate-dir",)),
        Flag(("-v", "--verbose"), action="store_true"),
        Flag(("-o", "--log-output")),
        Flag(("--no-environment",), action="store_true"),
        Flag(("--strict",), action="store_true"),
        Flag(("--strict-equivalent",), action="store_true"),
        Flag(("-d", "--direct"), action="store_true"),
    ]


class JiriSchema(CommandSchema):
    """Schema for `jiri` global flags (`JIRI_GLOBAL_FLAGS_PATTERN`).

    Consumes global options (`-j`, `-root`, `-v`, etc.) so subcommands like
    `jiri upload` (`external_changes.txt`) and `jiri update` resolve as leading
    `positionals`.
    """

    prog = "jiri"
    global_flags = [
        Flag(("-color", "--color")),
        Flag(("-j", "--j")),
        Flag(("-progress-window", "--progress-window")),
        Flag(("-q", "--q"), action="store_true"),
        Flag(("-quiet", "--quiet"), action="store_true"),
        Flag(("-root", "--root")),
        Flag(("-show-progress", "--show-progress"), nargs="?"),
        Flag(("-time", "--time"), action="store_true"),
        Flag(("-time-log-threshold", "--time-log-threshold")),
        Flag(("-timefile", "--timefile")),
        Flag(("-v", "--v"), action="store_true"),
        Flag(("-vv", "--vv"), action="store_true"),
    ]


class RmSchema(CommandSchema):
    """Schema for `rm` flags backing `rm -rf /` and `rm -rf ~` in `never_allow.txt`."""

    prog = "rm"
    global_flags = [
        Flag(("-f", "--force"), action="store_true"),
        Flag(("-i",), action="store_true"),
        Flag(("-I",), action="store_true"),
        Flag(("--interactive",), nargs="?"),
        Flag(("--one-file-system",), action="store_true"),
        Flag(("--no-preserve-root",), action="store_true"),
        Flag(("--preserve-root",), nargs="?"),
        Flag(("-r", "-R", "--recursive"), action="store_true"),
        Flag(("-d", "--dir"), action="store_true"),
        Flag(("-v", "--verbose"), action="store_true"),
        Flag(("--help",), action="store_true"),
        Flag(("--version",), action="store_true"),
    ]


class SedSchema(CommandSchema):
    """Schema for `sed` flags backing in-place `sed -i` gating in `grant_policy.py`."""

    prog = "sed"
    global_flags = [
        Flag(("-n", "--quiet", "--silent"), action="store_true"),
        Flag(("--debug",), action="store_true"),
        Flag(("-e", "--expression")),
        Flag(("-f", "--file")),
        Flag(("--follow-symlinks",), action="store_true"),
        Flag(("-i", "--in-place"), nargs="?"),
        Flag(("-l", "--line-length")),
        Flag(("--posix",), action="store_true"),
        Flag(("-E", "-r", "--regexp-extended"), action="store_true"),
        Flag(("-s", "--separate"), action="store_true"),
        Flag(("--sandbox",), action="store_true"),
        Flag(("-u", "--unbuffered"), action="store_true"),
        Flag(("-z", "--null-data"), action="store_true"),
        Flag(("--help",), action="store_true"),
        Flag(("--version",), action="store_true"),
    ]


SCHEMAS_BY_PROG: dict[str, type[CommandSchema]] = {
    "ffx": FfxSchema,
    "fx": FxSchema,
    "git": GitSchema,
    "jiri": JiriSchema,
    "rm": RmSchema,
    "sed": SedSchema,
}
