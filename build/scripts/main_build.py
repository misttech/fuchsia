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
import enum
import functools
import getpass
import json
import os
import pathlib
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid
from collections.abc import Callable, Iterable, Mapping, Sequence
from typing import Any, TextIO

# Ensure the fuchsia root is in sys.path so we can import the build packages hermetically
_FUCHSIA_ROOT = pathlib.Path(__file__).parent.parent.parent
if str(_FUCHSIA_ROOT) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_ROOT))

from build.auth import gcloud
from build.profile import procfs
from build.rbe import build_summary, casfs, rbe_profile, rbe_settings
from build.resultstore import rs_profile
from build.scripts import build_timeout, properties, pstree, signal_utils

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]


class PreflightStatus(enum.Enum):
    """Outcome of a PreflightAction."""

    # Action succeeded; continue with remaining actions and the build.
    OK = "ok"
    # Action failed; run remaining actions, then abort before the build.
    ERROR = "error"
    # Action failed fatally; abort immediately without running remaining actions.
    FATAL = "fatal"


# A preflight action is a callback that runs under the build lock immediately
# before a build command is spawned. This is used for build-tool-specific setup
# or health checks (e.g. checking if a persistent Ninja server is wedged and
# killing it before Ninja runs; Bazel does not need this).
#
# - Attached to a BuildCommandExecution via BuildInvocation.new_build_command_execution(),
#   not stored globally on FuchsiaBuildConfig.
# - Skipped entirely during dry runs (--dry-run).
# - Intended for build commands (is_build=True); non-build commands like queries
#   do not register them.
PreflightAction = Callable[[], PreflightStatus]

_SCRIPT = pathlib.Path(__file__)

# Use the executable of the active Python interpreter to ensure that all
# spawned sub-processes (like fint_build.py) run under the exact same
# interpreter environment.
PYTHON_BIN = pathlib.Path(sys.executable)


def msg(text: str, file: TextIO | None = None) -> None:
    """Print a message prefixed with the script's basename."""
    if file is None:
        file = sys.stdout
    print(f"[{_SCRIPT.name}] {text}", file=file)


def ts_msg(
    text: str, verbose: bool = False, file: TextIO | None = None
) -> None:
    """Print a message with high-precision epoch timestamps for verbose mode."""
    if file is None:
        file = sys.stderr
    if verbose:
        print(f"[{time.time():.9f}] [{_SCRIPT.name}]: {text}", file=file)


def _parse_cfg_text(text: str) -> dict[str, str]:
    """Parses a key-value .cfg string into a dictionary.

    Lines starting with '#' or empty lines are ignored.
    """
    cfg_data: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # Key-value lines are formatted as 'key=value'
        key, _, value = line.partition("=")
        key = key.strip()
        if key:
            cfg_data[key] = value.strip()
    return cfg_data


def _parse_cfg_file(config_path: pathlib.Path) -> dict[str, str]:
    """Safely reads a key-value .cfg file and parses it into a dictionary."""
    try:
        if config_path.is_file():
            return _parse_cfg_text(config_path.read_text())
    except Exception as e:
        # Speculative configuration parsing of user cfg files is non-critical to
        # core build orchestration. Print a diagnostic warning but proceed.
        msg(
            f"Warning: Failed to read RBE/ResultStore config file {config_path}: {e}",
            file=sys.stderr,
        )
    return {}


def _extract_project_id_from_instance(instance_val: str) -> str | None:
    """Extracts the GCP project ID from an RBE/ResultStore instance string.

    Format expected: 'projects/<project-id>/instances/...'
    """
    if instance_val.startswith("projects/"):
        _, _, projects_tail = instance_val.partition("projects/")
        project_id, _, _ = projects_tail.partition("/")
        if project_id:
            return project_id
    return None


