#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""input trace metrics."""

import logging
import statistics
from typing import MutableSequence

from reporting import metrics
from trace_processing import trace_metrics, trace_model, trace_time, trace_utils

_LOGGER: logging.Logger = logging.getLogger("InputLatencyMetricsProcessor")
_CATEGORY_INPUT: str = "input"
_INPUT_EVENT_NAME: str = "input-device-process-reports"
_CATEGORY_GFX: str = "gfx"
_DISPLAY_VSYNC_EVENT_NAME: str = "Flatland::DisplayCompositor::OnVsync"


class InputLatencyMetricsProcessor(trace_metrics.MetricsProcessor):
    """Measures the time it takes for an input event to result in user-visible change on-screen.

    Calculates the time needed for an input event to progress through the through input pipeline to
    and result in a change on the screen at vsync. Flow events in the trace enable this class to
    reliably corellate input events to the first user-visible-output that results.

    By default, this module reports aggregate latency measurements -- such as min, max, average, and
    percentiles -- calculated across all input events generated during the test. It can be
    configured to instead report a time series of latency measurements, one for each input event.
    """

    def __init__(self, aggregates_only: bool = True):
        """Constructor.

        Args:
            aggregates_only: When True, generates InputLatencyMin,
                InputLatencyMax, InputLatencyAverage and
                InputLatencyP* (percentiles).
                Otherwise generates InputLatency metric with all
                InputLatency values.
        """
        self._aggregates_only: bool = aggregates_only

    @property
    def event_patterns(self) -> set[str]:
        """This processor follows a flow with many parts, so defer to category_names."""
        return set()

    @property
    def category_names(self) -> set[str]:
        """This processor follows a flow with many parts across `gfx` and `input`."""
        return {_CATEGORY_GFX, _CATEGORY_INPUT}

    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        input_events = trace_utils.filter_events(
            model.all_events(),
            category=_CATEGORY_INPUT,
            name=_INPUT_EVENT_NAME,
            type=trace_model.DurationEvent,
        )

        latencies: list[float] = []

        max_latency: float = -1.0
        max_latency_ts: trace_time.TimePoint | None = None

        for e in input_events:
            vsync = trace_utils.get_nearest_following_flow_event(
                e, _CATEGORY_GFX, _DISPLAY_VSYNC_EVENT_NAME
            )

            if vsync is None:
                continue

            latency = vsync.start - e.start
            latency_ms = latency.to_milliseconds_f()
            latencies.append(latency_ms)
            if latency_ms > max_latency:
                max_latency = latency_ms
                max_latency_ts = e.start

        if max_latency_ts is not None:
            _LOGGER.info(
                f"InputLatencyMax: {max_latency} ms at timestamp {max_latency_ts}"
            )

        latency_mean: float = statistics.mean(latencies)
        _LOGGER.info(f"Average Present Latency: {latency_mean}")

        if self._aggregates_only:
            return trace_utils.standard_metrics_set(
                values=latencies,
                label_prefix="InputLatency",
                unit=metrics.Unit.milliseconds,
            )
        return [
            metrics.TestCaseResult(
                "total_input_latency",
                metrics.Unit.milliseconds,
                latencies,
            ),
        ]
