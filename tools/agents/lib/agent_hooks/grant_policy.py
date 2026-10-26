# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Grant-based policy evaluation against config.json and state.json."""

import functools
import itertools
import json
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path

from agents.lib.agent_hooks.engine.parser import CommandMatch, CommandSchema
from agents.lib.agent_hooks.engine.router import PolicyResult
from agents.lib.agent_hooks.engine.shell_lexer import (
    strip_env_assignments,
    strip_redirections,
    tokenize_shell,
)
from agents.lib.agent_hooks.schemas import SCHEMAS_BY_PROG, FxSchema
from agents.lib.permissions import PYTHON_COMMAND_NAMES

_FX_DELEGATED_PROGS: frozenset[str] = frozenset({"ffx", "jiri", "cipd"})
_FX_VALUE_FLAGS: frozenset[str] = frozenset(
    itertools.chain.from_iterable(
        flag.flags
        for flag in FxSchema.global_flags
        if flag.action != "store_true"
    )
)
_FX_BOOL_FLAGS: frozenset[str] = frozenset(
    itertools.chain.from_iterable(
        flag.flags
        for flag in FxSchema.global_flags
        if flag.action == "store_true"
    )
)


def _unwrap_fx_delegated(tokens: Sequence[str]) -> list[str] | None:
    """Unwrap `fx [...global_flags...] <ffx|jiri|cipd> ...` into `[<prog>, ...]`."""
    if len(tokens) < 2 or Path(tokens[0]).name != "fx":
        return None

    idx = 1
    n = len(tokens)
    while idx < n:
        tok = tokens[idx]
        if tok == "--":
            idx += 1
            break
        if not tok.startswith("-"):
            break
        if "=" in tok:
            opt_name = tok.split("=", 1)[0]
            if (
                opt_name not in _FX_VALUE_FLAGS
                and opt_name not in _FX_BOOL_FLAGS
            ):
                return None
            idx += 1
        elif tok in _FX_VALUE_FLAGS:
            idx += 2
        elif tok in _FX_BOOL_FLAGS:
            idx += 1
        else:
            return None

    if idx < n and tokens[idx] in _FX_DELEGATED_PROGS:
        return list(tokens[idx:])
    return None


def _prog_matches(req: str, cand: str) -> bool:
    """Return True if candidate executable name matches rule executable."""
    if req in PYTHON_COMMAND_NAMES:
        return cand in PYTHON_COMMAND_NAMES
    if req == "mkfs":
        return cand == "mkfs" or cand.startswith("mkfs.")
    return cand == req


def _positional_matches(req: str, cand: str) -> bool:
    """Return True if candidate positional matches required rule positional."""
    if req == "/":
        return bool(re.fullmatch(r"/+\*?", cand))
    if req == "~":
        return bool(re.fullmatch(r"(?:~|\$(?:HOME|\{HOME\}))(?:/+\*?)?", cand))
    return req == cand


@dataclass(frozen=True)
class CommandRule:
    """Schema-driven token and flag matcher for a `command(...)` grant rule.

    Attributes:
        prog: Canonical executable name for the rule.
        rule_tokens: Stripped token tuple extracted from the grant rule.
    """

    prog: str
    rule_tokens: tuple[str, ...]

    @functools.cached_property
    def schema_cls(self) -> type[CommandSchema]:
        """`CommandSchema` class used to parse tokens for `self.prog`."""
        return SCHEMAS_BY_PROG.get(self.prog, CommandSchema)

    @functools.cached_property
    def rule_match(self) -> CommandMatch | None:
        """Optional `CommandMatch` parsed from `self.rule_tokens`."""
        return self.schema_cls.parse_tokens(
            self.rule_tokens, allow_dangling_flag_value=True
        )

    @classmethod
    @functools.lru_cache(maxsize=256)
    def from_grant(cls, grant_rule: str) -> CommandRule | None:
        """Parse a literal `command(<rule>)` grant into a `CommandRule`.

        Args:
            grant_rule: Grant string from `config.json` (`command(...)`).

        Returns:
            Parsed `CommandRule`, or `None` if `grant_rule` is not a literal
            command grant.
        """
        rule = grant_rule.strip()
        if not (rule.startswith("command(") and rule.endswith(")")):
            return None
        inner = rule[len("command(") : -1].strip()
        if not inner or inner.startswith("regex:"):
            return None
        tokens = strip_env_assignments(
            strip_redirections(tokenize_shell(inner))
        )
        if not tokens:
            return None
        tokens = _unwrap_fx_delegated(tokens) or tokens
        return cls(Path(tokens[0]).name, tuple(tokens))

    def matches_tokens(self, tokens: Sequence[str]) -> bool:
        """Return `True` if a tokenized subcommand matches this rule.

        Args:
            tokens: Stripped, tokenized candidate subcommand.

        Returns:
            `True` if the candidate invokes `self.prog` with all subcommands,
            flags, and positionals required by this rule.
        """
        if not tokens:
            return False

        cand_prog = Path(tokens[0]).name
        if self.prog in _FX_DELEGATED_PROGS and cand_prog == "fx":
            unwrapped = _unwrap_fx_delegated(tokens)
            if unwrapped and unwrapped[0] == self.prog:
                tokens = unwrapped
                cand_prog = self.prog

        if not _prog_matches(self.prog, cand_prog):
            return False

        # Fast-path filter: if the rule starts with a literal subcommand or
        # positional token (e.g. `clean` in `git clean -f`), skip schema
        # parsing when that token does not appear anywhere in `tokens`.
        if (
            len(self.rule_tokens) > 1
            and not self.rule_tokens[1].startswith("-")
            and self.rule_tokens[1] not in ("/", "~")
            and self.rule_tokens[1] not in tokens[1:]
        ):
            return False

        cand = self.schema_cls.parse_tokens(tokens)
        if (
            self.rule_match is None
            or cand is None
            or not cand.matches_flags_of(self.rule_match)
        ):
            return False
        req_pos = self.rule_match.positionals
        cand_pos = cand.positionals

        if len(cand_pos) < len(req_pos):
            return False
        for i, (req, cand_tok) in enumerate(zip(req_pos, cand_pos)):
            # Root and home targets (`/` and `~`) scan all remaining candidate
            # positionals so multi-path invocations like `rm -rf /tmp /` match.
            if req in ("/", "~"):
                if not any(_positional_matches(req, p) for p in cand_pos[i:]):
                    return False
            elif not _positional_matches(req, cand_tok):
                return False
        return True