GLOBAL_RESULTSTORE_CONFIG = pathlib.Path(".fx/config/resultstore")
LOCAL_RESULTSTORE_CONFIG = pathlib.Path(".resultstore")
# LINT.IfChange(ninja_build_trace_filename)
NINJA_BUILD_TRACE_GZ = "ninja_build_trace.json.gz"
# LINT.ThenChange(/tools/integration/fint/fint_build.py:ninja_build_trace_filename)
NINJA_LOGS_DIR = "ninja_logs"
NINJA_ACTION_METRICS_JSON = "ninja_action_metrics.json"
NINJA_ERRORS_JSON = "ninja_errors.json"
TIMEOUT_LOGS_DIR = pathlib.Path("timeout_logs")
BUILD_TIMEOUT_SCRIPT = pathlib.Path("build/scripts/build_timeout.py")
FINT_BUILD_PY_RELATIVE_PATH = pathlib.Path(
    "tools/integration/fint/fint_build.py"
)
BAZEL_CRED_HELPER = pathlib.Path(
    "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
)


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
      rbe_casfs: if True, virtualize RBE outputs on casfs instead of downloading them
      resultstore: "all", "none", "ninja", or "bazel"
      profile: if True, collect system profile during build
      tui: if True, enable terminal UI for monitoring build
      verbose: if True, enable verbose logging output
      dry_run: if True, execute in dry-run mode
      status: if True, show build status metrics
      auth_mode: "auto", "user", "machine", or "none" (RBE/ResultStore auth)
      max_concurrency: upper bound on the automatically chosen -j, 0 for none
      fint_params_path: path to Fint static parameters if Fint wrapping is triggered
      fint_context_path: path to Fint context parameters if Fint wrapping is triggered
      output_metadata_json: path to write the structured metadata JSON of build artifacts
      remote_proxy_socket: path to the local RBE proxy socket if passed via CLI
      resultstore_proxy_socket: path to the local ResultStore/BES proxy socket if passed via CLI
      resultstore_instance: the target ResultStore instance name
      cas_instance: the target CAS instance name
      rbe_instance: the target RBE instance name
    """

    rbe: bool | None
    resultstore: str
    profile: bool
    tui: bool
    verbose: bool
    dry_run: bool
    auth_mode: str
    status: bool = True
    max_concurrency: int = 0
    fint_params_path: pathlib.Path | None = None
    fint_context_path: pathlib.Path | None = None
    output_metadata_json: pathlib.Path | None = None
    remote_proxy_socket: pathlib.Path | None = None
    resultstore_proxy_socket: pathlib.Path | None = None
    resultstore_instance: str | None = None
    cas_instance: str | None = None
    rbe_instance: str | None = None
    log_dir: pathlib.Path | None = None
    timeout: str | None = None
    timeout_grace_period: str | None = None
    rbe_casfs: bool = False

    @staticmethod
    def from_args(
        args: argparse.Namespace,
        default_resultstore: str | None = None,
        default_rbe_casfs: bool = False,
    ) -> "FuchsiaBuildConfig":
        resultstore_val = args.resultstore or default_resultstore or "none"
        rbe_casfs_val = (
            args.rbe_casfs if args.rbe_casfs is not None else default_rbe_casfs
        )

        return FuchsiaBuildConfig(
            rbe=args.rbe,
            rbe_casfs=rbe_casfs_val,
            resultstore=resultstore_val,
            profile=args.profile,
            tui=args.tui,
            verbose=args.verbose,
            dry_run=args.dry_run,
            status=args.status,
            fint_params_path=args.fint_params_path,
            fint_context_path=args.fint_context_path,
            output_metadata_json=args.output_metadata_json,
            auth_mode=args.auth_mode,
            max_concurrency=args.max_concurrency,
            remote_proxy_socket=args.remote_proxy_socket,
            resultstore_proxy_socket=args.resultstore_proxy_socket,
            resultstore_instance=args.resultstore_instance,
            cas_instance=args.cas_instance,
            rbe_instance=args.rbe_instance,
            log_dir=args.log_dir,
            timeout=args.timeout,
            timeout_grace_period=args.timeout_grace_period,
        )


def check_shell_command(cmd: str) -> bool:
    return shutil.which(cmd) is not None


@functools.lru_cache()
def has_loas() -> bool:
    """Checks whether the host system supports LOAS/gcert authentication."""
    return check_shell_command("gcert") and check_shell_command("gcertstatus")


def _collect_rbe_metadata(log_dir: pathlib.Path) -> JSONObject:
    """Scans the active reproxy logs directory for diagnostic files and CAS logs.

    Args:
        log_dir: Path to the active build invocation's log directory.

    Returns:
        A dictionary containing paths to the reproxy log directory, diagnostic
        log files, CAS upload candidates (.rrpl/.rpl), serialized proto logs,
        and summarized metrics.
    """
    rbe_metadata: JSONObject = {}
    # LINT.IfChange(reproxy_logs_dir)
    reproxy_log_dir = log_dir / "reproxy_logs"
    # LINT.ThenChange(//build/scripts/top_build_wrap.sh:reproxy_logs_dir)
    if not reproxy_log_dir.exists():
        return rbe_metadata

    reproxy_log_dir_abs = reproxy_log_dir.resolve()
    rbe_metadata["log_dir"] = str(reproxy_log_dir_abs)

    diagnostic_logs = {}
    for name in ["bootstrap.INFO", "reproxy.INFO", "rbe_metrics.txt"]:
        candidate = reproxy_log_dir_abs / name
        if candidate.exists():
            diagnostic_logs[name] = str(candidate)
    if diagnostic_logs:
        rbe_metadata["diagnostic_logs"] = diagnostic_logs

    cas_candidates = []
    try:
        for item in reproxy_log_dir_abs.iterdir():
            if item.is_file() and item.suffix in (".rrpl", ".rpl"):
                cas_candidates.append(str(item.resolve()))
    except OSError:
        pass
    if cas_candidates:
        rbe_metadata["cas_upload_candidates"] = cas_candidates

    for pb_name, pb_filename in [
        ("rbe_metrics_pb", "rbe_metrics.pb"),
        ("reproxy_log_pb", "reproxy_log.pb"),
    ]:
        candidate = reproxy_log_dir_abs / pb_filename
        if candidate.exists():
            rbe_metadata[pb_name] = str(candidate)

    try:
        metrics_summary = build_summary.summarize_rbe_metrics_from_logdir(
            reproxy_log_dir_abs
        )
    except Exception as e:
        msg(f"Warning: Failed to summarize RBE metrics: {e}", file=sys.stderr)
        metrics_summary = None

    if metrics_summary is not None:
        rbe_metadata["metrics_summary"] = metrics_summary

    return rbe_metadata


def _collect_resultstore_metadata(log_dir: pathlib.Path) -> JSONObject:
    """Scans the active build invocation's log directory for rsproxy diagnostic logs.

    Collects direct top-level rsproxy logs as well as recursively discovered sub-build
    rsproxy logs. Note: Sub-build log isolation assumes that sub-build directory basenames
    do not collide across different paths in the build graph (as structured by
    build/resultstore/fuchsia-rsproxy-wrap.sh).

    Args:
        log_dir: Path to the active build invocation's log directory.

    Returns:
        A dictionary containing paths to rsproxy diagnostic log files.
    """
    resultstore_metadata: JSONObject = {}
    # LINT.IfChange(rsproxy_logs_dir)
    rsproxy_log_dir = log_dir / "rsproxy_logs"
    # LINT.ThenChange(//build/scripts/top_build_wrap.sh:rsproxy_logs_dir)
    if not rsproxy_log_dir.exists():
        return resultstore_metadata

    diagnostic_logs = {}
    rsproxy_names = {"rsproxy.INFO", "rsproxy.WARNING", "rsproxy.ERROR"}

    # Recursively collect direct and nested sub-build rsproxy logs.
    for candidate in sorted(rsproxy_log_dir.rglob("rsproxy.*")):
        if candidate.is_file() and candidate.name in rsproxy_names:
            rel_path = candidate.relative_to(rsproxy_log_dir)
            diagnostic_logs[str(rel_path)] = str(candidate.resolve())

    if diagnostic_logs:
        resultstore_metadata["diagnostic_logs"] = diagnostic_logs

    return resultstore_metadata


def _iter_directory_file_map(
    root_dir: pathlib.Path,
) -> Iterable[tuple[str, list[str]]]:
    """Walks root_dir and yields (relative_subdir, list_of_filenames) tuples.

    Args:
        root_dir: Base directory on disk.

    Yields:
        Tuples of (relative_subdir_path, list_of_filenames) for each subdirectory
        containing non-symlink regular files.
    """
    if not root_dir.exists():
        return

    for root, dirnames, filenames in os.walk(root_dir, followlinks=False):
        dirnames.sort()  # Ensures deterministic, chronological traversal
        dir_path = pathlib.Path(root)
        if dir_path == root_dir:
            continue
        try:
            rel_dir = str(dir_path.relative_to(root_dir))
            real_files = [
                f
                for f in sorted(filenames)
                if not os.path.islink(os.path.join(root, f))
            ]
            if real_files:
                yield rel_dir, real_files
        except (OSError, ValueError):
            continue


def _collect_bazel_metadata(log_dir: pathlib.Path) -> JSONObject:
    """Scans the active build invocation's log directory for Bazel diagnostic logs.

    Args:
        log_dir: Path to the active build invocation's log directory.

    Returns:
        A dictionary containing the bazel log_dir and a mapping of invocation
        directory names to the list of artifact filenames present in each invocation.
    """
    # LINT.IfChange(bazel_logs_dir)
    bazel_log_dir = log_dir / "bazel_logs"
    # LINT.ThenChange(//build/bazel/wrapper.bazel.sh:bazel_logs_dir)
    invocations = dict(_iter_directory_file_map(bazel_log_dir))
    if not invocations:
        return {}

    return {
        "log_dir": str(bazel_log_dir.resolve()),
        "invocations": invocations,
    }


# Persistent background daemons that run throughout the entire build session.
# These ambient IPC servers are naturally alive and waiting on pipes/sockets,
# so excluding them prevents crowding out the actual hung build actions
# (e.g. compilers, linkers, code generators) in at-a-glance timeout summaries.
# Note: Raw process snapshots (pstree_*.json) and full ASCII trees still capture
# these processes in their entirety for post-mortem forensics.
DEFAULT_TIMEOUT_EXCLUDED_PROCESSES: frozenset[str] = frozenset(
    {
        # RBE (Remote Build Execution) client daemon handling remote caching and execution.
        "reproxy",
        # ResultStore streaming proxy daemon recording build telemetry events.
        "rsproxy",
        # C++ header dependency scanning server for remote compile actions.
        "scandeps_server",
    }
)


def _collect_timeout_metadata(
    log_dir: pathlib.Path,
    filter_fn: Callable[[pstree.ProcessNode], bool] | None = None,
    max_leaves: int = build_timeout.DEFAULT_METADATA_MAX_LEAVES,
) -> build_timeout.TimeoutMetadata | None:
    """Discovers diagnostic snapshots generated during build timeout and extracts active leaf processes."""
    timeout_dir = log_dir / TIMEOUT_LOGS_DIR
    if filter_fn is None:
        filter_fn = lambda n: n.comm not in DEFAULT_TIMEOUT_EXCLUDED_PROCESSES
    return build_timeout.TimeoutMetadata.collect(
        timeout_dir, filter_fn=filter_fn, max_leaves=max_leaves
    )


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
        return json.loads(path.read_text())
    except json.JSONDecodeError as e:
        raise BuildConfigurationError(f"Failed to parse {path}: {e}")
    except Exception as e:
        raise BuildConfigurationError(f"Failed to read {path}: {e}")


def get_cpu_count() -> int:
    return os.cpu_count() or 1


def rbe_cpu_concurrency(rbe_enabled: bool) -> int:
    """Computes the build concurrency (-j) supported by the host CPU."""
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
        msg(
            f"Warning: The following environment variables starting with 'RBE_' "
            f"are set and may override RBE tool configurations: {', '.join(rbe_vars)}"
        )


def str_to_resultstore(value: str) -> str:
    val_lower = value.lower()
    if val_lower in ("true", "1", "yes", "all"):
        return "all"
    elif val_lower in ("false", "0", "no", "none"):
        return "none"
    elif val_lower in ("ninja", "bazel"):
        return val_lower
    raise argparse.ArgumentTypeError(
        f"Invalid --resultstore value: '{value}'. Expected true/false/all/none/ninja/bazel."
    )


def normalize_resultstore_value(val: Any) -> str:
    if val is True or val == "all":
        return "all"
    elif val is False or val is None or val == "none":
        return "none"
    elif val in ("ninja", "bazel"):
        return val
    return "none"


def load_user_preference(path: pathlib.Path) -> str | None:
    """Loads resultstore preference, supporting both new and legacy formats."""
    props = properties.load_properties(path)
    if "resultstore" in props:
        return normalize_resultstore_value(props["resultstore"])
    elif "RESULTSTORE_ENABLED" in props:
        # Legacy binary representation: 1=all, 0=none
        val = props["RESULTSTORE_ENABLED"]
        if val == "1":
            return "all"
        elif val == "0":
            return "none"
    return None


def gcp_instance_name(value: str) -> str:
    """Validates and returns a 4-component GCP instance name.

    Expected format: 'projects/<project-id>/instances/<instance-name>'
    """
    parts = value.split("/")
    if (
        len(parts) != 4
        or parts[0] != "projects"
        or parts[2] != "instances"
        or not parts[1]
        or not parts[3]
    ):
        raise argparse.ArgumentTypeError(
            f"Invalid instance format '{value}'. "
            "Expected format: 'projects/<project-id>/instances/<instance-name>'"
        )
    return value


class BuildLock:
    """This context-manager ensures at most one build is running per build dir.

    Importantly, it prints a lock-acquired message so noninteractive agents
    understand what is happening.
    """

    def __init__(
        self, build_dir: pathlib.Path, print_message: bool = False
    ) -> None:
        # LINT.IfChange(build_lock)
        self.build_lock_file = pathlib.Path(f"{build_dir}.build_lock")
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


def resolve_source_dir(environ: dict[str, str]) -> pathlib.Path:
    """Resolves and returns the absolute path to the Fuchsia source checkout directory."""
    source_dir_str = environ.get("FUCHSIA_DIR", "")
    if source_dir_str:
        return pathlib.Path(source_dir_str).resolve()
    try:
        return find_fuchsia_dir()
    except ValueError:
        # Fallback to finding it relative to this script: //build/scripts/main_build.py
        return _SCRIPT.resolve().parent.parent.parent


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

    @functools.cached_property
    def authenticated_user(self) -> str:
        """Resolves and caches the user identity for RBE/ResultStore authentication.

        Returns:
            The resolved user name string.

        Raises:
            BuildConfigurationError: If the user cannot be resolved and
              gcert authentication is required.
        """
        user = None
        if "USER" in self.env:
            user = self.env["USER"]
        else:
            try:
                user = getpass.getuser()
            except Exception:
                pass

        if not user:
            if self.rbe_auth_type in (
                "loas",
                "oauth",
            ) or self.resultstore_auth_type in ("loas", "oauth"):
                raise BuildConfigurationError(
                    "USER environment variable is not set and could not be "
                    "inferred. This is required for RBE/ResultStore user authentication."
                )
            user = "builder"
        return user

    @functools.cached_property
    def rbe_instance(self) -> str | None:
        """Returns the active RBE instance name, prioritizing CLI flag over configs."""
        if self.config.rbe_instance:
            return self.config.rbe_instance

        if not self.rbe_enabled:
            return None

        for config_path in self.get_reproxy_configs():
            cfg_dict = _parse_cfg_file(config_path)
            instance = cfg_dict.get("instance")
            if instance:
                return instance
        return None

    @functools.cached_property
    def resultstore_instance(self) -> str | None:
        """Returns the active ResultStore instance name, prioritizing CLI flag over configs."""
        if self.config.resultstore_instance:
            return self.config.resultstore_instance

        if not self.needs_auth:
            return None

        for config_path in self.get_rsproxy_configs():
            cfg_dict = _parse_cfg_file(config_path)
            instance = cfg_dict.get("rs_instance")
            if instance:
                return instance
        return None

    @functools.cached_property
    def cas_instance(self) -> str | None:
        """Returns the active CAS instance name, prioritizing CLI flag over configs."""
        if self.config.cas_instance:
            return self.config.cas_instance

        if not self.needs_auth:
            return None

        for config_path in self.get_rsproxy_configs():
            cfg_dict = _parse_cfg_file(config_path)
            instance = cfg_dict.get("cas_instance")
            if instance:
                return instance
        return None

    @functools.cached_property
    def rbe_quota_project(self) -> str:
        """Returns the GCP project ID used for RBE quota billing.

        Infers from the active RBE instance name.
        """
        if self.rbe_instance:
            project_id = _extract_project_id_from_instance(self.rbe_instance)
            if project_id:
                return project_id
        return ""

    @functools.cached_property
    def resultstore_quota_project(self) -> str:
        """Returns the GCP project ID used for ResultStore quota billing.

        Infers from the active ResultStore instance name.
        """
        if self.resultstore_instance:
            project_id = _extract_project_id_from_instance(
                self.resultstore_instance
            )
            if project_id:
                return project_id
        return ""

    def _isolate_gcloud_credentials(self) -> pathlib.Path | None:
        """Invokes the credential isolation script to isolate Google Application Credentials.

        Returns:
            The Path to the isolated credentials file, or None if isolation failed
            or was bypassed.
        """
        # Skip credential isolation if neither service requires user-based credentials.
        if self.rbe_auth_type in (
            "machine",
            "none",
        ) and self.resultstore_auth_type in ("machine", "none"):
            return None

        isolate_script = self.gcloud_creds_script
        if not is_executable(isolate_script):
            raise BuildConfigurationError(
                f"Required in-tree credentials management script is missing or unreadable: {isolate_script}"
            )

        # Use ResultStore's quota project if RBE does not require user credentials.
        quota_project = (
            self.rbe_quota_project
            if self.rbe_auth_type not in ("machine", "none")
            else self.resultstore_quota_project
        )
        if not quota_project:
            return None

        try:
            output = subprocess.check_output(
                [
                    str(PYTHON_BIN),
                    str(isolate_script),
                    "isolate",
                    "--out-dir",
                    str(self.out_dir),
                    "--quota-project",
                    quota_project,
                ],
                text=True,
                stderr=None,
                env=self.env,
            ).strip()
            if output:
                return pathlib.Path(output)
        except subprocess.CalledProcessError:
            pass

        return None

    @functools.cached_property
    def auth_env(self) -> dict[str, str]:
        """Returns a dictionary of authentication-related environment variables."""
        env: dict[str, str] = {}
        env["USER"] = self.authenticated_user
        env["FX_BUILD_RBE_AUTH_TYPE"] = self.rbe_auth_type
        env["FX_BUILD_RS_AUTH_TYPE"] = self.resultstore_auth_type

        if not self.needs_auth:
            return env

        # Isolate Google Application Credentials to avoid global quota_project_id contamination.
        local_adc_path = self._isolate_gcloud_credentials()
        if local_adc_path:
            env["GOOGLE_APPLICATION_CREDENTIALS"] = str(local_adc_path)
        elif "GOOGLE_APPLICATION_CREDENTIALS" in self.env:
            env["GOOGLE_APPLICATION_CREDENTIALS"] = self.env[
                "GOOGLE_APPLICATION_CREDENTIALS"
            ]
        elif self.rbe_auth_type not in (
            "machine",
            "none",
        ) or self.resultstore_auth_type not in ("machine", "none"):
            try:
                default_adc = pathlib.Path.home() / gcloud.ADC_SUBPATH
                env["GOOGLE_APPLICATION_CREDENTIALS"] = str(default_adc)
            except (RuntimeError, KeyError):
                pass

        # Forward GCE metadata host overrides to support Java/Bazel inside sandboxes.
        # This automatically translates standard GCE_METADATA_HOST to the Java-specific
        # G_CLOUD_METADATA_HOST and GCLOUD_METADATA_HOST variables, allowing Bazel to
        # natively route credentials requests to our local metadata proxy.
        if "GCE_METADATA_HOST" in self.env:
            gce_host = self.env["GCE_METADATA_HOST"]
            env["GCE_METADATA_HOST"] = gce_host
            env["G_CLOUD_METADATA_HOST"] = gce_host
            env["GCLOUD_METADATA_HOST"] = gce_host

        return env

    @functools.cached_property
    def build_service_env(self) -> dict[str, str]:
        """Returns a dictionary of environment variables related to build-service proxying/overrides."""
        env: dict[str, str] = {}

        # Override the remote execution and resultstore proxy endpoints with
        # unix:// socket paths if explicit CLI sockets are passed.
        # LINT.IfChange(bazel_socket_env_vars)
        if self.config.remote_proxy_socket:
            socket_str = str(self.config.remote_proxy_socket)
            # Override for reproxy, through build/rbe/fuchsia-reproxy-wrap.sh:
            env["RBE_service"] = f"unix://{socket_str}"
            # Override for rsproxy, through build/resultstore/fuchsia-rsproxy-wrap.sh:
            env["RS_cas_service"] = f"unix://{socket_str}"
            # Consumed by build/bazel/scripts/generate_invocation_bazelrc.py to route Bazel remote traffic
            env["FX_INTERNAL_BAZEL_RBE_SOCKET_PATH"] = socket_str

        if self.config.resultstore_proxy_socket:
            socket_str = str(self.config.resultstore_proxy_socket)
            # Override for rsproxy, through build/resultstore/fuchsia-rsproxy-wrap.sh:
            env["RS_rs_service"] = f"unix://{socket_str}"
            # Consumed by build/bazel/scripts/generate_invocation_bazelrc.py to route Bazel resultstore traffic
            env["FX_INTERNAL_BAZEL_RESULTSTORE_SOCKET_PATH"] = socket_str
        # LINT.ThenChange(
        #   //build/bazel/scripts/generate_invocation_bazelrc.py:bazel_socket_env_vars,
        #   //build/bazel_sdk/tests/scripts/run_tests.py:bazel_socket_env_vars
        # )

        # Export instance configurations from active profiles (or CLI overrides),
        # which directly drive/influence the reproxy and rsproxy daemons.
        if self.config.resultstore != "none":
            env.update(self.resultstore_profile.to_env_dict())

        if self.rbe_enabled:
            env.update(self.rbe_profile.to_env_dict())

        return env

    @staticmethod
    def from_args(
        args: argparse.Namespace,
        environ: dict[str, str],
    ) -> "FuchsiaBuildContext":
        source_dir = resolve_source_dir(environ)

        out_dir = args.out_dir
        if not out_dir:
            out_dir = source_dir / "out"

        # Resolve sticky user preferences (local overrides global)
        resultstore_pref = None
        global_config = source_dir / GLOBAL_RESULTSTORE_CONFIG
        local_config = args.build_dir / LOCAL_RESULTSTORE_CONFIG

        global_pref = load_user_preference(global_config)
        if global_pref:
            resultstore_pref = global_pref

        local_pref = load_user_preference(local_config)
        if local_pref:
            resultstore_pref = local_pref

        rbe_casfs_pref = casfs.load(source_dir, environ).virtualize

        return FuchsiaBuildContext(
            source_dir=source_dir,
            out_dir=out_dir,
            build_dir=args.build_dir,
            env=environ,
            config=FuchsiaBuildConfig.from_args(
                args,
                default_resultstore=resultstore_pref,
                default_rbe_casfs=rbe_casfs_pref,
            ),
        )

    @functools.cached_property
    def rbe_profile(self) -> rbe_profile.RbeProfile:
        try:
            prof = rbe_profile.load(self.source_dir, self.env)
            # Apply user CLI overrides if present
            if self.config.rbe_instance:
                prof.rbe_instance = self.config.rbe_instance
            return prof
        except (ValueError, FileNotFoundError) as e:
            raise BuildConfigurationError(str(e))

    @functools.cached_property
    def resultstore_profile(self) -> rs_profile.RsProfile:
        try:
            prof = rs_profile.load(self.source_dir, self.env)
            # Apply user CLI overrides if present
            if self.config.resultstore_instance:
                prof.rs_instance = self.config.resultstore_instance
            if self.config.cas_instance:
                prof.cas_instance = self.config.cas_instance
            return prof
        except (ValueError, FileNotFoundError) as e:
            raise BuildConfigurationError(str(e))

    @property
    def rbe_settings_file(self) -> pathlib.Path:
        return self.build_dir / "rbe_settings.json"

    @property
    def fint_build_py(self) -> pathlib.Path:
        """Returns the path to the fint_build.py script."""
        return self.source_dir / FINT_BUILD_PY_RELATIVE_PATH

    @property
    def fint_build_cmd_prefix(self) -> list[str]:
        """Returns the common command prefix to invoke fint_build.py under the active interpreter."""
        return [str(PYTHON_BIN), "-S", "-u", str(self.fint_build_py)]

    def _fint_wrapper_cmd(
        self,
        static_path: pathlib.Path | None = None,
        context_path: pathlib.Path | None = None,
        print_artifact_dir: bool = False,
        print_job_count: bool = False,
        ninja_error_logging_output: pathlib.Path | None = None,
        ninja_action_metrics_output: pathlib.Path | None = None,
    ) -> Iterable[str]:
        """Constructs and yields command-line arguments for executing fint_build.py.

        Args:
            static_path: Path to the Fint static parameters textproto.
            context_path: Path to the Fint context parameters textproto.
            print_artifact_dir: If True, appends the query flag to print the
              artifact directory path and exits instead of running the build.
            print_job_count: If True, appends the query flag to print the
              job_count value and exits instead of running the build.
            ninja_error_logging_output: Path where Ninja should write its error logs (ninja_errors.json).
            ninja_action_metrics_output: Path where Ninja should write its action metrics (ninja_action_metrics.json).
        """
        yield from self.fint_build_cmd_prefix

        if static_path:
            yield "--static"
            yield str(static_path)
        if context_path:
            yield "--context"
            yield str(context_path)
        elif self.build_dir:
            yield "--build-dir"
            yield str(self.build_dir)
        if self.config.verbose:
            yield "--verbose"
        if print_artifact_dir:
            yield "--print-artifact-dir"
        elif print_job_count:
            yield "--print-job-count"
        else:
            if ninja_error_logging_output:
                yield "--ninja-error-logging-output"
                yield str(ninja_error_logging_output)
            if ninja_action_metrics_output:
                yield "--ninja-action-metrics-output"
                yield str(ninja_action_metrics_output)
            yield "--"

    def fint_build_cmd(
        self,
        ninja_error_logging_output: pathlib.Path | None = None,
        ninja_action_metrics_output: pathlib.Path | None = None,
    ) -> Iterable[str]:
        """Constructs and yields command-line arguments for standard Fint build execution."""
        if not self.config.fint_params_path:
            return
        yield from self._fint_wrapper_cmd(
            static_path=self.config.fint_params_path,
            context_path=self.config.fint_context_path,
            ninja_error_logging_output=ninja_error_logging_output,
            ninja_action_metrics_output=ninja_action_metrics_output,
        )

    @functools.cached_property
    def fint_artifact_dir(self) -> pathlib.Path | None:
        """Parses and returns the Fint artifact directory path from context parameters if specified."""
        if not self.config.fint_context_path:
            return None

        try:
            cmd = list(
                self._fint_wrapper_cmd(
                    context_path=self.config.fint_context_path,
                    print_artifact_dir=True,
                )
            )
            output = subprocess.check_output(
                cmd,
                text=True,
                stderr=subprocess.PIPE,
            ).strip()
            if output:
                return pathlib.Path(output)
        except (subprocess.CalledProcessError, OSError) as e:
            msg(
                f"Failed to delegate artifact_dir parsing to fint_build.py: {e}",
                file=sys.stderr,
            )
        return None

    @functools.cached_property
    def fint_job_count(self) -> int | None:
        """Parses and returns the Fint job count from context parameters if specified."""
        if not self.config.fint_context_path:
            return None

        try:
            cmd = list(
                self._fint_wrapper_cmd(
                    context_path=self.config.fint_context_path,
                    print_job_count=True,
                )
            )
            output = subprocess.check_output(
                cmd,
                text=True,
                stderr=subprocess.PIPE,
            ).strip()
            if output:
                return int(output)
        except (subprocess.CalledProcessError, OSError, ValueError) as e:
            msg(
                f"Failed to delegate job_count parsing to fint_build.py: {e}",
                file=sys.stderr,
            )
        return None

    @property
    def rbe_config_json(self) -> pathlib.Path:
        return self.build_dir / "rbe_config.json"

    @property
    def select_auth_script(self) -> pathlib.Path:
        return self.source_dir / "build/auth/select_auth_method.py"

    @property
    def gcloud_creds_script(self) -> pathlib.Path:
        return self.source_dir / "build/auth/gcloud_creds.py"

    @property
    def top_build_wrapper(self) -> pathlib.Path:
        return self.source_dir / "build/scripts/top_build_wrap.sh"

    @property
    def build_timeout_script(self) -> pathlib.Path:
        """Path to build_timeout.py wrapper script."""
        return self.source_dir / BUILD_TIMEOUT_SCRIPT

    @property
    def args_gn(self) -> pathlib.Path:
        return self.build_dir / "args.gn"

    @property
    def args_json(self) -> pathlib.Path:
        return self.build_dir / "args.json"

    @functools.cached_property
    def enable_jobserver(self) -> bool:
        """Returns True if enable_jobserver is enabled in args.json."""
        if not exists(self.args_json):
            return False
        data = read_json(self.args_json)
        return isinstance(data, dict) and data.get("enable_jobserver") is True

    @property
    def gn_trace_path(self) -> pathlib.Path:
        """Returns the path to the GN-generated trace file."""
        return self.build_dir / "fuchsia_gn_trace.json"

    @property
    def ninja_build_trace_path(self) -> pathlib.Path:
        """The path where Ninja's raw Chrome build trace is recorded."""
        return self.build_dir / NINJA_BUILD_TRACE_GZ

    @property
    def rsninja_sh(self) -> pathlib.Path:
        return self.source_dir / "build/resultstore/rsninja.sh"

    @property
    def ninja_edge_weights_csv(self) -> pathlib.Path:
        return self.build_dir / "ninja_edge_weights.csv"

    def get_reproxy_configs(self) -> Iterable[pathlib.Path]:
        """Yields the paths to the RBE reproxy configuration files."""
        for cfg in self._rbe_config_data:
            path_val = cfg.get("path")
            if isinstance(path_val, str):
                yield self.build_dir / path_val

    def get_rsproxy_configs(self) -> Iterable[pathlib.Path]:
        """Yields the paths to the ResultStore/rsproxy configuration files."""
        yield self.source_dir / "build/resultstore/fuchsia-resultstore.cfg"
        yield self.source_dir / "build/resultstore/fuchsia-resultstore-gcertauth.cfg"

    @functools.cached_property
    def _rbe_config_data(self) -> Iterable[JSONObject]:
        """Read and parse RBE config data."""
        data = read_json(self.rbe_config_json)
        if isinstance(data, list):
            return data
        return []

    @functools.cached_property
    def _rbe_settings(self) -> rbe_settings.RbeSettings:
        """Automatically detect RBE usage from a GN-generated JSON file."""
        try:
            return rbe_settings.load(self.build_dir)
        except Exception as e:
            raise BuildConfigurationError(
                f"RBE settings could not be loaded from {self.build_dir}: {e}. "
                "Make sure you have run 'fx set'."
            )

    @functools.cached_property
    def rbe_enabled(self) -> bool:
        if self.config.rbe is not None:
            return self.config.rbe

        return self._rbe_settings.needs_reproxy

    @functools.cached_property
    def needs_auth(self) -> bool:
        if (
            self.config.resultstore != "none"
            and self.resultstore_profile.auth_type != "none"
        ):
            return True

        if self.rbe_enabled and self.rbe_profile.auth_type != "none":
            return self._rbe_settings.needs_auth

        return False

    @property
    def concurrency(self) -> int:
        """The -j value to use. Returns the Fint job_count override if present,
        otherwise the minimum over all limiting factors."""
        # If a Fint job_count is specified in the context, treat it as a pure override
        fint_jobs = self.fint_job_count
        if fint_jobs is not None and fint_jobs > 0:
            return fint_jobs

        factors = [rbe_cpu_concurrency(self.rbe_enabled)]
        if self.config.max_concurrency > 0:
            factors.append(self.config.max_concurrency)

        return min(factors)

    def _run_select_auth_script(self) -> str:
        """Invokes select_auth_method.py to determine the auth type."""
        select_script = self.select_auth_script
        try:
            output = subprocess.check_output(
                [str(PYTHON_BIN), str(select_script)],
                text=True,
                stderr=None,  # Let warnings flow through to the console directly
                env=self.env,
            )
            lines = output.strip().splitlines()
            if lines:
                return lines[-1]
            raise BuildConfigurationError(
                "Authentication selection script succeeded but returned no output."
            )
        except subprocess.CalledProcessError:
            raise BuildConfigurationError(
                "Failed to detect valid build authentication. See error messages above."
            )

    @functools.cached_property
    def _detected_auth_type(self) -> str:
        """Automatically detect the host authentication type (loas, oauth, machine, or none)."""
        mode = self.config.auth_mode
        if mode == "none" or not self.needs_auth:
            return "none"

        if mode == "machine":
            return "machine"

        # Under 'user' or 'auto' mode: run our auth selection script
        return self._run_select_auth_script()

    @functools.cached_property
    def rbe_auth_type(self) -> str:
        """Determines the authentication mechanism for RBE."""
        if self.config.auth_mode == "none":
            return "none"
        if not self.rbe_enabled or self.rbe_profile.auth_type == "none":
            return "none"
        if not self._rbe_settings.needs_auth:
            return "none"
        return self._detected_auth_type

    @functools.cached_property
    def resultstore_auth_type(self) -> str:
        """Determines the authentication mechanism for ResultStore."""
        if self.config.auth_mode == "none":
            return "none"
        if (
            self.config.resultstore == "none"
            or self.resultstore_profile.auth_type == "none"
        ):
            return "none"
        return self._detected_auth_type


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

    def write_metadata_json(self, output_path: pathlib.Path) -> None:
        """Serializes build log paths and metadata to a structured JSON file.

        Enables downstream build recipes to dynamically resolve and upload logs
        (like RBE and ResultStore) without hardcoding source-tree path patterns.

        Args:
            output_path: Path where the structured metadata JSON will be written.
        """
        context = self.context
        log_dir = self.log_dir

        fint_build_artifacts = None
        artifact_dir = context.fint_artifact_dir
        if artifact_dir:
            candidate = artifact_dir / "build_artifacts.json"
            if candidate.exists():
                fint_build_artifacts = str(candidate.resolve())

        gn_trace = None
        gn_trace_candidate = context.gn_trace_path
        if gn_trace_candidate.exists():
            gn_trace = str(gn_trace_candidate.resolve())

        metadata: JSONObject = {}
        if fint_build_artifacts:
            metadata["fint_build_artifacts"] = fint_build_artifacts
        if gn_trace:
            metadata["gn_trace"] = gn_trace

        # LINT.IfChange(build_profile_filenames)
        profile_dir = log_dir / "build_profile"
        system_profile_path = profile_dir / "system_profile.json"
        hardware_profile_path = profile_dir / "hardware_profile.json"

        build_profile: JSONObject = {}
        if system_profile_path.exists():
            build_profile["system_profile"] = str(system_profile_path.resolve())
        if hardware_profile_path.exists():
            build_profile["hardware_profile"] = str(
                hardware_profile_path.resolve()
            )

        if build_profile:
            metadata["build_profile"] = build_profile
        # LINT.ThenChange(//build/scripts/top_build_wrap.sh:build_profile_filenames)

        rbe_metadata = _collect_rbe_metadata(log_dir)
        if rbe_metadata:
            metadata["rbe"] = rbe_metadata

        resultstore_metadata = _collect_resultstore_metadata(log_dir)
        if resultstore_metadata:
            metadata["resultstore"] = resultstore_metadata

        bazel_metadata = _collect_bazel_metadata(log_dir)
        if bazel_metadata:
            metadata["bazel"] = bazel_metadata

        timeout_metadata = _collect_timeout_metadata(log_dir)
        if timeout_metadata:
            metadata["timeout"] = timeout_metadata.to_dict()

        try:
            mkdir(output_path.parent)
            output_path.write_text(json.dumps(metadata, indent=2) + "\n")
        except (OSError, TypeError, ValueError) as e:
            msg(
                f"Failed to serialize or write metadata JSON: {e}",
                file=sys.stderr,
            )
            return
        except Exception as e:
            msg(f"Unexpected error writing metadata JSON: {e}", file=sys.stderr)
            return

        if timeout_metadata and timeout_metadata.active_processes:
            self._run_fint_ingest_timeout(output_path)

    def _run_fint_ingest_timeout(self, metadata_path: pathlib.Path) -> None:
        """Invokes fint_build.py to ingest timeout diagnostics into build_artifacts.json."""
        context_path = self.context.config.fint_context_path
        if not context_path or not self.context.fint_build_py.exists():
            return
        cmd = self.context.fint_build_cmd_prefix + [
            "--context",
            str(context_path),
            "--ingest-timeout-metadata",
            str(metadata_path),
        ]
        if self.context.config.verbose:
            cmd.append("--verbose")
        try:
            subprocess.run(
                cmd,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=15.0,
            )
        except subprocess.TimeoutExpired as e:
            msg(
                f"Timed out after {e.timeout}s ingesting timeout metadata into fint build artifacts",
                file=sys.stderr,
            )
        except subprocess.CalledProcessError as e:
            msg(
                f"Failed to ingest timeout metadata into fint build artifacts: {e.stderr.strip()}",
                file=sys.stderr,
            )
        except OSError as e:
            msg(
                f"Failed to execute fint timeout ingestion: {e}",
                file=sys.stderr,
            )

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
        if self.context.config.log_dir:
            log_dir = self.context.config.log_dir.resolve()
        else:
            logs_root = self.context.out_dir / "_build_logs"
            build_dir_name = self.context.build_dir.name
            log_dir_base = logs_root / build_dir_name
            mkdir(log_dir_base)

            # Use consistent UUID and timestamp
            log_dir = (
                log_dir_base / f"build.{self.timestamp}.{self.build_uuid[:8]}"
            )

        mkdir(log_dir)

        # Record the invocation Id
        write_text(log_dir / "invocation_id", self.build_uuid + "\n")
        return log_dir

    # LINT.ThenChange(//tools/devshell/lib/vars.sh:build_log_dir_structure)

    @functools.cached_property
    def temp_home(self) -> pathlib.Path:
        """Returns a temporary fallback directory to use as $HOME.

        This is intended solely as an ephemeral, non-persistent workaround to
        support user-less, home-less environments (such as sandboxed infra
        containers in b/559830870) where $HOME is not set and the current UID
        does not have an entry in /etc/passwd (preventing standard tool lookups
        from falling back successfully).
        """
        path = self.log_dir / ".home"
        mkdir(path)
        return path

    @property
    def ninja_errors_path(self) -> pathlib.Path:
        """The path where Ninja-specific structured action failures are recorded."""
        return self.log_dir / NINJA_ERRORS_JSON

    @property
    def ninja_action_metrics_path(self) -> pathlib.Path:
        """The path where Ninja action metrics are recorded."""
        return self.log_dir / NINJA_LOGS_DIR / NINJA_ACTION_METRICS_JSON

    @property
    def timeout_log_dir(self) -> pathlib.Path:
        """The directory where timeout diagnostics and process snapshots are recorded."""
        return self.log_dir / TIMEOUT_LOGS_DIR

    def top_build_command_prefix(self) -> Iterable[str]:
        """Construct the prefix command for the top-level wrapper."""
        context = self.context
        # top_build_wrapper is a wrapper orchestrator whose purpose is to
        # auto-start/stop processes around the build.
        yield str(context.top_build_wrapper)

        if context.config.dry_run:
            yield "--dry-run"

        if context.config.tui:
            yield "--tui"

        if context.rbe_enabled:
            yield "--rbe"
            for cfg_path in context.get_reproxy_configs():
                yield "--reproxy-cfg"
                yield str(cfg_path)

        # Auth type handling
        yield "--auth-type"
        yield context.rbe_auth_type
        # Only pass --use-machine-credentials if ResultStore is disabled or also using machine auth,
        # avoiding overriding ResultStore's auth_type: none in fuchsia-rsproxy-wrap.sh.
        if context.rbe_auth_type == "machine" and (
            context.config.resultstore == "none"
            or context.resultstore_auth_type == "machine"
        ):
            yield "--use-machine-credentials"

        # Log directory setup
        yield "--build-dir"
        yield str(context.build_dir)
        yield "--log-dir"
        yield str(self.log_dir)

        if context.config.resultstore in ("all", "ninja"):
            yield "--resultstore"
            args_gn = context.args_gn
            if exists(args_gn):
                yield "--pre-build-uploads"
                yield str(args_gn)

        if context.config.profile:
            yield "--profile"

        if context.config.verbose:
            yield "--verbose"

    def timeout_command_prefix(self) -> Iterable[str]:
        """Constructs the build_timeout wrapper prefix.

        Returns command tokens up to and including '--':
            [python3, build_timeout.py, --timeout, <duration>, --log-dir, <log_dir>, --]
        """
        context = self.context
        if not context.config.timeout:
            return

        yield sys.executable
        yield str(context.build_timeout_script)
        yield "--timeout"
        yield context.config.timeout
        yield "--log-dir"
        yield str(self.timeout_log_dir)
        if context.config.timeout_grace_period:
            yield "--grace-period"
            yield context.config.timeout_grace_period
        if context.config.verbose:
            yield "--verbose"
        if context.config.tui:
            yield "--no-separate-pgrp"
        for proc in sorted(DEFAULT_TIMEOUT_EXCLUDED_PROCESSES):
            yield "--exclude-process-from-summary"
            yield proc

        yield "--"

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
            # Ensure HOME is defined to support user-less/home-less environments (b/559830870)
            "HOME": self.context.env.get("HOME") or str(self.temp_home),
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
            "SSH_AUTH_SOCK",  # need to forward the authentication socket (used by gnubby) for bazel
            "MAKEFLAGS",
            "TMPDIR",  # was passed for Goma on macOS, but it might have other uses.
            "CLICOLOR_FORCE",
            "FUCHSIA_BAZEL_DISK_CACHE",
            "FUCHSIA_BAZEL_DISK_CACHE_SIZE",
            "FUCHSIA_BAZEL_JOB_COUNT",
            "FUCHSIA_BAZEL_PRINT_COMMANDS",
            "FUCHSIA_DEBUG_BAZEL_SANDBOX",
            "NINJA_PERSISTENT_TIMEOUT_SECONDS",
            "NINJA_PERSISTENT_LOG_FILE",
            "FX_BUILD_RBE_STATS",
            "FX_BUILD_QUIET",
            "FX_REMOTE_BUILD_METRICS",  # Honor environment variable to disable RBE build metrics.
            "BUILDBUCKET_ID",  # Propagated in infra environments to tag ResultStore/BES uploads
            "BUILDBUCKET_BUILDER",
            "SWARMING_TASK_ID",
        ]
        for var in forward_vars:
            if var in self.context.env:
                build_env[var] = self.context.env[var]

        build_env.update(self.context.auth_env)

        # Inject ResultStore induction signals to allow passive wrappers to dynamically
        # configure themselves at runtime.
        resultstore = self.context.config.resultstore
        # LINT.IfChange(resultstore_ninja_env_vars)
        build_env["FX_INTERNAL_RESULTSTORE_NINJA"] = (
            "1" if resultstore in ("all", "ninja") else "0"
        )
        # LINT.ThenChange(//build/resultstore/fuchsia-rsproxy-wrap.sh:resultstore_ninja_env_vars)

        # LINT.IfChange(resultstore_bazel_env_vars)
        resultstore_bazel = "0"
        if resultstore in ("all", "bazel"):
            # Decide between "resultstore" (developer) and "resultstore_infra" (infra).
            if (
                self.context.resultstore_auth_type == "loas"
                and "BUILDBUCKET_ID" not in self.context.env
            ):
                resultstore_bazel = "resultstore"
            else:
                resultstore_bazel = "resultstore_infra"
        build_env["FX_INTERNAL_RESULTSTORE_BAZEL"] = resultstore_bazel
        # LINT.ThenChange(//build/bazel/wrapper.bazel.sh:resultstore_bazel_env_vars)

        build_env[casfs.ENV_CASFS_VIRTUALIZE] = (
            "1" if self.context.config.rbe_casfs else "0"
        )

        build_env.update(self.context.build_service_env)

        return build_env

    def new_build_command_execution(
        self,
        command_type: str,
        build_command: list[str],
        is_build: bool = True,
        preflight_actions: Sequence[PreflightAction] = (),
    ) -> "BuildCommandExecution":
        """Creates a self-contained BuildCommandExecution.

        Args:
          command_type: one of 'ninja', 'bazel' or 'other'.
          build_command: the build tool command tokens.
          is_build: whether this is a real build (vs. a query/tool command).
          preflight_actions: optional build-tool-specific callables to run under
            the build lock, just before the build command is spawned.
        """
        build_env = self.get_build_env()
        context = self.context

        # For non-build commands (e.g. 'bazel query' or 'bazel info'), we bypass
        # top_build_wrap.sh completely since no remote execution (reproxy) or
        # ResultStore (rsproxy) background wrappers/daemons are required.
        if not is_build:
            return BuildCommandExecution(
                full_command=list(build_command),
                env=build_env,
                invocation=self,
                is_build=is_build,
                preflight_actions=preflight_actions,
            )

        top_cmd = list(self.top_build_command_prefix())
        resultstore_post_build_uploads = []

        if context.config.resultstore in ("all", "ninja"):
            if command_type == "ninja":
                ninja_log_dir = self.log_dir / "ninja_logs"
                resultstore_post_build_uploads.extend(
                    [
                        ninja_log_dir / "ninja_action_metrics.json",
                        ninja_log_dir / "ninja_dirty_sources.log",
                        context.ninja_edge_weights_csv,
                        context.ninja_build_trace_path,
                    ]
                )

        top_cmd.extend(
            arg
            for f in resultstore_post_build_uploads
            for arg in ("--post-build-uploads", str(f))
        )

        # Prepare Ninja-specific options
        if command_type == "ninja":
            build_command = self._inject_ninja_args(build_command)

        fint_cmd = list(
            context.fint_build_cmd(
                ninja_error_logging_output=self.ninja_errors_path,
                ninja_action_metrics_output=(
                    self.ninja_action_metrics_path
                    if command_type == "ninja"
                    else None
                ),
            )
        )
        if fint_cmd:
            build_command = fint_cmd + list(build_command)

        full_cmd = top_cmd + ["--"] + list(build_command)

        if is_build and context.config.timeout:
            mkdir(self.timeout_log_dir)
            full_cmd = list(self.timeout_command_prefix()) + full_cmd

        return BuildCommandExecution(
            full_command=full_cmd,
            env=build_env,
            invocation=self,
            is_build=is_build,
            preflight_actions=preflight_actions,
        )

    def _inject_ninja_args(
        self,
        build_command: list[str],
    ) -> list[str]:
        """Return new build command with Ninja-specific flags injected in the right place."""
        ninja_log_dir = self.log_dir / "ninja_logs"
        mkdir(ninja_log_dir)

        # Record the set of inputs that triggered build actions.
        dirty_sources = ninja_log_dir / "ninja_dirty_sources.log"
        # Record action count metrics.
        action_metrics = self.ninja_action_metrics_path
        # Record structured action failures
        error_logging_output = self.ninja_errors_path
        # Record Ninja's raw chrome build trace file
        chrome_trace_output = self.context.ninja_build_trace_path

        ninja_bin = build_command[0]
        remaining_args = build_command[1:]
        injected = [
            ninja_bin,
            "--dirty_sources_list",
            str(dirty_sources),
            "--action_metrics_output",
            str(action_metrics),
            "--error_logging_output",
            str(error_logging_output),
            "--chrome_trace",
            str(chrome_trace_output),
        ]
        if (
            self.context.enable_jobserver
            and "--jobserver" not in remaining_args
            and "--jobserver-pool" not in remaining_args
        ):
            injected.append("--jobserver")
        return injected + list(remaining_args)


