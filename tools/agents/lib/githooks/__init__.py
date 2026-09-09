# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia Git hook infrastructure and integration."""

from agents.lib.githooks.adapters import (
    HookAction,
    HookContext,
    commit_msg_action,
    format_code_action,
)
from agents.lib.githooks.reporters import ConsoleReporter
from agents.lib.githooks.runner import (
    main,
    run_commit_msg_hook,
    run_pre_commit_hook,
)

__all__ = [
    "ConsoleReporter",
    "HookAction",
    "HookContext",
    "commit_msg_action",
    "format_code_action",
    "main",
    "run_commit_msg_hook",
    "run_pre_commit_hook",
]
