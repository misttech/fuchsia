# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utility library for checking if a tool is being run by an automated agent."""

import os
import typing
from importlib.resources import files

from agents import data  # type: ignore[attr-defined]


def get_agent_env_vars() -> typing.List[str]:
    """Read the list of agent environment variable names from agents.txt."""
    with files(data).joinpath("agents.txt").open("r", encoding="utf-8") as f:
        return [
            line.strip()
            for line in f
            if line.strip() and not line.strip().startswith("#")
        ]


def is_invoked_by_agent(env: typing.Mapping[str, str] | None = None) -> bool:
    """Determine if the given environment (or os.environ) indicates execution by an AI agent.

    Args:
        env: Optional mapping representing the environment. Defaults to os.environ.

    Returns:
        bool: True if an agent environment variable is present, False otherwise.
    """
    if env is None:
        env = os.environ
    for var in get_agent_env_vars():
        if env.get(var) is not None:
            return True
    return False
