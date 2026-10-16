#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Python implementation of 'fint build' as a command wrapper."""

import pathlib
import shlex
import sys

# Find the fuchsia root containing prebuilt/third_party/protobuf-py3.
# This search is necessary because in hermetic test sandboxes (like fx_build_test),
# the directory structure is flattened to 3 levels deep instead of the usual 4,
# so a fixed relative path lookup (parent.parent.parent.parent) would fail.
PREBUILT_PROTOBUF_DIR = pathlib.Path("prebuilt/third_party/protobuf-py3")

fint_dir = pathlib.Path(__file__).resolve().parent
fuchsia_root = fint_dir
found_root = False
for _ in range(5):
    if (fuchsia_root / PREBUILT_PROTOBUF_DIR).exists():
        found_root = True
        break
    fuchsia_root = fuchsia_root.parent

if not found_root:
    raise FileNotFoundError(
        f"Could not find valid Fuchsia root containing {PREBUILT_PROTOBUF_DIR}"
    )

# Append fuchsia_root to sys.path so we can import tools.integration.fint.proto
if fuchsia_root.exists() and str(fuchsia_root) not in sys.path:
    sys.path.insert(0, str(fuchsia_root))

protobuf_wheel = fuchsia_root / PREBUILT_PROTOBUF_DIR
if protobuf_wheel.exists() and str(protobuf_wheel) not in sys.path:
    sys.path.insert(0, str(protobuf_wheel))

# Bypass strict protobuf gencode/runtime version check to align prebuilts at build-time (see b/537501139)
try:
    import google.protobuf.runtime_version as rv

    rv.ValidateProtobufRuntimeVersion = lambda *args, **kwargs: None
except ImportError:
    pass

import argparse
import functools
import json
import os
import platform
import re
import shutil
import subprocess
import tempfile
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Any, Generator, Iterable, TextIO

from build.scripts import signal_utils
from google.protobuf import json_format, text_format
from tools.integration.fint.proto import (
    build_artifacts_pb2,
    context_pb2,
    static_pb2,
)

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]

# Module-scope constants for file names
BUILD_ARTIFACTS_JSON = "build_artifacts.json"
NINJA_ERRORS_JSON = "ninja_errors.json"
NINJA_ACTION_METRICS_JSON = "ninja_action_metrics.json"
TOOL_PATHS_JSON = "tool_paths.json"
GENERATED_SOURCES_JSON = "generated_sources.json"
PREBUILT_BINARY_SETS_JSON = "prebuilt_binaries.json"
FORCE_NONHERMETIC_REBUILD_SENTINEL = "force_nonhermetic_rebuild"
LAST_NINJA_BUILD_SUCCESS_STAMP = "last_ninja_build_success.stamp"
RUST_TARGET_MAPPING_JSON = "rust_target_mapping.json"
TESTS_JSON = "tests.json"
NINJATRACE2JSON_PY_RELATIVE_PATH = pathlib.Path(
    "build/scripts/ninjatrace2json.py"
)
# LINT.IfChange(ninja_build_trace_filename)
NINJA_BUILD_TRACE_GZ = "ninja_build_trace.json.gz"
# LINT.ThenChange(/build/scripts/main_build.py:ninja_build_trace_filename)
NINJATRACE_JSON_GZ = "ninjatrace.json.gz"
BUILDSTATS_JSON_GZ = "buildstats.json.gz"

# These tests use a system image as data, so they appear affected by a broad
# range of changes, but they're almost never actually sensitive to said changes.
# https://fxbug.dev/42146209 tracks generating this list automatically.
# Keep in sync with neverAffectedTestLabels in affected.go.
NEVER_AFFECTED_TEST_LABELS = frozenset(
    [
        "//src/recovery/simulator:recovery_simulator_boot_test",
        "//src/recovery/simulator:recovery_simulator_serial_test",
    ]
)


@dataclass
class BuildExecution:
    """Represents the parameters and result of a wrapped build command run."""

    command: list[str]
    exit_code: int = 0
    affected_tests: list[str] = field(default_factory=list)
    build_not_affected: bool = False
    # Set when a post-build step fails, so the failure summary doesn't blame
    # the delegated build command.
    failure_summary: str | None = None


class Timer:
    """Context manager to measure elapsed duration in seconds."""

    def __init__(self) -> None:
        self.duration: float = 0.0
        self._start: float = 0.0

    def __enter__(self) -> "Timer":
        self._start = time.time()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: Any,
    ) -> None:
        self.duration = time.time() - self._start


_SCRIPT_NAME = pathlib.Path(__file__).name


def msg(msg: str, file: TextIO = sys.stdout) -> None:
    """Standardized logger that prefixes messages with the script name."""
    print(f"[{_SCRIPT_NAME}] {msg}", file=file)


def ts_msg(msg: str, verbose: bool = False, file: TextIO = sys.stderr) -> None:
    """Logs high-precision timestamps to stderr in verbose mode."""
    if verbose:
        print(f"[{time.time():.9f}] [{_SCRIPT_NAME}]: {msg}", file=file)


@dataclass(frozen=True)
class HostProperties:
    """Represents host system properties like OS and CPU architecture."""

    os: str
    cpu: str

    @classmethod
    def detect(cls) -> "HostProperties":
        """Auto-detects the host system's OS and CPU architecture."""
        host_os = platform.system().lower()
        if host_os == "darwin":
            host_os = "mac"
        host_cpu = platform.machine()
        if host_cpu == "x86_64":
            host_cpu = "x64"
        elif host_cpu in ["aarch64", "arm64"]:
            host_cpu = "arm64"
        return cls(os=host_os, cpu=host_cpu)

    @property
    def is_mac(self) -> bool:
        """Returns True if the host is running macOS."""
        return self.os == "mac"

    @property
    def is_linux(self) -> bool:
        """Returns True if the host is running Linux."""
        return self.os == "linux"

    @property
    def platform_dir(self) -> str:
        """Returns the prebuilt platform directory name (e.g. linux-x64, mac-x64)."""
        return f"{self.os}-{self.cpu}"

    @property
    def gn_relative_path(self) -> pathlib.Path:
        """Returns the relative path to the prebuilt GN tool."""
        return (
            pathlib.Path("prebuilt")
            / "third_party"
            / "gn"
            / self.platform_dir
            / "gn"
        )

    @property
    def ninja_relative_path(self) -> pathlib.Path:
        """Returns the relative path to the prebuilt Ninja tool."""
        return (
            pathlib.Path("prebuilt")
            / "third_party"
            / "ninja"
            / self.platform_dir
            / "ninja"
        )

    def matches_tool(self, tool: "ToolPathSpec") -> bool:
        """Returns True if the tool's OS and CPU match this host."""
        return tool.os == self.os and tool.cpu == self.cpu


def load_static_spec(path: pathlib.Path) -> static_pb2.Static:
    """Loads and unmarshals the static spec textproto."""
    spec = static_pb2.Static()
    text_format.Merge(path.read_text(), spec)
    return spec


def load_context_spec(path: pathlib.Path) -> context_pb2.Context:
    """Loads and unmarshals the context spec textproto."""
    spec = context_pb2.Context()
    text_format.Merge(path.read_text(), spec)
    return spec


