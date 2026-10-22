#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fint_build.py."""

import pathlib
import sys

# Find the fuchsia root containing prebuilt/third_party/protobuf-py3.
# This search is necessary because in hermetic test sandboxes (like fx_build_test),
# the directory structure is flattened to 3 levels deep instead of the usual 4,
# so a fixed relative path lookup (parent.parent.parent.parent) would fail.
fint_dir = pathlib.Path(__file__).resolve().parent
fuchsia_root = fint_dir
for _ in range(5):
    if (fuchsia_root / "prebuilt" / "third_party" / "protobuf-py3").exists():
        break
    fuchsia_root = fuchsia_root.parent

# Append fuchsia_root to sys.path so we can import tools.integration.fint.proto
if fuchsia_root.exists() and str(fuchsia_root) not in sys.path:
    sys.path.insert(0, str(fuchsia_root))

protobuf_wheel = fuchsia_root / "prebuilt" / "third_party" / "protobuf-py3"
if protobuf_wheel.exists() and str(protobuf_wheel) not in sys.path:
    sys.path.insert(0, str(protobuf_wheel))

# Bypass strict protobuf gencode/runtime version check to align prebuilts at build-time (see b/537501139)
try:
    import google.protobuf.runtime_version as rv

    rv.ValidateProtobufRuntimeVersion = lambda *args, **kwargs: None
except ImportError:
    pass

import contextlib
import io
import json
import os
import subprocess
import tempfile
import time
import unittest
from typing import Any, Generator
from unittest import mock
from unittest.mock import MagicMock

import fint_build
from build.scripts import signal_utils
from google.protobuf import json_format, text_format
from tools.integration.fint.proto import (
    build_artifacts_pb2,
    context_pb2,
    static_pb2,
)


class SpecificationsParserTest(unittest.TestCase):
    """Tests the parsing of protobuf specifications."""

    def test_parse_specs(self) -> None:
        """Verifies that static_pb2 textproto parsing works correctly."""
        static_text = 'incremental: true\nninja_targets: "target1"\nninja_targets: "target2"\n'
        static_spec = static_pb2.Static()
        text_format.Merge(static_text, static_spec)
        self.assertTrue(static_spec.incremental)
        self.assertEqual(
            list(static_spec.ninja_targets), ["target1", "target2"]
        )


