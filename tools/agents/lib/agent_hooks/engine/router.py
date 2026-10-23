# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Command and tool routing engine for agent hook policy evaluation."""

from __future__ import annotations

import enum
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Literal

from agents.lib.agent_hooks.engine.shell_lexer import (
    split_compound_commands,
    strip_env_assignments,
    strip_redirections,
    tokenize_shell,
)


@dataclass(frozen=True)
class PolicyResult:
    """Result of evaluating a command or tool call against policy rules.

    Attributes:
        decision: Hook decision (`"allow"`, `"deny"`, or `"force_ask"`).
        reason: Optional human-readable explanation for `"deny"`/`"force_ask"`.
    """

    decision: Literal["allow", "deny", "force_ask"]
    reason: str | None = None

    @classmethod
    def allow(cls) -> PolicyResult:
        """Return an `"allow"` decision."""
        return cls("allow")

    @classmethod
    def deny(cls, reason: str) -> PolicyResult:
        """Return a `"deny"` decision with the given reason.

        Args:
            reason: Explanation surfaced when blocking the tool call.

        Returns:
            `PolicyResult` with `decision="deny"`.
        """
        return cls("deny", reason)

    @classmethod
    def force_ask(cls, reason: str) -> PolicyResult:
        """Return a `"force_ask"` decision prompting the user for confirmation.

        Args:
            reason: Explanation surfaced in the confirmation prompt.

        Returns:
            `PolicyResult` with `decision="force_ask"`.
        """
        return cls("force_ask", reason)

    @classmethod
    def merge(cls, results: Iterable[PolicyResult]) -> PolicyResult:
        """Combine results with `deny > force_ask > allow` precedence.

        Short-circuits immediately on the first `"deny"` result; otherwise
        retains the first `"force_ask"` result or defaults to `"allow"`.

        Args:
            results: Iterable of `PolicyResult` evaluations.

        Returns:
            Highest-precedence `PolicyResult`.
        """
        pending_ask: PolicyResult | None = None
        for res in results:
            if res.decision == "deny":
                return res
            if res.decision == "force_ask" and pending_ask is None:
                pending_ask = res
        return pending_ask or cls.allow()

    def to_dict(self) -> dict[str, Any]:
        """Convert result to a JSON-serializable hook output dictionary.

        Returns:
            Dictionary containing `"decision"` and optional `"reason"`.
        """
        if self.reason:
            return {"decision": self.decision, "reason": self.reason}
        return {"decision": self.decision}


class ToolName(enum.StrEnum):
    """Canonical agent tool names for hook routing."""

    RUN_COMMAND = "run_command"


@dataclass(frozen=True)
class ToolCallContext:
    """Typed context for an agent tool invocation.

    Attributes:
        name: Tool name being invoked (e.g. `"run_command"`).
        args: Mapping of tool arguments passed by the agent.
    """

    name: str
    args: Mapping[str, Any]

    @classmethod
    def from_payload(cls, data: Mapping[str, Any]) -> ToolCallContext | None:
        """Parse a `toolCall` dictionary (`{"name": ..., "args": ...}`).

        Args:
            data: Raw `toolCall` mapping.

        Returns:
            Populated `ToolCallContext`, or `None` if required fields are
            missing or malformed.
        """
        if not isinstance(data, Mapping):
            return None
        name = data.get("name")
        args = data.get("args")
        if (
            not isinstance(name, str)
            or not name
            or not isinstance(args, Mapping)
        ):
            return None
        return cls(name=name, args=args)