@dataclasses.dataclass
class BuildCommandExecution(object):
    """BuildCommandExecution represents a single build command.

    To execute the build command, call .run().

    Fields:
      full_command: shell command tokens
      env: environment variables
      invocation: the build invocation this execution belongs to
      is_build: whether this is a real build (vs. a query/tool command)
      preflight_actions: tool-specific callables run under the build lock
        just before the command is spawned (skipped in dry-run mode)
      cleanup_files: list of files to remove after execution
    """

    full_command: Sequence[str]
    env: dict[str, str]
    invocation: BuildInvocation
    is_build: bool = True
    preflight_actions: Sequence[PreflightAction] = ()
    cleanup_files: list[pathlib.Path] = dataclasses.field(default_factory=list)

    def _run_without_locking(self) -> BuildResult:
        """Execute the build command."""
        config = self.invocation.context.config
        if config.verbose or config.dry_run:
            env_str = " ".join(
                f"{k}={shlex.quote(v)}" for k, v in sorted(self.env.items())
            )
            msg(
                f"Running: {env_str} {' '.join(shlex.quote(c) for c in self.full_command)}"
            )

        if config.verbose:
            auth_summary = (
                f"\n[Auth Configuration Resolved]:\n"
                f"  Config Auth Mode: {config.auth_mode}\n"
                f"  RBE Auth:         {self.invocation.context.rbe_auth_type}\n"
                f"  ResultStore Auth: {self.invocation.context.resultstore_auth_type}\n"
                f"  Auth User:       {self.invocation.context.authenticated_user}\n"
                f"  Metadata Host:   {self.env.get('GCE_METADATA_HOST', 'NOT_SET')}\n"
                f"  Java Metadata:   {self.env.get('G_CLOUD_METADATA_HOST', 'NOT_SET')}\n"
                f"  G-A-C Path:      {self.env.get('GOOGLE_APPLICATION_CREDENTIALS', 'NOT_SET')}\n"
                f"--------------------------------------------------"
            )
            msg(auth_summary)
        # Note: when config.dry_run is set, we still execute the command,
        # but we have forwarded --dry-run to the top_build_wrapper, which
        # will skip the actual build execution. This allows for high-fidelity
        # verification of the entire wrapper orchestration stack.

        # Run preflight actions here so they execute under BuildLock (acquired in
        # run()) and cannot race a concurrent build of the same directory.
        if not config.dry_run:
            preflight_failed = False
            for action in self.preflight_actions:
                status = action()
                if status == PreflightStatus.FATAL:
                    return BuildResult(return_code=1)
                if status == PreflightStatus.ERROR:
                    preflight_failed = True
            if preflight_failed:
                return BuildResult(return_code=1)

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

        ts_msg("Starting build command execution...", config.verbose)
        return_code = -1
        try:
            return_code = managed.run()
        finally:
            ts_msg(
                f"Build command execution completed with status {return_code}",
                config.verbose,
            )

        return BuildResult(return_code=return_code)

    def run(self) -> BuildResult:
        """Execute the build command, guarded by a build lock if it is a build command.

        Returns:
          exit code of the command, 0 for success.
        """
        try:
            if self.is_build:
                quiet = os.getenv("FX_BUILD_QUIET") == "1"
                with BuildLock(
                    self.invocation.context.build_dir, print_message=quiet
                ):
                    return self._run_without_locking()
            else:
                return self._run_without_locking()
        finally:
            for f in self.cleanup_files:
                f.unlink(missing_ok=True)


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


