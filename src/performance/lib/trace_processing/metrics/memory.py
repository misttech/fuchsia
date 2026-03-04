#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Computes metrics from memory traces."""

import collections
from dataclasses import dataclass
from typing import MutableSequence

from reporting import metrics
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

MEMORY_SYSTEM_CATEGORY = "memory:kernel"
KERNEL_EVENT_NAMES = (
    "kmem_stats_a",
    "kmem_stats_b",
    "kmem_stats_compression",
    "memory_stall",
)
# Name of the metric that are cumulative, monotonic counters, as opposed to gauges.
CUMULATIVE_METRIC_NAMES = {
    "compression_time",
    "decompression_time",
    "stall_time_some_ns",
    "stall_time_full_ns",
    "page_refaults",
}


@dataclass
class StructuredMetricName:
    structured_name: str
    unit: metrics.Unit


# Names and units of the metrics we will export as structured metrics.
# The key is the name of the metric in the trace, and the value is a StructuredMetricName object.
STRUCTURED_METRIC_NAMES = {
    "stall_time_some_ns": StructuredMetricName(
        "Memory/System/StallTimeSome", metrics.Unit.nanoseconds
    ),
    "stall_time_full_ns": StructuredMetricName(
        "Memory/System/StallTimeFull", metrics.Unit.nanoseconds
    ),
    "compression_time": StructuredMetricName(
        "Memory/System/CompressionTime", metrics.Unit.nanoseconds
    ),
    "decompression_time": StructuredMetricName(
        "Memory/System/DecompressionTime", metrics.Unit.nanoseconds
    ),
    "page_refaults": StructuredMetricName(
        "Memory/System/PageRefaults", metrics.Unit.count
    ),
    "total_heap_bytes": StructuredMetricName(
        "Memory/System/ZirconHeapBytes", metrics.Unit.bytes
    ),
}


def safe_divide(numerator: float, denominator: float) -> float | None:
    """Divides numerator by denominator, returning None if denominator is 0."""
    if denominator == 0:
        return None
    else:
        return numerator / denominator


def cumulative_metrics_value(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> tuple[int | float, float | None]:
    """Returns the change and the rate for the specified cumulative metric."""
    (t0, v0), (t1, v1) = values[0], values[-1]
    return (v1 - v0, safe_divide(v1 - v0, (t1 - t0).to_nanoseconds()))


def cumulative_metrics_json(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> metrics.JSON:
    """Returns a JSON object holding the change and the rate for the specified cumulative metric."""
    (delta, rate) = cumulative_metrics_value(values)
    return {
        "Delta": delta,
        "Rate": rate,
    }


def gauges_metrics_values(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> list[int | float]:
    """Returns a JSON object holding the standard metric value keyed by metric name."""
    return list(v[1] for v in values)


def gauges_metrics_json(
    values: list[tuple[trace_time.TimePoint, int | float]]
) -> metrics.JSON:
    """Returns a JSON object holding the standard metric value keyed by metric name."""
    results = trace_utils.standard_metrics_set(
        values=gauges_metrics_values(values),
        label_prefix="",
        unit=metrics.Unit.bytes,
    )
    return {result.label: result.values[0] for result in results}


class MemoryMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes statistics for values published by zircon kernel.

    Returns both freeform metrics and structured metrics.

    Freeform metrics is a JSON object with nested structure with the following path:

    "kernel" / field name / statistic label / float value

    Where:
        kernel: constant "kernel"
        field name: name of a field from `zx_info_kmem_stats_extended` and
            `zx_info_kmem_stats_compression` structures.
        statistic name: label of the statistic published by `trace_utils.standard_metrics_set`.
        float value: metric value.

    Sample:

    {
        "kernel": {
            "total_bytes": {
                "Min": 112
                "P5": 130
                [...]
            } ,
            [...]
        }
    }
    """

    @property
    def event_patterns(self) -> set[str]:
        """Patterns describing the trace events needed to generate these metrics."""
        return set(KERNEL_EVENT_NAMES)

    FREEFORM_METRICS_FILENAME = "memory"

    def process_freeform_metrics(
        self, model: trace_model.Model
    ) -> tuple[str, metrics.JSON]:
        series_by_name = collections.defaultdict(list)
        for event in trace_utils.filter_events(
            model.all_events(),
            category=MEMORY_SYSTEM_CATEGORY,
            type=trace_model.CounterEvent,
        ):
            if event.name in KERNEL_EVENT_NAMES:
                for name, value in event.args.items():
                    series_by_name[name].append((event.start, value))

        return (
            self.FREEFORM_METRICS_FILENAME,
            dict(
                kernel={
                    name: cumulative_metrics_json(series)
                    if name in CUMULATIVE_METRIC_NAMES
                    else gauges_metrics_json(series)
                    for name, series in series_by_name.items()
                }
            ),
        )

    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        series_by_name = collections.defaultdict(list)
        for event in trace_utils.filter_events(
            model.all_events(),
            category=MEMORY_SYSTEM_CATEGORY,
            type=trace_model.CounterEvent,
        ):
            if event.name in KERNEL_EVENT_NAMES:
                for name, value in event.args.items():
                    if name in STRUCTURED_METRIC_NAMES:
                        series_by_name[name].append((event.start, value))

        results = []
        for name, series in series_by_name.items():
            if name in CUMULATIVE_METRIC_NAMES:
                results.append(
                    metrics.TestCaseResult(
                        label=STRUCTURED_METRIC_NAMES[name].structured_name,
                        values=[cumulative_metrics_value(series)[0]],
                        unit=STRUCTURED_METRIC_NAMES[name].unit,
                    )
                )
            else:
                results.append(
                    metrics.TestCaseResult(
                        label=STRUCTURED_METRIC_NAMES[name].structured_name,
                        values=gauges_metrics_values(series),
                        unit=STRUCTURED_METRIC_NAMES[name].unit,
                    )
                )
        return results
