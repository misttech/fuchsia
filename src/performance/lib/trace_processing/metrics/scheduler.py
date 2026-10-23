#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Scheduler trace metrics."""

import dataclasses
import enum
import logging
import typing
from collections.abc import MutableSequence

from reporting import metrics
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

_LOGGER: logging.Logger = logging.getLogger("SchedulerMetricsProcessor")

_PREEMPTED_STATES: frozenset[trace_model.ThreadState] = frozenset(
    {
        trace_model.ThreadState.ZX_THREAD_STATE_NEW,
        trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
    }
)


class _WaitKind(enum.Enum):
    WAKEUP = 1
    PREEMPTION = 2


@dataclasses.dataclass(frozen=True)
class _PendingWait:
    kind: _WaitKind
    start: trace_time.TimePoint


class SchedulerMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes kernel scheduler task wait-time metrics (wakeup latency and preemption delay).

    Calculates system-wide distributions of:
    - SchedWakeupLatency: Time from a thread waking up (`Waking` record) until it is scheduled
      onto a CPU (`ContextSwitch` record).
    - SchedPreemptionDelay: Time a runnable thread that was preempted (`ContextSwitch` with
      `outgoing_state` indicating it remains runnable) waits before being scheduled back onto
      a CPU.
    - SchedTaskWaitTime: Combined distribution of runnable wait times (both wakeup latencies
      and preemption delays).

    Idle threads and synthetic priority-inheritance self-switches (`incoming_tid == outgoing_tid`)
    are excluded from all calculations.
    """

    def __init__(self, aggregates_only: bool = False) -> None:
        """Constructor.

        Args:
            aggregates_only: When True, generates Min, Max, Average, and P* (percentiles)
                via `standard_metrics_set`. Otherwise generates raw value series for
                SchedWakeupLatency, SchedPreemptionDelay, and SchedTaskWaitTime.
        """
        self._aggregates_only: bool = aggregates_only

    @property
    @typing.override
    def event_patterns(self) -> set[str]:
        """Scheduler records are always present in the model; no named events are required."""
        return set()

    @typing.override
    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        if not model.scheduling_records:
            _LOGGER.warning(
                "No scheduling records are present in the trace model."
            )
            return []

        # Seed with TID 0, which the FXT importer uses for synthetic idle states
        # (including a CPU's initial ContextSwitch where outgoing_prio is None).
        idle_tids: set[int] = {0}
        all_records: list[trace_model.SchedulingRecord] = []
        for records in model.scheduling_records.values():
            for record in records:
                if record.is_idle():
                    idle_tids.add(record.tid)
                if (
                    isinstance(record, trace_model.ContextSwitch)
                    and record.outgoing_prio == trace_model.INT32_MIN
                ):
                    idle_tids.add(record.outgoing_tid)
                all_records.append(record)

        # Sort chronologically across all CPUs. For identical timestamps, process Waking
        # records before ContextSwitch records so zero-latency wakeups are captured.
        all_records.sort(
            key=lambda r: (
                r.start,
                0 if isinstance(r, trace_model.Waking) else 1,
            )
        )

        pending_waits: dict[int, _PendingWait] = {}
        wakeup_latencies: list[float] = []
        preemption_delays: list[float] = []
        task_wait_times: list[float] = []

        for record in all_records:
            if isinstance(record, trace_model.Waking):
                if record.tid not in idle_tids:
                    pending_waits[record.tid] = _PendingWait(
                        kind=_WaitKind.WAKEUP, start=record.start
                    )
            elif isinstance(record, trace_model.ContextSwitch):
                # Ignore synthetic self-switches emitted during priority-inheritance updates.
                if record.tid == record.outgoing_tid:
                    continue

                in_tid = record.tid
                pending = pending_waits.pop(in_tid, None)
                if pending is not None and record.start >= pending.start:
                    wait_ms = (record.start - pending.start).to_milliseconds_f()
                    if pending.kind == _WaitKind.WAKEUP:
                        wakeup_latencies.append(wait_ms)
                    else:
                        preemption_delays.append(wait_ms)
                    task_wait_times.append(wait_ms)

                out_tid = record.outgoing_tid
                if out_tid not in idle_tids:
                    if record.outgoing_state in _PREEMPTED_STATES:
                        pending_waits[out_tid] = _PendingWait(
                            kind=_WaitKind.PREEMPTION, start=record.start
                        )
                    else:
                        # The thread blocked or terminated; discard any stale pending wait such as one left
                        # behind when its switch-in record was dropped.
                        pending_waits.pop(out_tid, None)

        metric_series: list[tuple[str, list[float], str]] = [
            (
                "SchedWakeupLatency",
                wakeup_latencies,
                "Time from thread wakeup until scheduled on a CPU",
            ),
            (
                "SchedPreemptionDelay",
                preemption_delays,
                "Time a preempted runnable thread waits until next scheduled on a CPU",
            ),
            (
                "SchedTaskWaitTime",
                task_wait_times,
                "Total runnable wait time (wakeup latency and preemption delay) before being scheduled on a CPU",
            ),
        ]

        results: list[metrics.TestCaseResult] = []
        for label_prefix, values, doc_prefix in metric_series:
            if not values:
                _LOGGER.info(f"No {label_prefix} samples found in trace.")
                continue
            if self._aggregates_only:
                results.extend(
                    trace_utils.standard_metrics_set(
                        values=values,
                        label_prefix=label_prefix,
                        unit=metrics.Unit.milliseconds,
                        doc_prefix=doc_prefix,
                    )
                )
            else:
                results.append(
                    metrics.TestCaseResult(
                        label=label_prefix,
                        unit=metrics.Unit.milliseconds,
                        values=values,
                        doc=doc_prefix,
                    )
                )

        return results
