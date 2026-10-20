# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the app-launch diagnostic analysis plugin."""

import unittest
from typing import Any
from unittest.mock import MagicMock, patch

from app_launch import AppLaunchPlugin, _build_launch_cte
from plugins import PluginArgumentError


class AppLaunchPluginTest(unittest.TestCase):
    """Tests for the app-launch analysis plugin using mock trace processor queries."""

    def setUp(self) -> None:
        self.all_tables = {
            "slice",
            "thread_track",
            "thread",
            "process",
            "thread_state",
        }

    def test_analyze_success_flow(self) -> None:
        """Tests that all 5 diagnostic sections are executed in order with expected outputs."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            if "pre_fork_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "process_name": "rformanceLaunch",
                        "startup_type": "cold",
                        "total_launch_ms": 420.5,
                        "pre_fork_ms": 18.2,
                        "process_fork_ms": 24.3,
                        "attach_app_ms": 31.0,
                        "bind_app_ms": 115.0,
                        "activity_lifecycle_ms": 82.0,
                        "first_frame_ui_ms": 65.0,
                        "first_frame_render_ms": 85.0,
                        "display_present_tail_ms": 12.4,
                    }
                ]
            elif "substages_with_sum" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "total_launch_ms": 420.5,
                        "init": 135.2,
                        "draw": 85.0,
                        "render": 6.6,
                        "traversal": 58.4,
                        "animation": 0.0,
                        "scenic": 3.4,
                    }
                ]
            elif "open_dex_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "open_dex_ms": 48.5,
                        "dlopen_ms": 12.1,
                        "verify_class_ms": 14.0,
                        "class_init_ms": 9.3,
                        "get_resources_ms": 18.2,
                        "inflate_ms": 34.6,
                        "relayout_window_ms": 11.4,
                        "dequeue_buffer_ms": 22.0,
                        "shader_compile_ms": 19.5,
                        "queue_present_ms": 38.2,
                    }
                ]
            elif "running_cpu_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "segment": "S3_BindApplication",
                        "thread_name": "rformanceLaunch",
                        "segment_dur_ms": 115.0,
                        "running_cpu_ms": 70.0,
                        "runnable_starved_ms": 15.0,
                        "sleeping_blocked_ms": 30.0,
                        "running_pct": 60.9,
                    }
                ]
            elif "fxfs_page_in_ms" in sql:
                return [
                    {
                        "startup_id": 1,
                        "package": "com.android.performanceLaunch",
                        "main_binder_calls": 14,
                        "main_binder_ms": 26.4,
                        "monitor_contention_ms": 4.2,
                        "native_lock_contention_ms": 8.1,
                        "fxfs_page_in_ms": 19.8,
                        "sysmem_busy_ms": 16.5,
                        "selinux_hooks_ms": 1.3,
                    }
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(
                ["--package", "performanceLaunch", "--limit", "10"],
                "dummy_trace",
            )

        self.assertEqual(len(results), 5)

        # 1. Critical Path Phase Breakdown (Sequential Segments)
        self.assertEqual(
            results[0].name,
            "Critical Path Phase Breakdown (Sequential Segments)",
        )
        res0 = results[0].results
        assert res0 is not None
        self.assertEqual(len(res0), 1)
        self.assertEqual(res0[0]["pre_fork_ms"], 18.2)
        self.assertEqual(res0[0]["process_fork_ms"], 24.3)
        self.assertEqual(res0[0]["bind_app_ms"], 115.0)
        self.assertEqual(res0[0]["first_frame_render_ms"], 85.0)

        # 2. CUJ Sub-Stage Breakdown
        self.assertEqual(results[1].name, "CUJ Sub-Stage Breakdown")
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 1)
        self.assertEqual(res1[0]["init"], 135.2)
        self.assertEqual(res1[0]["traversal"], 58.4)
        self.assertEqual(res1[0]["draw"], 85.0)
        self.assertEqual(res1[0]["scenic"], 3.4)

        # 3. Intra-Segment Sub-Operation Breakdown
        self.assertEqual(
            results[2].name, "Intra-Segment Sub-Operation Breakdown"
        )
        res2 = results[2].results
        assert res2 is not None
        self.assertEqual(len(res2), 1)
        self.assertEqual(res2[0]["open_dex_ms"], 48.5)
        self.assertEqual(res2[0]["inflate_ms"], 34.6)

        # 4. Critical Path Thread State Breakdown
        self.assertEqual(
            results[3].name, "Critical Path Thread State Breakdown"
        )
        res3 = results[3].results
        assert res3 is not None
        self.assertEqual(len(res3), 1)
        self.assertEqual(res3[0]["segment"], "S3_BindApplication")
        self.assertEqual(res3[0]["running_cpu_ms"], 70.0)

        # 5. Cross-Layer Contention & Fuchsia Subsystem Attribution
        self.assertEqual(
            results[4].name,
            "Cross-Layer Contention & Fuchsia Subsystem Attribution",
        )
        res4 = results[4].results
        assert res4 is not None
        self.assertEqual(len(res4), 1)
        self.assertEqual(res4[0]["fxfs_page_in_ms"], 19.8)
        self.assertEqual(res4[0]["sysmem_busy_ms"], 16.5)

    def test_analyze_empty_results(self) -> None:
        """Tests that empty query results format as empty lists across all 5 sections."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        mock_tp.run_query.return_value = []

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 5)
        for section in results:
            self.assertTrue(section.name)
            self.assertEqual(section.results, [])

    def test_analyze_missing_schema(self) -> None:
        """Tests that missing schema tables are handled gracefully with section error messages."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = set()
        mock_tp.run_query.return_value = []

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 5)
        for section in results:
            assert section.error is not None
            self.assertIn("Required schema tables/views missing", section.error)

    def test_analyze_query_failure(self) -> None:
        """Tests that runtime SQL query failures return formatted error dicts without crashing."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        mock_tp.run_query.side_effect = RuntimeError(
            "database disk image is malformed"
        )

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_trace")

        self.assertEqual(len(results), 5)
        for section in results:
            assert section.error is not None
            self.assertIn(
                "Query execution failed: database disk image is malformed",
                section.error,
            )

    def test_analyze_invalid_limit(self) -> None:
        """Tests that invalid, non-positive, or non-integer limits raise PluginArgumentError."""
        plugin = AppLaunchPlugin()
        for invalid_limit in ["0", "-5"]:
            with self.subTest(limit=invalid_limit):
                with self.assertRaisesRegex(
                    PluginArgumentError, "Limit must be a positive integer"
                ):
                    plugin.analyze(["--limit", invalid_limit], "dummy_trace")

        with self.assertRaisesRegex(PluginArgumentError, "invalid int value"):
            plugin.analyze(["--limit", "abc"], "dummy_trace")

    def test_analyze_scaling_option(self) -> None:
        """Tests that --scaling proportional injects proportional SQL expressions and validates choices."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(
                ["--scaling", "proportional"], "dummy_trace"
            )

        self.assertEqual(len(results), 5)
        self.assertIn("stage_sum > 0 THEN", captured_queries[1])

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze(["--scaling", "raw"], "dummy_trace")

        # Scaling applies only to the CUJ sub-stages: the critical path segments already
        # sum to the launch duration.
        self.assertEqual(captured_queries[0], captured_queries[5])
        self.assertNotIn("stage_sum > 0 THEN", captured_queries[6])

        with self.assertRaisesRegex(PluginArgumentError, "invalid choice"):
            plugin.analyze(["--scaling", "invalid"], "dummy_trace")

    def test_critical_path_segments_cover_warm_launches(self) -> None:
        """Tests that warm launches get a pre-activity segment and gaps are reported."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze([], "dummy_trace")

        query = captured_queries[0]
        # Without bindApplication, the launch start to the first activityStart or
        # activityResume is its own segment instead of being left out.
        self.assertIn(
            "THEN t3b_activity_start_ts - t0b_activity_start_request_ts",
            query,
        )
        self.assertIn("AS pre_activity_ms", query)
        self.assertIn("COALESCE(pre_activity_dur, 0)", query)
        # Time no segment covers is reported, so the columns sum to total_launch_ms.
        self.assertIn(
            "ROUND((total_launch_dur - seg_sum) / 1e6, 2) AS unattributed_ms",
            query,
        )

    def _capture_queries(self, args: list[str]) -> list[str]:
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock
        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze(args, "dummy_trace")
        return captured_queries

    def test_critical_path_reports_time_from_activity_start(self) -> None:
        """Tests that launches are also measured from ActivityTaskManager's START."""
        query = self._capture_queries([])[0]
        # START is the last resolveIntent before shouldAbortBackgroundActivityStart.
        self.assertIn("sab.name = 'shouldAbortBackgroundActivityStart'", query)
        self.assertIn("SELECT MAX(ri.ts)", query)
        self.assertIn(
            "(t6_launch_end - t0b_activity_start_request_ts) AS displayed_dur",
            query,
        )
        self.assertIn(
            "(t0b_activity_start_request_ts - t0_launch_start) AS pre_start_dur",
            query,
        )
        # pre_start is its own segment, so the segments still sum to the launch.
        self.assertIn("COALESCE(pre_start_dur, 0)", query)
        self.assertIn(
            "(t1_start_proc_ts - t0b_activity_start_request_ts) AS pre_fork_dur",
            query,
        )
        # Without a START marker, the launch start stands in for it, so displayed_ms is
        # always a number.
        self.assertRegex(
            query,
            r"LIMIT 1\s*\),\s*m\.t0_launch_start\s*\) AS t0b_activity_start_request_ts",
        )

    def test_hot_launches_resolve_process_and_first_frame(self) -> None:
        """Tests that hot launches find a long truncated process name and the right frame."""
        query = self._capture_queries([])[0]
        # Starnix can truncate long process names to a prefix longer than 15 characters,
        # e.g. 'com.google.android.apps.nexusla'.
        self.assertEqual(
            query.count(
                "(LENGTH(p.name) >= 15 AND r.package GLOB p.name || '*')"
            ),
            2,
        )
        # Frames drawn before the app starts handling the launch (e.g. a hot launcher's
        # animations) are not the launch's first frame.
        self.assertIn(
            "a.name IN ('bindApplication', 'activityStart', 'activityResume')",
            query,
        )

    def test_contention_columns_cover_only_ui_threads(self) -> None:
        """Tests that binder and lock columns count outermost slices on the app's threads."""
        query = self._capture_queries([])[4]
        # Starnix emits BinderIoctl rather than the Linux binder transaction events.
        self.assertIn("s.name = 'BinderIoctl'", query)
        self.assertIn("AS main_binder_calls", query)
        self.assertIn("tt.utid IN (l.main_utid)\n", query)
        self.assertIn("tt.utid IN (l.main_utid, l.render_utid)\n", query)
        self.assertNotIn("t.upid = l.upid OR p.name = 'system_server'", query)
        # Nested matches are skipped and slices are clipped to the launch.
        self.assertIn("NOT EXISTS (", query)
        self.assertIn("FROM ancestor_slice(s.id) anc", query)
        self.assertIn(
            "SUM(MIN(l.t6_launch_end, s.ts + s.dur) - MAX(l.t0_launch_start, s.ts))",
            query,
        )
        # sysmem is reported as its own busy time, not summed across processes.
        self.assertIn("AS sysmem_busy_ms", query)
        self.assertNotIn("BufferCollection", query)

    def test_cuj_substages_count_self_time_of_nested_slices(self) -> None:
        """Tests that nested sub-stage slices are not counted twice."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            plugin.analyze([], "dummy_trace")

        query = captured_queries[1]
        # Each sub-stage slice finds its nearest sub-stage ancestor, which subtracts it.
        self.assertIn("FROM ancestor_slice(a.slice_id) anc", query)
        self.assertIn("c.stage_parent_id = p.slice_id", query)
        for stage in ["init", "traversal", "render", "draw", "animation"]:
            with self.subTest(stage=stage):
                self.assertIn(
                    f"SUM(CASE WHEN a.stage = '{stage}' THEN a.self_dur END)",
                    query,
                )
        # Slices are clipped to the launch end.
        self.assertIn(
            "MIN(s.ts + s.dur, m.t6_launch_end) - s.ts AS clipped_dur", query
        )

    def test_build_launch_cte_escapes_package_filter(self) -> None:
        """Tests that package filter quotes are escaped in SQL CTE construction."""
        cte = _build_launch_cte("foo'bar")
        self.assertIn("*foo''bar*", cte)

    def test_analyze_multiple_launches_in_single_trace(self) -> None:
        """Tests that multiple launches (e.g. cold + warm or multiple apps) in one trace are preserved."""
        plugin = AppLaunchPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = self.all_tables
        captured_queries: list[str] = []

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            captured_queries.append(sql)
            if "pre_fork_ms" in sql:
                return [
                    {
                        "startup_id": 14,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "cold",
                        "total_launch_ms": 1571.6,
                    },
                    {
                        "startup_id": 21,
                        "package": "com.google.android.calendar",
                        "process_name": "com.google.android.calendar",
                        "startup_type": "warm",
                        "total_launch_ms": 209.95,
                    },
                ]
            elif "substages_with_sum" in sql:
                return [
                    {
                        "startup_id": 14,
                        "package": "com.google.android.calendar",
                        "total_launch_ms": 1571.6,
                        "init": 389.49,
                        "draw": 57.53,
                        "render": 6.66,
                        "traversal": 86.26,
                        "animation": 0.0,
                        "scenic": 2.38,
                    },
                    {
                        "startup_id": 21,
                        "package": "com.google.android.calendar",
                        "total_launch_ms": 209.95,
                        "init": 73.45,
                        "draw": 51.27,
                        "render": 2.55,
                        "traversal": 6.53,
                        "animation": 0.0,
                        "scenic": 0.77,
                    },
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("app_launch.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze([], "dummy_multi_launch_trace")

        res0 = results[0].results

        assert res0 is not None

        self.assertEqual(len(res0), 2)
        self.assertEqual(res0[0]["startup_type"], "cold")
        self.assertEqual(res0[1]["startup_type"], "warm")
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 2)
        self.assertIn(
            "USING (startup_id, t0_launch_start)", captured_queries[1]
        )
        self.assertIn(
            "USING (startup_id, t0_launch_start)", captured_queries[2]
        )


if __name__ == "__main__":
    unittest.main()
