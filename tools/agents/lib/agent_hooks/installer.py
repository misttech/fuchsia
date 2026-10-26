# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Installer and uninstaller for PreToolUse agent hooks in hooks.json."""

import json
import shlex
import sys
from pathlib import Path
from typing import Any

from agents.lib import paths
from agents.lib.agent_hooks.engine.router import ToolName

HOOK_GROUP_NAME = "fuchsia-agents"
HOOKS_FILENAME = "hooks.json"


def get_hooks_path(config_path: Path) -> Path:
    """Return the `hooks.json` path colocated with `config_path`."""
    return config_path.parent / HOOKS_FILENAME


def _resolve_hook_script() -> Path:
    """Resolve the `pre_tool_use.py` hook script path."""
    # Prefer the checkout source path over `__file__` so running `setup` inside
    # a packaged `.pyz` host test still points `hooks.json` at the persistent
    # script in the tree rather than a temporary zip extraction directory.
    try:
        script = (
            paths.find_fuchsia_dir()
            / "tools/agents/lib/agent_hooks/pre_tool_use.py"
        )
        if script.is_file():
            return script.resolve()
    except RuntimeError:
        pass
    return Path(__file__).resolve().with_name("pre_tool_use.py")


def _resolve_python_executable() -> str:
    """Resolve the Python interpreter for running the hook script.

    Prefers the version-agnostic `python3` binary alongside `sys.executable`
    so `hooks.json` remains valid across minor Python version bumps without
    invoking `scripts/fuchsia-vendored-python` (`hermetic-env` rejects commands
    executed when `cwd` is outside that checkout).
    """
    generic_py = Path(sys.executable).with_name("python3")
    if generic_py.is_file():
        return str(generic_py)
    return sys.executable


def _load_hooks_file(hooks_path: Path) -> dict[str, Any]:
    """Load `hooks.json` if it exists and contains a JSON object."""
    if not hooks_path.is_file():
        return {}
    try:
        data = json.loads(hooks_path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def get_agent_hooks_status(config_path: Path) -> bool:
    """Return `True` if the managed hook is configured in `hooks.json`."""
    hooks_path = get_hooks_path(config_path)
    hooks_data = _load_hooks_file(hooks_path)
    return HOOK_GROUP_NAME in hooks_data


def _save_hooks_file(hooks_path: Path, hooks_data: dict[str, Any]) -> None:
    """Atomically write `hooks.json` via a temporary file."""
    hooks_path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = hooks_path.with_name(f".{hooks_path.name}.tmp")
    temp_path.write_text(
        json.dumps(hooks_data, indent=2) + "\n", encoding="utf-8"
    )
    temp_path.replace(hooks_path)


def install_agent_hooks(
    config_path: Path, state_path: Path | None = None
) -> Path:
    """Configure the managed `PreToolUse` command hook in `hooks.json`.

    Args:
        config_path: Path to the target `config.json` file.
        state_path: Optional path to the setup `state.json` file.

    Returns:
        Path to the updated `hooks.json` file.
    """
    hooks_path = get_hooks_path(config_path)
    python_exe = _resolve_python_executable()
    script_path = _resolve_hook_script()
    hook_cmd = (
        f"{shlex.quote(python_exe)} {shlex.quote(str(script_path))} "
        f"--config {shlex.quote(str(config_path.resolve()))}"
    )
    if state_path is not None:
        hook_cmd += f" --state {shlex.quote(str(state_path.resolve()))}"

    hooks_data = _load_hooks_file(hooks_path)
    hooks_data[HOOK_GROUP_NAME] = {
        "PreToolUse": [
            {
                "matcher": ToolName.RUN_COMMAND.value,
                "hooks": [
                    {
                        "type": "command",
                        "command": hook_cmd,
                    }
                ],
            }
        ]
    }
    _save_hooks_file(hooks_path, hooks_data)
    return hooks_path


def uninstall_agent_hooks(config_path: Path) -> bool:
    """Remove the managed `PreToolUse` command hook from `hooks.json`.

    Args:
        config_path: Path to the target `config.json` file.

    Returns:
        `True` if a managed hook entry was removed, `False` otherwise.
    """
    hooks_path = get_hooks_path(config_path)
    if not hooks_path.is_file():
        return False

    hooks_data = _load_hooks_file(hooks_path)
    if HOOK_GROUP_NAME not in hooks_data:
        return False

    hooks_data.pop(HOOK_GROUP_NAME, None)
    if hooks_data:
        _save_hooks_file(hooks_path, hooks_data)
    else:
        hooks_path.unlink(missing_ok=True)
    return True
