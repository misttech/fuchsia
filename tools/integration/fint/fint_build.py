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
    import google.protobuf.runtime_version as rv  # type: ignore

    rv.ValidateProtobufRuntimeVersion = lambda *args, **kwargs: None
except ImportError:
    pass

import argparse
import functools
import json
import platform
import subprocess
import tempfile
import time
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Generator, Iterable, TextIO

from build.scripts import signal_utils
from google.protobuf import json_format, text_format
from tools.integration.fint.proto import (
    build_artifacts_pb2,
    context_pb2,
    static_pb2,
)

JSONObject = dict[str, Any]
JSONArray = list[Any]


@dataclass
class BuildExecution:
    """Represents the parameters and result of a wrapped build command run."""

    command: list[str]
    exit_code: int = 0


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


def print_msg(msg: str, file: TextIO = sys.stdout) -> None:
    """Standardized logger that prefixes messages with the script name."""
    print(f"[{_SCRIPT_NAME}] {msg}", file=file)


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

    def matches_tool(self, tool: JSONObject) -> bool:
        """Returns True if the tool's OS and CPU match this host."""
        return tool.get("os") == self.os and tool.get("cpu") == self.cpu


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


def produce_build_artifacts(
    artifact_dir: pathlib.Path, duration_seconds: int
) -> None:
    """Serializes and writes the build_artifacts.json manifest to the artifact directory."""
    artifacts = build_artifacts_pb2.BuildArtifacts()
    artifacts.ninja_duration_seconds = duration_seconds

    json_manifest_path = artifact_dir / "build_artifacts.json"
    # MessageToJson formats with nice spacing/indentation
    json_data = json_format.MessageToJson(
        artifacts, always_print_fields_with_no_presence=True
    )
    json_manifest_path.write_text(json_data)
    print_msg(
        f"Successfully wrote build artifacts manifest to {json_manifest_path}"
    )


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
    print_msg("Running gn check...")
    if verbose:
        print_msg(f"Command: {shlex.join(cmd)}")
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

        print_msg("Verifying ninja build converges to no-op...")
        if verbose:
            print_msg(f"Command: {shlex.join(cmd)}")

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
        print_msg(
            "Error: Ninja build did not converge to no-op.",
            file=sys.stderr,
        )
        print(res.stdout[:1000], file=sys.stderr)

        # Print the dirty sources list if any were captured
        if dirty_sources_path.exists():
            dirty_content = dirty_sources_path.read_text().strip()
            if dirty_content:
                print_msg(
                    f"Identified dirty source files:\n{dirty_content}",
                    file=sys.stderr,
                )
        return 1


def collect_failure_diagnostics(build_dir: pathlib.Path) -> None:
    print_msg("Build failed. Diagnostic logs would be collected here.")


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

    @functools.cached_property
    def tool_paths(self) -> JSONArray:
        """Loads and returns the tool paths config list."""
        return load_json_list(self.build_dir / "tool_paths.json")

    @functools.cached_property
    def test_specs(self) -> JSONArray:
        """Loads and returns the test specs list."""
        path = self.build_dir / "test_specs.json"
        if not path.exists():
            return []
        return load_json_list(path)

    @functools.cached_property
    def generated_sources(self) -> JSONArray:
        """Loads and returns the generated sources list."""
        path = self.build_dir / "generated_sources.json"
        if not path.exists():
            return []
        return load_json_list(path)

    @functools.cached_property
    def prebuilt_binary_sets(self) -> JSONArray:
        """Loads and returns the prebuilt binary sets list."""
        path = self.build_dir / "prebuilt_binary_sets.json"
        if not path.exists():
            return []
        return load_json_list(path)

    def _default_and_host_test_targets(self) -> Iterable[str]:
        """Yields default targets or host test targets if configured."""
        if self.static_spec.include_default_ninja_target:
            yield ":default"
        elif self.static_spec.include_host_tests:
            for spec in self.test_specs:
                test_spec = spec.get("test", {})
                if test_spec.get("os") != "fuchsia":
                    path = test_spec.get("path")
                    if path:
                        yield path

    def _generated_source_targets(self) -> Iterable[str]:
        """Yields generated C++ source targets if configured."""
        if self.static_spec.include_generated_sources:
            for f in self.generated_sources:
                if f.endswith(".cc") or f.endswith(".h"):
                    yield f

    def _prebuilt_binary_manifests(self) -> Iterable[str]:
        """Yields prebuilt binary manifest targets if configured."""
        if self.static_spec.include_prebuilt_binary_manifests:
            for item in self.prebuilt_binary_sets:
                manifest = item.get("manifest")
                if manifest:
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

    def _stream_all_targets(self) -> Iterable[str]:
        """Streams all configured and resolved targets from all sources."""
        yield from self._default_and_host_test_targets()
        yield from self._generated_source_targets()
        yield from self._prebuilt_binary_manifests()
        yield from self._tool_targets()
        yield from self._custom_ninja_targets()

    def _get_targets(self) -> list[str]:
        """Resolves Ninja build targets based on specifications and build API JSON files."""
        return sorted(list(set(self._stream_all_targets())))

    def _build_bazel_host_tests(self) -> None:
        """Builds Bazel host tests if any are present in test_specs.json."""
        bazel_labels = []
        for spec in self.test_specs:
            test_spec = spec.get("test", {})
            label = test_spec.get("label", "")
            if label.startswith("@"):
                bazel_labels.append(label)

        if not bazel_labels:
            return

        top_dir_config_path = (
            self.checkout_dir / "build" / "bazel" / "config" / "bazel_top_dir"
        )
        bazel_top_dir = top_dir_config_path.read_text().strip()
        bazel_launcher = self.build_dir / bazel_top_dir / "bazel"

        cmd = [
            str(bazel_launcher),
            "build",
            "--config=host",
            "--build_runfile_links=true",
            "--enable_runfiles=true",
        ] + bazel_labels

        print_msg(f"Building Bazel host tests: {bazel_labels}")
        subprocess.run(cmd, check=True)

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

        rebuild_sentinel_path = self.build_dir / "force_nonhermetic_rebuild"
        success_stamp_path = self.build_dir / "last_ninja_build_success.stamp"

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
        self._build_bazel_host_tests()

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