def load_json_list(path: pathlib.Path) -> JSONArray:
    """Loads a JSON list file.

    Raises FileNotFoundError, JSONDecodeError, or ValueError if the file is
    missing, malformed, or does not contain a list.
    """
    data = json.loads(path.read_text())
    if not isinstance(data, list):
        raise ValueError(
            f"Expected JSON list in file {path}, but got: {type(data).__name__}"
        )
    return data


def load_json_dict(path: pathlib.Path) -> JSONObject | None:
    """Reads and decodes a JSON object file.

    Returns None and emits a warning if the file is unreadable, malformed, or
    does not contain a JSON object (dictionary).
    """
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        msg(f"Warning: Failed to read JSON from {path}: {e}", file=sys.stderr)
        return None

    if not isinstance(data, dict):
        msg(
            f"Warning: Expected JSON object in {path}, got: {type(data).__name__}",
            file=sys.stderr,
        )
        return None

    return data


def _to_int(val: JSONValue, context: str) -> int | None:
    """Converts a JSON value to an integer, warning on TypeError or ValueError."""
    if val is None:
        return None
    if isinstance(val, (int, str, float)):
        try:
            return int(val)
        except (TypeError, ValueError) as e:
            msg(
                f"Warning: Invalid integer value for {context} ({val!r}): {e}",
                file=sys.stderr,
            )
            return None
    msg(
        f"Warning: Expected integer for {context}, got {type(val).__name__}",
        file=sys.stderr,
    )
    return None


def parse_ninja_action_metrics(
    data: JSONValue,
) -> build_artifacts_pb2.NinjaActionMetrics:
    """Parses a decoded JSON value into a NinjaActionMetrics protobuf."""
    metrics = build_artifacts_pb2.NinjaActionMetrics()
    if not isinstance(data, dict):
        return metrics

    if "initial_actions" in data:
        initial = _to_int(data["initial_actions"], "initial_actions")
        if initial is not None:
            metrics.initial_actions = initial

    if "final_actions" in data:
        final = _to_int(data["final_actions"], "final_actions")
        if final is not None:
            metrics.final_actions = final

    action_counts = data.get("action_counts")
    if isinstance(action_counts, dict):
        for mnemonic, count in action_counts.items():
            val = _to_int(count, f"action_counts[{mnemonic}]")
            if val is not None:
                metrics.actions_by_type[str(mnemonic)] = val
    elif action_counts is not None:
        msg(
            f"Warning: Expected dict for action_counts, got {type(action_counts).__name__}",
            file=sys.stderr,
        )

    return metrics


# TODO(olivernewman): This is a stopgap. Delete the Bazel output condensing
# logic once infra failure summaries are generated from structured build
# results (e.g. a checkpoint produced by the build TUI in headless mode)
# rather than by scraping Bazel's console output.

# Matches lines that only appear in the output of a Bazel invocation.
_BAZEL_OUTPUT_RE = re.compile(
    r"^(INFO: Invocation ID: |Starting local Bazel server |\[[\d,]+ / [\d,]+\] )",
    re.MULTILINE,
)

# Matches lines of Bazel output that report progress or statistics rather than
# errors. They often make up the vast majority of a failed Bazel action's
# output, pushing the actual error message past the point where the recipe
# truncates the failure summary. These depend on the wording of Bazel's UI, so
# a Bazel upgrade may require updating them.
_BAZEL_NOISE_RE = re.compile(
    r"""^(
        \[[\d,]+\ /\ [\d,]+\]\    # e.g. "[1,234 / 5,678] Compiling foo.cc"
        | Computing\ main\ repo\ mapping:
        | Loading:
        | Analyzing:
        | Starting\ local\ Bazel\ server[ ]
        | \.\.\.\ still\ trying\ to\ connect\ to\ local\ Bazel\ server[ ]
        | \#\ Configuration:[ ]
        | INFO:\ (
            Invocation\ ID
            | Analyzed\ \d
            | Found\ \d
            | Elapsed\ time
            | \d[\d,]*\ process
            | Build\ completed\ successfully
        )
        | INFO:\s*$
    )""",
    re.VERBOSE,
)

# Matches the first line of the command that Bazel prints after an
# "ERROR: ... failed" line. Every line of the command except the last ends with
# a backslash.
# LINT.IfChange(verbose_failures)
_BAZEL_COMMAND_START_RE = re.compile(r"^  \(cd \S+ && \\$")
# LINT.ThenChange(/build/bazel/scripts/bazel_action_impl.py:verbose_failures)


def condense_bazel_output(output: str) -> str:
    """Strips progress noise from a Bazel invocation's output.

    If the output contains an error, everything before the first error is also
    stripped. Output that doesn't appear to come from Bazel is returned
    unchanged.
    """
    if not _BAZEL_OUTPUT_RE.search(output):
        return output

    lines = []
    in_command = False
    prev_blank = False
    for line in output.split("\n"):
        line = line.rstrip("\r")
        if in_command:
            if not line.endswith("\\"):
                in_command = False
            continue
        if _BAZEL_COMMAND_START_RE.match(line):
            # The full command is often many kilobytes long and is rarely
            # needed to diagnose the failure, since the compiler error comes
            # right after it.
            in_command = True
            lines.append("  (command elided)")
            prev_blank = False
            continue
        if _BAZEL_NOISE_RE.match(line):
            continue
        blank = not line.strip()
        if blank and prev_blank:
            continue
        prev_blank = blank
        lines.append(line)

    if in_command:
        # The output ended partway through a command, which suggests that
        # Bazel's command format has changed and we may have elided the actual
        # error message.
        return output

    # Anything before the first error is just preamble (warnings, links, output
    # from earlier successful Bazel invocations in the same action, etc.) that
    # pushes the error further down the summary.
    for i, line in enumerate(lines):
        if line.startswith("ERROR: "):
            lines = lines[i:]
            break
    return "\n".join(lines)


# Suffix of the stamp file that is the first output of every GN bazel_action().
# LINT.IfChange(bazel_action_stamp_suffix)
_BAZEL_ACTION_STAMP_SUFFIX = (
    ".bazel_target_can_only_be_referenced_by_one_bazel_action"
)
# LINT.ThenChange(/build/bazel/bazel_action.gni:bazel_action_stamp_suffix)


@dataclass(frozen=True)
class NinjaFailure:
    """Represents a single action failure recorded in ninja_errors.json."""

    artifacts: list[str]
    exit_code: int
    output: str

    @classmethod
    def from_dict(cls, data: JSONObject) -> "NinjaFailure":
        """Constructs a NinjaFailure from a JSONObject."""
        artifacts = data.get("artifacts")
        exit_code = data.get("exit_code")
        output = data.get("output", "")

        # Coerce/validate fields
        if not isinstance(artifacts, list):
            artifacts = [str(artifacts)] if artifacts is not None else []
        try:
            # Ensure we only pass numeric/string types to int() to keep Mypy completely happy.
            exit_code_int = (
                int(exit_code)
                if isinstance(exit_code, (int, float, str))
                else -1
            )
        except (ValueError, TypeError):
            # If the exit code cannot be cast to an integer (e.g. if it is malformed,
            # non-numeric, or absent), gracefully fallback to a sentinel value of -1.
            exit_code_int = -1

        return cls(
            artifacts=[str(a) for a in artifacts],
            exit_code=exit_code_int,
            output=condense_bazel_output(str(output)).strip(),
        )

    @property
    def is_eligible_for_deduplication(self) -> bool:
        """Returns True if the failure output is eligible for deduplication (>= 5 lines)."""
        return bool(self.output and self.output.count("\n") >= 5)

    @property
    def is_bazel_action(self) -> bool:
        """Returns True if the failed action is a GN bazel_action()."""
        return any(
            a.endswith(_BAZEL_ACTION_STAMP_SUFFIX) for a in self.artifacts
        )

    def format(self, include_output: bool = True) -> str:
        """Formats the failure into a high-signal error block."""
        artifacts_str = shlex.join(self.artifacts)
        header = f"FAILED: [code={self.exit_code}] {artifacts_str}"
        if include_output and self.output:
            return f"{header}\n\n{self.output}"
        return header