class LoadJsonDictTest(unittest.TestCase):
    """Tests load_json_dict helper."""

    def test_valid_dict(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            file_path = pathlib.Path(temp_dir) / "test.json"
            file_path.write_text('{"key": "value", "count": 42}')
            data = fint_build.load_json_dict(file_path)
            self.assertEqual(data, {"key": "value", "count": 42})

    def test_error_cases(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            cases = [
                ("list.json", "[1, 2, 3]", "Warning: Expected JSON object"),
                (
                    "str.json",
                    '"just a string"',
                    "Warning: Expected JSON object",
                ),
                (
                    "corrupt.json",
                    "{broken json",
                    "Warning: Failed to read JSON",
                ),
                ("missing.json", None, "Warning: Failed to read JSON"),
            ]
            for filename, content, expected_warn in cases:
                with self.subTest(filename=filename):
                    file_path = temp_path / filename
                    if content is not None:
                        file_path.write_text(content)
                    f = io.StringIO()
                    with contextlib.redirect_stderr(f):
                        data = fint_build.load_json_dict(file_path)
                    self.assertIsNone(data)
                    self.assertIn(expected_warn, f.getvalue())


class ParseNinjaActionMetricsTest(unittest.TestCase):
    """Tests parse_ninja_action_metrics helper."""

    def test_parse_ninja_action_metrics(self) -> None:
        cases: list[
            tuple[str, fint_build.JSONValue, int, int, dict[str, int]]
        ] = [
            (
                "valid_dict",
                {
                    "initial_actions": 100,
                    "final_actions": 50,
                    "action_counts": {"ACTION": 10, "CXX": 40},
                },
                100,
                50,
                {"ACTION": 10, "CXX": 40},
            ),
            ("empty_dict", {}, 0, 0, {}),
            (
                "malformed_numeric",
                {
                    "initial_actions": "not_an_int",
                    "final_actions": None,
                    "action_counts": {"ACTION": "bad", "CXX": 20},
                },
                0,
                0,
                {"CXX": 20},
            ),
            (
                "non_dict_action_counts",
                {"action_counts": [1, 2, 3]},
                0,
                0,
                {},
            ),
            ("non_dict_data", [1, 2, 3], 0, 0, {}),
        ]
        for name, data, exp_init, exp_final, exp_counts in cases:
            with self.subTest(name=name):
                with contextlib.redirect_stderr(io.StringIO()):
                    metrics = fint_build.parse_ninja_action_metrics(data)
                self.assertEqual(metrics.initial_actions, exp_init)
                self.assertEqual(metrics.final_actions, exp_final)
                self.assertEqual(dict(metrics.actions_by_type), exp_counts)


class BuildArtifactsTest(unittest.TestCase):
    """Tests the produce_build_artifacts method on BuildContext."""

    def test_produce_build_artifacts(self) -> None:
        """Verifies that produce_build_artifacts successfully writes build_artifacts.json."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            artifact_dir = temp_path / "artifact"
            build_dir.mkdir()
            artifact_dir.mkdir()

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            ctx.produce_build_artifacts(42)
            manifest_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
            self.assertTrue(manifest_path.exists())

            # Load and verify content
            manifest_content = json.loads(manifest_path.read_text())
            self.assertEqual(manifest_content.get("ninjaDurationSeconds"), 42)

    def test_produce_build_artifacts_with_failure(self) -> None:
        """Verifies that produce_build_artifacts successfully writes build_artifacts.json with a failure_summary."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            artifact_dir = temp_path / "artifact"
            build_dir.mkdir()
            artifact_dir.mkdir()

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            ctx.produce_build_artifacts(42, failure_summary="test failure")
            manifest_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
            self.assertTrue(manifest_path.exists())

            # Load and verify content
            manifest_content = json.loads(manifest_path.read_text())
            self.assertEqual(manifest_content.get("ninjaDurationSeconds"), 42)
            self.assertEqual(
                manifest_content.get("failureSummary"), "test failure"
            )

    def test_produce_build_artifacts_with_debug_symbols(self) -> None:
        """Verifies that produce_build_artifacts copies debug_symbols.json and populates log_files."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            artifact_dir = temp_path / "artifact"
            build_dir.mkdir()
            artifact_dir.mkdir()

            # Create fake debug_symbols.json inside build_dir/debug_symbols
            debug_symbols_dir = build_dir / "debug_symbols"
            debug_symbols_dir.mkdir()
            manifest_src = debug_symbols_dir / "debug_symbols.json"
            manifest_src.write_text("[]")

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            ctx.produce_build_artifacts(42)

            manifest_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
            self.assertTrue(manifest_path.exists())

            # Verify debug_symbols.json was copied
            symbols_dest = artifact_dir / "debug_symbols.json"
            self.assertTrue(symbols_dest.exists())
            self.assertEqual(symbols_dest.read_text(), "[]")

            # Load and verify content
            manifest_content = json.loads(manifest_path.read_text())
            self.assertEqual(manifest_content.get("ninjaDurationSeconds"), 42)
            self.assertIn(
                "debug_symbols.json", manifest_content.get("logFiles", {})
            )
            self.assertEqual(
                manifest_content.get("logFiles", {}).get("debug_symbols.json"),
                str(symbols_dest),
            )

    @mock.patch.object(subprocess, "run")
    def test_produce_build_artifacts_with_ninja_traces(
        self, mock_run: mock.Mock
    ) -> None:
        """Verifies that produce_build_artifacts generates and copies ninja traces."""
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = ""
        mock_run.return_value.stderr = ""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            checkout_dir = temp_path / "checkout"
            build_dir = checkout_dir / "out" / "default"
            artifact_dir = temp_path / "artifact"
            checkout_dir.mkdir()
            build_dir.mkdir(parents=True)
            artifact_dir.mkdir()

            # Create fake raw ninja_build_trace.json.gz
            raw_trace = build_dir / fint_build.NINJA_BUILD_TRACE_GZ
            raw_trace.write_text("raw-trace-content")

            # Create fake tool paths file to provide ninjatrace_prebuilt and buildstats_prebuilt
            # using relative paths starting with ../.. from build_dir
            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text(
                json.dumps(
                    [
                        {
                            "name": "ninjatrace_prebuilt",
                            "path": "../../prebuilt/tools/ninjatrace/linux-x64/ninjatrace",
                            "os": "linux",
                            "cpu": "x64",
                        },
                        {
                            "name": "buildstats_prebuilt",
                            "path": "../../prebuilt/tools/buildstats/linux-x64/buildstats",
                            "os": "linux",
                            "cpu": "x64",
                        },
                    ]
                )
            )

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                checkout_dir=str(checkout_dir),
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            executed_cmds: list[list[str]] = []

            # We must stub subprocess.run to touch the files they generate, since they are mocked
            def side_effect(
                cmd: list[str],
            ) -> subprocess.CompletedProcess[bytes]:
                executed_cmds.append(cmd)
                if "ninjatrace" in cmd[0]:
                    (build_dir / fint_build.NINJATRACE_JSON_GZ).write_text(
                        "processed-trace"
                    )
                elif "buildstats" in cmd[0]:
                    (build_dir / fint_build.BUILDSTATS_JSON_GZ).write_text(
                        "processed-stats"
                    )
                return subprocess.CompletedProcess(cmd, 0)

            mock_run.side_effect = side_effect

            ctx.produce_build_artifacts(42)

            # Verify that ninjatrace and buildstats tools were resolved relative to build_dir
            expected_ninjatrace_tool = str(
                build_dir
                / "../../prebuilt/tools/ninjatrace/linux-x64/ninjatrace"
            )
            expected_buildstats_tool = str(
                build_dir
                / "../../prebuilt/tools/buildstats/linux-x64/buildstats"
            )
            self.assertEqual(executed_cmds[0][0], expected_ninjatrace_tool)
            self.assertEqual(executed_cmds[1][0], expected_buildstats_tool)

            # Verify that ninjatrace.json.gz was copied to artifact_dir
            trace_dest = artifact_dir / fint_build.NINJATRACE_JSON_GZ
            self.assertTrue(trace_dest.exists())
            self.assertEqual(trace_dest.read_text(), "processed-trace")

            # Verify that buildstats.json.gz was copied to artifact_dir
            stats_dest = artifact_dir / fint_build.BUILDSTATS_JSON_GZ
            self.assertTrue(stats_dest.exists())
            self.assertEqual(stats_dest.read_text(), "processed-stats")

            # Verify that ninjatrace_json_files list is populated in the manifest
            manifest_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
            manifest_content = json.loads(manifest_path.read_text())
            self.assertEqual(
                manifest_content.get("ninjatraceJsonFiles"),
                [str(trace_dest)],
            )

            # Verify that buildstats_json_files list is populated in the manifest
            self.assertEqual(
                manifest_content.get("buildstatsJsonFiles"),
                [str(stats_dest)],
            )

    @mock.patch.object(subprocess, "run")
    def test_produce_build_artifacts_with_subbuild_ninja_traces(
        self, mock_run: mock.Mock
    ) -> None:
        """Verifies that subbuild traces are merged with ninjatrace tool resolved relative to build_dir."""
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = ""
        mock_run.return_value.stderr = ""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            checkout_dir = temp_path / "checkout"
            build_dir = checkout_dir / "out" / "default"
            artifact_dir = temp_path / "artifact"
            checkout_dir.mkdir()
            build_dir.mkdir(parents=True)
            artifact_dir.mkdir()

            # Create raw trace, ninja_subbuilds.json, and the ninjatrace2json.py script
            raw_trace = build_dir / fint_build.NINJA_BUILD_TRACE_GZ
            raw_trace.write_text("raw-trace-content")

            subbuilds_json = build_dir / "ninja_subbuilds.json"
            subbuilds_json.write_text("[]")

            ninjatrace2json_script = (
                checkout_dir / fint_build.NINJATRACE2JSON_PY_RELATIVE_PATH
            )
            ninjatrace2json_script.parent.mkdir(parents=True, exist_ok=True)
            ninjatrace2json_script.write_text("#!/usr/bin/env python3\n")

            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text(
                json.dumps(
                    [
                        {
                            "name": "ninjatrace_prebuilt",
                            "path": "../../prebuilt/tools/ninjatrace/linux-x64/ninjatrace",
                            "os": "linux",
                            "cpu": "x64",
                        },
                    ]
                )
            )

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                checkout_dir=str(checkout_dir),
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            executed_cmds: list[list[str]] = []

            def side_effect(
                cmd: list[str],
            ) -> subprocess.CompletedProcess[bytes]:
                executed_cmds.append(cmd)
                if "ninjatrace" in cmd[0]:
                    (build_dir / fint_build.NINJATRACE_JSON_GZ).write_text(
                        "processed-trace"
                    )
                return subprocess.CompletedProcess(cmd, 0)

            mock_run.side_effect = side_effect

            ctx.produce_build_artifacts(42)

            # Step 1: raw ninjatrace prebuilt
            expected_ninjatrace_tool = str(
                build_dir
                / "../../prebuilt/tools/ninjatrace/linux-x64/ninjatrace"
            )
            self.assertEqual(executed_cmds[0][0], expected_ninjatrace_tool)

            # Step 3: ninjatrace2json.py merging subbuild traces
            self.assertEqual(len(executed_cmds), 2)
            merge_cmd = executed_cmds[1]
            self.assertIn("--ninjatrace-path", merge_cmd)
            idx = merge_cmd.index("--ninjatrace-path")
            self.assertEqual(merge_cmd[idx + 1], expected_ninjatrace_tool)

    def test_produce_build_artifacts_with_ninja_traces_missing_tool_paths(
        self,
    ) -> None:
        """Verifies that a warning is logged when raw trace exists but tool_paths is missing."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            artifact_dir = temp_path / "artifact"
            build_dir.mkdir()
            artifact_dir.mkdir()

            # Create raw trace but NO tool_paths.json
            raw_trace = build_dir / fint_build.NINJA_BUILD_TRACE_GZ
            raw_trace.write_text("raw-trace-content")

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            # Execution should succeed, but print warning using contextlib redirect_stderr
            f = io.StringIO()
            with contextlib.redirect_stderr(f):
                ctx.produce_build_artifacts(42)

            self.assertIn(
                "Warning: Skipping Ninja trace analysis: tool_paths.json is missing or empty.",
                f.getvalue(),
            )

    def test_produce_build_artifacts_with_ninja_traces_missing_ninjatrace_tool(
        self,
    ) -> None:
        """Verifies that a warning is logged when raw trace exists but ninjatrace tool is missing."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            artifact_dir = temp_path / "artifact"
            build_dir.mkdir()
            artifact_dir.mkdir()

            # Create raw trace
            raw_trace = build_dir / fint_build.NINJA_BUILD_TRACE_GZ
            raw_trace.write_text("raw-trace-content")

            # Create a non-empty tool_paths.json list missing ninjatrace_prebuilt
            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text(
                json.dumps(
                    [
                        {
                            "name": "some_other_tool",
                            "path": "prebuilt/some_other_tool",
                            "os": "linux",
                            "cpu": "x64",
                        }
                    ]
                )
            )

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                artifact_dir=str(artifact_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec=static_spec,
                context_spec=context_spec,
                host=host,
                verbose=False,
            )

            # Execution should succeed, but print warning using contextlib redirect_stderr
            f = io.StringIO()
            with contextlib.redirect_stderr(f):
                ctx.produce_build_artifacts(42)

            self.assertIn(
                "Warning: Skipping Ninja trace analysis: ninjatrace_prebuilt tool path not found in tool paths.",
                f.getvalue(),
            )

    def test_resolve_targets_generated_sources_warning_missing(self) -> None:
        """Verifies a warning is logged when include_generated_sources is enabled but generated_sources.json is missing."""
        static_spec = static_pb2.Static(include_generated_sources=True)
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)

        f = io.StringIO()
        with contextlib.redirect_stderr(f):
            targets = list(ctx._generated_source_targets())

        self.assertEqual(targets, [])
        self.assertIn(
            "Warning: include_generated_sources is enabled, but generated_sources.json is missing on disk.",
            f.getvalue(),
        )

    def test_resolve_targets_prebuilt_binaries_warning_missing(self) -> None:
        """Verifies a warning is logged when include_prebuilt_binary_manifests is enabled but prebuilt_binaries.json is missing."""
        static_spec = static_pb2.Static(include_prebuilt_binary_manifests=True)
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)

        f = io.StringIO()
        with contextlib.redirect_stderr(f):
            targets = list(ctx._prebuilt_binary_manifests())

        self.assertEqual(targets, [])
        self.assertIn(
            f"Warning: include_prebuilt_binary_manifests is enabled, but {fint_build.PREBUILT_BINARY_SETS_JSON} is missing on disk.",
            f.getvalue(),
        )

    def test_produce_build_artifacts_action_metrics(self) -> None:
        """Verifies that produce_build_artifacts populates ninjaActionMetrics from custom path or default build_dir."""
        cases = [
            ("custom_path", True),
            ("default_path", False),
        ]
        for name, use_custom_path in cases:
            with self.subTest(name=name):
                with tempfile.TemporaryDirectory() as temp_dir:
                    temp_path = pathlib.Path(temp_dir)
                    build_dir = temp_path / "build"
                    artifact_dir = temp_path / "artifact"
                    build_dir.mkdir()
                    artifact_dir.mkdir()

                    if use_custom_path:
                        metrics_dir = temp_path / "ninja_logs"
                        metrics_dir.mkdir()
                        metrics_file = (
                            metrics_dir / fint_build.NINJA_ACTION_METRICS_JSON
                        )
                        explicit_arg = metrics_file
                    else:
                        metrics_file = (
                            build_dir / fint_build.NINJA_ACTION_METRICS_JSON
                        )
                        explicit_arg = None

                    metrics_file.write_text(
                        json.dumps(
                            {
                                "initial_actions": 120,
                                "final_actions": 60,
                                "action_counts": {
                                    "ACTION": 10,
                                    "CXX": 45,
                                    "LINK": 5,
                                },
                            }
                        )
                    )

                    ctx = fint_build.BuildContext(
                        static_spec=static_pb2.Static(),
                        context_spec=context_pb2.Context(
                            build_dir=str(build_dir),
                            artifact_dir=str(artifact_dir),
                        ),
                        host=fint_build.HostProperties(os="linux", cpu="x64"),
                        verbose=False,
                    )
                    ctx.produce_build_artifacts(
                        42, ninja_action_metrics_path=explicit_arg
                    )

                    manifest_path = (
                        artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
                    )
                    self.assertTrue(manifest_path.exists())

                    manifest_content = json.loads(manifest_path.read_text())
                    metrics = manifest_content.get("ninjaActionMetrics", {})
                    self.assertEqual(metrics.get("initialActions"), 120)
                    self.assertEqual(metrics.get("finalActions"), 60)
                    self.assertEqual(
                        metrics.get("actionsByType"),
                        {"ACTION": 10, "CXX": 45, "LINK": 5},
                    )


class ResolveAffectedTestNamesTest(unittest.TestCase):
    """Tests the resolve_affected_test_names function."""

    def test_resolve_affected_test_names(self) -> None:
        specs = [
            fint_build.TestSpec(
                label="//src/foo:foo_test(//build/toolchain/fuchsia:x64)",
                os="fuchsia",
                cpu="x64",
                path="",
                name="fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
                package_label="//src/foo:foo_pkg(//build/toolchain/fuchsia:x64)",
            ),
            fint_build.TestSpec(
                label="@@//src/bar:bar_test",
                os="linux",
                cpu="x64",
                path="host_x64/bar_test",
                name="host_x64/bar_test",
                source_label="//src/bar:bar_test",
            ),
            fint_build.TestSpec(
                label="//src/recovery/simulator:recovery_simulator_boot_test(//build/toolchain/fuchsia:x64)",
                os="fuchsia",
                cpu="x64",
                path="",
                name="recovery_simulator_boot_test",
            ),
        ]
        cases = [
            (
                "exact label",
                ["//src/foo:foo_test(//build/toolchain/fuchsia:x64)"],
                ["fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"],
            ),
            (
                "package label without toolchain",
                ["//src/foo:foo_pkg"],
                ["fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"],
            ),
            (
                "bazel source label",
                ["//src/bar:bar_test"],
                ["host_x64/bar_test"],
            ),
            (
                "test name",
                ["host_x64/bar_test"],
                ["host_x64/bar_test"],
            ),
            (
                "never affected",
                ["//src/recovery/simulator:recovery_simulator_boot_test"],
                [],
            ),
            (
                "unknown label passes through",
                ["//src/unknown:test"],
                ["//src/unknown:test"],
            ),
            (
                "duplicates removed",
                ["//src/foo:foo_test", "//src/foo:foo_pkg"],
                ["fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"],
            ),
        ]
        for name, labels, expected in cases:
            with self.subTest(name=name):
                self.assertEqual(
                    fint_build.resolve_affected_test_names(specs, labels),
                    expected,
                )


class FindAffectedTestsTest(unittest.TestCase):
    """Tests the _find_affected_tests method on BuildContext."""

    def _make_ctx(
        self,
        temp_path: pathlib.Path,
        changed_files: list[str],
    ) -> fint_build.BuildContext:
        build_dir = temp_path / "build"
        artifact_dir = temp_path / "artifact"
        checkout_dir = temp_path / "checkout"
        for d in (build_dir, artifact_dir, checkout_dir):
            d.mkdir()
        (build_dir / fint_build.TESTS_JSON).write_text(
            json.dumps(
                [
                    {
                        "test": {
                            "label": "//src/foo:foo_test(//build/toolchain/fuchsia:x64)",
                            "name": "foo_test",
                            "os": "fuchsia",
                            "cpu": "x64",
                        }
                    }
                ]
            )
        )
        context_spec = context_pb2.Context(
            build_dir=str(build_dir),
            artifact_dir=str(artifact_dir),
            checkout_dir=str(checkout_dir),
            changed_files=[
                context_pb2.Context.ChangedFile(path=f) for f in changed_files
            ],
        )
        return fint_build.BuildContext(
            static_pb2.Static(),
            context_spec,
            fint_build.HostProperties(os="linux", cpu="x64"),
        )

    @mock.patch.object(subprocess, "run")
    def test_success(self, mock_run: MagicMock) -> None:
        files_list_contents: list[str] = []

        def fake_run(cmd: list[str], **kwargs: Any) -> MagicMock:
            files_list = next(
                a for a in cmd if a.startswith("--files-list=")
            ).removeprefix("--files-list=")
            files_list_contents.append(pathlib.Path(files_list).read_text())
            return MagicMock(
                returncode=0,
                stdout=json.dumps(
                    {
                        "test_targets": [
                            {"label": "//src/foo:foo_test", "env": "device"},
                        ],
                        "build_not_affected": False,
                    }
                ),
            )

        mock_run.side_effect = fake_run
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            ctx = self._make_ctx(
                temp_path, ["src/foo/foo.cc", "src/foo/BUILD.gn"]
            )
            result = fint_build.BuildExecution(command=["ninja"])
            ctx._find_affected_tests(result)

            self.assertEqual(result.affected_tests, ["foo_test"])
            self.assertFalse(result.build_not_affected)
            self.assertEqual(
                files_list_contents, ["src/foo/foo.cc\nsrc/foo/BUILD.gn\n"]
            )
            cmd = mock_run.call_args.args[0]
            self.assertEqual(
                cmd[:4],
                [
                    str(temp_path / "checkout" / "build" / "api" / "client"),
                    "--build-dir",
                    str(temp_path / "build"),
                    "affected_tests",
                ],
            )
            self.assertEqual(cmd[-1], "--format=json")
            # The temporary changed files list should be cleaned up.
            self.assertEqual(list((temp_path / "artifact").iterdir()), [])

    @mock.patch.object(subprocess, "run")
    def test_build_not_affected(self, mock_run: MagicMock) -> None:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps({"test_targets": [], "build_not_affected": True}),
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            ctx = self._make_ctx(pathlib.Path(temp_dir), ["README.md"])
            result = fint_build.BuildExecution(command=["ninja"])
            ctx._find_affected_tests(result)
            self.assertEqual(result.affected_tests, [])
            self.assertTrue(result.build_not_affected)

    @mock.patch.object(subprocess, "run")
    def test_no_changed_files(self, mock_run: MagicMock) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            ctx = self._make_ctx(pathlib.Path(temp_dir), [])
            result = fint_build.BuildExecution(command=["ninja"])
            ctx._find_affected_tests(result)
            mock_run.assert_not_called()
            self.assertEqual(result.affected_tests, [])
            self.assertFalse(result.build_not_affected)

    @mock.patch.object(subprocess, "run")
    def test_tool_failure(self, mock_run: MagicMock) -> None:
        mock_run.return_value = MagicMock(returncode=1, stdout="")
        with tempfile.TemporaryDirectory() as temp_dir:
            ctx = self._make_ctx(pathlib.Path(temp_dir), ["src/foo/foo.cc"])
            result = fint_build.BuildExecution(command=["ninja"])
            with self.assertRaises(RuntimeError):
                ctx._find_affected_tests(result)

    @mock.patch.object(fint_build, "check_ninja_noop", return_value=(0, None))
    @mock.patch.object(fint_build, "run_gn_check", return_value=(0, None))
    def test_failure_sets_failure_summary(
        self, mock_gn_check: MagicMock, mock_noop: MagicMock
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            ctx = self._make_ctx(temp_path, ["src/foo/foo.cc"])
            result = fint_build.BuildExecution(command=["ninja"])
            with (
                mock.patch.object(
                    fint_build.BuildContext, "_export_debug_symbols"
                ),
                mock.patch.object(
                    fint_build.BuildContext,
                    "_find_affected_tests",
                    side_effect=RuntimeError("boom"),
                ),
            ):
                ctx._run_post_ninja_checks(
                    result, ["default"], temp_path / "build" / "stamp"
                )
            self.assertEqual(result.exit_code, 1)
            self.assertEqual(
                result.failure_summary, "Failed to find affected tests: boom"
            )

    @mock.patch.object(fint_build, "check_ninja_noop", return_value=(0, None))
    @mock.patch.object(fint_build, "run_gn_check", return_value=(0, None))
    def test_export_debug_symbols_failure_sets_failure_summary(
        self, mock_gn_check: MagicMock, mock_noop: MagicMock
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            ctx = self._make_ctx(temp_path, ["src/foo/foo.cc"])
            result = fint_build.BuildExecution(command=["ninja"])
            with mock.patch.object(
                fint_build.BuildContext,
                "_export_debug_symbols",
                side_effect=RuntimeError(
                    "export_last_build_debug_symbols failed with exit code 1"
                ),
            ):
                ctx._run_post_ninja_checks(
                    result, ["default"], temp_path / "build" / "stamp"
                )
            self.assertEqual(result.exit_code, 1)
            self.assertEqual(
                result.failure_summary,
                "Failed to export debug symbols: export_last_build_debug_symbols failed with exit code 1",
            )

    def test_produce_build_artifacts_includes_affected_tests(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            ctx = self._make_ctx(temp_path, [])
            ctx.produce_build_artifacts(
                42,
                affected_tests=["foo_test", "bar_test"],
                build_not_affected=True,
            )
            manifest = json.loads(
                (
                    temp_path / "artifact" / fint_build.BUILD_ARTIFACTS_JSON
                ).read_text()
            )
            self.assertEqual(
                manifest["affectedTests"], ["foo_test", "bar_test"]
            )
            self.assertTrue(manifest["buildNotAffected"])


class ExportDebugSymbolsTest(unittest.TestCase):
    """Tests the _export_debug_symbols method on BuildContext."""

    def test_should_export_breakpad_symbols(self) -> None:
        """Verifies that should_export_breakpad_symbols handles whitespace and formatting robustly."""
        test_cases = [
            (["output_breakpad_syms=true"], True),
            (["output_breakpad_syms = true"], True),
            (["output_breakpad_syms=True"], True),
            (["  output_breakpad_syms  =  true  "], True),
            (["output_breakpad_syms=false"], False),
            (["output_breakpad_syms = false"], False),
            (["other_arg=true"], False),
            ([], False),
        ]
        for gn_args, expected in test_cases:
            with self.subTest(gn_args=gn_args):
                static_spec = static_pb2.Static(gn_args=gn_args)
                context_spec = context_pb2.Context()
                host = fint_build.HostProperties(os="linux", cpu="x64")
                ctx = fint_build.BuildContext(static_spec, context_spec, host)
                self.assertEqual(ctx.should_export_breakpad_symbols, expected)

    @mock.patch.object(subprocess, "run")
    def test_export_debug_symbols_success(self, mock_run: mock.Mock) -> None:
        """Verifies successful invocation of export_last_build_debug_symbols command."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            checkout_dir = temp_path / "checkout"
            build_dir.mkdir()
            checkout_dir.mkdir()

            # Test using the spaces-formatted GN argument used in production infra
            static_spec = static_pb2.Static(
                gn_args=["output_breakpad_syms = true"]
            )
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                checkout_dir=str(checkout_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec, context_spec, host, verbose=False
            )

            # Mock subprocess run to simulate successful execution
            mock_proc = mock.Mock()
            mock_proc.returncode = 0
            mock_run.return_value = mock_proc

            ctx._export_debug_symbols(targets=["default"])

            mock_run.assert_called_once_with(
                [
                    str(checkout_dir / "build" / "api" / "client"),
                    "--build-dir",
                    str(build_dir),
                    "export_last_build_debug_symbols",
                    f"--output-dir={build_dir / 'debug_symbols'}",
                    "--with-breakpad-symbols",
                ]
            )
            self.assertTrue((build_dir / "debug_symbols").is_dir())

    @mock.patch.object(subprocess, "run")
    def test_export_debug_symbols_empty_targets(
        self, mock_run: mock.Mock
    ) -> None:
        """Verifies _export_debug_symbols writes empty manifests without subprocess when targets is empty."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            build_dir = temp_path / "build"
            checkout_dir = temp_path / "checkout"
            build_dir.mkdir()
            checkout_dir.mkdir()

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(
                build_dir=str(build_dir),
                checkout_dir=str(checkout_dir),
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(
                static_spec, context_spec, host, verbose=False
            )

            ctx._export_debug_symbols(targets=[])

            mock_run.assert_not_called()
            debug_dir = build_dir / "debug_symbols"
            self.assertTrue(debug_dir.is_dir())
            self.assertEqual((debug_dir / "build-ids.json").read_text(), "{}\n")
            self.assertEqual((debug_dir / "build-ids.txt").read_text(), "")
            self.assertEqual(
                (debug_dir / "debug_symbols.json").read_text(), "[]\n"
            )


class ParseNinjaFailuresTest(unittest.TestCase):
    """Tests the parse_ninja_failures, format_ninja_failures, and NinjaFailure classes."""

    def test_ninja_failure_dataclass(self) -> None:
        """Verifies individual NinjaFailure methods and properties behave correctly."""
        # Test creation from valid dict
        failure = fint_build.NinjaFailure.from_dict(
            {
                "artifacts": ["out/default/foo.o"],
                "exit_code": 1,
                "output": "some error\n",
            }
        )
        self.assertIsNotNone(failure)
        self.assertEqual(failure.artifacts, ["out/default/foo.o"])
        self.assertEqual(failure.exit_code, 1)
        self.assertEqual(failure.output, "some error")
        self.assertFalse(failure.is_eligible_for_deduplication)
        self.assertEqual(
            failure.format(), "FAILED: [code=1] out/default/foo.o\n\nsome error"
        )
        self.assertEqual(
            failure.format(include_output=False),
            "FAILED: [code=1] out/default/foo.o",
        )

        # Test line-count based eligibility
        long_failure = fint_build.NinjaFailure.from_dict(
            {
                "artifacts": ["bin/bar"],
                "exit_code": 2,
                "output": "1\n2\n3\n4\n5\n6",
            }
        )
        self.assertTrue(long_failure.is_eligible_for_deduplication)

    def test_format_valid_failures(self) -> None:
        """Verifies format_ninja_failures correctly formats valid failures."""
        mock_data: fint_build.JSONObject = {
            "version": 1,
            "failures": [
                {
                    "artifacts": ["bin/foo"],
                    "exit_code": 1,
                    "output": "compiler error",
                }
            ],
        }
        res = fint_build.format_ninja_failures(mock_data)
        self.assertEqual(res, "FAILED: [code=1] bin/foo\n\ncompiler error")

    def test_format_deduplication(self) -> None:
        """Verifies format_ninja_failures deduplicates long compiler errors, but preserves headers."""
        mock_data: fint_build.JSONObject = {
            "version": 1,
            "failures": [
                {
                    "artifacts": ["bin/foo"],
                    "exit_code": 1,
                    "output": "line1\nline2\nline3\nline4\nline5\nline6",
                },
                {
                    "artifacts": ["bin/bar"],
                    "exit_code": 1,
                    "output": "line1\nline2\nline3\nline4\nline5\nline6",
                },
            ],
        }
        res = fint_build.format_ninja_failures(mock_data)
        expected = (
            "FAILED: [code=1] bin/foo\n\n"
            "line1\nline2\nline3\nline4\nline5\nline6\n\n"
            "FAILED: [code=1] bin/bar"
        )
        self.assertEqual(res, expected)

    def test_format_invalid_version_or_malformed(self) -> None:
        """Verifies format_ninja_failures returns None for malformed JSON or unsupported versions."""
        # Invalid version
        self.assertIsNone(
            fint_build.format_ninja_failures({"version": 2, "failures": []})
        )
        # Missing failures
        self.assertIsNone(fint_build.format_ninja_failures({"version": 1}))

    def test_format_elides_collateral_failures(self) -> None:
        """Verifies output-less failures are omitted if a Bazel action failed with output."""
        stamp = ".fuchsia_sdk_arm64.bazel_target_can_only_be_referenced_by_one_bazel_action"
        mock_data: fint_build.JSONObject = {
            "version": 1,
            "failures": [
                {
                    "artifacts": [f"obj/foo{stamp}", "obj/foo.far"],
                    "exit_code": 1,
                    "output": "",
                },
                {
                    "artifacts": [f"obj/bar{stamp}", "obj/bar.far"],
                    "exit_code": 1,
                    "output": "ERROR: bar failed",
                },
                {
                    "artifacts": ["obj/baz.stamp"],
                    "exit_code": 1,
                    "output": "",
                },
                {
                    "artifacts": ["obj/qux.o"],
                    "exit_code": 1,
                    "output": "qux.cc: error",
                },
            ],
        }
        res = fint_build.format_ninja_failures(mock_data)
        expected = (
            f"FAILED: [code=1] obj/bar{stamp} obj/bar.far\n\n"
            "ERROR: bar failed\n\n"
            "FAILED: [code=1] obj/qux.o\n\n"
            "qux.cc: error"
        )
        self.assertEqual(res, expected)

    def test_format_keeps_output_less_failures_without_bazel_output(
        self,
    ) -> None:
        """Verifies output-less failures are kept if no Bazel action failed with output."""
        stamp = ".fuchsia_sdk_arm64.bazel_target_can_only_be_referenced_by_one_bazel_action"
        mock_data: fint_build.JSONObject = {
            "version": 1,
            "failures": [
                {
                    "artifacts": [f"obj/foo{stamp}"],
                    "exit_code": 1,
                    "output": "",
                },
                {
                    "artifacts": ["obj/qux.o"],
                    "exit_code": 1,
                    "output": "qux.cc: error",
                },
            ],
        }
        res = fint_build.format_ninja_failures(mock_data)
        expected = (
            f"FAILED: [code=1] obj/foo{stamp}\n\n"
            "FAILED: [code=1] obj/qux.o\n\n"
            "qux.cc: error"
        )
        self.assertEqual(res, expected)

    def test_format_condenses_bazel_output(self) -> None:
        """Verifies format_ninja_failures strips Bazel progress noise."""
        bazel_output = "\n".join(
            [
                "Starting local Bazel server (8.5.1) and connecting to it...",
                "... still trying to connect to local Bazel server (298708) after 10 seconds ...",
                "INFO: Invocation ID: 83f12aa8-a9bf-4d83-b876-22dcd83493d1",
                "INFO: Streaming build results to: http://go/fxbtx/83f12aa8",
                "Computing main repo mapping: ",
                "Loading: 5 packages loaded",
                "Analyzing: 4 targets (12 packages loaded, 8 targets configured)",
                "",
                "",
                "[3 / 3] no actions running",
                "INFO: Analyzed 4 targets (1207 packages loaded, 28611 targets configured).",
                "[1,222 / 4,436] Compiling foo.cc; 4s remote ... (318 actions, 229 running)",
                "ERROR: /out/BUILD.bazel:53:11: Compiling foo.cc failed: (Exit 1): clang failed: error executing CppCompile command (from target //foo:foo)",
                "  (cd /out/execroot/_main && \\",
                "  exec env - \\",
                "    PATH=/bin:/usr/bin:/usr/local/bin \\",
                "  external/clang/bin/clang -c foo.cc -o foo.o)",
                "# Configuration: e55a32e603a87d351114726c7559d6a5c72b7566a4fbd41dbd1e0c01e14999c9",
                "# Execution platform: //build/bazel/platforms:linux_x64",
                "foo.cc:1:2: error: something is wrong",
                "1 error generated.",
                "INFO: Elapsed time: 34.007s, Critical Path: 23.95s",
                "INFO: 8251 processes: 13403 action cache hit, 1 remote.",
                "ERROR: Build did NOT complete successfully",
            ]
        )
        mock_data: fint_build.JSONObject = {
            "version": 1,
            "failures": [
                {
                    "artifacts": ["obj/foo.stamp"],
                    "exit_code": 1,
                    "output": bazel_output,
                }
            ],
        }
        res = fint_build.format_ninja_failures(mock_data)
        expected = "\n".join(
            [
                "FAILED: [code=1] obj/foo.stamp",
                "",
                "ERROR: /out/BUILD.bazel:53:11: Compiling foo.cc failed: (Exit 1): clang failed: error executing CppCompile command (from target //foo:foo)",
                "  (command elided)",
                "# Execution platform: //build/bazel/platforms:linux_x64",
                "foo.cc:1:2: error: something is wrong",
                "1 error generated.",
                "ERROR: Build did NOT complete successfully",
            ]
        )
        self.assertEqual(res, expected)

    def test_condense_bazel_output_leaves_non_bazel_output_alone(self) -> None:
        """Verifies output that doesn't come from Bazel isn't modified."""
        output = "Loading: foo\n\n\n[1/2] something\nerror: bad"
        self.assertEqual(fint_build.condense_bazel_output(output), output)

    def test_condense_bazel_output_without_error(self) -> None:
        """Verifies non-noise lines are kept if there is no error."""
        output = "\n".join(
            [
                "INFO: Invocation ID: abc",
                "Loading: 5 packages loaded",
                "WARNING: something is off",
                "some tool failed",
            ]
        )
        self.assertEqual(
            fint_build.condense_bazel_output(output),
            "WARNING: something is off\nsome tool failed",
        )

    def test_condense_bazel_output_multiple_commands(self) -> None:
        """Verifies errors following each elided command are preserved."""
        output = "\n".join(
            [
                "INFO: Invocation ID: abc",
                "ERROR: first failed",
                "  (cd /out/execroot/_main && \\",
                "  exec env - \\",
                "  clang -c a.cc)",
                "a.cc:1:2: error: first error",
                "INFO: Invocation ID: def",
                "ERROR: second failed",
                "  (cd /out/execroot/_main && \\",
                "  clang -c b.cc)",
                "# Configuration: 1234",
                "b.cc:1:2: error: second error",
            ]
        )
        expected = "\n".join(
            [
                "ERROR: first failed",
                "  (command elided)",
                "a.cc:1:2: error: first error",
                "ERROR: second failed",
                "  (command elided)",
                "b.cc:1:2: error: second error",
            ]
        )
        self.assertEqual(fint_build.condense_bazel_output(output), expected)

    def test_condense_bazel_output_crlf(self) -> None:
        """Verifies CRLF line endings are handled."""
        output = (
            "INFO: Invocation ID: abc\r\nLoading: 5 packages\r\nERROR: bad\r\n"
        )
        self.assertEqual(
            fint_build.condense_bazel_output(output), "ERROR: bad\n"
        )

    def test_condense_bazel_output_unterminated_command(self) -> None:
        """Verifies output is left alone if it ends partway through a command."""
        output = "\n".join(
            [
                "INFO: Invocation ID: abc",
                "ERROR: foo failed",
                "  (cd /out/execroot/_main && \\",
                "  exec env - \\",
            ]
        )
        self.assertEqual(fint_build.condense_bazel_output(output), output)

    def test_parse_file_handling(self) -> None:
        """Verifies parse_ninja_failures correctly loads files and delegates."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            errors_json_path = (
                pathlib.Path(tmp_dir) / fint_build.NINJA_ERRORS_JSON
            )

            # Malformed JSON
            errors_json_path.write_text("{invalid")
            self.assertIsNone(fint_build.parse_ninja_failures(errors_json_path))

            # Non-existent file
            self.assertIsNone(
                fint_build.parse_ninja_failures(
                    pathlib.Path("/non/existent/file")
                )
            )

            # Valid file
            mock_data = {
                "version": 1,
                "failures": [
                    {
                        "artifacts": ["bin/foo"],
                        "exit_code": 1,
                        "output": "error",
                    }
                ],
            }
            errors_json_path.write_text(json.dumps(mock_data))
            res = fint_build.parse_ninja_failures(errors_json_path)
            self.assertEqual(res, "FAILED: [code=1] bin/foo\n\nerror")


class RunGnCheckTest(unittest.TestCase):
    """Tests the run_gn_check helper function."""

    @mock.patch.object(subprocess, "run")
    def test_run_gn_check_success(self, mock_run: MagicMock) -> None:
        """Verifies run_gn_check returns (0, None) on success."""
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = "Header dependency check OK\n"
        host = fint_build.HostProperties(os="linux", cpu="x64")
        status, summary = fint_build.run_gn_check(
            pathlib.Path("fake_checkout"), pathlib.Path("fake_build"), host
        )
        self.assertEqual(status, 0)
        self.assertIsNone(summary)
        mock_run.assert_called_once()

    @mock.patch.object(subprocess, "run")
    def test_run_gn_check_failure(self, mock_run: MagicMock) -> None:
        """Verifies run_gn_check returns non-zero and output summary on failure."""
        mock_run.return_value.returncode = 12
        mock_run.return_value.stdout = (
            "ERROR at //foo/bar.cc:10:11: Include not allowed.\n"
        )
        host = fint_build.HostProperties(os="linux", cpu="x64")
        status, summary = fint_build.run_gn_check(
            pathlib.Path("fake_checkout"), pathlib.Path("fake_build"), host
        )
        self.assertEqual(status, 12)
        self.assertEqual(
            summary, "ERROR at //foo/bar.cc:10:11: Include not allowed."
        )


class CheckNinjaNoopTest(unittest.TestCase):
    """Tests the check_ninja_noop helper function."""

    @mock.patch.object(subprocess, "run")
    def test_check_ninja_noop_converges(self, mock_run: MagicMock) -> None:
        """Verifies check_ninja_noop returns (0, None) if ninja converges."""
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = (
            "ninja: Entering directory ...\nninja: no work to do."
        )
        mock_run.return_value.stderr = ""
        host = fint_build.HostProperties(os="linux", cpu="x64")
        status, summary = fint_build.check_ninja_noop(
            pathlib.Path("fake_checkout"),
            pathlib.Path("fake_build"),
            host,
            [],
        )
        self.assertEqual(status, 0)
        self.assertIsNone(summary)

    @mock.patch.object(subprocess, "run")
    def test_check_ninja_noop_diverges(self, mock_run: MagicMock) -> None:
        """Verifies check_ninja_noop returns non-zero and a failure summary if ninja diverges."""

        def mock_run_impl(
            cmd: list[str], *args: Any, **kwargs: Any
        ) -> MagicMock:
            # Locate `--dirty_sources_list` and write simulated dirty files into its path
            for i, arg in enumerate(cmd):
                if arg == "--dirty_sources_list" and i + 1 < len(cmd):
                    path = cmd[i + 1]
                    pathlib.Path(path).write_text("sdk/lib/fdio/private.h\n")
            return MagicMock(
                returncode=0,
                stdout="ninja: Entering directory ...\nbuilding target foo.o",
                stderr="ninja explain: output foo.o older than most recent input foo.cc\nninja explain: foo.o is dirty\n",
            )

        mock_run.side_effect = mock_run_impl
        host = fint_build.HostProperties(os="linux", cpu="x64")
        status, summary = fint_build.check_ninja_noop(
            pathlib.Path("fake_checkout"),
            pathlib.Path("fake_build"),
            host,
            [],
        )
        self.assertEqual(status, 1)
        self.assertEqual(
            summary,
            "\n".join(
                [
                    "Ninja build did not converge to no-op.",
                    "See: https://fuchsia.dev/fuchsia-src/development/build/ninja_no_op",
                    "",
                    "First line of ninja explain output:",
                    "",
                    "output foo.o older than most recent input foo.cc",
                ]
            ),
        )

    @mock.patch.object(subprocess, "run")
    def test_check_ninja_noop_subprocess_fails(
        self, mock_run: MagicMock
    ) -> None:
        """Verifies check_ninja_noop returns ninja's exit code and summary if subprocess fails."""
        mock_run.return_value.returncode = 2
        mock_run.return_value.stdout = "ninja: error: ..."
        mock_run.return_value.stderr = ""
        host = fint_build.HostProperties(os="linux", cpu="x64")
        status, summary = fint_build.check_ninja_noop(
            pathlib.Path("fake_checkout"),
            pathlib.Path("fake_build"),
            host,
            [],
        )
        self.assertEqual(status, 2)
        self.assertEqual(
            summary, "ninja dry-run check failed:\nninja: error: ..."
        )


class HostPropertiesTest(unittest.TestCase):
    """Tests the HostProperties domain model detection, properties, and matching."""

    def test_host_properties_detection_and_matching(self) -> None:
        """Verifies HostProperties attributes, properties, and matches_tool method."""
        host = fint_build.HostProperties(os="linux", cpu="x64")
        self.assertEqual(host.os, "linux")
        self.assertEqual(host.cpu, "x64")
        self.assertTrue(host.is_linux)
        self.assertFalse(host.is_mac)
        self.assertEqual(host.platform_dir, "linux-x64")
        self.assertEqual(
            host.gn_relative_path,
            pathlib.Path("prebuilt/third_party/gn/linux-x64/gn"),
        )
        self.assertEqual(
            host.ninja_relative_path,
            pathlib.Path("prebuilt/third_party/ninja/linux-x64/ninja"),
        )

        # Test matches_tool
        tool_matching = fint_build.ToolPathSpec(
            name="tool", os="linux", cpu="x64", path="path/to/tool"
        )
        tool_mismatch_os = fint_build.ToolPathSpec(
            name="tool", os="mac", cpu="x64", path="path/to/tool"
        )
        tool_mismatch_cpu = fint_build.ToolPathSpec(
            name="tool", os="linux", cpu="arm64", path="path/to/tool"
        )

        self.assertTrue(host.matches_tool(tool_matching))
        self.assertFalse(host.matches_tool(tool_mismatch_os))
        self.assertFalse(host.matches_tool(tool_mismatch_cpu))


class MainArgParserTest(unittest.TestCase):
    """Tests the command-line argument parser construction and parsing validation."""

    def test_main_arg_parser(self) -> None:
        """Verifies that the command-line argument parser works as expected."""
        parser = fint_build._main_arg_parser()
        args = parser.parse_args(
            [
                "--static",
                "/path/to/static",
                "--context",
                "/path/to/context",
                "--ninja-action-metrics-output",
                "/path/to/metrics.json",
                "--",
                "ninja",
                "target",
            ]
        )
        self.assertEqual(args.static, pathlib.Path("/path/to/static"))
        self.assertEqual(args.context, pathlib.Path("/path/to/context"))
        self.assertEqual(
            args.ninja_action_metrics_output,
            pathlib.Path("/path/to/metrics.json"),
        )
        self.assertEqual(args.wrapped_cmd, ["ninja", "target"])


class NinjaBuildWrapTest(unittest.TestCase):
    """Tests target resolution, lifecycles, and bazel builds inside wrap_ninja and _get_targets."""

    def test_resolve_targets_default(self) -> None:
        """Verifies target resolution returns :default when requested."""
        static_spec = static_pb2.Static(include_default_ninja_target=True)
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )

        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        targets = ctx._get_targets()
        self.assertEqual(targets, [":default"])

    def test_resolve_targets_custom_ninja(self) -> None:
        """Verifies custom targets are correctly appended and resolved."""
        static_spec = static_pb2.Static(ninja_targets=["foo", "bar"])
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )

        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        targets = ctx._get_targets()
        self.assertEqual(targets, ["bar", "foo"])

    def test_resolve_targets_prebuilt_binaries(self) -> None:
        """Verifies prebuilt_binaries.json targets are loaded and resolved cleanly."""
        static_spec = static_pb2.Static(include_prebuilt_binary_manifests=True)
        with tempfile.TemporaryDirectory() as tmp_dir:
            (pathlib.Path(tmp_dir) / "prebuilt_binaries.json").write_text(
                json.dumps(
                    [
                        {
                            "name": "packages",
                            "manifest": "prebuilt_binaries.manifest",
                        }
                    ]
                )
            )
            context_spec = context_pb2.Context(
                checkout_dir="fake_checkout", build_dir=tmp_dir
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)
            self.assertEqual(ctx._get_targets(), ["prebuilt_binaries.manifest"])

    def test_wrap_ninja_empty_targets_skips_ninja(self) -> None:
        """Verifies wrap_ninja runs no-op python command when targets is empty and default ninja target is disabled."""
        static_spec = static_pb2.Static(include_default_ninja_target=False)
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        self.assertTrue(ctx.ninja_skipped)
        with ctx.wrap_ninja(["ninja", "-C", "out/default"]) as run:
            self.assertEqual(run.command, [sys.executable, "-c", ""])
            run.exit_code = 1  # prevent post-build checks in mock dir

    def test_wrap_ninja_with_targets(self) -> None:
        """Verifies wrap_ninja appends resolved targets to base command."""
        static_spec = static_pb2.Static(include_default_ninja_target=True)
        context_spec = context_pb2.Context(
            checkout_dir="fake_checkout", build_dir="fake_dir"
        )
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        self.assertFalse(ctx.ninja_skipped)
        with ctx.wrap_ninja(["ninja", "-C", "out/default"]) as run:
            self.assertEqual(
                run.command, ["ninja", "-C", "out/default", ":default"]
            )
            run.exit_code = 1  # prevent post-build checks in mock dir

    def test_clippy_target_spec_from_dict_pure(self) -> None:
        """Verifies ClippyTargetSpec.from_dict safely parses dictionaries."""
        valid_dict: fint_build.JSONObject = {
            "clippy_output": "gen/src/foo.clippy",
            "src": ["src/foo.rs", "src/bar.rs"],
            "disable_clippy": False,
        }
        spec = fint_build.ClippyTargetSpec.from_dict(valid_dict)
        self.assertIsNotNone(spec)
        assert spec is not None
        self.assertEqual(spec.output, pathlib.Path("gen/src/foo.clippy"))
        self.assertEqual(
            spec.sources,
            [pathlib.Path("src/foo.rs"), pathlib.Path("src/bar.rs")],
        )
        self.assertFalse(spec.disable_clippy)

        # Non-dict and incorrect type structures raise ValueError strictly
        with self.assertRaises(ValueError):
            fint_build.ClippyTargetSpec.from_dict({})
        invalid_dict: fint_build.JSONObject = {"clippy_output": 123}
        with self.assertRaises(ValueError):
            fint_build.ClippyTargetSpec.from_dict(invalid_dict)

    def test_resolve_targets_clippy_all(self) -> None:
        """Verifies ALL_LINT_TARGETS includes all enabled clippy targets."""
        static_spec = static_pb2.Static(
            include_lint_targets=static_pb2.Static.ALL_LINT_TARGETS
        )
        with tempfile.TemporaryDirectory() as tmp_dir:
            (pathlib.Path(tmp_dir) / "rust_target_mapping.json").write_text(
                json.dumps(
                    [
                        {
                            "clippy_output": "gen/src/foo.clippy",
                            "src": ["src/foo.rs"],
                            "disable_clippy": False,
                        },
                        {
                            "clippy_output": "gen/src/bar.clippy",
                            "src": ["src/bar.rs"],
                            "disable_clippy": True,
                        },
                    ]
                )
            )
            context_spec = context_pb2.Context(
                checkout_dir="fake_checkout", build_dir=tmp_dir
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)
            self.assertEqual(ctx._get_targets(), ["gen/src/foo.clippy"])

    def test_resolve_targets_clippy_affected(self) -> None:
        """Verifies AFFECTED_LINT_TARGETS yields only modified clippy targets."""
        static_spec = static_pb2.Static(
            include_lint_targets=static_pb2.Static.AFFECTED_LINT_TARGETS
        )
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            checkout_dir = tmp_path / "checkout"
            build_dir = checkout_dir / "out" / "default"
            build_dir.mkdir(parents=True)

            (build_dir / "rust_target_mapping.json").write_text(
                json.dumps(
                    [
                        {
                            "clippy_output": "gen/src/foo.clippy",
                            "src": ["../../src/foo.rs"],
                            "disable_clippy": False,
                        },
                        {
                            "clippy_output": "gen/src/bar.clippy",
                            "src": ["../../src/bar.rs"],
                            "disable_clippy": False,
                        },
                    ]
                )
            )
            context_spec = context_pb2.Context(
                checkout_dir=str(checkout_dir),
                build_dir=str(build_dir),
                changed_files=[
                    context_pb2.Context.ChangedFile(path="src/foo.rs")
                ],
            )
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)
            self.assertEqual(ctx._get_targets(), ["gen/src/foo.clippy"])

    def test_test_spec_from_dict_pure(self) -> None:
        """Verifies TestSpec.from_dict safely parses dictionaries."""
        valid_dict: fint_build.JSONObject = {
            "test": {
                "label": "@//src/foo:foo_test",
                "os": "linux",
                "cpu": "x64",
                "path": "host_test_1",
            }
        }
        spec = fint_build.TestSpec.from_dict(valid_dict)
        self.assertIsNotNone(spec)
        assert spec is not None
        self.assertEqual(spec.label, "@//src/foo:foo_test")
        self.assertEqual(spec.os, "linux")
        self.assertEqual(spec.cpu, "x64")
        self.assertEqual(spec.path, "host_test_1")
        self.assertEqual(spec.name, "")

        bazel_spec = fint_build.TestSpec.from_dict(
            {
                "test": {
                    "label": "@@//src/foo:foo_test",
                    "name": "host_x64/foo_test",
                    "package_label": "//src/foo:pkg",
                    "source_label": "//src/foo:foo_test",
                }
            }
        )
        self.assertEqual(bazel_spec.name, "host_x64/foo_test")
        self.assertEqual(bazel_spec.package_label, "//src/foo:pkg")
        self.assertEqual(bazel_spec.source_label, "//src/foo:foo_test")

        # Non-dict and missing key structures raise ValueError strictly
        with self.assertRaises(ValueError):
            fint_build.TestSpec.from_dict({})
        # Create invalid dict with typed values to keep Mypy happy
        invalid_dict: fint_build.JSONObject = {"test": "not-a-dict"}
        with self.assertRaises(ValueError):
            fint_build.TestSpec.from_dict(invalid_dict)

    def test_tool_path_spec_from_dict_pure(self) -> None:
        """Verifies ToolPathSpec.from_dict safely parses dictionaries."""
        valid_dict: fint_build.JSONObject = {
            "name": "gn",
            "path": "prebuilt/third_party/gn/linux-x64/gn",
            "os": "linux",
            "cpu": "x64",
        }
        tool = fint_build.ToolPathSpec.from_dict(valid_dict)
        self.assertIsNotNone(tool)
        assert tool is not None
        self.assertEqual(tool.name, "gn")
        self.assertEqual(tool.path, "prebuilt/third_party/gn/linux-x64/gn")
        self.assertEqual(tool.os, "linux")
        self.assertEqual(tool.cpu, "x64")

        # Missing and incorrect type structures raise ValueError strictly
        with self.assertRaises(ValueError):
            fint_build.ToolPathSpec.from_dict({})
        invalid_dict: fint_build.JSONObject = {"name": 123}
        with self.assertRaises(ValueError):
            fint_build.ToolPathSpec.from_dict(invalid_dict)

    @mock.patch.object(subprocess, "run")
    def test_lifecycle_context_manager(self, mock_run: MagicMock) -> None:
        """Verifies wrap_ninja touches files and manages success stamp correctly."""
        # Mock subprocess run to simulate successful execution of GN check and Ninja no-op check
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = "ninja: no work to do."
        mock_run.return_value.stderr = ""

        with tempfile.TemporaryDirectory() as tmp_dir:
            static_spec = static_pb2.Static(incremental=True)
            context_spec = context_pb2.Context(
                checkout_dir="fake_checkout", build_dir=tmp_dir
            )

            tmp_path = pathlib.Path(tmp_dir)
            stamp_path = tmp_path / fint_build.LAST_NINJA_BUILD_SUCCESS_STAMP
            stamp_path.write_text("old-content")

            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)
            with mock.patch.object(
                fint_build.BuildContext, "_export_debug_symbols"
            ) as mock_export:
                with ctx.wrap_ninja(["ninja"]) as run:
                    # Entering should remove the success stamp
                    self.assertFalse(stamp_path.exists())

                    # Rebuild sentinel should be touched on incremental builds
                    sentinel = (
                        tmp_path / fint_build.FORCE_NONHERMETIC_REBUILD_SENTINEL
                    )
                    self.assertTrue(sentinel.exists())

                    run.exit_code = 0

                # Exiting with success should write a new success stamp
                self.assertTrue(stamp_path.exists())
                mock_export.assert_called_once()