def lookup_tool_path(
    tool_paths: list[JSONObject], tool_name: str, host: HostProperties
) -> str | None:
    """Looks up the relative path of a host tool in tool_paths."""
    for tool in tool_paths:
        if tool.get("name") == tool_name and host.matches_tool(tool):
            return tool.get("path")
    return None


def make_build_context(
    static_path: pathlib.Path,
    context_path: pathlib.Path | None,
    verbose: bool = False,
) -> BuildContext:
    """Creates a BuildContext by loading and parsing specifications, auto-detecting host properties."""
    static_spec = load_static_spec(static_path)

    if context_path:
        context_spec = load_context_spec(context_path)
    else:
        context_spec = context_pb2.Context(
            checkout_dir=str(fuchsia_root.resolve()),
            build_dir=str(pathlib.Path.cwd().resolve()),
        )

    host = HostProperties.detect()
    return BuildContext(static_spec, context_spec, host, verbose)


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
        "--print-artifact-dir",
        action="store_true",
        help="Print the resolved artifact directory from the context spec and exit.",
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
        "wrapped_cmd",
        nargs="*",
        default=None,
        help="The wrapped command to execute (optionally after '--')",
    )
    return parser


def main(argv: list[str]) -> int:
    # Verify that we are running in a valid Fuchsia checkout environment when executed.
    if not (fuchsia_root / ".jiri_manifest").exists():
        print_msg(
            f"INTERNAL ERROR: Could not find valid Fuchsia root: {fuchsia_root}",
            file=sys.stderr,
        )
        return 1

    parser = _main_arg_parser()
    args = parser.parse_args(argv)

    if args.print_artifact_dir:
        if not args.context:
            print_msg(
                "Error: --context is required with --print-artifact-dir",
                file=sys.stderr,
            )
            return 1
        context_spec = load_context_spec(args.context)
        if context_spec.artifact_dir:
            print(context_spec.artifact_dir)
        return 0

    # Ensure build execution requirements are satisfied if not querying
    is_static_valid = args.static and str(args.static) not in (".", "")
    required_build_args = [
        (is_static_valid, "the following arguments are required: --static"),
        (args.wrapped_cmd, "wrapped_cmd is required for build execution"),
    ]
    for val, err_msg in required_build_args:
        if not val:
            print_msg(f"Error: {err_msg}", file=sys.stderr)
            return 2

    ctx = make_build_context(args.static, args.context, verbose=args.verbose)

    # Select Build Strategy
    wrappers = {
        "ninja": ctx.wrap_ninja,
        "bazel": ctx.wrap_bazel,
    }
    wrapper = wrappers[args.mode]

    with wrapper(args.wrapped_cmd) as run:
        if args.verbose:
            print_msg(f"Delegated command: {shlex.join(run.command)}")

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
                print_msg(f"Build interrupted by signal {e.signum}")
        duration_seconds = round(t.duration)

        run.exit_code = exit_code

        if run.exit_code == 0:
            # If artifact_dir is specified, serialize and write build_artifacts.json
            if ctx.context_spec.artifact_dir:
                produce_build_artifacts(
                    pathlib.Path(ctx.context_spec.artifact_dir),
                    duration_seconds,
                )

    return run.exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