@dataclass(frozen=True)
class TestSpec:
    """Represents a parsed and statically typed test specification from tests.json."""

    label: str
    os: str
    cpu: str
    path: str
    name: str = ""
    package_label: str = ""
    source_label: str = ""

    @classmethod
    def from_dict(cls, data: JSONObject) -> "TestSpec":
        """Constructs a TestSpec safely from a JSONObject."""
        test_dict = data.get("test")
        if not isinstance(test_dict, dict):
            raise ValueError(
                f"Expected 'test' object inside test spec, but got: {test_dict}"
            )

        label = test_dict.get("label", "")
        os_val = test_dict.get("os", "")
        cpu_val = test_dict.get("cpu", "")
        path_val = test_dict.get("path", "")
        name = test_dict.get("name", "")
        package_label = test_dict.get("package_label", "")
        source_label = test_dict.get("source_label", "")

        if not (
            isinstance(label, str)
            and isinstance(os_val, str)
            and isinstance(cpu_val, str)
            and isinstance(path_val, str)
            and isinstance(name, str)
            and isinstance(package_label, str)
            and isinstance(source_label, str)
        ):
            raise ValueError(
                f"TestSpec has invalid field types: label={type(label).__name__}, "
                f"os={type(os_val).__name__}, cpu={type(cpu_val).__name__}, path={type(path_val).__name__}, "
                f"name={type(name).__name__}, package_label={type(package_label).__name__}, "
                f"source_label={type(source_label).__name__}"
            )
        return cls(
            label=label,
            os=os_val,
            cpu=cpu_val,
            path=path_val,
            name=name,
            package_label=package_label,
            source_label=source_label,
        )


def resolve_affected_test_names(
    test_specs: Iterable[TestSpec], target_labels: Iterable[str]
) -> list[str]:
    """Maps labels from `build/api/client affected_tests` to tests.json names.

    The affected_tests tool may return a test's GN label, package label, or
    source label (Bazel tests have no package label), with or without a
    toolchain suffix, so index tests by all of them. Labels that don't match
    any test are passed through as-is, matching Go fint's behavior.
    """

    def strip_toolchain(label: str) -> str:
        return label.split("(")[0]

    tests_by_label: dict[str, list[str]] = {}
    tests_by_no_toolchain_label: dict[str, list[str]] = {}
    for spec in test_specs:
        if strip_toolchain(spec.label) in NEVER_AFFECTED_TEST_LABELS:
            continue
        for label in (spec.label, spec.package_label, spec.source_label):
            if label:
                tests_by_label.setdefault(label, []).append(spec.name)
                tests_by_no_toolchain_label.setdefault(
                    strip_toolchain(label), []
                ).append(spec.name)
        tests_by_label.setdefault(spec.name, []).append(spec.name)

    affected: list[str] = []
    for label in target_labels:
        no_toolchain = strip_toolchain(label)
        if no_toolchain in NEVER_AFFECTED_TEST_LABELS:
            continue
        if label in tests_by_label:
            affected.extend(tests_by_label[label])
        elif no_toolchain in tests_by_no_toolchain_label:
            affected.extend(tests_by_no_toolchain_label[no_toolchain])
        else:
            affected.append(label)
    return list(dict.fromkeys(affected))


@dataclass(frozen=True)
class ToolPathSpec:
    """Represents a parsed and statically typed host tool path from tool_paths.json."""

    name: str
    path: str
    os: str
    cpu: str

    @classmethod
    def from_dict(cls, data: JSONObject) -> "ToolPathSpec":
        """Constructs a ToolPathSpec safely from a JSONObject."""
        name = data.get("name")
        path_val = data.get("path")
        os_val = data.get("os")
        cpu_val = data.get("cpu")

        if not (
            isinstance(name, str)
            and isinstance(path_val, str)
            and isinstance(os_val, str)
            and isinstance(cpu_val, str)
        ):
            raise ValueError(
                f"ToolPathSpec has invalid field types: name={type(name).__name__}, "
                f"path={type(path_val).__name__}, os={type(os_val).__name__}, cpu={type(cpu_val).__name__}"
            )
        return cls(name=name, path=path_val, os=os_val, cpu=cpu_val)


@dataclass(frozen=True)
class ClippyTargetSpec:
    """Represents a parsed and statically typed clippy target specification."""

    output: pathlib.Path
    sources: list[pathlib.Path]
    disable_clippy: bool

    @classmethod
    def from_dict(cls, data: JSONObject) -> "ClippyTargetSpec":
        """Constructs a ClippyTargetSpec safely from a JSONObject."""
        output_val = data.get("clippy_output")
        sources_val = data.get("src", data.get("sources", []))
        disable_clippy = data.get("disable_clippy", False)

        if not (
            isinstance(output_val, str)
            and isinstance(sources_val, list)
            and all(isinstance(s, str) for s in sources_val)
            and isinstance(disable_clippy, bool)
        ):
            raise ValueError(
                f"ClippyTargetSpec has invalid field types: output={type(output_val).__name__}, "
                f"sources={type(sources_val).__name__}, disable_clippy={type(disable_clippy).__name__}"
            )
        return cls(
            output=pathlib.Path(output_val),
            sources=[pathlib.Path(s) for s in sources_val],
            disable_clippy=disable_clippy,
        )


def parse_ninja_failures(errors_json_path: pathlib.Path) -> str | None:
    """Parses ninja_errors.json file to construct a precise, formatted FailureSummary.

    Args:
        errors_json_path: Path to the ninja_errors.json file.

    Returns:
        The formatted FailureSummary string, or None if malformed, empty, or missing.
    """
    if not errors_json_path.exists():
        return None

    try:
        with open(errors_json_path, "r") as f:
            data = json.load(f)
        return format_ninja_failures(data)
    except (json.JSONDecodeError, FileNotFoundError):
        return None


def format_ninja_failures(data: JSONObject) -> str | None:
    """Formats already loaded ninja_errors.json data into a precise FailureSummary.

    Args:
        data: The decoded JSON dictionary.

    Returns:
        The formatted FailureSummary string, or None if malformed or unsupported.
    """
    if not isinstance(data, dict) or data.get("version") != 1:
        return None

    raw_failures = data.get("failures", [])
    if not isinstance(raw_failures, list) or not raw_failures:
        return None

    failures = []
    for raw in raw_failures:
        if isinstance(raw, dict):
            failures.append(NinjaFailure.from_dict(raw))

    if not failures:
        return None

    # Ninja batches pending bazel_action()s into a single Bazel invocation, so
    # if one Bazel target fails then every GN action in the batch fails, but
    # only one of them gets the Bazel output. The rest are collateral and
    # their headers just bury the real error, as do any other output-less
    # failures, which are unlikely to be actionable on their own.
    if any(f.is_bazel_action and f.output for f in failures):
        failures = [f for f in failures if f.output]

    msg_lines = []
    seen_outputs = set()
    for failure in failures:
        # Check deduplication eligibility
        if failure.is_eligible_for_deduplication:
            if failure.output in seen_outputs:
                # Still output the failure header, but omit the redundant compiler logs
                msg_lines.append(failure.format(include_output=False))
                msg_lines.append("")
                continue
            seen_outputs.add(failure.output)

        msg_lines.append(failure.format(include_output=True))
        msg_lines.append("")

    return "\n".join(msg_lines).strip()


