# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Binder delay and missed wakeup analysis plugin.

Analysis plugin that identifies potential Binder transaction delays and missed wakeups.

Binder is the name for interprocess-communication in Android. These calls are handled by
"binder threads". The binder threads are identified by having slices with the category
"starnix:binder" or the name "binder_ioctl".

Bottlenecks on these threads will cause slowdowns in performance.

A "bottleneck" is when a thread is in the runnable state, but is not run on the CPU due to scheduling constraints, or
when a slice is longer than some cut-off (e.g. 10ms) indicating it is blocked by another thread or resource.

See: go/systemperf-perfetto-queries#binder for additional info.
"""

import math
from collections.abc import Set
from typing import Sequence

from plugins import (
    AnalyzePlugin,
    PluginArgumentError,
    PluginArgumentParser,
    SectionResult,
)
from tp_shell import PerfettoTraceProcessor


def _analyze_missed_wakeups(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    threshold_ns: int,
) -> SectionResult:
    """Analyzes thread runnable state durations to detect scheduling delays and missed wakeups."""
    required_tables = {
        "thread_state",
        "slice",
        "thread_track",
        "thread",
        "process",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name="Missed Wakeups (Wakeup Latencies)",
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
        )

    query = f"""
    WITH binder_threads AS (
      SELECT DISTINCT utid
      FROM slice
      JOIN thread_track ON slice.track_id = thread_track.id
      WHERE slice.category = 'starnix:binder'
         OR slice.name = 'binder_ioctl'
    )
    SELECT
      ts.ts as wakeup_ts,
      ts.dur as scheduling_delay_ns,
      t.name as thread_name,
      t.tid as tid,
      p.name as process_name,
      p.pid as pid,
      waker.name as waker_thread_name,
      waker.tid as waker_tid
    FROM thread_state ts
    JOIN binder_threads USING (utid)
    JOIN thread t USING (utid)
    LEFT JOIN process p USING (upid)
    LEFT JOIN thread waker ON ts.waker_utid = waker.utid
    WHERE ts.state LIKE 'R%' AND ts.dur > {threshold_ns}
    ORDER BY ts.dur DESC
    """
    try:
        missed_wakeups = tp.run_query(query)
        return SectionResult(
            name="Missed Wakeups (Wakeup Latencies)",
            results=missed_wakeups,
        )
    except Exception as e:
        return SectionResult(
            name="Missed Wakeups (Wakeup Latencies)",
            error=f"Query execution failed: {e}",
        )


def _analyze_binder_delays(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    threshold_ns: int,
    complete_only: bool,
) -> SectionResult:
    """Analyzes flow slice latencies to identify delayed or incomplete binder transactions."""
    required_tables = {
        "flow",
        "slice",
        "thread_track",
        "thread",
        "args",
        "trace_bounds",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name="Binder Delays (Transaction Queue Latencies)",
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
        )

    completed_query = f"""
        SELECT
          flow.id as flow_id,
          s_out.ts as send_ts,
          s_in.ts as recv_ts,
          (s_in.ts - s_out.ts) as queue_latency_ns,
          t_out.name as sender_thread,
          t_out.tid as sender_tid,
          t_in.name as receiver_thread,
          t_in.tid as receiver_tid,
          'Completed' as status,
          (SELECT string_value FROM args WHERE arg_set_id = s_out.arg_set_id AND key = 'cmd') as cmd
        FROM flow
        JOIN slice s_out ON flow.slice_out = s_out.id
        JOIN slice s_in ON flow.slice_in = s_in.id
        JOIN thread_track track_out ON s_out.track_id = track_out.id
        JOIN thread t_out ON track_out.utid = t_out.utid
        JOIN thread_track track_in ON s_in.track_id = track_in.id
        JOIN thread t_in ON track_in.utid = t_in.utid
        WHERE s_out.category = 'starnix:binder'
          AND (s_in.ts - s_out.ts) > {threshold_ns}
        """

    if complete_only:
        query = f"{completed_query}\n        ORDER BY queue_latency_ns DESC"
    else:
        incomplete_query = f"""
        SELECT
          NULL as flow_id,
          s.ts as send_ts,
          NULL as recv_ts,
          ((SELECT end_ts FROM trace_bounds) - s.ts) as queue_latency_ns,
          t.name as sender_thread,
          t.tid as sender_tid,
          NULL as receiver_thread,
          NULL as receiver_tid,
          'Incomplete' as status,
          (SELECT string_value FROM args WHERE arg_set_id = s.arg_set_id AND key = 'cmd') as cmd
        FROM slice s
        JOIN thread_track tr ON s.track_id = tr.id
        JOIN thread t USING(utid)
        WHERE s.category = 'starnix:binder'
          AND s.name = 'Transaction'
          AND s.id NOT IN (SELECT slice_out FROM flow WHERE slice_out IS NOT NULL)
          AND s.id NOT IN (SELECT slice_in FROM flow WHERE slice_in IS NOT NULL)
          AND ((SELECT end_ts FROM trace_bounds) - s.ts) > {threshold_ns}
        """
        query = f"{completed_query}\n        UNION ALL\n{incomplete_query}\n        ORDER BY queue_latency_ns DESC"

    try:
        binder_delays = tp.run_query(query)
        return SectionResult(
            name="Binder Delays (Transaction Queue Latencies)",
            results=binder_delays,
        )
    except Exception as e:
        return SectionResult(
            name="Binder Delays (Transaction Queue Latencies)",
            error=f"Query execution failed: {e}",
        )


def _analyze_spawn_loopers(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    threshold_ns: int,
) -> SectionResult:
    """Analyzes SpawnLooper commands to identify late-spawned binder waker threads."""
    required_tables = {
        "flow",
        "slice",
        "thread_track",
        "thread",
        "args",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name="Spawn Looper Events (Late-Spawned Wakers)",
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
        )

    query = f"""
    SELECT
      flow.id as flow_id,
      s_out.ts as trigger_ts,
      s_in.ts as handle_ts,
      (s_in.ts - s_out.ts) as spawn_latency_ns,
      t_in.name as handle_thread,
      t_in.tid as handle_tid
    FROM flow
    JOIN slice s_out ON flow.slice_out = s_out.id
    JOIN slice s_in ON flow.slice_in = s_in.id
    JOIN thread_track track_in ON s_in.track_id = track_in.id
    JOIN thread t_in ON track_in.utid = t_in.utid
    WHERE s_out.category = 'starnix:binder'
      AND (s_in.ts - s_out.ts) > {threshold_ns}
      AND (SELECT string_value FROM args WHERE arg_set_id = s_out.arg_set_id AND key = 'cmd') LIKE '%SpawnLooper%'
    ORDER BY spawn_latency_ns DESC
    """
    try:
        spawn_loopers = tp.run_query(query)
        return SectionResult(
            name="Spawn Looper Events (Late-Spawned Wakers)",
            results=spawn_loopers,
        )
    except Exception as e:
        return SectionResult(
            name="Spawn Looper Events (Late-Spawned Wakers)",
            error=f"Query execution failed: {e}",
        )


class BinderPlugin(AnalyzePlugin):
    name: str = "binder"
    description: str = "Analyze Starnix binder delays and missed wakeups"

    def analyze(
        self,
        remaining_args: Sequence[str],
        trace_path: str,
        cache: bool = True,
    ) -> Sequence[SectionResult]:
        parser = PluginArgumentParser(
            prog=f"perf-analyze analyze --plugin {self.name}"
        )
        parser.add_argument(
            "--threshold-ms",
            type=float,
            default=10.0,
            help="Threshold for scheduling delay and queue latency in milliseconds (default: 10.0)",
        )
        parser.add_argument(
            "--complete-only",
            action="store_true",
            help="Only return complete transactions (default: False, includes incomplete)",
        )
        args = parser.parse_args(remaining_args)

        if not math.isfinite(args.threshold_ms) or args.threshold_ms < 0.0:
            raise PluginArgumentError(
                "Latency threshold must be a finite, non-negative number."
            )

        threshold_ns = int(args.threshold_ms * 1_000_000)

        with PerfettoTraceProcessor(trace_path, cache=cache) as tp:
            db_objects = tp.get_tables()
            return [
                _analyze_missed_wakeups(tp, db_objects, threshold_ns),
                _analyze_binder_delays(
                    tp, db_objects, threshold_ns, args.complete_only
                ),
                _analyze_spawn_loopers(tp, db_objects, threshold_ns),
            ]
