#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import collections
from collections.abc import Mapping, Sequence
from typing import ReadOnly, TypedDict

from trace_processing.metrics import cpu

# Default cut-off for the percentage CPU. Any process that has CPU below this
# won't be listed in the results. User can pass in a cutoff.
DEFAULT_PERCENT_CUTOFF = 0.0


class AggregateRecord(TypedDict):
    process_name: ReadOnly[str]
    thread_name: ReadOnly[str]
    duration: ReadOnly[float]
    percent: ReadOnly[float]


class AggCpuBreakdownMetricsProcessor:
    """
    Aggregates a given breakdown over the cores for each available frequency,
    and outputs in a free-form metrics format.
    """

    def __init__(
        self,
        # A map from cpu numbers to their frequency.
        # e.g. { 0: 1.8, 1: 1.8, 2: 2.2, 3: 2.2, 4: : 2.2, 5: 2.2 }
        cpu_to_freq: Mapping[int, float],
        total_time: float,
        percent_cutoff: float = DEFAULT_PERCENT_CUTOFF,
    ):
        # Transforms the frequency config to a map from cpu to frequency. Used
        # to determine which frequency each record should contribute its duration to.
        self._cpu_to_freq = cpu_to_freq
        self._percent_cutoff = percent_cutoff
        self._total_time = total_time

    def aggregate_metrics(
        self, breakdown: cpu.ThreadBreakdown
    ) -> Mapping[float, Sequence[AggregateRecord]]:
        """
        Given the breakdown of duration per thread, iterates through all the threads' durations for each
        CPU and aggregates them over each CPU frequency.

        Args:
            breakdown: The per-thread CPU breakdown metrics.
        """
        # Map from frequency to tid to aggregated duration.
        freq_to_tid_to_durs: collections.defaultdict[
            float, collections.defaultdict[int, float]
        ] = collections.defaultdict(lambda: collections.defaultdict(float))
        # Tracks the final output. Contains a map from frequency to list of
        # threads with their durations and percentages.
        agg_breakdown: dict[float, Sequence[AggregateRecord]] = {}
        tid_to_thread_name: dict[int, str] = {}
        tid_to_process_name: dict[int, str] = {}
        for thread_metric in breakdown:
            # Save process and thread name for tid
            tid = thread_metric["tid"]
            tid_to_process_name[tid] = thread_metric["process_name"]
            tid_to_thread_name[tid] = thread_metric["thread_name"]

            # Get frequency for the cpu
            freq = self._cpu_to_freq[thread_metric["cpu"]]

            # Add the duration to the tid
            duration = thread_metric["duration"]
            freq_to_tid_to_durs[freq][tid] += duration

        for freq, tid_to_durs in freq_to_tid_to_durs.items():
            dur_list: list[AggregateRecord] = []
            for tid, dur in tid_to_durs.items():
                percent = (dur / self._total_time) * 100
                if percent >= self._percent_cutoff:
                    dur_list.append(
                        AggregateRecord(
                            {
                                "process_name": tid_to_process_name[tid],
                                "thread_name": tid_to_thread_name[tid],
                                "duration": round(dur, 3),
                                "percent": round(percent, 3),
                            }
                        )
                    )
            agg_breakdown[freq] = sorted(
                dur_list,
                key=lambda m: (m["duration"]),
                reverse=True,
            )
        return agg_breakdown