def run_gn_check(
    checkout_dir: pathlib.Path,
    build_dir: pathlib.Path,
    host: HostProperties,
    verbose: bool = False,
) -> int:
    """Runs 'gn check' to verify header dependency rules inside the build directory.

    Args:
        checkout_dir: Path to the Fuchsia checkout root.
        build_dir: Path to the active build directory.
        host: Resolved properties of the host platform.
        verbose: Enable verbose logging.

    Returns:
        The exit code of the 'gn check' subprocess.
    """
    gn_bin = checkout_dir / host.gn_relative_path
    cmd = [
        str(gn_bin),
        "check",
        str(build_dir),
        f"--root={checkout_dir}",
        "--check-generated",
        "--check-system",
    ]
    msg("Running gn check...")
    if verbose:
        msg(f"Command: {shlex.join(cmd)}")
    res = subprocess.run(cmd)
    return res.returncode


def check_ninja_noop(
    checkout_dir: pathlib.Path,
    build_dir: pathlib.Path,
    host: HostProperties,
    targets: list[str],
    verbose: bool = False,
) -> int:
    """Verifies that the Ninja build converges to a no-op state.

    This function does a dry-run invocation of Ninja with 'explain' and
    '--dirty_sources_list' arguments to verify that there are no pending dirty
    rebuild targets. If Ninja identifies any dirty source files, they are
    logged and displayed to help diagnose the non-no-op build state.

    Args:
        checkout_dir: Path to the Fuchsia checkout root.
        build_dir: Path to the active build directory.
        host: Resolved properties of the host platform.
        targets: Concrete list of Ninja targets to verify.
        verbose: Enable verbose logging.

    Returns:
        0 if the build successfully converges to a no-op, non-zero if Ninja
        diverges or fails.
    """
    ninja_bin = checkout_dir / host.ninja_relative_path

    with tempfile.TemporaryDirectory() as td:
        dirty_sources_path = pathlib.Path(td) / "dirty_sources.txt"

        cmd = [
            str(ninja_bin),
            "-C",
            str(build_dir),
            "-n",
            "-v",
            "-d",
            "explain",
            "--dirty_sources_list",
            str(dirty_sources_path),
        ] + targets

        msg("Verifying ninja build converges to no-op...")
        if verbose:
            msg(f"Command: {shlex.join(cmd)}")

        res = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
        )

        if res.returncode != 0:
            return res.returncode

        if "ninja: no work to do." in res.stdout:
            return 0

        # Handle non-noop build
        msg(
            "Error: Ninja build did not converge to no-op.",
            file=sys.stderr,
        )
        print(res.stdout[:1000], file=sys.stderr)

        # Print the dirty sources list if any were captured
        if dirty_sources_path.exists():
            dirty_content = dirty_sources_path.read_text().strip()
            if dirty_content:
                msg(
                    f"Identified dirty source files:\n{dirty_content}",
                    file=sys.stderr,
                )
        return 1


def collect_failure_diagnostics(build_dir: pathlib.Path) -> None:
    msg("Build failed. Diagnostic logs would be collected here.")