# Timeout for probing whether the Ninja persistent server is responding.
_NINJA_SERVER_PROBE_TIMEOUT_S = 5


def _kill_ninja_persistent_server(
    build_dir: pathlib.Path,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
) -> None:
    """Kills any Ninja persistent server for build_dir by scanning /proc.

    Identifies the server by process name ('ninja'), working directory
    (build_dir), and NINJA_PERSISTENT_MODE=server in its environment.
    Linux-only; a no-op elsewhere.
    """
    if sys.platform != "linux":
        return
    try:
        resolved_build_dir = build_dir.resolve()
    except OSError:
        return

    for pid in procfs.iter_pids(proc_root):
        # Check cheap procfs entries (comm, cwd) before reading environ.
        if procfs.read_process_comm(pid, proc_root) != "ninja":
            continue
        if procfs.read_process_cwd(pid, proc_root) != resolved_build_dir:
            continue
        environ = procfs.read_process_environ(pid, proc_root)
        if not environ or environ.get("NINJA_PERSISTENT_MODE") != "server":
            continue
        msg(f"Killing Ninja persistent server (PID {pid})")
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            # The process exited, or belongs to another user; skip it.
            continue


def _kill_ninja_persistent_server_if_stuck(
    build_dir: pathlib.Path,
    env: dict[str, str],
    ninja_bin: str,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
) -> PreflightStatus:
    """Kills the Ninja persistent server for build_dir if it is unresponsive.

    Probes the server with `ninja -t server pid` (`status` and `stop` do not
    wait for an IPC reply from the server). If the probe times out, the server
    is considered stuck and terminated via /proc. Other outcomes (a PID, -1
    when no server is running, or an error) leave the server alone.
    """
    cmd = [ninja_bin, "-C", str(build_dir), "-t", "server", "pid"]
    try:
        subprocess.run(
            cmd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=_NINJA_SERVER_PROBE_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        msg("Ninja persistent server is not responding, killing it.")
        _kill_ninja_persistent_server(build_dir, proc_root=proc_root)
    except OSError as e:
        msg(f"Failed to probe Ninja persistent server: {e}")
    return PreflightStatus.OK


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
    is_build = is_ninja_build_command(build_cmd)
    invocation = BuildInvocation(context)

    # Ensure any wedged persistent server from an interrupted build is
    # cleaned up before starting a new build.
    preflight_actions: list[PreflightAction] = []
    if is_build and is_ninja_persistent_mode_enabled(context.env):
        preflight_actions.append(
            lambda: _kill_ninja_persistent_server_if_stuck(
                context.build_dir, context.env, ninja_bin
            )
        )

    return invocation.new_build_command_execution(
        "ninja",
        build_cmd,
        is_build=is_build,
        preflight_actions=preflight_actions,
    )


def is_ninja_persistent_mode_enabled(env: Mapping[str, str]) -> bool:
    """Returns True if NINJA_PERSISTENT_MODE is enabled for client builds."""
    return env.get("NINJA_PERSISTENT_MODE") in ("1", "on", "client")


def is_ninja_build_command(ninja_args: list[str]) -> bool:
    """Returns True if the ninja command is a build-like command."""
    for arg in ninja_args:
        if arg in (
            "-n",
            "--dry-run",
            "-h",
            "--help",
            "--version",
        ) or arg.startswith("-t"):
            return False
    return True


def _is_bazel_binary(path_str: str) -> bool:
    """Returns True if the path_str points to a bazel binary."""
    return "bazel" in pathlib.Path(path_str).name


def is_bazel_build_command(bazel_args: list[str]) -> bool:
    """Returns True if the bazel command is a build-like command."""
    for arg in bazel_args:
        # Skip leading options, env assignments, env prefix, and the bazel binary itself
        if (
            arg.startswith("-")
            or "=" in arg
            or arg in ("env", "bazel")
            or _is_bazel_binary(arg)
        ):
            continue
        # The first non-option/non-env/non-binary argument is the subcommand!
        return arg in ("build", "test", "run", "coverage")
    return False


def new_bazel_build_command_execution(
    context: FuchsiaBuildContext,
    bazel_args: list[str],
) -> BuildCommandExecution:
    """Construct a bazel build command.

    Behavior:
    - Selects the Bazel binary: uses the first argument if 'bazel' in its name,
      otherwise defaults to 'bazel'.
    """
    if bazel_args and _is_bazel_binary(bazel_args[0]):
        # Already has bazel binary
        build_cmd = bazel_args
    else:
        build_cmd = ["bazel"] + list(bazel_args)

    is_build = is_bazel_build_command(build_cmd)
    invocation = BuildInvocation(context)
    return invocation.new_build_command_execution(
        "bazel", build_cmd, is_build=is_build
    )


def new_other_build_command_execution(
    context: FuchsiaBuildContext,
    other_args: list[str],
) -> BuildCommandExecution:
    """Construct an arbitrary build command.

    Note: main_build.py is the orchestrator for actual, standard build execution
    pipelines (such as Ninja and Bazel). It is NOT expected or intended to receive
    or wrap fint-specific commands, as fint is managed and executed externally.

    Args:
        context: FuchsiaBuildContext.
        other_args: list of arguments for the command.
    """
    if not other_args:
        raise BuildConfigurationError(
            "other command requires at least one argument."
        )

    is_build = True
    invocation = BuildInvocation(context)
    return invocation.new_build_command_execution(
        "other", other_args, is_build=is_build
    )


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=pathlib.Path, required=True)
    parser.add_argument("--out-dir", type=pathlib.Path)
    parser.add_argument(
        "--log-dir",
        type=pathlib.Path,
        default=None,
        help="Specify the directory to write all build logs and diagnostics. "
        "If not specified, defaults to a timestamped and UUID-tagged subdirectory "
        "under the build logs root.",
    )

    # Custom handling for boolean flags to support --flag=true/false
    parser.add_argument(
        "--rbe", type=properties.str_to_bool, nargs="?", const=True
    )
    parser.add_argument("--no-rbe", action="store_false", dest="rbe")

    parser.add_argument(
        "--rbe-casfs",
        type=properties.str_to_bool,
        nargs="?",
        const=True,
        default=None,
        help="Virtualize RBE build artifacts on casfs instead of downloading them "
        f"(can also be set via {casfs.ENV_CASFS_VIRTUALIZE}).",
    )
    parser.add_argument(
        "--no-rbe-casfs",
        action="store_false",
        dest="rbe_casfs",
        help="Disable RBE artifact virtualization on casfs.",
    )

    parser.add_argument(
        "--resultstore",
        type=str_to_resultstore,
        nargs="?",
        const="all",
        default=None,
        help="Upload build events and metadata to ResultStore (all, ninja, bazel, or none; default: none).",
    )
    parser.add_argument(
        "--no-resultstore",
        action="store_const",
        const="none",
        dest="resultstore",
        help="Disable uploading build events to ResultStore.",
    )

    parser.add_argument(
        "--profile", type=properties.str_to_bool, nargs="?", const=True
    )
    parser.add_argument("--no-profile", action="store_false", dest="profile")

    parser.add_argument(
        "--tui", type=properties.str_to_bool, nargs="?", const=True
    )
    parser.add_argument("--no-tui", action="store_false", dest="tui")

    parser.add_argument(
        "--auth-mode",
        choices=["auto", "user", "machine", "none"],
        default="auto",
        help="Specify RBE/ResultStore authentication mode (auto, user, machine, or none; default: auto).",
    )

    parser.add_argument(
        "--max-concurrency",
        type=int,
        default=0,
        help="Upper bound on the automatically chosen -j value, or 0 for no bound. Ignored when -j is passed explicitly or when the Fint context specifies job_count.",
    )

    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--no-status", action="store_false", dest="status")
    parser.add_argument(
        "--timeout",
        type=str,
        default=None,
        help="Specify an overall build timeout (e.g. '90m', '5400s'). "
        "Wraps execution in build_timeout.py with progressive signaling and diagnostic snapshots.",
    )
    parser.add_argument(
        "--timeout-grace-period",
        type=str,
        default=None,
        help="Specify an explicit in-tree shutdown grace period (e.g. '60s'). "
        "Intermediate escalation steps in build_timeout.py are scaled to fit within this duration.",
    )

    parser.add_argument(
        "--fint-params-path",
        type=pathlib.Path,
        default=None,
        help="Path to the Fint static parameters textproto. If provided, main_build.py automatically wraps execution inside fint_build.py.",
    )
    parser.add_argument(
        "--fint-context-path",
        type=pathlib.Path,
        default=None,
        help="Path to the Fint context parameters textproto. If provided, forwarded to fint_build.py --context.",
    )
    parser.add_argument(
        "--output-metadata-json",
        type=pathlib.Path,
        default=None,
        help="Path to write the structured metadata JSON describing all build logs and artifacts.",
    )
    parser.add_argument(
        "--remote-proxy-socket",
        type=pathlib.Path,
        default=None,
        help="Path to the local RBE proxy socket.",
    )
    parser.add_argument(
        "--resultstore-proxy-socket",
        type=pathlib.Path,
        default=None,
        help="Path to the local ResultStore/BES proxy socket.",
    )
    parser.add_argument(
        "--resultstore-instance",
        type=gcp_instance_name,
        default=None,
        help="Specify the target ResultStore instance name (format: projects/<project-id>/instances/<instance-name>).",
    )
    parser.add_argument(
        "--cas-instance",
        type=gcp_instance_name,
        default=None,
        help="Specify the target CAS instance name (format: projects/<project-id>/instances/<instance-name>).",
    )
    parser.add_argument(
        "--rbe-instance",
        type=gcp_instance_name,
        default=None,
        help="Specify the target RBE instance name (format: projects/<project-id>/instances/<instance-name>).",
    )

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

    invocation = None
    try:
        exec_info = args.func(context, unknown)
        invocation = exec_info.invocation
        return exec_info.run().return_code
    except BuildConfigurationError as e:
        msg(f"Error: {e}", file=sys.stderr)
        return 1
    except signal_utils.BuildInterruptedError as e:
        # SignalManagedProcess ensures that we have already waited for any
        # child processes before this is raised.
        sig_name = signal.Signals(e.signum).name
        msg(
            f"Interrupted by {sig_name}, exiting ({e.return_code})",
            file=sys.stderr,
        )
        return e.return_code
    except KeyboardInterrupt:
        # Fallback for standard interrupts outside SignalManagedProcess
        msg("Received KeyboardInterrupt, exiting (130)", file=sys.stderr)
        return 130
    finally:
        if args.output_metadata_json:
            active_invocation = invocation
            if not active_invocation:
                try:
                    active_invocation = BuildInvocation(context)
                except Exception:
                    pass
            if active_invocation:
                active_invocation.write_metadata_json(args.output_metadata_json)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
