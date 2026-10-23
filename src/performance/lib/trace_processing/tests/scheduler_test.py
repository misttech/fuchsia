#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for scheduler.py."""

import unittest

from reporting import metrics
from trace_processing import trace_model, trace_time, trace_utils
from trace_processing.metrics import scheduler


def _tp_ms(ms: float) -> trace_time.TimePoint:
    return trace_time.TimePoint.from_epoch_delta(
        trace_time.TimeDelta.from_milliseconds(ms)
    )


class SchedulerMetricsTest(unittest.TestCase):
    """Scheduler metrics tests."""

    def test_empty_model(self) -> None:
        model = trace_model.Model(
            processes=[],
            scheduling_records={},
        )
        processor = scheduler.SchedulerMetricsProcessor()
        self.assertEqual(processor.process_metrics(model), [])

    def test_wakeup_and_preemption_raw_and_aggregates(self) -> None:
        records_cpu0: list[trace_model.SchedulingRecord] = [
            # Thread 10 wakes at 10ms on CPU 0, switches in at 12ms on CPU 0.
            trace_model.Waking(_tp_ms(10.0), tid=10, prio=20, args={}),
            trace_model.ContextSwitch(
                start=_tp_ms(12.0),
                incoming_tid=10,
                outgoing_tid=1000,
                incoming_prio=20,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
            # Priority-inheritance self-switch (should be ignored).
            trace_model.ContextSwitch(
                start=_tp_ms(15.0),
                incoming_tid=10,
                outgoing_tid=10,
                incoming_prio=25,
                outgoing_prio=20,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                args={},
            ),
            trace_model.Waking(_tp_ms(19.0), tid=20, prio=30, args={}),
            # Thread 10 preempted by Thread 20 at 20ms.
            trace_model.ContextSwitch(
                start=_tp_ms(20.0),
                incoming_tid=20,
                outgoing_tid=10,
                incoming_prio=30,
                outgoing_prio=25,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
            # Thread 20 preempted at 22ms (with ZX_THREAD_STATE_RUNNING, as emitted by FXT importer).
            trace_model.ContextSwitch(
                start=_tp_ms(22.0),
                incoming_tid=30,
                outgoing_tid=20,
                incoming_prio=31,
                outgoing_prio=30,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                args={},
            ),
            # Thread 20 switches back in at 24ms (preemption delay = 2ms).
            trace_model.ContextSwitch(
                start=_tp_ms(24.0),
                incoming_tid=20,
                outgoing_tid=30,
                incoming_prio=30,
                outgoing_prio=31,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
                args={},
            ),
        ]

        records_cpu1: list[trace_model.SchedulingRecord] = [
            # Thread 10 (preempted at 20ms on CPU 0) switches in at 25ms on CPU 1 (preemption delay = 5ms).
            trace_model.ContextSwitch(
                start=_tp_ms(25.0),
                incoming_tid=10,
                outgoing_tid=1001,
                incoming_prio=25,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
            # Thread 10 blocks at 30ms; idle thread 1001 switches in (idle thread should not be tracked).
            trace_model.ContextSwitch(
                start=_tp_ms(30.0),
                incoming_tid=1001,
                outgoing_tid=10,
                incoming_prio=trace_model.INT32_MIN,
                outgoing_prio=25,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
                args={},
            ),
            # Thread 10 switches in later at 40ms without a Waking record (e.g. dropped wakeup);
            # should not use stale preemption state since it blocked at 30ms.
            trace_model.ContextSwitch(
                start=_tp_ms(40.0),
                incoming_tid=10,
                outgoing_tid=1001,
                incoming_prio=25,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
        ]

        model = trace_model.Model(
            processes=[],
            scheduling_records={0: records_cpu0, 1: records_cpu1},
        )

        raw_processor = scheduler.SchedulerMetricsProcessor()
        raw_results = list(raw_processor.process_metrics(model))
        self.assertEqual(
            raw_results,
            [
                metrics.TestCaseResult(
                    label="SchedWakeupLatency",
                    unit=metrics.Unit.milliseconds,
                    values=[2.0, 1.0],
                    doc="Time from thread wakeup until scheduled on a CPU",
                ),
                metrics.TestCaseResult(
                    label="SchedPreemptionDelay",
                    unit=metrics.Unit.milliseconds,
                    values=[2.0, 5.0],
                    doc="Time a preempted runnable thread waits until next scheduled on a CPU",
                ),
                metrics.TestCaseResult(
                    label="SchedTaskWaitTime",
                    unit=metrics.Unit.milliseconds,
                    values=[2.0, 1.0, 2.0, 5.0],
                    doc="Total runnable wait time (wakeup latency and preemption delay) before being scheduled on a CPU",
                ),
            ],
        )

        agg_processor = scheduler.SchedulerMetricsProcessor(
            aggregates_only=True
        )
        agg_results = list(agg_processor.process_metrics(model))
        expected_agg = (
            trace_utils.standard_metrics_set(
                values=[2.0, 1.0],
                label_prefix="SchedWakeupLatency",
                unit=metrics.Unit.milliseconds,
                doc_prefix="Time from thread wakeup until scheduled on a CPU",
            )
            + trace_utils.standard_metrics_set(
                values=[2.0, 5.0],
                label_prefix="SchedPreemptionDelay",
                unit=metrics.Unit.milliseconds,
                doc_prefix="Time a preempted runnable thread waits until next scheduled on a CPU",
            )
            + trace_utils.standard_metrics_set(
                values=[2.0, 1.0, 2.0, 5.0],
                label_prefix="SchedTaskWaitTime",
                unit=metrics.Unit.milliseconds,
                doc_prefix="Total runnable wait time (wakeup latency and preemption delay) before being scheduled on a CPU",
            )
        )
        self.assertEqual(agg_results, expected_agg)

    def test_same_timestamp_wakeup_and_switch(self) -> None:
        # Even if ContextSwitch appears before Waking in the input list at the same timestamp,
        # Waking is ordered first so a 0.0ms wakeup latency is recorded.
        records: list[trace_model.SchedulingRecord] = [
            trace_model.ContextSwitch(
                start=_tp_ms(5.0),
                incoming_tid=10,
                outgoing_tid=0,
                incoming_prio=20,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
            trace_model.Waking(_tp_ms(5.0), tid=10, prio=20, args={}),
        ]
        model = trace_model.Model(processes=[], scheduling_records={0: records})
        processor = scheduler.SchedulerMetricsProcessor(aggregates_only=False)
        results = list(processor.process_metrics(model))
        self.assertEqual(
            results,
            [
                metrics.TestCaseResult(
                    label="SchedWakeupLatency",
                    unit=metrics.Unit.milliseconds,
                    values=[0.0],
                    doc="Time from thread wakeup until scheduled on a CPU",
                ),
                metrics.TestCaseResult(
                    label="SchedTaskWaitTime",
                    unit=metrics.Unit.milliseconds,
                    values=[0.0],
                    doc="Total runnable wait time (wakeup latency and preemption delay) before being scheduled on a CPU",
                ),
            ],
        )

    def test_dropped_switch_in_clears_stale_pending_wait(self) -> None:
        # Thread 10 wakes at 10ms, its switch-in record is dropped, and it blocks at 20ms.
        # Blocking at 20ms must clear the stale 10ms pending wait so a subsequent switch-in
        # at 30ms (with a dropped Waking record) does not emit a bogus 20ms wakeup latency.
        records: list[trace_model.SchedulingRecord] = [
            trace_model.Waking(_tp_ms(10.0), tid=10, prio=20, args={}),
            trace_model.ContextSwitch(
                start=_tp_ms(20.0),
                incoming_tid=0,
                outgoing_tid=10,
                incoming_prio=trace_model.INT32_MIN,
                outgoing_prio=20,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
                args={},
            ),
            trace_model.ContextSwitch(
                start=_tp_ms(30.0),
                incoming_tid=10,
                outgoing_tid=0,
                incoming_prio=20,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_NEW,
                args={},
            ),
        ]
        model = trace_model.Model(processes=[], scheduling_records={0: records})
        processor = scheduler.SchedulerMetricsProcessor(aggregates_only=False)
        self.assertEqual(processor.process_metrics(model), [])

    def test_non_runnable_outgoing_states_not_preempted(self) -> None:
        # Outgoing threads in SUSPENDED, DYING, or DEAD states are not runnable and
        # must not be recorded as preempted.
        records: list[trace_model.SchedulingRecord] = [
            trace_model.ContextSwitch(
                start=_tp_ms(10.0),
                incoming_tid=20,
                outgoing_tid=10,
                incoming_prio=20,
                outgoing_prio=20,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_SUSPENDED,
                args={},
            ),
            trace_model.ContextSwitch(
                start=_tp_ms(20.0),
                incoming_tid=30,
                outgoing_tid=20,
                incoming_prio=20,
                outgoing_prio=20,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_DYING,
                args={},
            ),
            trace_model.ContextSwitch(
                start=_tp_ms(30.0),
                incoming_tid=10,
                outgoing_tid=30,
                incoming_prio=20,
                outgoing_prio=20,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_DEAD,
                args={},
            ),
        ]
        model = trace_model.Model(processes=[], scheduling_records={0: records})
        processor = scheduler.SchedulerMetricsProcessor(aggregates_only=False)
        self.assertEqual(processor.process_metrics(model), [])

    def test_idle_thread_waking_ignored(self) -> None:
        # Waking and preemption records for idle threads (synthetic tid 0 with prio=None,
        # or a CPU idle thread with INT32_MIN priority) must not be added to pending_waits.
        records: list[trace_model.SchedulingRecord] = [
            trace_model.Waking(_tp_ms(5.0), tid=0, prio=None, args={}),
            trace_model.Waking(
                _tp_ms(6.0), tid=1000, prio=trace_model.INT32_MIN, args={}
            ),
            trace_model.ContextSwitch(
                start=_tp_ms(10.0),
                incoming_tid=1000,
                outgoing_tid=0,
                incoming_prio=trace_model.INT32_MIN,
                outgoing_prio=None,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                args={},
            ),
            trace_model.ContextSwitch(
                start=_tp_ms(15.0),
                incoming_tid=0,
                outgoing_tid=1000,
                incoming_prio=None,
                outgoing_prio=trace_model.INT32_MIN,
                outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
                args={},
            ),
        ]
        model = trace_model.Model(processes=[], scheduling_records={0: records})
        processor = scheduler.SchedulerMetricsProcessor(aggregates_only=False)
        self.assertEqual(processor.process_metrics(model), [])


if __name__ == "__main__":
    unittest.main()