@dataclass(frozen=True)
class BuildContext:
    """Cohesive grouping of the static spec, context spec, and host properties."""

    static_spec: static_pb2.Static
    context_spec: context_pb2.Context
    host: HostProperties
    verbose: bool = False

    @property
    def build_dir(self) -> pathlib.Path:
        """Returns the path to the build directory."""
        return pathlib.Path(self.context_spec.build_dir)

    @property
    def checkout_dir(self) -> pathlib.Path:
        """Returns the path to the checkout directory."""
        return pathlib.Path(self.context_spec.checkout_dir)

    @property
    def api_client_path(self) -> pathlib.Path:
        """Returns the absolute path to the build API client executable."""
        return self.checkout_dir / "build" / "api" / "client"

    @property
    def debug_symbols_dir(self) -> pathlib.Path:
        """Returns the path to the debug symbols export directory."""
        return self.build_dir / "debug_symbols"

    @property
    def debug_symbols_manifest(self) -> pathlib.Path:
        """Returns the path to the debug symbols JSON manifest file."""
        return self.debug_symbols_dir / "debug_symbols.json"

    @property
    def last_ninja_build_targets_path(self) -> pathlib.Path:
        """Returns the absolute path to the last ninja build targets file."""
        return self.build_dir / "last_ninja_build_targets.txt"

    @property
    def tests_json_path(self) -> pathlib.Path:
        """Returns the absolute path to the tests.json build API module."""
        return self.build_dir / TESTS_JSON

    @property
    def rust_target_mapping_json_path(self) -> pathlib.Path:
        """Returns the absolute path to the Rust target mapping JSON file."""
        return self.build_dir / RUST_TARGET_MAPPING_JSON

    @property
    def raw_ninja_trace_path(self) -> pathlib.Path:
        """Returns the absolute path to the raw input Chrome trace file written by Ninja."""
        return self.build_dir / NINJA_BUILD_TRACE_GZ

    @property
    def ninjatrace2json_py_path(self) -> pathlib.Path:
        """Returns the absolute path to the ninjatrace2json.py helper script."""
        return self.checkout_dir / NINJATRACE2JSON_PY_RELATIVE_PATH

    @property
    def processed_trace_path(self) -> pathlib.Path:
        """Returns the absolute path to the generated Perfetto ninjatrace file (the output trace file)."""
        return self.build_dir / NINJATRACE_JSON_GZ

    @property
    def buildstats_json_path(self) -> pathlib.Path:
        """Returns the absolute path to the generated build stats file (the output stats file)."""
        return self.build_dir / BUILDSTATS_JSON_GZ

    @property
    def generated_sources_json_path(self) -> pathlib.Path:
        """Returns the absolute path to the generated C++ sources JSON file."""
        return self.build_dir / GENERATED_SOURCES_JSON

    @property
    def prebuilt_binary_sets_json_path(self) -> pathlib.Path:
        """Returns the absolute path to the prebuilt binary sets JSON file."""
        return self.build_dir / PREBUILT_BINARY_SETS_JSON

    @property
    def default_ninja_action_metrics_path(self) -> pathlib.Path:
        """Returns the default absolute path to the Ninja action metrics JSON file."""
        return self.build_dir / NINJA_ACTION_METRICS_JSON

    @property
    def artifact_dir(self) -> pathlib.Path | None:
        """Returns the path to the artifact directory if specified in the context spec."""
        return (
            pathlib.Path(self.context_spec.artifact_dir)
            if self.context_spec.artifact_dir
            else None
        )

    @property
    def artifact_debug_symbols_manifest(self) -> pathlib.Path | None:
        """Returns the path to the debug symbols JSON manifest inside the artifact directory."""
        if self.artifact_dir:
            return self.artifact_dir / "debug_symbols.json"
        return None

    @property
    def should_export_breakpad_symbols(self) -> bool:
        """Returns True if output_breakpad_syms is set to true in the static spec's GN args."""
        # TODO: Use a centralized, shared GN parsing utility module here in the future
        # to parse GN arguments robustly across different build tools and integrators.
        for arg in self.static_spec.gn_args:
            key, eq, value = arg.partition("=")
            if (
                eq
                and key.strip() == "output_breakpad_syms"
                and value.strip().lower() == "true"
            ):
                return True
        return False

    @functools.cached_property
    def tool_paths(self) -> list[ToolPathSpec]:
        """Loads and returns the tool paths config list."""
        path = self.build_dir / TOOL_PATHS_JSON
        if not path.exists():
            return []
        try:
            # Narrow the try clause strictly to loading/decoding the JSON file.
            paths_data = load_json_list(path)
        except ValueError as e:
            raise ValueError(f"Failed to decode {TOOL_PATHS_JSON}: {e}")

        # Process the decoded data list with strict validation.
        paths = []
        for item in paths_data:
            if not isinstance(item, dict):
                raise ValueError(
                    f"Expected dict entry inside {TOOL_PATHS_JSON}, but got: {type(item).__name__}"
                )
            paths.append(ToolPathSpec.from_dict(item))
        return paths

    def resolve_tool_path(self, tool_name: str) -> pathlib.Path | None:
        """Resolves a host tool from tool_paths.json relative to build_dir."""
        try:
            paths = self.tool_paths
        except ValueError:
            return None
        rel_path = lookup_tool_path(paths, tool_name, self.host)
        return (self.build_dir / rel_path) if rel_path else None

    @functools.cached_property
    def ninjatrace_tool_path(self) -> pathlib.Path | None:
        """Returns the resolved path to ninjatrace_prebuilt, if configured."""
        return self.resolve_tool_path("ninjatrace_prebuilt")

    @functools.cached_property
    def buildstats_tool_path(self) -> pathlib.Path | None:
        """Returns the resolved path to buildstats_prebuilt, if configured."""
        return self.resolve_tool_path("buildstats_prebuilt")

    @functools.cached_property
    def clippy_targets(self) -> list[ClippyTargetSpec]:
        """Loads and returns the clippy/rust target mapping list."""
        if not self.rust_target_mapping_json_path.exists():
            return []
        try:
            # Narrow the try clause strictly to loading/decoding the JSON file.
            targets_data = load_json_list(self.rust_target_mapping_json_path)
        except ValueError as e:
            raise ValueError(
                f"Failed to decode {RUST_TARGET_MAPPING_JSON}: {e}"
            )

        # Process the decoded data list with strict validation.
        targets = []
        for item in targets_data:
            if not isinstance(item, dict):
                raise ValueError(
                    f"Expected dict entry inside {RUST_TARGET_MAPPING_JSON}, but got: {type(item).__name__}"
                )
            targets.append(ClippyTargetSpec.from_dict(item))
        return targets

    @functools.cached_property
    def test_specs(self) -> list[TestSpec]:
        """Loads and returns the test specs from tests.json."""
        if not self.tests_json_path.exists():
            return []
        try:
            # Narrow the try clause strictly to loading/decoding the JSON file.
            tests_data = load_json_list(self.tests_json_path)
        except ValueError as e:
            raise ValueError(f"Failed to decode {TESTS_JSON}: {e}")

        specs = []
        for item in tests_data:
            if not isinstance(item, dict):
                raise ValueError(
                    f"Expected dict entry inside {TESTS_JSON}, but got: {type(item).__name__}"
                )
            specs.append(TestSpec.from_dict(item))
        return specs

    @functools.cached_property
    def generated_sources(self) -> JSONArray:
        """Loads and returns the generated sources list."""
        if not self.generated_sources_json_path.exists():
            return []
        return load_json_list(self.generated_sources_json_path)

    @functools.cached_property
    def prebuilt_binary_sets(self) -> JSONArray:
        """Loads and returns the prebuilt binary sets list."""
        if not self.prebuilt_binary_sets_json_path.exists():
            return []
        return load_json_list(self.prebuilt_binary_sets_json_path)

    def _default_and_host_test_targets(self) -> Iterable[str]:
        """Yields default targets or host test targets if configured."""
        if self.static_spec.include_default_ninja_target:
            yield ":default"

    def _generated_source_targets(self) -> Iterable[str]:
        """Yields generated C++ source targets if configured."""
        if self.static_spec.include_generated_sources:
            if not self.generated_sources_json_path.exists():
                msg(
                    f"Warning: include_generated_sources is enabled, but {GENERATED_SOURCES_JSON} is missing on disk.",
                    file=sys.stderr,
                )
                return
            for f in self.generated_sources:
                if isinstance(f, str) and (
                    f.endswith(".cc") or f.endswith(".h")
                ):
                    yield f

    def _prebuilt_binary_manifests(self) -> Iterable[str]:
        """Yields prebuilt binary manifest targets if configured."""
        if self.static_spec.include_prebuilt_binary_manifests:
            if not self.prebuilt_binary_sets_json_path.exists():
                msg(
                    f"Warning: include_prebuilt_binary_manifests is enabled, but {PREBUILT_BINARY_SETS_JSON} is missing on disk.",
                    file=sys.stderr,
                )
                return
            for item in self.prebuilt_binary_sets:
                if not isinstance(item, dict):
                    raise ValueError(
                        f"Expected dict entry inside {PREBUILT_BINARY_SETS_JSON}, but got: {type(item).__name__}"
                    )
                manifest = item.get("manifest")
                if not isinstance(manifest, str):
                    raise ValueError(
                        f"Prebuilt binary set entry has invalid 'manifest' field type: {type(manifest).__name__}"
                    )
                yield manifest

    def _tool_targets(self) -> Iterable[str]:
        """Yields prebuilt host tool targets if configured."""
        if self.static_spec.tools:
            for tool in self.static_spec.tools:
                path = lookup_tool_path(self.tool_paths, tool, self.host)
                if path:
                    yield path

    def _custom_ninja_targets(self) -> Iterable[str]:
        """Yields custom static Ninja targets if configured."""
        if self.static_spec.ninja_targets:
            yield from self.static_spec.ninja_targets

    def _clippy_targets(self) -> Iterable[str]:
        """Yields clippy target output files to build based on static spec configuration."""
        include_lint_targets = self.static_spec.include_lint_targets
        if include_lint_targets == static_pb2.Static.NO_LINT_TARGETS:
            return

        # Build lookup set of changed files paths for O(1) checks
        changed_files = {f.path for f in self.context_spec.changed_files}

        for clippy in self.clippy_targets:
            if clippy.disable_clippy:
                continue

            # Filter clippy targets based on include_lint_targets setting
            if include_lint_targets == static_pb2.Static.ALL_LINT_TARGETS:
                yield str(clippy.output)
            elif (
                include_lint_targets == static_pb2.Static.AFFECTED_LINT_TARGETS
            ):
                # Check if any clippy source file has been modified in the changed_files set
                for source in clippy.sources:
                    # Purely lexical path computation avoids expensive filesystem lookups
                    # and correctly matches Go fint filepath.Rel/Clean behavior.
                    checkout_path_str = os.path.relpath(
                        os.path.normpath(self.build_dir / source),
                        self.checkout_dir,
                    )
                    checkout_path_posix = pathlib.Path(
                        checkout_path_str
                    ).as_posix()

                    if checkout_path_posix in changed_files:
                        yield str(clippy.output)
                        break
            else:
                raise ValueError(
                    f"Unknown include_lint_targets value: {include_lint_targets}"
                )

    def _stream_all_targets(self) -> Iterable[str]:
        """Streams all configured and resolved targets from all sources."""
        yield from self._default_and_host_test_targets()
        yield from self._generated_source_targets()
        yield from self._prebuilt_binary_manifests()
        yield from self._tool_targets()
        yield from self._custom_ninja_targets()
        yield from self._clippy_targets()

    def _get_targets(self) -> list[str]:
        """Resolves Ninja build targets based on specifications and build API JSON files."""
        return sorted(list(set(self._stream_all_targets())))

    @contextmanager
    def wrap_ninja(
        self,
        base_command: list[str],
    ) -> Generator[BuildExecution, None, None]:
        """Context manager wrapping the Ninja pre-build and post-build events."""
        if not self.context_spec.checkout_dir:
            raise ValueError(
                "checkout_dir is required in the Context specification"
            )

        rebuild_sentinel_path = (
            self.build_dir / FORCE_NONHERMETIC_REBUILD_SENTINEL
        )
        success_stamp_path = self.build_dir / LAST_NINJA_BUILD_SUCCESS_STAMP

        # Pre-build: Touch rebuild sentinel if incremental
        if self.static_spec.incremental:
            rebuild_sentinel_path.write_text("")

        # Clear previous success stamp
        if success_stamp_path.exists():
            success_stamp_path.unlink()

        # Target expansion and handling
        targets = self._get_targets()
        full_command = base_command + targets

        result = BuildExecution(command=full_command)
        success = False
        try:
            yield result
            if result.exit_code != 0:
                return

            self._run_post_ninja_checks(result, targets, success_stamp_path)
            success = result.exit_code == 0
        finally:
            if not success:
                collect_failure_diagnostics(self.build_dir)

    def _run_post_ninja_checks(
        self,
        result: BuildExecution,
        targets: list[str],
        success_stamp_path: pathlib.Path,
    ) -> None:
        """Runs post-build tests and validation checks for Ninja, modifying result.exit_code if any fail."""
        # Update last_ninja_build_targets.txt cleanly to prevent unnecessary Ninja artifacts invalidations.
        targets_str = " ".join(targets)
        if (
            not self.last_ninja_build_targets_path.exists()
            or self.last_ninja_build_targets_path.read_text() != targets_str
        ):
            self.last_ninja_build_targets_path.write_text(targets_str)

        # Post-build success stamp
        success_stamp_path.write_text("")

        # Post-build verification checks
        gn_status = run_gn_check(
            checkout_dir=self.checkout_dir,
            build_dir=self.build_dir,
            host=self.host,
            verbose=self.verbose,
        )
        if gn_status != 0:
            result.exit_code = gn_status
            return

        if not self.context_spec.skip_ninja_noop_check:
            noop_status = check_ninja_noop(
                checkout_dir=self.checkout_dir,
                build_dir=self.build_dir,
                host=self.host,
                targets=targets,
                verbose=self.verbose,
            )
            if noop_status != 0:
                result.exit_code = noop_status
                return

        # Export debug symbols at the very end to avoid spending time on symbols
        # dumping/exporting if the gn_check or ninja_noop verifications fail.
        try:
            self._export_debug_symbols()
        except RuntimeError as e:
            msg(f"Error: {e}", file=sys.stderr)
            result.exit_code = 1
            return

        try:
            self._find_affected_tests(result)
        except (RuntimeError, ValueError) as e:
            msg(f"Error: {e}", file=sys.stderr)
            result.exit_code = 1
            result.failure_summary = f"Failed to find affected tests: {e}"
            return

    @contextmanager
    def wrap_bazel(
        self,
        base_command: list[str],
    ) -> Generator[BuildExecution, None, None]:
        """Context manager wrapping the Bazel pre-build and post-build events."""
        if not self.context_spec.checkout_dir:
            raise ValueError(
                "checkout_dir is required in the Context specification"
            )

        result = BuildExecution(command=base_command)
        success = False
        try:
            yield result
            if result.exit_code == 0:
                # TODO: Implement Bazel-specific post-build checks and success actions
                pass
            success = True
        finally:
            if not success:
                # TODO: Implement Bazel-specific post-build failure/diagnostic collections
                pass

    def _export_debug_symbols(self) -> None:
        """Invokes the build API to export last build's debug symbols."""
        if self.debug_symbols_dir.exists():
            shutil.rmtree(self.debug_symbols_dir)
        self.debug_symbols_dir.mkdir(parents=True, exist_ok=True)

        cmd = [
            str(self.api_client_path),
            "--build-dir",
            str(self.build_dir),
            "export_last_build_debug_symbols",
            f"--output-dir={self.debug_symbols_dir}",
        ]
        if self.should_export_breakpad_symbols:
            cmd.append("--with-breakpad-symbols")

        msg("Exporting last build debug symbols...")
        if self.verbose:
            msg(f"Command: {shlex.join(cmd)}")

        res = subprocess.run(cmd)
        if res.returncode != 0:
            raise RuntimeError(
                f"export_last_build_debug_symbols failed with exit code {res.returncode}"
            )

    def _find_affected_tests(self, result: BuildExecution) -> None:
        """Populates result with the tests affected by the changed files.

        The infra recipes use the affected tests to skip unaffected tests in
        presubmit, and skip testing entirely if build_not_affected is set.
        """
        changed_files = [f.path for f in self.context_spec.changed_files]
        # Without an artifact dir there's nowhere to report the results, and
        # without changed files or tests there's nothing to analyze.
        if not self.artifact_dir or not changed_files or not self.test_specs:
            return

        with tempfile.NamedTemporaryFile(
            "w", dir=self.artifact_dir, prefix="changed_files_", suffix=".txt"
        ) as files_list:
            files_list.write("\n".join(changed_files) + "\n")
            files_list.flush()
            cmd = [
                str(self.api_client_path),
                "--build-dir",
                str(self.build_dir),
                "affected_tests",
                f"--files-list={files_list.name}",
                "--format=json",
            ]
            msg("Finding affected tests...")
            if self.verbose:
                msg(f"Command: {shlex.join(cmd)}")
            res = subprocess.run(cmd, stdout=subprocess.PIPE, text=True)

        if res.returncode != 0:
            raise RuntimeError(
                f"affected_tests failed with exit code {res.returncode}"
            )
        try:
            output = json.loads(res.stdout)
        except json.JSONDecodeError as e:
            raise RuntimeError(f"Failed to decode affected_tests output: {e}")
        if not isinstance(output, dict):
            raise RuntimeError(
                f"Expected JSON object from affected_tests, but got: {type(output).__name__}"
            )

        labels = [t["label"] for t in output.get("test_targets", [])]
        result.affected_tests = resolve_affected_test_names(
            self.test_specs, labels
        )
        result.build_not_affected = bool(
            output.get("build_not_affected", False)
        )
        msg(
            f"Found {len(result.affected_tests)} affected tests "
            f"(build_not_affected={result.build_not_affected})"
        )

    def _run_buildstats(self) -> pathlib.Path | None:
        """Runs the buildstats prebuilt tool on the raw trace to generate buildstats.json.gz (Step 2)."""
        if not self.buildstats_tool_path:
            return None
        stats_cmd = [
            str(self.buildstats_tool_path),
            "--ninjatrace",
            str(self.processed_trace_path),
            "--output",
            str(self.buildstats_json_path),
        ]
        if self.verbose:
            msg(f"Running buildstats: {shlex.join(stats_cmd)}")
        res_stats = subprocess.run(stats_cmd)
        if res_stats.returncode == 0:
            return self.buildstats_json_path
        return None

    def _merge_subbuild_traces(self) -> None:
        """Merges and interleaves nested sub-build traces into processed_trace_path in-place (Step 3)."""
        if not (
            (self.build_dir / "ninja_subbuilds.json").is_file()
            and self.ninjatrace2json_py_path.is_file()
        ):
            return

        merge_cmd = [
            sys.executable,
            str(self.ninjatrace2json_py_path),
            "--fuchsia-build-dir",
            str(self.build_dir),
            "--ninja-path",
            str(self.checkout_dir / self.host.ninja_relative_path),
            "--ninjatrace-path",
            str(self.ninjatrace_tool_path),
            "--subbuilds-in-place",
        ]
        if self.verbose:
            msg(f"Running ninjatrace2json: {shlex.join(merge_cmd)}")
        subprocess.run(merge_cmd)

    def _generate_ninja_traces(
        self,
    ) -> tuple[pathlib.Path | None, pathlib.Path | None]:
        """Generates Perfetto ninjatrace and buildstats files from Ninja's build trace, if available."""
        if not (
            self.raw_ninja_trace_path and self.raw_ninja_trace_path.is_file()
        ):
            return None, None

        # Warn if the raw trace exists but tool paths fail to load
        try:
            paths = self.tool_paths
        except ValueError as e:
            msg(
                f"Warning: Skipping Ninja trace analysis: Failed to decode {TOOL_PATHS_JSON}: {e}",
                file=sys.stderr,
            )
            return None, None

        if not paths:
            msg(
                f"Warning: Skipping Ninja trace analysis: {TOOL_PATHS_JSON} is missing or empty.",
                file=sys.stderr,
            )
            return None, None

        if not self.ninjatrace_tool_path:
            msg(
                "Warning: Skipping Ninja trace analysis: ninjatrace_prebuilt tool path not found in tool paths.",
                file=sys.stderr,
            )
            return None, None

        ninjatrace_out = None
        buildstats_out = None

        try:
            # Step 1: Run the raw ninjatrace prebuilt tool to generate a raw unmerged processed trace!
            cmd = [
                str(self.ninjatrace_tool_path),
                "-ninjabuildtrace",
                str(self.raw_ninja_trace_path),
                "-trace-json",
                str(self.processed_trace_path),
            ]
            if self.verbose:
                msg(f"Running ninjatrace: {shlex.join(cmd)}")
            res = subprocess.run(cmd)
            if res.returncode == 0:
                ninjatrace_out = self.processed_trace_path

                # Step 2: Run buildstats on this unmerged raw trace to generate buildstats.json.gz.
                # This completely prevents any Go string-pid unmarshal crashes or subbuild double-counting!
                buildstats_out = self._run_buildstats()

                # Step 3: Run the in-tree ninjatrace2json.py script with --subbuilds-in-place.
                # This chronologically merges and interleaves sub-build traces into processed_trace_path
                # to produce the final, consolidated trace for Perfetto, with full safety guarantees!
                self._merge_subbuild_traces()
        except OSError as e:
            msg(
                f"Warning: Failed to execute Ninja trace post-processing: {e}",
                file=sys.stderr,
            )

        return ninjatrace_out, buildstats_out

    def produce_build_artifacts(
        self,
        duration_seconds: int,
        failure_summary: str | None = None,
        ninja_action_metrics_path: pathlib.Path | None = None,
        affected_tests: Iterable[str] = (),
        build_not_affected: bool = False,
    ) -> None:
        """Serializes and writes the build_artifacts.json manifest to the artifact directory."""
        if not self.artifact_dir:
            return

        artifacts = build_artifacts_pb2.BuildArtifacts()
        artifacts.ninja_duration_seconds = duration_seconds
        if failure_summary:
            artifacts.failure_summary = failure_summary
        artifacts.affected_tests.extend(affected_tests)
        artifacts.build_not_affected = build_not_affected

        # Collect Ninja action metrics if present
        action_metrics_file = (
            ninja_action_metrics_path
            if ninja_action_metrics_path is not None
            else (
                self.default_ninja_action_metrics_path
                if self.context_spec.build_dir
                else None
            )
        )
        if action_metrics_file and action_metrics_file.is_file():
            metrics_data = load_json_dict(action_metrics_file)
            if metrics_data is not None:
                parsed_metrics = parse_ninja_action_metrics(metrics_data)
                artifacts.ninja_action_metrics.CopyFrom(parsed_metrics)

        # Generate and register the interactive Perfetto Ninja traces!
        ninjatrace_path, buildstats_path = self._generate_ninja_traces()

        if ninjatrace_path:
            artifact_trace_path = self.artifact_dir / ninjatrace_path.name
            try:
                shutil.copy2(ninjatrace_path, artifact_trace_path)
                artifacts.ninjatrace_json_files.append(str(artifact_trace_path))
            except OSError as e:
                msg(
                    f"Warning: Failed to copy {ninjatrace_path.name} to artifact directory: {e}",
                    file=sys.stderr,
                )

        if buildstats_path:
            artifact_stats_path = self.artifact_dir / buildstats_path.name
            try:
                shutil.copy2(buildstats_path, artifact_stats_path)
                artifacts.buildstats_json_files.append(str(artifact_stats_path))
            except OSError as e:
                msg(
                    f"Warning: Failed to copy {buildstats_path.name} to artifact directory: {e}",
                    file=sys.stderr,
                )

        # Copy debug_symbols.json from build_dir/debug_symbols to artifact_dir if present,
        # and register it in log_files.
        if (
            self.debug_symbols_manifest.is_file()
            and self.artifact_debug_symbols_manifest
        ):
            try:
                shutil.copy2(
                    self.debug_symbols_manifest,
                    self.artifact_debug_symbols_manifest,
                )
                artifacts.log_files["debug_symbols.json"] = str(
                    self.artifact_debug_symbols_manifest
                )
            except OSError as e:
                msg(
                    f"Warning: Failed to copy debug_symbols.json: {e}",
                    file=sys.stderr,
                )

        # MessageToJson formats with nice spacing/indentation
        json_data = json_format.MessageToJson(
            artifacts, always_print_fields_with_no_presence=True
        )
        json_manifest_path = self.artifact_dir / BUILD_ARTIFACTS_JSON
        json_manifest_path.write_text(json_data)
        msg(
            f"Successfully wrote build artifacts manifest to {json_manifest_path}"
        )


