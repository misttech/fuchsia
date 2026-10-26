# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for executing in-tree Python scripts in hermetic subprocesses."""

import os
import pathlib
import subprocess
import sys
from collections.abc import Mapping, Sequence
from typing import Any

# Under fuchsia-vendored-python, sys.executable points to the platform prebuilt Python.
PYTHON_BIN = pathlib.Path(sys.executable)


def _prepend_env_path(
    env: dict[str, str],
    varname: str,
    path_entry: str | pathlib.Path,
) -> None:
    """Prepends a path to an environment variable value such as PATH or PYTHONPATH.

    Ensures that path_entry is at the front of the colon-separated list of paths,
    moving any existing occurrence to the front to avoid duplicates.

    Args:
        env: Environment dictionary to mutate in-place.
        varname: Name of the environment variable (e.g. "PYTHONPATH", "PATH").
        path_entry: Path entry to prepend.
    """
    entry_str = str(path_entry)
    existing = env.get(varname)
    if not existing:
        env[varname] = entry_str
        return

    paths = [p for p in existing.split(os.pathsep) if p and p != entry_str]
    env[varname] = os.pathsep.join([entry_str, *paths])


def _curate_python_env(
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Curates an environment for executing in-tree Python scripts.

    Ensures that PYTHONPATH includes the source_dir (usually the FUCHSIA_DIR root)
    so that first-party absolute imports (such as build.auth and tools.integration.fint)
    resolve hermetically regardless of the ambient environment. Inherits
    PYTHONPYCACHEPREFIX as set by the outer build orchestrator (fx or fint).

    Args:
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.

    Returns:
        A curated environment dictionary.

    Raises:
        ValueError: If source_dir is empty or represents the current directory dot.
    """
    if not source_dir or str(source_dir) in ("", "."):
        raise ValueError("source_dir must be a valid non-empty directory path")

    sub_env = dict(base_env if base_env is not None else os.environ)
    _prepend_env_path(sub_env, "PYTHONPATH", source_dir)
    return sub_env


def _prepare_cmd_and_env(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
    popen_kwargs: dict[str, Any] | None = None,
) -> tuple[list[str], dict[str, str]]:
    """Prepares command argv and environment for hermetic Python child execution.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR).
        base_env: Optional base environment mapping.
        popen_kwargs: Optional dictionary of keyword arguments for subprocess.

    Returns:
        A tuple of (full_cmd_argv, env_dict).
    """
    bin_str = str(PYTHON_BIN)
    if not cmd or cmd[0] != bin_str:
        full_cmd = [bin_str, "-S", *cmd]
    elif len(cmd) < 2 or cmd[1] != "-S":
        full_cmd = [bin_str, "-S", *cmd[1:]]
    else:
        full_cmd = list(cmd)

    env = _curate_python_env(
        source_dir=source_dir,
        base_env=base_env,
    )
    if popen_kwargs is not None:
        ext_env = popen_kwargs.pop("env", None)
        if ext_env is not None:
            env.update(ext_env)
        # Guarantee text mode and prevent TypeError if callers pass text kwargs
        popen_kwargs.pop("text", None)
        popen_kwargs.pop("universal_newlines", None)

    return full_cmd, env


def run(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
    **popen_kwargs: Any,
) -> subprocess.CompletedProcess[str]:
    """Runs an in-tree Python script hermetically using PYTHON_BIN.

    Automatically:
    - Prepends PYTHON_BIN and ensures the site-package isolation flag ("-S")
      is present immediately after the interpreter.
    - Injects curated env ensuring PYTHONPATH includes source_dir.
    - Sets text=True.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.
        **popen_kwargs: Additional keyword arguments forwarded to subprocess.run.

    Returns:
        A subprocess.CompletedProcess instance containing execution results.

    Raises:
        ValueError: If source_dir is empty.
        subprocess.CalledProcessError: If check=True is passed and the command fails.
    """
    full_cmd, env = _prepare_cmd_and_env(
        cmd=cmd,
        source_dir=source_dir,
        base_env=base_env,
        popen_kwargs=popen_kwargs,
    )
    return subprocess.run(
        full_cmd,
        text=True,
        env=env,
        **popen_kwargs,
    )


def check_output(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
    **popen_kwargs: Any,
) -> str:
    """Invokes an in-tree Python script and returns its stripped stdout string.

    Ensures that PYTHONPATH includes source_dir to resolve in-tree first-party absolute imports.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.
        **popen_kwargs: Additional keyword arguments forwarded to run/subprocess.
            Note: stdout is set to subprocess.PIPE and check is set to True.

    Returns:
        The stripped stdout string of the completed process.

    Raises:
        ValueError: If source_dir is empty.
        subprocess.CalledProcessError: If the process exits with a non-zero exit code.
    """
    completed = run(
        cmd=cmd,
        source_dir=source_dir,
        base_env=base_env,
        stdout=subprocess.PIPE,
        check=True,
        **popen_kwargs,
    )
    return completed.stdout.strip()
