#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import bisect
import collections
import dataclasses
import itertools
import logging
import sys
from typing import Any, Iterable, Iterator, MutableSequence, Self, TypeAlias

from reporting import metrics
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

_LOGGER: logging.Logger = logging.getLogger(__name__)
_CPU_USAGE_EVENT_NAME = "cpu_usage"
_PROCESSING_RATE_EVENT_NAME = "Processing Rate"
# The kernel reports processing rates where 1000 represents 100% capacity.
# This constant is used as a default value when rate events are missing.
_DEFAULT_PROCESSING_RATE = 1000.0
_DEFAULT_PERCENT_CUTOFF = 0.0
_ONE_S_IN_NS = 1_000_000_000

Breakdown: TypeAlias = list[dict[str, metrics.JSON]]


@dataclasses.dataclass(frozen=True)
class ProcessingRateSample:
    """A single processing rate sample at a given timestamp."""

    timestamp_ms: float
    rate: float


@dataclasses.dataclass(frozen=True)
class VirtualProcessingSlice:
    """A slice with a constant processing rate."""

    duration: float
    rate: float


class CpuProcessingRateTimeline:
    """Encapsulates the processing rate shifts for a single CPU core."""

    def __init__(self, rates: Iterable[ProcessingRateSample]):
        """Constructor.

        Args:
            rates: An iterable of ProcessingRateSample objects.
        """
        self.rates = sorted(rates, key=lambda r: r.timestamp_ms)

    def get_virtual_slices(
        self, start_ts: float, stop_ts: float
    ) -> list[VirtualProcessingSlice]:
        """Splits a schedule slice into virtual slices based on rate changes.

        Args:
            start_ts: Start of the slice (ms).
            stop_ts: End of the slice (ms).

        Returns:
            A list of VirtualProcessingSlice objects.
        """
        if start_ts >= stop_ts:
            return []

        virtual_slices: list[VirtualProcessingSlice] = []
        curr_ts = start_ts

        idx = bisect.bisect_right(
            self.rates, curr_ts, key=lambda r: r.timestamp_ms
        )
        current_rate = (
            _DEFAULT_PROCESSING_RATE if idx == 0 else self.rates[idx - 1].rate
        )

        while curr_ts < stop_ts:
            next_change_ts = (
                self.rates[idx].timestamp_ms
                if idx < len(self.rates)
                else float("inf")
            )
            seg_end = min(stop_ts, next_change_ts)

            virtual_slices.append(
                VirtualProcessingSlice(
                    duration=seg_end - curr_ts,
                    rate=current_rate,
                )
            )

            curr_ts = seg_end
            if curr_ts >= next_change_ts:
                current_rate = self.rates[idx].rate
                idx += 1

        return virtual_slices


@dataclasses.dataclass
class ProcessingRateStats:
    """Aggregates durations spent at different processing rates."""

    duration_per_rate: dict[str, float] = dataclasses.field(
        default_factory=dict
    )

    def add(self, rate: float, duration: float) -> None:
        rate_str = str(int(rate))  # One of "1000", "798", "506", and "360"
        self.duration_per_rate[rate_str] = (
            self.duration_per_rate.get(rate_str, 0.0) + duration
        )


@dataclasses.dataclass
class ThreadCpuStats:
    """Encapsulates CPU usage statistics for a single thread on a single CPU."""

    duration: float = 0.0
    normalized_duration: float = 0.0
    rate_stats: ProcessingRateStats = dataclasses.field(
        default_factory=ProcessingRateStats
    )


class CpuMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes CPU utilization metrics, both structured and freeform.

    CPU load metrics are reported as a percentage of the total load across all cores, with 100%
    mapping to full utilization of every core simultaneously. Some tools will report a process using
    both cores on a dual core system as taking up 200% of CPU; that is not the case here. Depending
    on configuration, this module will report a time series of average utiliztion samples taken
    periodically for the duration of the test, or aggregate measurements-- such as min, max,
    average, and percentiles -- calculated over that data.

    Freeform CPU load metrics report per-core average load for every thread in every process on the
    system. These data are not suited for automated changepoint detection, and so are instead
    piped to a dashboard.
    """

    FREEFORM_METRICS_FILENAME = "cpu_breakdown"

    def __init__(
        self,
        aggregates_only: bool = True,
        percent_cutoff: float = _DEFAULT_PERCENT_CUTOFF,
    ):
        """Constructor.

        Args:
            aggregates_only: When True, generates CpuMin, CpuMax, CpuAverage and CpuP* (%iles).
                Otherwise generates CpuLoad metric with all cpu values.
            percent_cutoff: Any process that has CPU below this won't be listed in the CPU usage
                breakdown reported in freeform metrics.
        """
        self.aggregates_only: bool = aggregates_only
        self._percent_cutoff = percent_cutoff

    @property
    def event_patterns(self) -> set[str]:
        return {_CPU_USAGE_EVENT_NAME, f"{_PROCESSING_RATE_EVENT_NAME}.*"}

    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        all_events: Iterator[trace_model.Event] = model.all_events()
        cpu_usage_events: Iterable[
            trace_model.CounterEvent
        ] = trace_utils.filter_events(
            all_events,
            category="system_metrics",
            name=_CPU_USAGE_EVENT_NAME,
            type=trace_model.CounterEvent,
        )

        cpu_starts: list[float] = []
        cpu_percentages: list[float] = []

        # Parse the start time and percentage for each `Event`.
        for event in cpu_usage_events:
            cpu_starts.append(event.start.to_epoch_delta().to_nanoseconds())
            cpu_percentages.append(
                event.args.get("average_cpu_percentage") or 0
            )

        # The suspend times calculated from https://source.corp.google.com/h/turquoise-internal/turquoise/+/main:src/cobalt/bin/system-metrics/cpu_stats_fetcher_impl.cc
        # are offset by 1 event. Add that event in. It's approximately 1 s.
        cpu_durations: list[float] = [_ONE_S_IN_NS] + [
            curr - prev for prev, curr in itertools.pairwise(cpu_starts)
        ]

        if len(cpu_percentages) == 0:
            _LOGGER.info(
                "No cpu usage measurements are present. Perhaps the trace "
                "duration is too short to provide cpu usage information"
            )
            return []

        if len(cpu_durations) != len(cpu_percentages):
            _LOGGER.warning(
                "The number of CPU duration segments don't match the number "
                "of percentages. Data may be truncated."
            )

        if self.aggregates_only:
            return trace_utils.standard_metrics_set(
                values=cpu_percentages,
                label_prefix="Cpu",
                unit=metrics.Unit.percent,
                durations=cpu_durations,
            )
        return [
            metrics.TestCaseResult(
                "CpuLoad", metrics.Unit.percent, cpu_percentages
            )
        ]

    def process_freeform_metrics(
        self, model: trace_model.Model
    ) -> tuple[str, Breakdown]:
        """
        Given trace_model.Model, iterates through all the SchedulingRecords and calculates the
        duration for each Process's Threads, and saves them by CPU.

        Args:
            model: The input trace model.

        Returns:
            str: stable identifier to use in freeform metrics file name.
            Breakdown: Per-process, per-thread CPU usage breakdown.
        """
        (breakdown, _) = self.process_metrics_and_get_total_time(model)
        return self.FREEFORM_METRICS_FILENAME, breakdown

    def process_metrics_and_get_total_time(
        self, model: trace_model.Model
    ) -> tuple[Breakdown, float]:
        """
        Given trace_model.Model, iterates through all the SchedulingRecords and calculates the
        duration for each Process's Threads, and saves them by CPU.

        Args:
            model: The input trace model.

        Returns:
            Breakdown: Per-process, per-thread CPU usage breakdown.
            float: The total duration of the trace.
        """
        # Map tids to names.
        tid_to_process_name: dict[int, str] = {}
        tid_to_thread_name: dict[int, str] = {}
        for p in model.processes:
            for t in p.threads:
                tid_to_process_name[t.tid] = p.name
                tid_to_thread_name[t.tid] = t.name

        # Calculate durations for each CPU for each tid.
        durations = DurationsBreakdown.calculate(
            model.scheduling_records,
            tid_to_thread_name,
        )

        # Calculate the percent of time the thread spent on this CPU,
        # compared to the total CPU duration.
        # If the percent spent is at or above our cutoff, add metric to
        # breakdown.
        full_breakdown: list[dict[str, metrics.JSON]] = []
        for tid, cpu_stats_map in durations.tid_to_stats.items():
            if tid in tid_to_thread_name:
                for cpu, stats in cpu_stats_map.items():
                    duration = stats.duration
                    percent = (
                        duration / durations.cpu_to_total_duration[cpu] * 100
                        if durations.cpu_to_total_duration[cpu] > 0
                        else 0.0
                    )
                    if percent >= self._percent_cutoff:
                        metric: dict[str, metrics.JSON] = {
                            "process_name": tid_to_process_name[tid],
                            "thread_name": tid_to_thread_name[tid],
                            "tid": tid,
                            "cpu": cpu,
                            "percent": percent,
                            "duration": duration,
                        }
                        full_breakdown.append(metric)

        if durations.cpu_to_skipped_duration:
            _LOGGER.warning(
                "Possibly missing ContextSwitch record(s) in trace for these CPUs and durations: "
                f"{durations.cpu_to_skipped_duration}"
            )

        # Sort metrics by CPU (desc) and percent (desc).
        full_breakdown.sort(
            key=lambda m: (m["cpu"], m["percent"]),
            reverse=True,
        )
        return full_breakdown, durations.max_timestamp - durations.min_timestamp


class DurationsBreakdown:
    def __init__(self) -> None:
        # Maps TID to a dict of CPUs to stats on that CPU.
        self.tid_to_stats: collections.defaultdict[
            int, collections.defaultdict[int, ThreadCpuStats]
        ] = collections.defaultdict(
            lambda: collections.defaultdict(ThreadCpuStats)
        )
        # Map of CPU to total duration used (ms).
        self.cpu_to_total_duration: dict[int, float] = {}
        self.cpu_to_skipped_duration: dict[int, float] = {}
        self.min_timestamp: float = sys.float_info.max
        self.max_timestamp: float = 0

    def _calculate_duration_per_cpu(
        self,
        cpu: int,
        records: list[trace_model.ContextSwitch],
        tid_to_thread_name: dict[int, str],
    ) -> None:
        """
        Calculates the total duration for each thread, on a particular CPU.

        Uses a list of sorted ContextSwitch records to sum up the duration for each thread.
        It's possible that consecutive records do not have matching incoming_tid and outgoing_tid.
        """
        smallest_timestamp = self._timestamp_ms(records[0].start)
        if smallest_timestamp < self.min_timestamp:
            self.min_timestamp = smallest_timestamp
        largest_timestamp = self._timestamp_ms(records[-1].start)
        if largest_timestamp > self.max_timestamp:
            self.max_timestamp = largest_timestamp
        total_duration = largest_timestamp - smallest_timestamp
        skipped_duration = 0.0
        self.cpu_to_total_duration[cpu] = total_duration

        for prev_record, curr_record in itertools.pairwise(records):
            # Check that the previous ContextSwitch's incoming_tid ("this thread is starting work
            # on this CPU") matches the current ContextSwitch's outgoing_tid ("this thread is being
            # switched away from"). If so, there is a duration to calculate. Otherwise, it means
            # maybe there is skipped data or something.
            if prev_record.tid != curr_record.outgoing_tid:
                start_ts = self._timestamp_ms(prev_record.start)
                stop_ts = self._timestamp_ms(curr_record.start)
                skipped_duration += stop_ts - start_ts
            # Purposely skip saving idle thread durations.
            elif prev_record.is_idle():
                continue
            else:
                start_ts = self._timestamp_ms(prev_record.start)
                stop_ts = self._timestamp_ms(curr_record.start)
                duration = stop_ts - start_ts
                assert duration >= 0
                if curr_record.outgoing_tid in tid_to_thread_name:
                    # Add stats to the total duration for that tid and CPU.
                    cpu_stats = self.tid_to_stats[curr_record.outgoing_tid][cpu]
                    cpu_stats.duration += duration

        if skipped_duration > 0:
            self.cpu_to_skipped_duration[cpu] = skipped_duration

    @staticmethod
    def _timestamp_ms(timestamp: trace_time.TimePoint) -> float:
        """
        Return timestamp in ms.
        """
        return timestamp.to_epoch_delta().to_milliseconds_f()

    @classmethod
    def calculate(
        cls,
        per_cpu_scheduling_records: dict[
            int, list[trace_model.SchedulingRecord]
        ],
        tid_to_thread_name: dict[int, str],
    ) -> Self:
        durations = cls()
        for cpu, records in per_cpu_scheduling_records.items():
            durations._calculate_duration_per_cpu(
                cpu,
                sorted(
                    trace_utils.filter_records(
                        records, trace_model.ContextSwitch
                    ),
                    key=lambda record: record.start,
                ),
                tid_to_thread_name,
            )
        return durations


def group_by_process_name(breakdown: Breakdown) -> Breakdown:
    """
    Given a breakdown, group the metrics by process_name only,
    ignoring thread name.
    """
    breakdown.sort(key=lambda m: (m["cpu"], m["process_name"]))
    if not breakdown:
        return []
    consolidated_breakdown: Breakdown = [breakdown[0]]
    for metric in breakdown[1:]:
        if (
            metric["cpu"] == consolidated_breakdown[-1]["cpu"]
            and metric["process_name"]
            == consolidated_breakdown[-1]["process_name"]
        ):
            consolidated_breakdown[-1] = _merge(
                metric, consolidated_breakdown[-1]
            )
        else:
            metric.pop("thread_name", None)
            metric.pop("tid", None)
            consolidated_breakdown.append(metric)

    return sorted(
        consolidated_breakdown,
        key=lambda m: (m["cpu"], m["percent"]),
        reverse=True,
    )


def _merge(a: dict[str, Any], b: dict[str, Any]) -> dict[str, Any]:
    return {
        "process_name": a["process_name"],
        "cpu": a["cpu"],
        "duration": a["duration"] + b["duration"],
        "percent": a["percent"] + b["percent"],
    }