def lookup_tool_path(
    tool_paths: list[ToolPathSpec], tool_name: str, host: HostProperties
) -> str | None:
    """Looks up the relative path of a host tool in tool_paths."""
    for tool in tool_paths:
        if tool.name == tool_name and host.matches_tool(tool):
            return tool.path
    return None


def resolve_fallback_build_dir(
    build_dir: pathlib.Path | None = None,
) -> pathlib.Path:
    """Resolves build directory when context spec is not provided.

    Args:
        build_dir: Optional explicit path to the build directory.

    Returns:
        The resolved absolute Path to the build directory.
    """
    if build_dir:
        return build_dir.resolve()

    fx_build_dir_file = fuchsia_root / ".fx-build-dir"
    if fx_build_dir_file.is_file():
        rel_dir = fx_build_dir_file.read_text().strip()
        if rel_dir:
            candidate = pathlib.Path(rel_dir)
            if not candidate.is_absolute():
                candidate = fuchsia_root / candidate
            if candidate.is_dir():
                return candidate.resolve()

    default_out = fuchsia_root / "out" / "default"
    if default_out.is_dir():
        return default_out.resolve()

    return pathlib.Path.cwd().resolve()


def make_build_context(
    static_path: pathlib.Path,
    context_path: pathlib.Path | None,
    verbose: bool = False,
    build_dir: pathlib.Path | None = None,
) -> BuildContext:
    """Creates a BuildContext by loading and parsing specifications, auto-detecting host properties.

    Args:
        static_path: Path to the static spec textproto.
        context_path: Path to the context spec textproto, or None to generate
            a fallback context spec.
        verbose: Whether to enable verbose output logging.
        build_dir: Path to the build directory. If omitted and context_path is
            None, the build directory will be inferred from .fx-build-dir,
            out/default, or the current working directory.

    Returns:
        The initialized BuildContext instance.
    """
    static_spec = load_static_spec(static_path)

    if context_path:
        context_spec = load_context_spec(context_path)
    else:
        resolved_build_dir = resolve_fallback_build_dir(build_dir=build_dir)
        context_spec = context_pb2.Context(
            checkout_dir=str(fuchsia_root.resolve()),
            build_dir=str(resolved_build_dir),
        )

    host = HostProperties.detect()
    return BuildContext(
        static_spec=static_spec,
        context_spec=context_spec,
        host=host,
        verbose=verbose,
    )