class BuildContextToolPathsTest(unittest.TestCase):
    """Tests BuildContext tool path resolution and cached properties."""

    def test_resolve_tool_path_success(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            build_dir = pathlib.Path(tmp_dir) / "out" / "default"
            build_dir.mkdir(parents=True)

            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text(
                json.dumps(
                    [
                        {
                            "name": "ninjatrace_prebuilt",
                            "path": "../../prebuilt/tools/ninjatrace",
                            "os": "linux",
                            "cpu": "x64",
                        },
                        {
                            "name": "buildstats_prebuilt",
                            "path": "../../prebuilt/tools/buildstats",
                            "os": "linux",
                            "cpu": "x64",
                        },
                    ]
                )
            )

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(build_dir=str(build_dir))
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)

            expected_ninjatrace = build_dir / "../../prebuilt/tools/ninjatrace"
            expected_buildstats = build_dir / "../../prebuilt/tools/buildstats"

            self.assertEqual(
                ctx.resolve_tool_path("ninjatrace_prebuilt"),
                expected_ninjatrace,
            )
            self.assertEqual(
                ctx.resolve_tool_path("buildstats_prebuilt"),
                expected_buildstats,
            )
            self.assertEqual(ctx.ninjatrace_tool_path, expected_ninjatrace)
            self.assertEqual(ctx.buildstats_tool_path, expected_buildstats)
            self.assertIsNone(ctx.resolve_tool_path("nonexistent_tool"))

    def test_resolve_tool_path_missing_or_invalid_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            build_dir = pathlib.Path(tmp_dir) / "out" / "default"
            build_dir.mkdir(parents=True)

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(build_dir=str(build_dir))
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)

            # When tool_paths.json does not exist
            self.assertIsNone(ctx.resolve_tool_path("ninjatrace_prebuilt"))
            self.assertIsNone(ctx.ninjatrace_tool_path)
            self.assertIsNone(ctx.buildstats_tool_path)

            # When tool_paths.json is invalid JSON
            ctx_invalid = fint_build.BuildContext(
                static_spec, context_spec, host
            )
            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text("invalid json")
            self.assertIsNone(
                ctx_invalid.resolve_tool_path("ninjatrace_prebuilt")
            )
            self.assertIsNone(ctx_invalid.ninjatrace_tool_path)

    def test_cached_properties(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            build_dir = pathlib.Path(tmp_dir) / "out" / "default"
            build_dir.mkdir(parents=True)

            tool_paths_file = build_dir / fint_build.TOOL_PATHS_JSON
            tool_paths_file.write_text(
                json.dumps(
                    [
                        {
                            "name": "ninjatrace_prebuilt",
                            "path": "../../prebuilt/tools/ninjatrace",
                            "os": "linux",
                            "cpu": "x64",
                        },
                    ]
                )
            )

            static_spec = static_pb2.Static()
            context_spec = context_pb2.Context(build_dir=str(build_dir))
            host = fint_build.HostProperties(os="linux", cpu="x64")
            ctx = fint_build.BuildContext(static_spec, context_spec, host)

            first_trace = ctx.ninjatrace_tool_path
            self.assertIsNotNone(first_trace)

            # Delete the file from disk; cached property should still return the cached Path
            tool_paths_file.unlink()
            second_trace = ctx.ninjatrace_tool_path
            self.assertEqual(first_trace, second_trace)


class MainExecutionTest(unittest.TestCase):
    """Integration tests for the wrapper main() execution flow."""

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_writes_build_artifacts_json(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that main writes build_artifacts.json when artifact_dir is present."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as static_file:
                static_file.write("incremental: true")
                static_path = static_file.name

            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as context_file:
                context_file.write(
                    f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"'
                )
                context_path = context_file.name

            try:
                # Mock successful build command execution
                mock_managed.return_value.run.return_value = 0
                mock_run.return_value.returncode = 0
                mock_run.return_value.stdout = "ninja: no work to do."
                mock_run.return_value.stderr = ""

                real_argv = [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                    "--",
                    "ninja",
                    "target",
                ]

                # Run main() passing arguments directly
                exit_code = fint_build.main(real_argv)
                self.assertEqual(exit_code, 0)

                # Verify build_artifacts.json was written to the artifact directory
                manifest_path = os.path.join(
                    artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
                )
                self.assertTrue(os.path.exists(manifest_path))

                # Load and verify content
                with open(manifest_path, "r") as f:
                    manifest_content = json.loads(f.read())
                self.assertIn("ninjaDurationSeconds", manifest_content)

            finally:
                os.unlink(static_path)
                os.unlink(context_path)

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_writes_build_artifacts_json_with_ninja_error_logging_output(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that main writes build_artifacts.json with a parsed failure summary when ninja_error_logging_output is present."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as static_file:
                static_file.write("incremental: true")
                static_path = static_file.name

            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as context_file:
                context_file.write(
                    f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"'
                )
                context_path = context_file.name

            errors_json_path = os.path.join(
                build_dir, fint_build.NINJA_ERRORS_JSON
            )
            mock_errors_data = {
                "version": 1,
                "failures": [
                    {
                        "artifacts": ["out/default/foo.o"],
                        "exit_code": 1,
                        "output": "FAILED: foo.o\nsome compiler error output\nline 2",
                    }
                ],
            }
            with open(errors_json_path, "w") as f:
                json.dump(mock_errors_data, f)

            try:
                # Mock failed build command execution
                mock_managed.return_value.run.return_value = 1
                mock_run.return_value.returncode = 0
                mock_run.return_value.stdout = "ninja: build failed."
                mock_run.return_value.stderr = ""

                real_argv = [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                    "--ninja-error-logging-output",
                    errors_json_path,
                    "--",
                    "ninja",
                    "target",
                ]

                # Run main() passing arguments directly
                exit_code = fint_build.main(real_argv)
                self.assertEqual(exit_code, 1)

                # Verify build_artifacts.json was written to the artifact directory
                manifest_path = os.path.join(
                    artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
                )
                self.assertTrue(os.path.exists(manifest_path))

                # Load and verify content
                with open(manifest_path, "r") as f:
                    manifest_content = json.loads(f.read())
                self.assertIn("ninjaDurationSeconds", manifest_content)
                self.assertIn(
                    "FAILED: [code=1] out/default/foo.o",
                    manifest_content["failureSummary"],
                )
                self.assertIn(
                    "some compiler error output",
                    manifest_content["failureSummary"],
                )

            finally:
                os.unlink(static_path)
                os.unlink(context_path)

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_propagates_ninja_noop_failure_summary(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that a post-build ninja no-op failure propagates its summary to build_artifacts.json."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            static_path = os.path.join(build_dir, "static.textproto")
            pathlib.Path(static_path).write_text(
                "incremental: true\ninclude_default_ninja_target: true\n"
            )
            context_path = os.path.join(build_dir, "context.textproto")
            pathlib.Path(context_path).write_text(
                f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"'
            )

            def mock_run_impl(
                cmd: list[str], *args: Any, **kwargs: Any
            ) -> MagicMock:
                if "check" in cmd:
                    return MagicMock(returncode=0, stdout="", stderr="")
                return MagicMock(
                    returncode=0,
                    stdout="ninja: Entering directory ...\n[1/1] CXX foo.o",
                    stderr="ninja explain: restat of output foo.o older than input foo.cc\n",
                )

            mock_managed.return_value.run.return_value = 0
            mock_run.side_effect = mock_run_impl

            exit_code = fint_build.main(
                [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                    "--",
                    "ninja",
                    "target",
                ]
            )
            self.assertEqual(exit_code, 1)

            manifest_path = os.path.join(
                artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
            )
            with open(manifest_path, "r") as f:
                manifest_content = json.loads(f.read())
            self.assertEqual(
                manifest_content["failureSummary"],
                "\n".join(
                    [
                        "Ninja build did not converge to no-op.",
                        "See: https://fuchsia.dev/fuchsia-src/development/build/ninja_no_op",
                        "",
                        "First line of ninja explain output:",
                        "",
                        "restat of output foo.o older than input foo.cc",
                    ]
                ),
            )

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_propagates_gn_check_failure_summary(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that a post-build gn check failure propagates its output to build_artifacts.json."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            static_path = os.path.join(build_dir, "static.textproto")
            pathlib.Path(static_path).write_text("incremental: true")
            context_path = os.path.join(build_dir, "context.textproto")
            pathlib.Path(context_path).write_text(
                f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"'
            )

            mock_managed.return_value.run.return_value = 0
            mock_run.return_value = MagicMock(
                returncode=1,
                stdout="ERROR at //src/foo/bar.cc:5:10: Include not allowed.\n",
                stderr="",
            )

            exit_code = fint_build.main(
                [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                    "--",
                    "ninja",
                    "target",
                ]
            )
            self.assertEqual(exit_code, 1)

            manifest_path = os.path.join(
                artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
            )
            with open(manifest_path, "r") as f:
                manifest_content = json.loads(f.read())
            self.assertEqual(
                manifest_content["failureSummary"],
                "ERROR at //src/foo/bar.cc:5:10: Include not allowed.",
            )

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_propagates_affected_tests(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that post-build affected tests are written to build_artifacts.json."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            static_path = os.path.join(build_dir, "static.textproto")
            pathlib.Path(static_path).write_text("incremental: true")
            context_path = os.path.join(build_dir, "context.textproto")
            pathlib.Path(context_path).write_text(
                f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"\nchanged_files {{ path: "src/foo.cc" }}'
            )
            tests_json_path = pathlib.Path(build_dir) / fint_build.TESTS_JSON
            tests_json_path.write_text(
                json.dumps(
                    [
                        {
                            "test": {
                                "name": "foo_test",
                                "label": "//src:foo_test(//toolchain)",
                                "os": "linux",
                            }
                        },
                        {
                            "test": {
                                "name": "bar_test",
                                "label": "//src:bar_test(//toolchain)",
                                "os": "linux",
                            }
                        },
                    ]
                )
            )

            def mock_run_impl(
                cmd: list[str], *args: Any, **kwargs: Any
            ) -> MagicMock:
                if "affected_tests" in cmd:
                    return MagicMock(
                        returncode=0,
                        stdout=json.dumps(
                            {
                                "test_targets": [
                                    {"label": "//src:foo_test"},
                                    {"label": "//src:bar_test"},
                                ],
                                "build_not_affected": True,
                            }
                        ),
                        stderr="",
                    )
                return MagicMock(
                    returncode=0,
                    stdout="ninja: no work to do.",
                    stderr="",
                )

            mock_managed.return_value.run.return_value = 0
            mock_run.side_effect = mock_run_impl

            exit_code = fint_build.main(
                [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                    "--",
                    "ninja",
                    "target",
                ]
            )
            self.assertEqual(exit_code, 0)

            manifest_path = os.path.join(
                artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
            )
            with open(manifest_path, "r") as f:
                manifest_content = json.loads(f.read())
            self.assertEqual(
                manifest_content["affectedTests"],
                ["foo_test", "bar_test"],
            )
            self.assertTrue(manifest_content["buildNotAffected"])

    def _run_main_with_action_metrics(
        self,
        mock_managed: MagicMock,
        mock_run: MagicMock,
        extra_argv: list[str],
        metrics_data: dict[str, Any],
    ) -> dict[str, Any]:
        """Helper to run main() with mocked action metrics and return parsed build_artifacts.json."""
        with tempfile.TemporaryDirectory() as artifact_dir, tempfile.TemporaryDirectory() as build_dir:
            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as static_file:
                static_file.write("incremental: true")
                static_path = static_file.name

            with tempfile.NamedTemporaryFile(
                mode="w", suffix=".textproto", delete=False
            ) as context_file:
                context_file.write(
                    f'checkout_dir: "fake_checkout"\nartifact_dir: "{artifact_dir}"\nbuild_dir: "{build_dir}"'
                )
                context_path = context_file.name

            metrics_json_path = os.path.join(
                build_dir, fint_build.NINJA_ACTION_METRICS_JSON
            )
            with open(metrics_json_path, "w") as f:
                json.dump(metrics_data, f)

            resolved_argv = [
                arg.replace("{metrics_path}", metrics_json_path)
                for arg in extra_argv
            ]

            try:
                mock_managed.return_value.run.return_value = 0
                mock_run.return_value.returncode = 0
                mock_run.return_value.stdout = "ninja: no work to do."
                mock_run.return_value.stderr = ""

                real_argv = [
                    "--static",
                    static_path,
                    "--context",
                    context_path,
                ] + resolved_argv

                exit_code = fint_build.main(real_argv)
                self.assertEqual(exit_code, 0)

                manifest_path = os.path.join(
                    artifact_dir, fint_build.BUILD_ARTIFACTS_JSON
                )
                self.assertTrue(os.path.exists(manifest_path))

                with open(manifest_path, "r") as f:
                    return json.loads(f.read())
            finally:
                os.unlink(static_path)
                os.unlink(context_path)

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_main_writes_build_artifacts_json_with_ninja_action_metrics_output(
        self, mock_managed: MagicMock, mock_run: MagicMock
    ) -> None:
        """Verifies that main writes build_artifacts.json with ninjaActionMetrics when --ninja-action-metrics-output is passed."""
        manifest = self._run_main_with_action_metrics(
            mock_managed,
            mock_run,
            [
                "--ninja-action-metrics-output",
                "{metrics_path}",
                "--",
                "ninja",
                "target",
            ],
            {
                "initial_actions": 50,
                "final_actions": 25,
                "action_counts": {"ACTION": 5, "CXX": 20},
            },
        )
        metrics = manifest.get("ninjaActionMetrics", {})
        self.assertEqual(metrics.get("initialActions"), 50)
        self.assertEqual(metrics.get("finalActions"), 25)
        self.assertEqual(
            metrics.get("actionsByType"),
            {"ACTION": 5, "CXX": 20},
        )

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(signal_utils, "SignalManagedProcess")
    @mock.patch.object(fint_build, "load_json_list")
    @mock.patch.object(pathlib.Path, "cwd")
    def test_main_omitted_context(
        self,
        mock_cwd: MagicMock,
        mock_load_json_list: MagicMock,
        mock_managed: MagicMock,
        mock_run: MagicMock,
    ) -> None:
        """Verifies that omitting --context automatically generates context_spec."""
        mock_load_json_list.return_value = []
        with tempfile.TemporaryDirectory() as tmp_dir, tempfile.TemporaryDirectory() as fake_root_dir:
            mock_cwd.return_value = pathlib.Path(tmp_dir)
            fake_root = pathlib.Path(fake_root_dir)
            (fake_root / ".jiri_manifest").touch()
            with mock.patch.object(fint_build, "fuchsia_root", fake_root):
                with tempfile.NamedTemporaryFile(
                    mode="w", suffix=".textproto", delete=False
                ) as static_file:
                    static_file.write("")
                    static_path = static_file.name

                try:
                    # Mock successful build command execution
                    mock_managed.return_value.run.return_value = 0
                    mock_run.return_value.returncode = 0
                    mock_run.return_value.stdout = "ninja: no work to do."
                    mock_run.return_value.stderr = ""

                    real_argv = [
                        "--static",
                        static_path,
                        "--",
                        "ninja",
                        "-j",
                        "64",
                        "target",
                    ]

                    # Run main() passing arguments directly without --context
                    exit_code = fint_build.main(real_argv)
                    self.assertEqual(exit_code, 0)

                finally:
                    os.unlink(static_path)

    @mock.patch.object(signal_utils, "SignalManagedProcess")
    @mock.patch.object(fint_build.BuildContext, "wrap_bazel")
    @mock.patch.object(fint_build.BuildContext, "wrap_ninja")
    @mock.patch.object(fint_build, "make_build_context")
    def test_main_selects_wrapper_ninja(
        self,
        mock_make_ctx: MagicMock,
        mock_wrap_ninja: MagicMock,
        mock_wrap_bazel: MagicMock,
        mock_managed: MagicMock,
    ) -> None:
        """Verifies that main() selects wrap_ninja in default or ninja mode."""
        # Setup mock context to be a real BuildContext instance
        static_spec = static_pb2.Static()
        context_spec = context_pb2.Context()
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        mock_make_ctx.return_value = ctx

        # Mock successful subprocess execution
        mock_managed.return_value.run.return_value = 0

        @contextlib.contextmanager
        def fake_wrap_ninja(
            wrapped_cmd: list[str],
        ) -> Generator[fint_build.BuildExecution, None, None]:
            yield fint_build.BuildExecution(command=["ninja"])

        mock_wrap_ninja.side_effect = fake_wrap_ninja

        fint_build.main(["--static", "fake_static", "--", "ninja"])
        mock_wrap_ninja.assert_called_once()
        mock_wrap_bazel.assert_not_called()

    @mock.patch.object(signal_utils, "SignalManagedProcess")
    @mock.patch.object(fint_build.BuildContext, "wrap_bazel")
    @mock.patch.object(fint_build.BuildContext, "wrap_ninja")
    @mock.patch.object(fint_build, "make_build_context")
    def test_main_selects_wrapper_bazel(
        self,
        mock_make_ctx: MagicMock,
        mock_wrap_ninja: MagicMock,
        mock_wrap_bazel: MagicMock,
        mock_managed: MagicMock,
    ) -> None:
        """Verifies that main() selects wrap_bazel in bazel mode."""
        # Setup mock context to be a real BuildContext instance
        static_spec = static_pb2.Static()
        context_spec = context_pb2.Context()
        host = fint_build.HostProperties(os="linux", cpu="x64")
        ctx = fint_build.BuildContext(static_spec, context_spec, host)
        mock_make_ctx.return_value = ctx

        # Mock successful subprocess execution
        mock_managed.return_value.run.return_value = 0

        @contextlib.contextmanager
        def fake_wrap_bazel(
            wrapped_cmd: list[str],
        ) -> Generator[fint_build.BuildExecution, None, None]:
            yield fint_build.BuildExecution(command=["bazel"])

        mock_wrap_bazel.side_effect = fake_wrap_bazel

        fint_build.main(
            ["--static", "fake_static", "--mode", "bazel", "--", "bazel"]
        )
        mock_wrap_bazel.assert_called_once()
        mock_wrap_ninja.assert_not_called()


class FintBuildMainTest(unittest.TestCase):
    """Tests for the main() function entrypoint of fint_build.py."""

    def test_main_print_artifact_dir_success(self) -> None:
        """Verifies that --print-artifact-dir outputs the correct directory."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            context_file = tmp_path / "context.textproto"
            context_file.write_text('artifact_dir: "/mock/artifact/dir"\n')

            # Redirect stdout to capture the printed path
            f_stdout = io.StringIO()
            with contextlib.redirect_stdout(f_stdout):
                exit_code = fint_build.main(
                    ["--context", str(context_file), "--print-artifact-dir"]
                )

            self.assertEqual(exit_code, 0)
            self.assertEqual(f_stdout.getvalue().strip(), "/mock/artifact/dir")

    def test_main_print_artifact_dir_missing_context(self) -> None:
        """Verifies that using --print-artifact-dir without --context returns an error."""
        f_stderr = io.StringIO()
        with contextlib.redirect_stderr(f_stderr):
            exit_code = fint_build.main(["--print-artifact-dir"])

        self.assertEqual(exit_code, 1)
        self.assertIn(
            "Error: --context is required with --print-artifact-dir",
            f_stderr.getvalue(),
        )

    def test_main_print_job_count_success(self) -> None:
        """Verifies that --print-job-count outputs the correct job count."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            context_file = tmp_path / "context.textproto"
            context_file.write_text("job_count: 32\n")

            # Redirect stdout to capture the printed job count
            f_stdout = io.StringIO()
            with contextlib.redirect_stdout(f_stdout):
                exit_code = fint_build.main(
                    ["--context", str(context_file), "--print-job-count"]
                )

            self.assertEqual(exit_code, 0)
            self.assertEqual(f_stdout.getvalue().strip(), "32")

    def test_main_print_job_count_default_zero(self) -> None:
        """Verifies that --print-job-count outputs 0 when job_count is omitted."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            context_file = tmp_path / "context.textproto"
            context_file.write_text('checkout_dir: "/mock/checkout"\n')

            # Redirect stdout to capture the printed job count
            f_stdout = io.StringIO()
            with contextlib.redirect_stdout(f_stdout):
                exit_code = fint_build.main(
                    ["--context", str(context_file), "--print-job-count"]
                )

            self.assertEqual(exit_code, 0)
            self.assertEqual(f_stdout.getvalue().strip(), "0")

    def test_main_print_job_count_missing_context(self) -> None:
        """Verifies that using --print-job-count without --context returns an error."""
        f_stderr = io.StringIO()
        with contextlib.redirect_stderr(f_stderr):
            exit_code = fint_build.main(["--print-job-count"])

        self.assertEqual(exit_code, 1)
        self.assertIn(
            "Error: --context is required with --print-job-count",
            f_stderr.getvalue(),
        )

    def test_main_missing_required_args(self) -> None:
        """Verifies that missing static or wrapped_cmd returns 2."""
        # 1. Missing static
        f_stderr = io.StringIO()
        with contextlib.redirect_stderr(f_stderr):
            exit_code = fint_build.main(["--static", "", "--", "ninja"])
        self.assertEqual(exit_code, 2)
        self.assertIn(
            "Error: the following arguments are required: --static",
            f_stderr.getvalue(),
        )

        # 2. Missing wrapped_cmd
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".textproto", delete=False
        ) as f:
            f.write("")
            static_path = f.name
        try:
            f_stderr = io.StringIO()
            with contextlib.redirect_stderr(f_stderr):
                exit_code = fint_build.main(["--static", static_path])
            self.assertEqual(exit_code, 2)
            self.assertIn(
                "Error: wrapped_cmd is required for build execution",
                f_stderr.getvalue(),
            )
        finally:
            os.unlink(static_path)


class TimerTest(unittest.TestCase):
    """Tests the Timer context manager."""

    def test_timer_measures_duration(self) -> None:
        """Verifies that the Timer correctly measures elapsed time."""
        with mock.patch.object(time, "time") as mock_time:
            # Simulate passage of 5.5 seconds
            mock_time.side_effect = [100.0, 105.5]
            with fint_build.Timer() as t:
                pass
            self.assertEqual(t.duration, 5.5)


class MakeBuildContextTest(unittest.TestCase):
    """Tests the make_build_context helper function."""

    @mock.patch.object(fint_build, "load_static_spec")
    @mock.patch.object(fint_build, "load_context_spec")
    @mock.patch.object(fint_build.HostProperties, "detect")
    def test_make_build_context_with_context_path(
        self,
        mock_detect: MagicMock,
        mock_load_context: MagicMock,
        mock_load_static: MagicMock,
    ) -> None:
        """Verifies make_build_context loads both specs when context path is provided."""
        mock_detect.return_value = fint_build.HostProperties(
            os="linux", cpu="x64"
        )
        static_path = pathlib.Path("fake_static.textproto")
        context_path = pathlib.Path("fake_context.textproto")

        ctx = fint_build.make_build_context(static_path, context_path)

        mock_load_static.assert_called_once_with(static_path)
        mock_load_context.assert_called_once_with(context_path)
        self.assertEqual(ctx.host.os, "linux")

    @mock.patch.object(fint_build, "load_static_spec")
    @mock.patch.object(fint_build.HostProperties, "detect")
    @mock.patch.object(pathlib.Path, "cwd")
    def test_make_build_context_omitted_context_path(
        self,
        mock_cwd: MagicMock,
        mock_detect: MagicMock,
        mock_load_static: MagicMock,
    ) -> None:
        """Verifies make_build_context generates a fallback context spec when context path is omitted."""
        mock_detect.return_value = fint_build.HostProperties(
            os="linux", cpu="x64"
        )
        mock_cwd.return_value = pathlib.Path("/mock/cwd")
        static_path = pathlib.Path("fake_static.textproto")

        with tempfile.TemporaryDirectory() as fake_root_dir:
            fake_root = pathlib.Path(fake_root_dir)
            with mock.patch.object(fint_build, "fuchsia_root", fake_root):
                ctx = fint_build.make_build_context(static_path, None)

                mock_load_static.assert_called_once_with(static_path)
                self.assertEqual(
                    ctx.context_spec.checkout_dir,
                    str(fake_root.resolve()),
                )
                self.assertEqual(ctx.context_spec.build_dir, "/mock/cwd")

    def test_make_build_context_with_explicit_build_dir(self) -> None:
        """Verifies make_build_context uses explicit build_dir if provided."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            static_path = pathlib.Path(tmp_dir) / "static.textproto"
            static_path.write_text("")
            out_dir = pathlib.Path(tmp_dir) / "out_custom"
            out_dir.mkdir()

            ctx = fint_build.make_build_context(
                static_path,
                None,
                build_dir=out_dir,
            )
            self.assertEqual(ctx.context_spec.build_dir, str(out_dir.resolve()))


class BuildDirResolutionTest(unittest.TestCase):
    """Tests for resolving the build directory fallback."""

    def test_resolve_fallback_explicit_build_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            explicit_dir = pathlib.Path(tmp_dir)
            resolved = fint_build.resolve_fallback_build_dir(
                build_dir=explicit_dir
            )
            self.assertEqual(resolved, explicit_dir.resolve())

    def test_resolve_fallback_to_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as fake_root_dir, tempfile.TemporaryDirectory() as tmp_cwd:
            fake_root = pathlib.Path(fake_root_dir)
            with mock.patch.object(fint_build, "fuchsia_root", fake_root):
                with mock.patch.object(pathlib.Path, "cwd") as mock_cwd:
                    mock_cwd.return_value = pathlib.Path(tmp_cwd)
                    resolved = fint_build.resolve_fallback_build_dir()
                    self.assertEqual(resolved, pathlib.Path(tmp_cwd).resolve())

    def test_resolve_fallback_from_fx_build_dir(self) -> None:
        with tempfile.TemporaryDirectory() as fake_root_dir:
            fake_root = pathlib.Path(fake_root_dir)
            fx_build_dir = fake_root / ".fx-build-dir"
            custom_out = fake_root / "out/my_target"
            custom_out.mkdir(parents=True)
            fx_build_dir.write_text("out/my_target\n")

            with mock.patch.object(pathlib.Path, "cwd") as mock_cwd:
                mock_cwd.return_value = fake_root
                with mock.patch.object(fint_build, "fuchsia_root", fake_root):
                    resolved = fint_build.resolve_fallback_build_dir()
                    self.assertEqual(resolved, custom_out.resolve())

    def test_resolve_fallback_from_out_default(self) -> None:
        with tempfile.TemporaryDirectory() as fake_root_dir:
            fake_root = pathlib.Path(fake_root_dir)
            default_out = fake_root / "out/default"
            default_out.mkdir(parents=True)

            with mock.patch.object(pathlib.Path, "cwd") as mock_cwd:
                mock_cwd.return_value = fake_root
                with mock.patch.object(fint_build, "fuchsia_root", fake_root):
                    resolved = fint_build.resolve_fallback_build_dir()
                    self.assertEqual(resolved, default_out.resolve())

    def test_ingest_timeout_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            artifact_dir = tmp_path / "artifacts"
            artifact_dir.mkdir(parents=True)

            context_path = tmp_path / "context.textproto"
            context_path.write_text(
                f'artifact_dir: "{artifact_dir}"\n', encoding="utf-8"
            )

            artifacts_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON
            initial_artifacts = build_artifacts_pb2.BuildArtifacts(
                failure_summary="Build failed: exit status 124"
            )
            artifacts_path.write_text(
                json_format.MessageToJson(initial_artifacts), encoding="utf-8"
            )

            metadata_path = tmp_path / "output_metadata.json"
            metadata_path.write_text(
                json.dumps(
                    {
                        "timeout": {
                            "log_dir": "/path/to/timeout_logs",
                            "snapshots": ["pstree_stop.json"],
                            "active_processes": [
                                "[CPU] rustc foo.rs (pid 1001)",
                                "[DISK-WAIT] clang++ bar.cc (pid 1002)",
                            ],
                            "total_matching_processes": 3,
                            "total_leaf_processes": 5,
                        }
                    }
                ),
                encoding="utf-8",
            )

            ret = fint_build.ingest_timeout_metadata(
                context_path, metadata_path
            )
            self.assertEqual(ret, 0)

            # Verify build_artifacts.json was updated with active processes
            updated_artifacts = build_artifacts_pb2.BuildArtifacts()
            json_format.Parse(
                artifacts_path.read_text(encoding="utf-8"), updated_artifacts
            )
            summary = updated_artifacts.failure_summary
            self.assertIn("Build failed: exit status 124", summary)
            self.assertIn("Active processes at timeout:", summary)
            self.assertIn("* [CPU] rustc foo.rs (pid 1001)", summary)
            self.assertIn("* [DISK-WAIT] clang++ bar.cc (pid 1002)", summary)
            self.assertIn("... and 1 more (see pstree_stop.json)", summary)

    def test_ingest_timeout_metadata_initializes_fresh_artifacts_when_absent(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = pathlib.Path(tmp_dir)
            artifact_dir = tmp_path / "artifacts"
            artifact_dir.mkdir(parents=True)

            context_path = tmp_path / "context.textproto"
            context_path.write_text(
                f'artifact_dir: "{artifact_dir}"\n', encoding="utf-8"
            )

            # Note: build_artifacts.json does NOT exist yet!
            artifacts_path = artifact_dir / fint_build.BUILD_ARTIFACTS_JSON

            metadata_path = tmp_path / "output_metadata.json"
            metadata_path.write_text(
                json.dumps(
                    {
                        "timeout": {
                            "log_dir": "/path/to/timeout_logs",
                            "snapshots": ["pstree_stop.json"],
                            "active_processes": [
                                "[CPU] rustc foo.rs (pid 1001)",
                            ],
                            "total_matching_processes": 1,
                            "total_leaf_processes": 1,
                        }
                    }
                ),
                encoding="utf-8",
            )

            ret = fint_build.ingest_timeout_metadata(
                context_path, metadata_path
            )
            self.assertEqual(ret, 0)

            # Verify build_artifacts.json was created with active processes
            self.assertTrue(artifacts_path.exists())
            updated_artifacts = build_artifacts_pb2.BuildArtifacts()
            json_format.Parse(
                artifacts_path.read_text(encoding="utf-8"), updated_artifacts
            )
            summary = updated_artifacts.failure_summary
            self.assertIn("Active processes at timeout:", summary)
            self.assertIn("* [CPU] rustc foo.rs (pid 1001)", summary)


if __name__ == "__main__":
    unittest.main()
