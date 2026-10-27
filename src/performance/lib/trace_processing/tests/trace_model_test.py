#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for trace_model.py."""

import types
import typing
import unittest

import test_utils
import trace_processing.trace_model as trace_model
import trace_processing.trace_time as trace_time


# Helper for bypassing static type checking: mypy rejects item assignment on a
# read-only mapping, but these tests need to attempt it to check the runtime
# behavior.
def _set_item(mapping: typing.Any, key: typing.Any, value: typing.Any) -> None:
    mapping[key] = value


class TraceModelTest(unittest.TestCase):
    """Trace model tests"""

    def assert_args_not_modifiable(
        self, obj: trace_model.Event | trace_model.SchedulingRecord
    ) -> None:
        """Asserts that in-place modification of `obj.args` is rejected."""
        with self.assertRaises(TypeError):
            _set_item(obj.args, "k", "new")

    def test_trace_slicing(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()

        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(sliced_model.all_events())), 8)

        tail_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            None,
        )
        self.assertEqual(len(list(tail_model.all_events())), 9)

        head_model: trace_model.Model = model.slice(
            None,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(head_model.all_events())), 14)

        # Slicing with a doubly-infinite interval should result in an identical
        # model.
        copied_model: trace_model.Model = model.slice(None, None)
        test_utils.assertModelsEqual(self, model, copied_model)

    def test_trace_slicing_sched(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()

        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697503118)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
        )
        self.assertEqual(len(sliced_model.scheduling_records[0]), 2)
        self.assertEqual(len(sliced_model.scheduling_records[1]), 0)

        tail_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
            None,
        )
        self.assertEqual(len(tail_model.scheduling_records[0]), 3)
        self.assertEqual(len(tail_model.scheduling_records[1]), 2)

        head_model: trace_model.Model = model.slice(
            None,
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778309)
            ),
        )
        self.assertEqual(len(head_model.scheduling_records[0]), 2)
        self.assertEqual(len(head_model.scheduling_records[1]), 0)

    def test_slice_doesnt_reference_old_model(self) -> None:
        model: trace_model.Model = test_utils.get_test_model()
        sliced_model: trace_model.Model = model.slice(
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697800000)
            ),
            trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(698600000)
            ),
        )
        self.assertEqual(len(list(sliced_model.all_events())), 8)

        sentinel_name: str = "OLD_MODEL_SENTINEL"
        for event in model.all_events():
            event.name = sentinel_name

        for event in sliced_model.all_events():
            if isinstance(event, trace_model.DurationEvent):
                for child_duration in event.child_durations:
                    self.assertIsNotNone(child_duration)
                    self.assertNotEqual(child_duration.name, sentinel_name)
                for child_flow in event.child_flows:
                    self.assertIsNotNone(child_flow)
                    self.assertNotEqual(child_flow.name, sentinel_name)
            elif isinstance(event, trace_model.FlowEvent):
                previous_flow_name: str | None = (
                    event.previous_flow.name
                    if event.previous_flow is not None
                    else None
                )
                next_flow_name: str | None = (
                    event.next_flow.name
                    if event.next_flow is not None
                    else None
                )
                self.assertNotEqual(previous_flow_name, sentinel_name)
                self.assertNotEqual(next_flow_name, sentinel_name)

    def test_event_args_immutable_mapping(self) -> None:
        e = trace_model.Event(
            "cat", "name", trace_time.TimePoint(0), 1, 2, None
        )
        self.assertEqual(e.args, {})
        self.assert_args_not_modifiable(e)

        # Updates via setter work and remain immutable.
        e.args = {"k": 1}
        self.assertEqual(e.args, {"k": 1})
        self.assert_args_not_modifiable(e)
        e.args = {}
        self.assertEqual(e.args, {})
        self.assert_args_not_modifiable(e)

    def test_event_args_share_caller_mapping(self) -> None:
        # Event wraps args without copying them, so events built from the same
        # dict share it rather than each holding a copy. The FXT importer
        # relies on this to avoid duplicating args across events that share an
        # arg set.
        src = {"a": 1}
        t0 = trace_time.TimePoint(0)
        e1 = trace_model.Event("cat", "e1", t0, 1, 2, src)
        e2 = trace_model.Event("cat", "e2", t0, 1, 2, src)
        src["b"] = 2
        self.assertEqual(e1.args, {"a": 1, "b": 2})
        self.assertEqual(e2.args, {"a": 1, "b": 2})
        self.assert_args_not_modifiable(e1)

    def test_event_subclass_construction_uses_base_args(self) -> None:
        t0 = trace_time.TimePoint(0)
        d0 = trace_time.TimeDelta(0)
        for base_args in (None, {"k": "v"}):
            base = trace_model.Event("cat", "name", t0, 1, 2, base_args)
            subclass_events: list[trace_model.Event] = [
                trace_model.InstantEvent(
                    trace_model.InstantEventScope.GLOBAL, base
                ),
                trace_model.CounterEvent(1, base),
                trace_model.DurationEvent(d0, None, [], [], base),
                trace_model.AsyncEvent(1, d0, base),
                trace_model.FlowEvent(
                    "f1",
                    trace_model.FlowEventPhase.START,
                    None,
                    None,
                    None,
                    base,
                ),
            ]
            for event in subclass_events:
                with self.subTest(cls=type(event).__name__, args=base_args):
                    self.assertEqual(event.args, base_args or {})
                    self.assertIs(event.args, base.args)

    def test_scheduling_record_args_immutable_mapping(self) -> None:
        t0 = trace_time.TimePoint(0)
        factories: list[
            tuple[str, typing.Callable[..., trace_model.SchedulingRecord]]
        ] = [
            (
                "ContextSwitch",
                lambda *a: trace_model.ContextSwitch(
                    t0,
                    1,
                    2,
                    10,
                    20,
                    trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                    *a,
                ),
            ),
            (
                "Waking",
                lambda *a: trace_model.Waking(t0, 1, 10, *a),
            ),
        ]
        for name, make in factories:
            with self.subTest(cls=name):
                # Constructed with args={}.
                rec_empty = make({})
                self.assertEqual(rec_empty.args, {})
                self.assert_args_not_modifiable(rec_empty)

                # Constructed with args omitted.
                rec_default = make()
                self.assertEqual(rec_default.args, {})
                self.assert_args_not_modifiable(rec_default)

                # Constructed with a dict: isolated from caller mutations.
                src = {"a": 1}
                rec_with_src = make(src)
                self.assertEqual(rec_with_src.args, src)
                src["b"] = 2
                self.assertEqual(rec_with_src.args, {"a": 1})
                self.assert_args_not_modifiable(rec_with_src)

                # Constructed with a read-only view of a dict: still isolated
                # from mutations to the underlying dict.
                viewed_src = {"a": 1}
                rec_with_view = make(types.MappingProxyType(viewed_src))
                viewed_src["b"] = 2
                self.assertEqual(rec_with_view.args, {"a": 1})

                # Updates via setter.
                rec_empty.args = {"k": 1}
                self.assertEqual(rec_empty.args, {"k": 1})
                self.assert_args_not_modifiable(rec_empty)
                rec_empty.args = {}
                self.assertEqual(rec_empty.args, {})

    def test_slice_preserves_args(self) -> None:
        t0 = trace_time.TimePoint(0)
        t1 = trace_time.TimePoint(100)
        t2 = trace_time.TimePoint(200)
        e_empty = trace_model.Event("cat", "e_empty", t1, 1, 2, None)
        e_with_args = trace_model.Event("cat", "e_args", t1, 1, 2, {"k": "v"})
        thread = trace_model.Thread(2, "t", [e_empty, e_with_args])
        proc = trace_model.Process(1, "p", [thread])
        model = trace_model.Model([proc], {})

        sliced = model.slice(t0, t2)

        sliced_events = list(sliced.all_events())
        sliced_empty = sliced_events[0]
        sliced_with_args = sliced_events[1]

        self.assertEqual(e_empty.args, {})
        self.assertEqual(sliced_empty.args, {})
        self.assertEqual(e_with_args.args, {"k": "v"})
        self.assertEqual(sliced_with_args.args, {"k": "v"})

        self.assert_args_not_modifiable(sliced_empty)
        self.assert_args_not_modifiable(sliced_with_args)