def _main_arg_parser() -> argparse.ArgumentParser:
    """Constructs and returns the command line argument parser."""
    parser = argparse.ArgumentParser(
        description="Python-based fint build command wrapper"
    )
    parser.add_argument(
        "--static",
        required=False,
        default=None,
        type=pathlib.Path,
        help="Path to static spec",
    )
    parser.add_argument(
        "--context",
        required=False,
        default=None,
        type=pathlib.Path,
        help="Path to context spec. If omitted, it will be automatically generated.",
    )
    parser.add_argument(
        "--build-dir",
        required=False,
        default=None,
        type=pathlib.Path,
        help="Path to build directory. If omitted, inferred from --context or active build config.",
    )
    parser.add_argument(
        "--print-artifact-dir",
        action="store_true",
        help="Print the resolved artifact directory from the context spec and exit.",
    )
    parser.add_argument(
        "--print-job-count",
        action="store_true",
        help="Print the resolved job_count from the context spec and exit.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Enable verbose output logging.",
    )
    parser.add_argument(
        "--mode",
        choices=["ninja", "bazel"],
        default="ninja",
        help="Build system wrapper mode (ninja or bazel). Default is ninja.",
    )
    parser.add_argument(
        "--ninja-error-logging-output",
        dest="ninja_error_logging_output",
        type=pathlib.Path,
        default=None,
        help="Path where Ninja should write its error logs (ninja_errors.json).",
    )
    parser.add_argument(
        "--ninja-action-metrics-output",
        dest="ninja_action_metrics_output",
        type=pathlib.Path,
        default=None,
        help="Path where Ninja should write its action metrics (ninja_action_metrics.json).",
    )
    parser.add_argument(
        "wrapped_cmd",
        nargs="*",
        default=None,
        help="The wrapped command to execute (optionally after '--')",
    )
    return parser


