# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the binder analysis plugin."""

import unittest
from typing import Any
from unittest.mock import MagicMock, patch

from binder import BinderPlugin
from plugins import PluginArgumentError


class BinderPluginTest(unittest.TestCase):
    """Tests for the binder analysis plugin using mock trace processor queries."""

    def test_analyze_no_bottleneck(self) -> None:
        """Tests that when there are no delays, results are empty lists."""
        plugin = BinderPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = {
            "thread_state",
            "slice",
            "thread_track",
            "thread",
            "process",
            "flow",
            "args",
            "trace_bounds",
        }
        mock_tp.run_query.return_value = []

        with patch("binder.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(["--threshold-ms", "10.0"], "dummy_trace")

        self.assertEqual(len(results), 3)
        self.assertEqual(results[0].name, "Missed Wakeups (Wakeup Latencies)")
        self.assertEqual(results[0].results, [])
        self.assertEqual(
            results[1].name, "Binder Delays (Transaction Queue Latencies)"
        )
        self.assertEqual(results[1].results, [])
        self.assertEqual(
            results[2].name, "Spawn Looper Events (Late-Spawned Wakers)"
        )
        self.assertEqual(results[2].results, [])

    def test_analyze_normal_waker_delay(self) -> None:
        """Tests identifying wakeups and binder delays without spawning loopers."""
        plugin = BinderPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = {
            "thread_state",
            "slice",
            "thread_track",
            "thread",
            "process",
            "flow",
            "args",
            "trace_bounds",
        }

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            if "thread_state" in sql:
                return [
                    {
                        "wakeup_ts": 1000,
                        "scheduling_delay_ns": 15000000,
                        "thread_name": "binder:100",
                        "tid": 100,
                        "process_name": "target_proc",
                        "pid": 50,
                        "waker_thread_name": "waker_proc",
                        "waker_tid": 200,
                    }
                ]
            elif "flow" in sql and "SpawnLooper" not in sql:
                return [
                    {
                        "flow_id": 1,
                        "send_ts": 2000,
                        "recv_ts": 17000,
                        "queue_latency_ns": 15000000,
                        "sender_thread": "sender_thread",
                        "sender_tid": 200,
                        "receiver_thread": "binder:100",
                        "receiver_tid": 100,
                        "status": "Completed",
                        "cmd": "Transaction { ... }",
                    }
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("binder.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(["--threshold-ms", "10.0"], "dummy_trace")

        self.assertEqual(len(results), 3)
        res0 = results[0].results
        assert res0 is not None
        self.assertEqual(len(res0), 1)
        self.assertEqual(res0[0]["thread_name"], "binder:100")
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 1)
        self.assertEqual(res1[0]["queue_latency_ns"], 15000000)
        self.assertEqual(results[2].results, [])

    def test_analyze_late_spawned_waker(self) -> None:
        """Tests that SpawnLooper flows are populated during late-spawned waker events."""
        plugin = BinderPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = {
            "thread_state",
            "slice",
            "thread_track",
            "thread",
            "process",
            "flow",
            "args",
            "trace_bounds",
        }

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            if "thread_state" in sql:
                return []
            elif "flow" in sql and "SpawnLooper" in sql:
                return [
                    {
                        "flow_id": 2,
                        "trigger_ts": 2100,
                        "handle_ts": 51000,
                        "spawn_latency_ns": 48900000,
                        "handle_thread": "binder:spawned_101",
                        "handle_tid": 101,
                    }
                ]
            elif "flow" in sql:
                return [
                    {
                        "flow_id": 1,
                        "send_ts": 2000,
                        "recv_ts": 52000,
                        "queue_latency_ns": 50000000,
                        "sender_thread": "sender_thread",
                        "sender_tid": 200,
                        "receiver_thread": "binder:spawned_101",
                        "receiver_tid": 101,
                        "status": "Completed",
                        "cmd": "Transaction { ... }",
                    }
                ]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("binder.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(["--threshold-ms", "10.0"], "dummy_trace")

        self.assertEqual(len(results), 3)
        self.assertEqual(results[0].results, [])
        res1 = results[1].results
        assert res1 is not None
        self.assertEqual(len(res1), 1)
        res2 = results[2].results
        assert res2 is not None
        self.assertEqual(len(res2), 1)
        self.assertEqual(res2[0]["spawn_latency_ns"], 48900000)

    def test_analyze_negative_threshold(self) -> None:
        """Tests that passing a negative threshold raises PluginArgumentError."""
        plugin = BinderPlugin()
        with self.assertRaisesRegex(
            PluginArgumentError, "must be a finite, non-negative number"
        ):
            plugin.analyze(["--threshold-ms", "-5.0"], "dummy_trace")

    def test_analyze_overflow_threshold(self) -> None:
        """Tests that passing NaN, infinity or huge values raises PluginArgumentError."""
        plugin = BinderPlugin()
        for val in ["NaN", "inf", "1e309"]:
            with self.subTest(val=val):
                with self.assertRaisesRegex(
                    PluginArgumentError, "must be a finite, non-negative number"
                ):
                    plugin.analyze(["--threshold-ms", val], "dummy_trace")

    def test_analyze_invalid_threshold_string(self) -> None:
        """Tests that passing an invalid non-float string raises PluginArgumentError (not SystemExit)."""
        plugin = BinderPlugin()
        with self.assertRaisesRegex(PluginArgumentError, "invalid float value"):
            plugin.analyze(["--threshold-ms", "abc"], "dummy_trace")

    def test_analyze_missing_schema(self) -> None:
        """Tests that missing schema tables are handled gracefully by returning error entries."""
        plugin = BinderPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = set()
        mock_tp.run_query.return_value = []

        with patch("binder.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp
            results = plugin.analyze(["--threshold-ms", "10.0"], "dummy_trace")

        self.assertEqual(len(results), 3)
        for item in results:
            assert item.error is not None
            self.assertIn("Required schema tables/views missing", item.error)

    def test_analyze_incomplete_transactions(self) -> None:
        """Tests that incomplete transactions are returned by default and filtered with --complete-only."""
        plugin = BinderPlugin()
        mock_tp = MagicMock()
        mock_tp.get_tables.return_value = {
            "thread_state",
            "slice",
            "thread_track",
            "thread",
            "process",
            "flow",
            "args",
            "trace_bounds",
        }

        def run_query_mock(sql: str) -> list[dict[str, Any]]:
            if "sqlite_master" in sql:
                return [
                    {"name": name}
                    for name in [
                        "thread_state",
                        "slice",
                        "thread_track",
                        "thread",
                        "process",
                        "flow",
                        "args",
                        "trace_bounds",
                    ]
                ]
            if "thread_state" in sql:
                return []
            elif "flow" in sql and "SpawnLooper" not in sql:
                completed_tx = {
                    "flow_id": 1,
                    "send_ts": 2000,
                    "recv_ts": 17000,
                    "queue_latency_ns": 15000000,
                    "sender_thread": "sender_thread",
                    "sender_tid": 200,
                    "receiver_thread": "binder:100",
                    "receiver_tid": 100,
                    "status": "Completed",
                    "cmd": "Transaction { ... }",
                }
                incomplete_tx = {
                    "flow_id": None,
                    "send_ts": 3000,
                    "recv_ts": None,
                    "queue_latency_ns": 20000000,
                    "sender_thread": "sender_thread",
                    "sender_tid": 200,
                    "receiver_thread": None,
                    "receiver_tid": None,
                    "status": "Incomplete",
                    "cmd": "Transaction { ... }",
                }
                if "UNION ALL" in sql:
                    # Default case: returns both
                    # Query orders by latency DESC, so incomplete (20ms) should be first, then completed (15ms)
                    return [incomplete_tx, completed_tx]
                else:
                    # --complete-only case
                    return [completed_tx]
            return []

        mock_tp.run_query.side_effect = run_query_mock

        with patch("binder.PerfettoTraceProcessor") as mock_tp_class:
            mock_tp_class.return_value.__enter__.return_value = mock_tp

            # Test default (includes incomplete)
            results_default = plugin.analyze(
                ["--threshold-ms", "10.0"], "dummy_trace"
            )
            self.assertEqual(len(results_default), 3)
            delays_default = results_default[1].results
            assert delays_default is not None
            self.assertEqual(len(delays_default), 2)
            self.assertEqual(delays_default[0]["status"], "Incomplete")
            self.assertEqual(delays_default[1]["status"], "Completed")

            # Test --complete-only
            results_complete_only = plugin.analyze(
                ["--threshold-ms", "10.0", "--complete-only"], "dummy_trace"
            )
            self.assertEqual(len(results_complete_only), 3)
            delays_complete_only = results_complete_only[1].results
            assert delays_complete_only is not None
            self.assertEqual(len(delays_complete_only), 1)
            self.assertEqual(delays_complete_only[0]["status"], "Completed")


if __name__ == "__main__":
    unittest.main()
