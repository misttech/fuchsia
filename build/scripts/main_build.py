#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia build script.

This script serves as the primary entry point for executing builds in the
Fuchsia source tree. It manages the build environment, handles locking,
orchestrates top-level wrappers (like reproxy for RBE and rsproxy for
ResultStore), and prepares commands for various build tools including
Ninja, Bazel, and fint.

Responsibilities are divided into the following classes:
  * FuchsiaBuildContext: Static configuration and environment state.
  * BuildInvocation: Per-run state including unique IDs and log directories.
  * BuildCommandExecution: Orchestration of the final subprocess execution.
"""

import argparse
import dataclasses
import datetime
import functools
import json
import os
import pathlib
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from typing import Any, Iterable, Sequence

import signal_utils

_SCRIPT = pathlib.Path(__file__)


@dataclasses.dataclass
class BuildResult(object):
    return_code: int


class BuildConfigurationError(Exception):
    """Raised when the build configuration is invalid or missing required files."""


@dataclasses.dataclass
class FuchsiaBuildConfig(object):
    """Configuration parameters for the build.

    Some of these configs can be inferred from $build_dir/args.gn.

    Fields:
      rbe: if True, build uses RBE
      resultstore: if True, build uploads status and metadata to ResultStore
      profile: if True, collect system profile during build
      tui: if True, enable terminal UI for monitoring build
    """

    rbe: bool | None
    resultstore: bool
    profile: bool
    tui: bool
    verbose: bool
    dry_run: bool
    status: bool = True

    @staticmethod
    def from_args(args: argparse.Namespace) -> "FuchsiaBuildConfig":
        return FuchsiaBuildConfig(
            rbe=args.rbe,
            resultstore=args.resultstore,
            profile=args.profile,
            tui=args.tui,
            verbose=args.verbose,
            dry_run=args.dry_run,
            status=args.status,
        )


def check_shell_command(cmd: str) -> bool:
    return shutil.which(cmd) is not None


def exists(path: pathlib.Path) -> bool:
    """Checks if a path exists."""
    return path.exists()


def is_executable(path: pathlib.Path) -> bool:
    """Checks if a path exists and is executable."""
    return path.exists() and os.access(path, os.X_OK)


def write_text(path: pathlib.Path, text: str) -> None:
    """Writes text to a file."""
    path.write_text(text)


def mkdir(
    path: pathlib.Path, parents: bool = True, exist_ok: bool = True
) -> None:
    """Creates a directory."""
    path.mkdir(parents=parents, exist_ok=exist_ok)


def read_json(path: pathlib.Path) -> Any:
    """Reads and parses a JSON file, raising BuildConfigurationError on failure."""
    if not exists(path):
        raise BuildConfigurationError(
            f"{path} does not exist. Make sure you have run 'fx set'."
        )
    try:
        with open(path) as f:
            return json.load(f)
    except json.JSONDecodeError as e:
        raise BuildConfigurationError(f"Failed to parse {path}: {e}")
    except Exception as e:
        raise BuildConfigurationError(f"Failed to read {path}: {e}")


def get_cpu_count() -> int:
    return os.cpu_count() or 1


def choose_concurrency(rbe_enabled: bool) -> int:
    cpus = get_cpu_count()
    if rbe_enabled:
        # The recommendation from the Goma team is to use 10*cpu-count for C++.
        return cpus * 10
    return cpus


def ensure_file_descriptor_limit(limit: int) -> None:
    """Ensures the soft limit for file descriptors is at least 'limit'."""
    try:
        import resource
    except ImportError:
        return

    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if soft < limit:
            resource.setrlimit(resource.RLIMIT_NOFILE, (limit, hard))
    except (ValueError, resource.error):
        pass


def _check_rbe_env_vars(environ: dict[str, str]) -> None:
    """Warns if environment variables starting with 'RBE_' are set."""
    rbe_vars = sorted([k for k in environ if k.startswith("RBE_")])
    if rbe_vars:
        print(
            f"Warning: The following environment variables starting with 'RBE_' "
            f"are set and may override RBE tool configurations: {', '.join(rbe_vars)}"
        )


def str_to_bool(value: str) -> bool:
    if isinstance(value, bool):
        return value
    if value.lower() in ("true", "1", "yes"):
        return True
    elif value.lower() in ("false", "0", "no"):
        return False
    raise argparse.ArgumentTypeError(f"Boolean value expected, got {value}")


class BuildLock:
    """This context-manager ensures at most one build is running per build dir.

    Importantly, it prints a lock-acquired message so noninteractive agents
    understand what is happening.
    """

    def __init__(
        self, build_dir: pathlib.Path, print_message: bool = False
    ) -> None:
        # LINT.IfChange(build_lock)
        self.build_lock_file = build_dir.with_suffix(".build_lock")
        # LINT.ThenChange(//tools/devshell/lib/vars.sh:build_lock)
        self._has_shlock = check_shell_command("shlock")
        self.print_message = print_message

    def __enter__(self) -> "BuildLock":
        if self._has_shlock:
            while (
                subprocess.call(
                    [
                        "shlock",
                        "-f",
                        str(self.build_lock_file),
                        "-p",
                        str(os.getpid()),
                    ]
                )
                != 0
            ):
                time.sleep(0.1)

            if self.print_message:
                # This message is critical for AI agents to understand when a build
                # is proceeding after acquiring a lock. Do not remove.
                print("Lock acquired, proceeding with build.")
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        if self._has_shlock:
            self.build_lock_file.unlink(missing_ok=True)
        if self.print_message:
            print("Build completed.")


class FuchsiaBuildContext(object):
    """FuchsiaBuildContext contains paths that are relevant to building.

    Fields:
      source_dir: location of source checkout (absolute)
      out_dir: write-able location where logs may be written (absolute)
      build_dir: where build is executed and artifacts are created (absolute)
      env: environment variables
      config: build parameters
    """

    def __init__(
        self,
        source_dir: pathlib.Path,
        out_dir: pathlib.Path,
        build_dir: pathlib.Path,
        env: dict[str, str],
        config: FuchsiaBuildConfig,
    ) -> None:
        self.source_dir = source_dir
        self.out_dir = out_dir
        self.build_dir = build_dir
        self.env = env
        self.config = config

    @staticmethod
    def from_args(
        args: argparse.Namespace,
        environ: dict[str, str],
    ) -> "FuchsiaBuildContext":
        source_dir = pathlib.Path(environ.get("FUCHSIA_DIR", ""))
        if not source_dir:
            try:
                source_dir = find_fuchsia_dir()
            except ValueError:
                # Fallback to finding it relative to this script: //build/scripts/main_build.py
                source_dir = _SCRIPT.resolve().parent.parent.parent

        out_dir = args.out_dir
        if not out_dir:
            out_dir = source_dir / "out"

        return FuchsiaBuildContext(
            source_dir=source_dir,
            out_dir=out_dir,
            build_dir=args.build_dir,
            env=environ,
            config=FuchsiaBuildConfig.from_args(args),
        )

    @property
    def rbe_settings_file(self) -> pathlib.Path:
        return self.build_dir / "rbe_settings.json"

    @property
    def rbe_config_json(self) -> pathlib.Path:
        return self.build_dir / "rbe_config.json"

    @property
    def check_loas_script(self) -> pathlib.Path:
        return self.source_dir / "build/rbe/check_loas_restrictions.sh"

    @property
    def top_build_wrapper(self) -> pathlib.Path:
        return self.source_dir / "build/scripts/top_build_wrap.sh"

    @property
    def args_gn(self) -> pathlib.Path:
        return self.build_dir / "args.gn"

    @property
    def rsninja_sh(self) -> pathlib.Path:
        return self.source_dir / "build/resultstore/rsninja.sh"

    @property
    def ninja_edge_weights_csv(self) -> pathlib.Path:
        return self.build_dir / "ninja_edge_weights.csv"

    def get_rbe_reproxy_configs(self) -> Iterable[pathlib.Path]:
        """Yields the paths to the RBE reproxy configuration files."""
        for cfg in self._rbe_config_data:
            yield self.build_dir / cfg["path"]

    @functools.cached_property
    def _rbe_config_data(self) -> Iterable[dict[str, Any]]:
        """Read and parse RBE config data."""
        data = read_json(self.rbe_config_json)
        if isinstance(data, list):
            return data
        return []

    @functools.cached_property
    def _rbe_settings(self) -> dict[str, Any]:
        """Automatically detect RBE usage from a GN-generated JSON file."""
        return read_json(self.rbe_settings_file)

    @property
    def rbe_enabled(self) -> bool:
        if self.config.rbe is not None:
            return self.config.rbe

        return self._rbe_settings.get("final", {}).get("needs_reproxy", False)

    @property
    def needs_auth(self) -> bool:
        if self.config.resultstore:
            return True

        return self._rbe_settings.get("final", {}).get("needs_auth", False)

    @property
    def concurrency(self) -> int:
        return choose_concurrency(self.rbe_enabled)

    @functools.cached_property
    def loas_type(self) -> str:
        """Automatically detect the LOAS type."""
        if not self.needs_auth:
            return "skip"

        check_loas_script = self.check_loas_script
        if is_executable(check_loas_script):
            try:
                output = subprocess.check_output(
                    [str(check_loas_script)],
                    text=True,
                    stderr=subprocess.DEVNULL,
                    env=self.env,
                )
                lines = output.strip().splitlines()
                if lines:
                    return lines[-1]
            except subprocess.CalledProcessError:
                pass
        return "skip"


class BuildInvocation(object):
    """BuildInvocation represents a single build run.

    It encapsulates the FuchsiaBuildContext, and is responsible
    for generating a unique ID, and creating a single-use log directory.
    """

    def __init__(self, context: FuchsiaBuildContext) -> None:
        self.context = context
        # Accessing log_dir triggers the cached_property evaluation,
        # which creates the directory and writes the invocation_id.
        _ = self.log_dir

    @functools.cached_property
    def build_uuid(self) -> str:
        """Generates a unique ID for this build."""
        return str(uuid.uuid4())

    @functools.cached_property
    def timestamp(self) -> str:
        """Returns the timestamp for the start of this build."""
        return datetime.datetime.now().strftime("%Y%m%d-%H%M%S")

    # LINT.IfChange(build_log_dir_structure)
    @functools.cached_property
    def log_dir(self) -> pathlib.Path:
        """Creates and returns the log directory for this invocation.

        The invocation id is recorded in the new log directory
        in a file named "invocation_id".
        """
        logs_root = self.context.out_dir / "_build_logs"
        build_dir_name = self.context.build_dir.name
        log_dir_base = logs_root / build_dir_name
        mkdir(log_dir_base)

        # Use consistent UUID and timestamp
        log_dir = log_dir_base / f"build.{self.timestamp}.{self.build_uuid[:8]}"
        mkdir(log_dir)

        # Record the invocation Id
        write_text(log_dir / "invocation_id", self.build_uuid + "\n")
        return log_dir

    # LINT.ThenChange(//tools/devshell/lib/vars.sh:build_log_dir_structure)

    def get_build_env(self) -> dict[str, str]:
        """Curate a build environment for this invocation."""
        build_env = {
            "FX_BUILD_UUID": self.build_uuid,
            "FX_BUILD_LOGDIR": str(self.log_dir),
            "TERM": self.context.env.get(
                "TERM", "dumb"
            ),  # passed for the pretty ninja UI
            "PATH": self.context.env.get(
                "PATH", ""
            ),  # passed through. The ninja actions should invoke tools without relying on PATH.
            # By default, also show the number of actively running actions.
            "NINJA_STATUS": self.context.env.get(
                "NINJA_STATUS", "[%f/%t][%p/%w](%r) "
            ),
            # By default, print the 4 oldest commands that are still running.
            "NINJA_STATUS_MAX_COMMANDS": self.context.env.get(
                "NINJA_STATUS_MAX_COMMANDS", "4"
            ),
            "NINJA_STATUS_REFRESH_MILLIS": self.context.env.get(
                "NINJA_STATUS_REFRESH_MILLIS", "100"
            ),
            "NINJA_PERSISTENT_MODE": self.context.env.get(
                "NINJA_PERSISTENT_MODE", "0"
            ),
            "PYTHONPYCACHEPREFIX": self.context.env.get(
                "PYTHONPYCACHEPREFIX",
                str(self.context.build_dir / "__pycache__"),
            ),
        }

        if not self.context.config.status:
            # Setting TERM=dumb is a standard convention to turn off interactive features.
            # See https://en.wikipedia.org/wiki/Computer_terminal#Dumb_terminals
            # and https://developers.google.com/style/inclusive-documentation#ableist-language
            # regarding non-inclusive language.
            build_env["TERM"] = "dumb"
            build_env["NINJA_STATUS"] = "[%f/%t] "

        # Forwarded standard variables
        forward_vars = [
            "USER",  # needs $USER for automatic auth with gcert (from re-client bootstrap)
            "SSH_AUTH_SOCK",  # need to forward the authentication socket (used by gnubby) for bazel
            "MAKEFLAGS",
            "TMPDIR",  # was passed for Goma on macOS, but it might have other uses.
            "CLICOLOR_FORCE",
            "FUCHSIA_BAZEL_DISK_CACHE",
            "FUCHSIA_BAZEL_DISK_CACHE_SIZE",
            "FUCHSIA_BAZEL_JOB_COUNT",
            "FUCHSIA_DEBUG_BAZEL_SANDBOX",
            "NINJA_PERSISTENT_TIMEOUT_SECONDS",
            "NINJA_PERSISTENT_LOG_FILE",
            "FX_BUILD_RBE_STATS",
            "FX_BUILD_QUIET",
            "FX_REMOTE_BUILD_METRICS",  # Honor environment variable to disable RBE build metrics.
        ]
        for var in forward_vars:
            if var in self.context.env:
                build_env[var] = self.context.env[var]

        if self.context.needs_auth:
            build_env["FX_BUILD_LOAS_TYPE"] = self.context.loas_type
            user = None
            if "USER" in self.context.env:
                user = self.context.env["USER"]
            elif hasattr(os, "getlogin"):
                try:
                    user = os.getlogin()
                except OSError:
                    pass

            if not user:
                raise BuildConfigurationError(
                    "USER environment variable is not set and could not be "
                    "inferred. This is required for RBE/ResultStore authentication."
                )
            build_env["USER"] = user

            default_adc = (
                pathlib.Path.home()
                / ".config/gcloud/application_default_credentials.json"
            )
            build_env["GOOGLE_APPLICATION_CREDENTIALS"] = self.context.env.get(
                "GOOGLE_APPLICATION_CREDENTIALS", str(default_adc)
            )
        return build_env


@dataclasses.dataclass
class BuildCommandExecution(object):
    """BuildCommandExecution represents a single build command.

    To execute the build command, call .run().

    Fields:
      full_command: shell command tokens
      env: environment variables
      invocation: the build invocation this execution belongs to
      cleanup_files: list of files to remove after execution
    """

    full_command: Sequence[str]
    env: dict[str, str]
    invocation: BuildInvocation
    cleanup_files: list[pathlib.Path] = dataclasses.field(default_factory=list)

    def _run_without_locking(self) -> BuildResult:
        """Execute the build command."""
        config = self.invocation.context.config
        if config.verbose or config.dry_run:
            env_str = " ".join(
                f"{k}={shlex.quote(v)}" for k, v in sorted(self.env.items())
            )
            print(
                f"Running: {env_str} {' '.join(shlex.quote(c) for c in self.full_command)}"
            )
        # Note: when config.dry_run is set, we still execute the command,
        # but we have forwarded --dry-run to the top_build_wrapper, which
        # will skip the actual build execution. This allows for high-fidelity
        # verification of the entire wrapper orchestration stack.

        # If the TUI is enabled, we MUST NOT use a separate process group,
        # otherwise the TUI will be suspended when it tries to interact with
        # the terminal (SIGTTIN/SIGTTOU).
        use_separate_pgrp = not config.tui

        # Use SignalManagedProcess to handle the entire lifecycle:
        # 1. Setup the child (un-ignore signals, optionally isolate PGID).
        # 2. Spawn the process.
        # 3. Forward signals while waiting for termination.
        #
        # Note: We must stay alive to maintain the build lock and cleanup files.
        managed = signal_utils.SignalManagedProcess(
            self.full_command,
            env=self.env,
            separate_pgrp=use_separate_pgrp,
            verbose=config.verbose,
        )

        return BuildResult(return_code=managed.run())

    def run(self) -> BuildResult:
        """Execute the build command, guarded by a build lock.

        Returns:
          exit code of the command, 0 for success.
        """
        try:
            quiet = os.getenv("FX_BUILD_QUIET") == "1"
            with BuildLock(
                self.invocation.context.build_dir, print_message=quiet
            ):
                return self._run_without_locking()
        finally:
            for f in self.cleanup_files:
                f.unlink(missing_ok=True)


def top_build_command_prefix(
    invocation: BuildInvocation,
) -> list[str]:
    """Construct the prefix command for the top-level wrapper."""
    context = invocation.context
    # top_build_wrapper is a wrapper orchestrator whose purpose is to
    # auto-start/stop processes around the build.
    top_cmd = [str(context.top_build_wrapper)]

    if context.config.dry_run:
        top_cmd.append("--dry-run")

    if context.config.tui:
        top_cmd.append("--tui")

    if context.rbe_enabled:
        top_cmd.append("--rbe")
        for cfg_path in context.get_rbe_reproxy_configs():
            top_cmd.extend(["--reproxy-cfg", str(cfg_path)])

    # LOAS handling
    top_cmd.extend(["--loas-type", context.loas_type])

    # Log directory setup
    top_cmd.extend(["--build-dir", str(context.build_dir)])
    top_cmd.extend(["--log-dir", str(invocation.log_dir)])

    if context.config.resultstore:
        top_cmd.append("--resultstore")
        args_gn = context.args_gn
        if exists(args_gn):
            top_cmd.extend(["--pre-build-uploads", str(args_gn)])

    if context.config.profile:
        top_cmd.append("--profile")

    return top_cmd


def inject_ninja_args(
    invocation: BuildInvocation,
    build_command: list[str],
) -> list[str]:
    """Return new build command with Ninja-specific flags injected in the right place."""
    ninja_log_dir = invocation.log_dir / "ninja_logs"
    mkdir(ninja_log_dir)
    # Record the set of inputs that triggered build actions.
    dirty_sources = ninja_log_dir / "ninja_dirty_sources.log"
    # Record action count metrics.
    action_metrics = ninja_log_dir / "ninja_action_metrics.json"

    ninja_bin = build_command[0]
    remaining_args = build_command[1:]
    return [
        ninja_bin,
        "--dirty_sources_list",
        str(dirty_sources),
        "--action_metrics_output",
        str(action_metrics),
    ] + list(remaining_args)


def new_build_command_execution(
    invocation: BuildInvocation,
    command_type: str,
    build_command: list[str],
) -> BuildCommandExecution:
    """Creates a self-contained BuildCommandExecution."""
    top_cmd = top_build_command_prefix(invocation)
    build_env = invocation.get_build_env()

    # Prepare Ninja-specific options
    if command_type == "ninja":
        build_command = inject_ninja_args(invocation, build_command)

    full_cmd = top_cmd + ["--"] + list(build_command)

    return BuildCommandExecution(
        full_command=full_cmd,
        env=build_env,
        invocation=invocation,
    )


# TODO: De-duplicate with find_fuchsia_dir in //build/bazel/scripts/build_utils.py.
def find_fuchsia_dir(from_path: pathlib.Path | None = None) -> pathlib.Path:
    """Find the Fuchsia checkout from a specific path.

    Args:
        from_path: Optional starting path for search. Defaults to the current directory.
    Returns:
        Path to the Fuchsia checkout directory (absolute).
    Raises:
        ValueError if the path could not be found.
    """
    start_path = from_path.resolve() if from_path else pathlib.Path.cwd()
    cur_path = start_path
    while True:
        if exists(cur_path / ".jiri_manifest"):
            return cur_path
        prev_path = cur_path
        cur_path = cur_path.parent
        if cur_path == prev_path:
            raise ValueError(
                f"Could not find Fuchsia checkout directory from: {start_path}"
            )


def new_ninja_build_command_execution(
    context: FuchsiaBuildContext,
    ninja_args: Sequence[str],
) -> BuildCommandExecution:
    """Construct a ninja build command.

    Behavior:
    - Selects the Ninja binary: uses the first argument if it ends in 'ninja'
      or 'rsninja.sh', otherwise defaults to $PREBUILT_NINJA or 'ninja'.
      If 'rsninja.sh' exists in the source tree, it is preferred.
    - Computes concurrency: uses -j if provided, otherwise defaults to the
      context's concurrency (which is auto-detected based on RBE and CPU count).
    - Ensures file descriptor limits are sufficient for the concurrency.
    - Sets a default load average limit on Darwin if not provided.
    - Always inserts '-C <build_directory>' to ensure Ninja runs in the correct
      build directory.
    - Adds '--edge_weights_list' to track build performance.
    """
    # Ninja argument massage logic
    concurrency: str | None = None
    load: str | None = None
    remaining = []

    # If the first argument looks like a ninja path, we'll use it.
    # Otherwise we'll use the default one.
    if ninja_args and (
        ninja_args[0].endswith("ninja") or ninja_args[0].endswith("rsninja.sh")
    ):
        ninja_bin = ninja_args[0]
        it = iter(ninja_args[1:])
    else:
        ninja_bin = context.env.get("PREBUILT_NINJA", "ninja")
        # Parity check for rsninja.sh
        rsninja = context.rsninja_sh
        if is_executable(rsninja):
            ninja_bin = str(rsninja)
        it = iter(ninja_args)

    for opt in it:
        if opt == "-j":
            try:
                concurrency = next(it)
            except StopIteration:
                raise BuildConfigurationError("-j requires an argument")
        elif opt.startswith("-j"):
            concurrency = opt[2:]
        elif opt == "-l":
            try:
                load = next(it)
            except StopIteration:
                raise BuildConfigurationError("-l requires an argument")
        elif opt.startswith("-l"):
            load = opt[2:]
        else:
            remaining.append(opt)

    if load is None and sys.platform == "darwin":
        load = str(get_cpu_count() * 20)

    if concurrency is None:
        concurrency = str(context.concurrency)
        # Check ulimit for file descriptors
        if concurrency:
            ensure_file_descriptor_limit(int(concurrency) * 2)

    ninja_args_list = ["-j", str(concurrency)]
    if load:
        ninja_args_list.extend(["-l", str(load)])

    # Add edge weights
    ninja_args_list.append(
        f"--edge_weights_list={context.ninja_edge_weights_csv}"
    )

    build_cmd = (
        [ninja_bin]
        + ninja_args_list
        + ["-C", str(context.build_dir)]
        + remaining
    )
    invocation = BuildInvocation(context)
    return new_build_command_execution(invocation, "ninja", build_cmd)


def new_bazel_build_command_execution(
    context: FuchsiaBuildContext,
    bazel_args: list[str],
) -> BuildCommandExecution:
    """Construct a bazel build command.

    Behavior:
    - Selects the Bazel binary: uses the first argument if it ends in 'bazel',
      otherwise defaults to 'bazel'.
    """
    if bazel_args and bazel_args[0].endswith("bazel"):
        # Already has bazel binary
        build_cmd = bazel_args
    else:
        build_cmd = ["bazel"] + list(bazel_args)

    invocation = BuildInvocation(context)
    return new_build_command_execution(invocation, "bazel", build_cmd)


def new_fint_build_command_execution(
    context: FuchsiaBuildContext,
    fint_args: list[str],
) -> BuildCommandExecution:
    """Construct a fint build command.

    Args:
        context: FuchsiaBuildContext.
        fint_args: list of arguments, where the first element must be the path
            to the fint binary. Remaining arguments are passed to 'fint build'.

    Behavior:
    - Generates a temporary textproto context file for fint.
    - Appends '-context=<path>' to the fint command.
    - Schedules the temporary context file for cleanup after execution.
    """
    # fint_args should be [fint_bin, build, -static=...]
    if not fint_args:
        raise BuildConfigurationError("fint requires at least the binary path.")

    fint_bin = fint_args[0]
    remaining = fint_args[1:]

    concurrency = context.concurrency
    # MacOS ulimit check
    ensure_file_descriptor_limit(int(concurrency) * 2)

    context_content = f"""