def matches_grant_rule(
    cmd: str,
    grant_rule: str,
    *,
    tokens: Sequence[str] | None = None,
) -> bool:
    """Return `True` if a shell command matches a `command(...)` grant string.

    Supports both `command(regex:...)` anchored regular expressions and literal
    `command(<rule>)` specifications evaluated via `CommandRule`.

    Args:
        cmd: Single shell command string.
        grant_rule: Permission grant string (`command(...)`).
        tokens: Optional pre-tokenized `cmd` tokens.

    Returns:
        `True` if `cmd` matches `grant_rule`.
    """
    rule = grant_rule.strip()
    if not (rule.startswith("command(") and rule.endswith(")")):
        return False
    inner = rule[len("command(") : -1]
    if inner.startswith("regex:"):
        pattern = inner[len("regex:") :]
        try:
            if re.fullmatch(pattern, cmd):
                return True
            stripped = " ".join(strip_redirections(tokenize_shell(cmd)))
            return bool(stripped and re.fullmatch(pattern, stripped))
        except re.error:
            return False
    cmd_rule = CommandRule.from_grant(rule)
    if cmd_rule is None:
        return False
    resolved_tokens = (
        tokens
        if tokens is not None
        else strip_env_assignments(strip_redirections(tokenize_shell(cmd)))
    )
    return cmd_rule.matches_tokens(resolved_tokens)


@dataclass(frozen=True)
class GrantPolicyConfig:
    """Loaded permission grants for PreToolUse evaluation.

    Attributes:
        allow_grants: Tuple of `allow` grant strings from `config.json`.
        deny_grants: Tuple of `deny` grant strings from `config.json`.
        deny_reasons: Mapping from `command(<rule>)` grant string to agent
            feedback loaded from `state.json`.
    """

    allow_grants: tuple[str, ...] = ()
    deny_grants: tuple[str, ...] = ()
    deny_reasons: Mapping[str, str] = field(default_factory=dict)

    @classmethod
    def from_files(
        cls,
        config_path: Path | None = None,
        state_path: Path | None = None,
    ) -> GrantPolicyConfig:
        """Load permission grants from `config.json` and reasons from `state.json`.

        Args:
            config_path: Optional path to the agent `config.json` file.
            state_path: Optional path to the setup `state.json` file.

        Returns:
            `GrantPolicyConfig` populated with `allow`/`deny` grants and reasons.
        """
        reasons: dict[str, str] = {}
        if state_path and state_path.is_file():
            try:
                state_data = json.loads(state_path.read_text(encoding="utf-8"))
                reasons = dict(state_data.get("grant_reasons", {}))
            except (OSError, ValueError, TypeError, AttributeError):
                # Fall back to empty reasons on unreadable or malformed state.
                pass

        if config_path and config_path.is_file():
            try:
                data = json.loads(config_path.read_text(encoding="utf-8"))
                grants = data["userSettings"]["globalPermissionGrants"]
                return cls(
                    allow_grants=tuple(grants.get("allow", ())),
                    deny_grants=tuple(dict.fromkeys(grants.get("deny", ()))),
                    deny_reasons=reasons,
                )
            except (OSError, ValueError, KeyError, TypeError, AttributeError):
                # Fall back to empty grants on unreadable or malformed config.
                pass
        return cls(deny_reasons=reasons)

    def is_sed_inplace_allowed(self) -> bool:
        """Return True if an active allow grant explicitly permits `sed -i`."""
        # `read_only.txt` grants stream `sed` (`...sed\b(?:\s+.*)?`), whereas
        # `local_changes.txt` explicitly adds `sed -i` (`...\s+\-i...`).
        for grant in self.allow_grants:
            if "sed" in grant and ("-i" in grant or "--in-place" in grant):
                return True
        return False

    def audit_command(self, cmd: str, tokens: Sequence[str]) -> PolicyResult:
        """Evaluate a shell command against `deny` grants and `sed -i` policy."""
        for rule in self.deny_grants:
            if matches_grant_rule(cmd, rule, tokens=tokens):
                stripped = rule.strip()
                reason = self.deny_reasons.get(stripped)
                msg = (
                    f"PreToolUse Gate: Command '{cmd}' matches "
                    f"deny rule '{stripped}'."
                )
                return PolicyResult.deny(f"{msg} {reason}" if reason else msg)

        if (
            matches_grant_rule(cmd, "command(sed -i)", tokens=tokens)
            and not self.is_sed_inplace_allowed()
        ):
            return PolicyResult.force_ask(
                "PreToolUse Gate: In-place file modification "
                "('sed -i' / '--in-place') requires confirmation "
                "under the current permission profile. "
                "Prefer the 'edit_file' tool for modifying files."
            )
        return PolicyResult.allow()
