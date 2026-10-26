#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""PreToolUse agent hook dispatching tool invocations through ToolRouter."""

import argparse
import json
import sys
from collections.abc import Sequence
from pathlib import Path

# Bootstrap sys.path when executed directly from the source tree.
_tools_dir = Path(__file__).resolve().parents[3]
if (_tools_dir / "agents").is_dir() and str(_tools_dir) not in sys.path:
    sys.path.insert(0, str(_tools_dir))

from agents.lib.agent_hooks.engine.router import (  # noqa: E402
    CommandRouter,
    HookPayload,
    PolicyResult,
    ToolCallContext,
    ToolName,
    ToolRouter,
)
from agents.lib.agent_hooks.grant_policy import GrantPolicyConfig  # noqa: E402


def create_default_command_router(
    grant_policy: GrantPolicyConfig | None = None,
) -> CommandRouter:
    """Create a `CommandRouter` configured with the default grant policy.

    Args:
        grant_policy: Optional `GrantPolicyConfig` to audit commands against.

    Returns:
        Configured `CommandRouter` instance.
    """
    policy = grant_policy or GrantPolicyConfig()
    return CommandRouter(checks=(policy.audit_command,))


def create_default_tool_router(
    grant_policy: GrantPolicyConfig | None = None,
) -> ToolRouter:
    """Create and configure the top-level `ToolRouter`.

    Args:
        grant_policy: Optional `GrantPolicyConfig` for shell tool evaluation.

    Returns:
        `ToolRouter` registered for `ToolName.RUN_COMMAND`.
    """
    cmd_router = create_default_command_router(grant_policy=grant_policy)

    def _handle_run_command(ctx: ToolCallContext) -> PolicyResult:
        cmd = ctx.args.get("CommandLine")
        if not isinstance(cmd, str) or not cmd:
            return PolicyResult.allow()
        return cmd_router.evaluate(cmd)

    router = ToolRouter()
    router.register(ToolName.RUN_COMMAND, _handle_run_command)
    return router


def _parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Fuchsia agents PreToolUse command hook."
    )
    parser.add_argument(
        "--config",
        type=Path,
        default=None,
        help="Path to agent config.json file.",
    )
    parser.add_argument(
        "--state",
        type=Path,
        default=None,
        help="Path to setup state.json file.",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    """Parse tool call from stdin and emit JSON decision on stdout."""
    args = _parse_args(argv)

    try:
        payload = HookPayload.from_payload(json.load(sys.stdin))
    except ValueError:
        payload = None
    if payload is None:
        print(json.dumps(PolicyResult.allow().to_dict()))
        return 0

    grant_policy = GrantPolicyConfig.from_files(
        config_path=args.config, state_path=args.state
    )
    router = create_default_tool_router(grant_policy=grant_policy)
    print(json.dumps(router.evaluate(payload).to_dict()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