@dataclass(frozen=True)
class HookPayload:
    """Parsed `PreToolUse` hook stdin payload.

    Attributes:
        tool_call: Parsed `ToolCallContext` for the invoked tool.
        transcript_path: Path to the active session transcript JSONL file.
        workspace_paths: Tuple of workspace root paths in the active session.
        conversation_id: Active conversation UUID.
    """

    tool_call: ToolCallContext
    transcript_path: str = ""
    workspace_paths: tuple[str, ...] = ()
    conversation_id: str = ""

    @classmethod
    def from_payload(cls, data: Mapping[str, Any]) -> HookPayload | None:
        """Parse a hook stdin JSON mapping or bare `toolCall` mapping.

        Args:
            data: Raw JSON mapping read from hook stdin.

        Returns:
            Populated `HookPayload`, or `None` if `toolCall` is missing or
            malformed.
        """
        if not isinstance(data, Mapping):
            return None
        raw_tc = data.get("toolCall")
        tc_mapping = raw_tc if isinstance(raw_tc, Mapping) else data
        tool_call = ToolCallContext.from_payload(tc_mapping)
        if tool_call is None:
            return None
        raw_ws = data.get("workspacePaths")
        workspace_paths = (
            tuple(str(p) for p in raw_ws)
            if isinstance(raw_ws, Sequence) and not isinstance(raw_ws, str)
            else ()
        )
        return cls(
            tool_call=tool_call,
            transcript_path=str(data.get("transcriptPath") or ""),
            workspace_paths=workspace_paths,
            conversation_id=str(data.get("conversationId") or ""),
        )


CommandCheckFn = Callable[[str, Sequence[str]], PolicyResult]
ToolHandlerFn = Callable[[ToolCallContext], PolicyResult]


class CommandRouter:
    """Router for evaluating decomposed shell commands against policy checks."""

    def __init__(self, checks: Sequence[CommandCheckFn] = ()) -> None:
        """Initialize the router with command check callbacks.

        Args:
            checks: Sequence of `(cmd, tokens) -> PolicyResult` callbacks.
        """
        self._checks: tuple[CommandCheckFn, ...] = tuple(checks)

    def evaluate(self, command_line: str) -> PolicyResult:
        """Decompose a shell command line and evaluate all registered checks.

        Splits `command_line` into individual commands (with heredoc bodies
        removed), tokenizes each command via `tokenize_shell`, strips
        redirections and leading environment/wrapper tokens, and merges all
        check results using `deny > force_ask > allow` precedence.

        Args:
            command_line: Raw shell command string from the tool call.

        Returns:
            Aggregated `PolicyResult` across all commands and checks.
        """

        def _iter_checks() -> Iterable[PolicyResult]:
            for cmd in split_compound_commands(command_line):
                tokens = tokenize_shell(cmd)
                cmd_tokens = strip_env_assignments(strip_redirections(tokens))
                if cmd_tokens:
                    for check_fn in self._checks:
                        yield check_fn(cmd, cmd_tokens)

        return PolicyResult.merge(_iter_checks())


class ToolRouter:
    """Route agent tool invocations by `ToolName` to policy handlers."""

    def __init__(self) -> None:
        """Initialize an empty tool router."""
        self._handlers: dict[str, list[ToolHandlerFn]] = {}

    def register(self, tool_name: ToolName, fn: ToolHandlerFn) -> None:
        """Register a policy handler for a `ToolName`.

        Args:
            tool_name: Target `ToolName` to intercept.
            fn: Handler accepting a `ToolCallContext` and returning a
                `PolicyResult`.
        """
        self._handlers.setdefault(tool_name.value, []).append(fn)

    def evaluate(
        self, payload: HookPayload | ToolCallContext | Mapping[str, Any]
    ) -> PolicyResult:
        """Evaluate a tool call payload against handlers for its tool name.

        Args:
            payload: `HookPayload`, `ToolCallContext`, or raw JSON hook payload
                mapping.

        Returns:
            Merged `PolicyResult` from all handlers registered for the tool.
        """
        if isinstance(payload, HookPayload):
            ctx: ToolCallContext | None = payload.tool_call
        elif isinstance(payload, ToolCallContext):
            ctx = payload
        else:
            hook_payload = HookPayload.from_payload(payload)
            ctx = hook_payload.tool_call if hook_payload is not None else None
        if ctx is None:
            return PolicyResult.allow()
        return PolicyResult.merge(
            fn(ctx) for fn in self._handlers.get(ctx.name, ())
        )