def main(argv: list[str]) -> int:
    # Verify that we are running in a valid Fuchsia checkout environment when executed.
    if not (fuchsia_root / ".jiri_manifest").exists():
        msg(
            f"INTERNAL ERROR: Could not find valid Fuchsia root: {fuchsia_root}",
            file=sys.stderr,
        )
        return 1

    parser = _main_arg_parser()
    args = parser.parse_args(argv)

    if args.print_artifact_dir:
        if not args.context:
            msg(
                "Error: --context is required with --print-artifact-dir",
                file=sys.stderr,
            )
            return 1
        context_spec = load_context_spec(args.context)
        if context_spec.artifact_dir:
            print(context_spec.artifact_dir)
        return 0

    if args.print_job_count:
        if not args.context:
            msg(
                "Error: --context is required with --print-job-count",
                file=sys.stderr,
            )
            return 1
        context_spec = load_context_spec(args.context)
        print(context_spec.job_count)
        return 0

    # Ensure build execution requirements are satisfied if not querying
    is_static_valid = args.static and str(args.static) not in (".", "")
    required_build_args = [
        (is_static_valid, "the following arguments are required: --static"),
        (args.wrapped_cmd, "wrapped_cmd is required for build execution"),
    ]
    for val, err_msg in required_build_args:
        if not val:
            msg(f"Error: {err_msg}", file=sys.stderr)
            return 2

    ts_msg("Fint build wrapper starting up...", args.verbose)

    ctx = make_build_context(
        static_path=args.static,
        context_path=args.context,
        verbose=args.verbose,
        build_dir=args.build_dir,
    )

    # Select Build Strategy
    wrappers = {
        "ninja": ctx.wrap_ninja,
        "bazel": ctx.wrap_bazel,
    }
    wrapper = wrappers[args.mode]

    with wrapper(args.wrapped_cmd) as run:
        if args.verbose:
            msg(f"Delegated command: {shlex.join(run.command)}")
            ts_msg(
                f"Executing delegated build command: {shlex.join(run.command)}",
                args.verbose,
            )

        with Timer() as t:
            try:
                # Wrap the delegated command execution in SignalManagedProcess to gracefully
                # handle and relay process signals (such as Ctrl+C / SIGINT) to Ninja/Bazel.
                managed = signal_utils.SignalManagedProcess(
                    run.command, verbose=args.verbose
                )
                exit_code = managed.run()
            except signal_utils.BuildInterruptedError as e:
                # If interrupted, propagate the signal-derived exit code (128 + signum)
                exit_code = e.return_code
                msg(f"Build interrupted by signal {e.signum}")
        duration_seconds = round(t.duration)

        run.exit_code = exit_code
        ts_msg(
            f"Delegated build command completed with status {run.exit_code} (duration: {duration_seconds}s)",
            args.verbose,
        )

        # If artifact_dir is specified, serialize and write build_artifacts.json
        # unconditionally on both success and failure.
        if ctx.context_spec.artifact_dir:
            ts_msg(
                f"Serializing build artifacts manifest to {ctx.context_spec.artifact_dir}...",
                args.verbose,
            )
            failure_summary = run.failure_summary
            if run.exit_code != 0 and not failure_summary:
                if args.ninja_error_logging_output:
                    failure_summary = parse_ninja_failures(
                        args.ninja_error_logging_output
                    )

                if not failure_summary:
                    failure_summary = (
                        f"Fuchsia build failed: delegated command "
                        f"'{shlex.join(run.command)}' exited with status {run.exit_code}"
                    )

            ctx.produce_build_artifacts(
                duration_seconds,
                failure_summary=failure_summary,
                ninja_action_metrics_path=args.ninja_action_metrics_output,
                affected_tests=run.affected_tests,
                build_not_affected=run.build_not_affected,
            )

    return run.exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
