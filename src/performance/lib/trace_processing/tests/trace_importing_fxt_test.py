# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for trace_importing_fxt.py."""

import contextlib
import pathlib
import unittest
from unittest import mock

from tp_shell import PerfettoTraceProcessor
from trace_processing import (
    trace_importing_fxt,
    trace_model,
    trace_time,
)


class QueriesTest(unittest.TestCase):
    """Dedicated unit test suite validating trace query helper methods on _FxtImporter."""

    _exit_stack: contextlib.ExitStack
    session: PerfettoTraceProcessor
    importer: trace_importing_fxt._FxtImporter

    @classmethod
    def setUpClass(cls) -> None:
        cls._exit_stack = contextlib.ExitStack()
        fxt_path = (
            pathlib.Path(__file__).resolve().parent.parent.parent
            / "runtime_deps"
            / "model.fxt"
        )
        cls.session = cls._exit_stack.enter_context(
            PerfettoTraceProcessor(trace_path=str(fxt_path))
        )
        cls.importer = trace_importing_fxt._FxtImporter(cls.session)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._exit_stack.close()

    def test_get_slices_with_filtering(self) -> None:
        # 1. Filter by pattern
        importer_filtered = trace_importing_fxt._FxtImporter(
            self.session, patterns={"Read"}
        )
        events, _, _ = importer_filtered.get_slices()
        self.assertGreater(len(events), 0)
        for event in events:
            self.assertIn("Read", event.name)

        # 2. Filter by category
        all_events, _, _ = self.importer.get_slices()
        target_cat = all_events[0].category
        importer_cat = trace_importing_fxt._FxtImporter(
            self.session, categories={target_cat}
        )
        cat_events, _, _ = importer_cat.get_slices()
        self.assertGreater(len(cat_events), 0)
        for event in cat_events:
            self.assertEqual(event.category, target_cat)

        # 3. Empty pattern set (discards all events)
        importer_empty = trace_importing_fxt._FxtImporter(
            self.session, patterns=set()
        )
        empty_events, _, _ = importer_empty.get_slices()
        self.assertEqual(len(empty_events), 0)

    def test_get_pid_to_name(self) -> None:
        processes = self.importer.get_pid_to_name()
        # Verify process mapping resolved actual Fuchsia processes from trace
        self.assertGreater(len(processes.pid_to_name), 0)
        self.assertIn("process_foo", processes.pid_to_name.values())

    def test_get_threads(self) -> None:
        tid_to_name, tid_to_pid = self.importer.get_threads()
        # Verify thread names mapped correctly
        self.assertGreater(len(tid_to_name), 0)
        self.assertIn("initial-thread", tid_to_name.values())

    def test_get_args_map(self) -> None:
        arg_set_id_to_args = self.importer._get_args_map()
        self.assertGreater(len(arg_set_id_to_args), 0)
        found_bool = False
        for args in arg_set_id_to_args.values():
            if "trace_id_is_process_scoped" in args:
                val = args["trace_id_is_process_scoped"]
                self.assertEqual(val, 0)
                found_bool = True
        self.assertTrue(found_bool)

    def test_get_args_map_excludes_chromium_binary_data(self) -> None:
        fresh_importer = trace_importing_fxt._FxtImporter(self.session)
        with mock.patch.object(
            fresh_importer, "_query", wraps=fresh_importer._query
        ) as mock_query:
            args_map = fresh_importer._get_args_map()

        mock_query.assert_called_once()
        executed_sql = mock_query.call_args[0][0]
        self.assertIn("WHERE key NOT LIKE 'debug.params.%.data'", executed_sql)
        self.assertGreater(len(args_map), 0)

        # Verify SQLite filter semantics on Perfetto engine
        filtered = self.session.run_query(
            "SELECT 'debug.params.page_state.data' AS key "
            "WHERE 'debug.params.page_state.data' NOT LIKE 'debug.params.%.data'"
        )
        self.assertEqual(len(filtered), 0)

        unfiltered = self.session.run_query(
            "SELECT 'trace_id_is_process_scoped' AS key "
            "WHERE 'trace_id_is_process_scoped' NOT LIKE 'debug.params.%.data'"
        )
        self.assertEqual(len(unfiltered), 1)

    def test_get_slices(self) -> None:
        (
            result_events,
            slice_id_to_event,
            slice_parent_map,
        ) = self.importer.get_slices()

        self.importer._restore_slice_hierarchy(
            slice_parent_map, slice_id_to_event
        )
        self.assertGreater(len(result_events), 0)
        slice_names = [e.name for e in result_events]
        self.assertIn("Read", slice_names)

        async_events = [
            e for e in result_events if isinstance(e, trace_model.AsyncEvent)
        ]
        self.assertGreater(len(async_events), 0)
        self.assertIsInstance(async_events[0], trace_model.AsyncEvent)

    def test_get_counter_events(self) -> None:
        counters = self.importer.get_counter_events()
        self.assertGreater(len(counters.events), 0)
        self.assertIsInstance(counters.events[0], trace_model.CounterEvent)
        self.assertEqual(counters.events[0].name, "cpu_usage")

    def test_get_scheduling_records(self) -> None:
        scheduling = self.importer.get_scheduling_records()
        # Verify context switches scheduling tracks populated with actual trace records
        self.assertIn(0, scheduling.records)
        self.assertGreater(len(scheduling.records[0]), 0)
        first_switch = scheduling.records[0][0]
        self.assertIsInstance(first_switch, trace_model.ContextSwitch)

    def test_get_scheduling_records_end_states(self) -> None:
        # Columns: ts, cpu, dur, end_state, priority, tid, state
        mock_rows = [
            ["1000", "0", "100", "R", "3122", "101", "Running"],
            ["1100", "0", "100", "S", "3122", "102", "Running"],
            ["1200", "0", "100", "D", "3122", "103", "Running"],
            ["1300", "0", "100", "T", "3122", "104", "Running"],
            ["1400", "0", "100", "Z", "3122", "105", "Running"],
            # Gap from 1500 to 1600 exercises outgoing_state across an idle gap.
            ["1600", "0", "100", "X", "3122", "106", "Running"],
            ["1700", "0", "100", "R", "3122", "107", "Running"],
        ]
        with mock.patch.object(self.importer, "_query", return_value=mock_rows):
            scheduling = self.importer.get_scheduling_records()

        switches = [
            r
            for r in scheduling.records[0]
            if isinstance(r, trace_model.ContextSwitch)
        ]
        outgoing = [(r.outgoing_tid, r.outgoing_state) for r in switches]
        self.assertEqual(
            outgoing,
            [
                (0, trace_model.ThreadState.ZX_THREAD_STATE_RUNNING),
                (101, trace_model.ThreadState.ZX_THREAD_STATE_RUNNING),
                (102, trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED),
                (103, trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED),
                (104, trace_model.ThreadState.ZX_THREAD_STATE_SUSPENDED),
                (105, trace_model.ThreadState.ZX_THREAD_STATE_DYING),
                (0, trace_model.ThreadState.ZX_THREAD_STATE_RUNNING),
                (106, trace_model.ThreadState.ZX_THREAD_STATE_DEAD),
                (107, trace_model.ThreadState.ZX_THREAD_STATE_RUNNING),
            ],
        )

    def test_get_scheduling_records_waking_during_idle_gap(self) -> None:
        # Columns: ts, cpu, dur, end_state, priority, tid, state
        mock_rows = [
            ["1000", "0", "500", "S", "3122", "101", "Running"],
            ["2000", "0", "0", "", "0", "102", "W"],
            ["2500", "0", "500", "R", "3122", "102", "Running"],
        ]
        with mock.patch.object(self.importer, "_query", return_value=mock_rows):
            scheduling = self.importer.get_scheduling_records()

        records = scheduling.records[0]
        self.assertEqual(len(records), 5)

        self.assertIsInstance(records[0], trace_model.ContextSwitch)
        assert isinstance(records[0], trace_model.ContextSwitch)
        self.assertEqual(
            records[0].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(1000.0)
            ),
        )
        self.assertEqual(records[0].tid, 101)
        self.assertEqual(records[0].outgoing_tid, 0)
        self.assertEqual(records[0].prio, 3122)
        self.assertIsNone(records[0].outgoing_prio)
        self.assertEqual(
            records[0].outgoing_state,
            trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
        )

        self.assertIsInstance(records[1], trace_model.ContextSwitch)
        assert isinstance(records[1], trace_model.ContextSwitch)
        self.assertEqual(
            records[1].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(1500.0)
            ),
        )
        self.assertEqual(records[1].tid, 0)
        self.assertEqual(records[1].outgoing_tid, 101)
        self.assertEqual(records[1].prio, trace_model.INT32_MIN)
        self.assertEqual(records[1].outgoing_prio, 3122)
        self.assertEqual(
            records[1].outgoing_state,
            trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
        )

        self.assertIsInstance(records[2], trace_model.Waking)
        self.assertEqual(
            records[2].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(2000.0)
            ),
        )
        self.assertEqual(records[2].tid, 102)
        self.assertEqual(records[2].prio, 0)

        self.assertIsInstance(records[3], trace_model.ContextSwitch)
        assert isinstance(records[3], trace_model.ContextSwitch)
        self.assertEqual(
            records[3].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(2500.0)
            ),
        )
        self.assertEqual(records[3].tid, 102)
        self.assertEqual(records[3].outgoing_tid, 0)
        self.assertEqual(records[3].prio, 3122)
        self.assertEqual(records[3].outgoing_prio, trace_model.INT32_MIN)
        self.assertEqual(
            records[3].outgoing_state,
            trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
        )

        self.assertIsInstance(records[4], trace_model.ContextSwitch)
        assert isinstance(records[4], trace_model.ContextSwitch)
        self.assertEqual(
            records[4].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(3000.0)
            ),
        )
        self.assertEqual(records[4].tid, 0)
        self.assertEqual(records[4].outgoing_tid, 102)
        self.assertEqual(records[4].prio, trace_model.INT32_MIN)
        self.assertEqual(records[4].outgoing_prio, 3122)
        self.assertEqual(
            records[4].outgoing_state,
            trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
        )

    def test_get_scheduling_records_final_outgoing_switch(self) -> None:
        # Columns: ts, cpu, dur, end_state, priority, tid, state
        # CPU 0 ends with a closed slice (dur=500, end_state="S") and should
        # emit a trailing ContextSwitch into IDLE_TID at ts=1500.
        # CPU 1 ends with an unclosed slice (dur=-1, end_state="") and should
        # NOT emit a trailing ContextSwitch into IDLE_TID.
        mock_rows = [
            ["1000", "0", "500", "S", "3122", "101", "Running"],
            ["1000", "1", "-1", "", "3122", "201", "Running"],
        ]
        with mock.patch.object(self.importer, "_query", return_value=mock_rows):
            scheduling = self.importer.get_scheduling_records()

        cpu0_records = scheduling.records[0]
        self.assertEqual(len(cpu0_records), 2)
        self.assertIsInstance(cpu0_records[1], trace_model.ContextSwitch)
        assert isinstance(cpu0_records[1], trace_model.ContextSwitch)
        self.assertEqual(
            cpu0_records[1].start,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(1500.0)
            ),
        )
        self.assertEqual(cpu0_records[1].tid, 0)
        self.assertEqual(cpu0_records[1].outgoing_tid, 101)
        self.assertEqual(cpu0_records[1].prio, trace_model.INT32_MIN)
        self.assertEqual(cpu0_records[1].outgoing_prio, 3122)
        self.assertEqual(
            cpu0_records[1].outgoing_state,
            trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
        )

        cpu1_records = scheduling.records[1]
        self.assertEqual(len(cpu1_records), 1)
        self.assertIsInstance(cpu1_records[0], trace_model.ContextSwitch)
        assert isinstance(cpu1_records[0], trace_model.ContextSwitch)
        self.assertEqual(cpu1_records[0].tid, 201)

    def test_get_flow_events(self) -> None:
        (
            _,
            slice_id_to_event,
            slice_parent_map,
        ) = self.importer.get_slices()
        flows = self.importer.get_flow_events(
            slice_id_to_event, slice_parent_map
        )

        flow_events = [
            e for e in flows.events if isinstance(e, trace_model.FlowEvent)
        ]
        self.assertGreater(len(flow_events), 0)
        self.assertIsNotNone(flow_events[0].id)

    def test_get_flow_events_with_category_filtering(self) -> None:
        all_slices = self.importer.get_slices()
        target_cat = all_slices.events[0].category
        filtered_importer = trace_importing_fxt._FxtImporter(
            self.session, categories={target_cat}
        )
        filtered_slices = filtered_importer.get_slices()
        flows = filtered_importer.get_flow_events(
            filtered_slices.id_to_event, filtered_slices.parent_map
        )
        flow_events = [
            e for e in flows.events if isinstance(e, trace_model.FlowEvent)
        ]
        self.assertGreater(len(flow_events), 0)
        has_enclosing = any(
            f.enclosing_duration is not None for f in flow_events
        )
        self.assertTrue(has_enclosing)

    def test_tp_session_query_and_create_model(self) -> None:
        fxt_path = (
            pathlib.Path(__file__).resolve().parent.parent.parent
            / "runtime_deps"
            / "model.fxt"
        )
        with PerfettoTraceProcessor(trace_path=str(fxt_path)) as session:
            # Query directly against the trace session without building a model
            res = session.run_query("SELECT count(*) as cnt FROM slice")
            self.assertEqual(len(res), 1)
            self.assertGreater(res[0]["cnt"], 0)

            # Build model from existing session without closing the session
            model = trace_importing_fxt.create_model_from_tp_session(session)
            self.assertGreater(len(list(model.all_events())), 0)

            # Verify session is still active after building model
            res2 = session.run_query("SELECT count(*) as cnt FROM slice")
            self.assertEqual(res, res2)

        # After context manager exits, querying closed session raises RuntimeError
        with self.assertRaises(RuntimeError):
            session.run_query("SELECT count(*) FROM slice")