checkout_dir: "{context.source_dir.resolve()}"
build_dir: "{context.build_dir.resolve()}"
job_count: {concurrency}
"""
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".textproto", delete=False
    ) as tf:
        tf.write(context_content)
        context_path = pathlib.Path(tf.name)

    build_cmd = [fint_bin] + remaining + [f"-context={context_path}"]
    invocation = BuildInvocation(context)
    exec_info = new_build_command_execution(invocation, "fint", build_cmd)
    exec_info.cleanup_files.append(context_path)
    return exec_info


def new_other_build_command_execution(
    context: FuchsiaBuildContext,
    other_args: list[str],
) -> BuildCommandExecution:
    """Construct an arbitrary build command.

    Args:
        context: FuchsiaBuildContext.
        other_args: list of arguments for the command.
    """
    if not other_args:
        raise BuildConfigurationError(
            "other command requires at least one argument."
        )

    invocation = BuildInvocation(context)
    return new_build_command_execution(invocation, "other", other_args)


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=pathlib.Path, required=True)
    parser.add_argument("--out-dir", type=pathlib.Path)

    # Custom handling for boolean flags to support --flag=true/false
    parser.add_argument("--rbe", type=str_to_bool, nargs="?", const=True)
    parser.add_argument("--no-rbe", action="store_false", dest="rbe")

    parser.add_argument(
        "--resultstore", type=str_to_bool, nargs="?", const=True
    )
    parser.add_argument(
        "--no-resultstore", action="store_false", dest="resultstore"
    )

    parser.add_argument("--profile", type=str_to_bool, nargs="?", const=True)
    parser.add_argument("--no-profile", action="store_false", dest="profile")

    parser.add_argument("--tui", type=str_to_bool, nargs="?", const=True)
    parser.add_argument("--no-tui", action="store_false", dest="tui")

    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--no-status", action="store_false", dest="status")

    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser(
        "ninja",
        help="Execute a Ninja build.",
        description="Expects arbitrary Ninja arguments to be passed after 'ninja'.",
    ).set_defaults(func=new_ninja_build_command_execution)
    subparsers.add_parser(
        "bazel",
        help="Execute a Bazel build.",
        description="Expects arbitrary Bazel arguments to be passed after 'bazel'.",
    ).set_defaults(func=new_bazel_build_command_execution)
    subparsers.add_parser(
        "fint",
        help="Execute a fint build.",
        description="Expects [fint_bin, build, -static=...] to be passed after 'fint'.",
    ).set_defaults(func=new_fint_build_command_execution)
    subparsers.add_parser(
        "other",
        help="Execute an arbitrary command.",
        description="Expects arbitrary arguments to be passed after 'other'.",
    ).set_defaults(func=new_other_build_command_execution)
    return parser


_MAIN_ARG_PARSER = _main_arg_parser()


def main(argv: list[str]) -> int:
    args, unknown = _MAIN_ARG_PARSER.parse_known_args(argv)

    environ = dict(os.environ)
    _check_rbe_env_vars(environ)
    context = FuchsiaBuildContext.from_args(args, environ)

    try:
        exec_info = args.func(context, unknown)
        return exec_info.run().return_code
    except BuildConfigurationError as e:
        print(f"Error: {e}")
        return 1
    except signal_utils.BuildInterruptedError as e:
        # SignalManagedProcess ensures that we have already waited for any
        # child processes before this is raised.
        sig_name = signal.Signals(e.signum).name
        print(
            f"[main_build.py] Interrupted by {sig_name}, exiting ({e.return_code})"
        )
        return e.return_code
    except KeyboardInterrupt:
        # Fallback for standard interrupts outside SignalManagedProcess
        print("[main_build.py] Received KeyboardInterrupt, exiting (130)")
        return 130


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
