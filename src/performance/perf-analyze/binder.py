# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Starnix binder delays, missed wakeups, and thread pool analysis plugin.

This plugin audits:
1. Thread wakeup scheduling delays (Runnable state duration) for binder worker threads.
2. Delayed and incomplete binder transactions, split into queue, server, and reply delivery time.
3. SpawnLooper instant events indicating binder waker threads spawned during high contention.
4. no_available_threads instant events indicating binder thread pool exhaustion.
5. process_queue_depth counters measuring transaction queue backlog.

Starnix binderfs emits neither of the last two, so those sections are empty on Fuchsia.

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

NOT_EMITTED_BY_STARNIX_NOTE: str = (
    "Starnix binderfs (src/starnix/modules/binderfs) does not emit {event}, so this section "
    "is empty for Fuchsia traces."
)

CATEGORY_NOT_FOUND_NOTE: str = (
    "The 'starnix:binder' category was not found in this trace. "
    "Fell back to 'binder_ioctl' and 'binder:%' thread name heuristics. "
    "Including 'starnix:binder' in the trace will strengthen the binder performance analysis "
    "with transaction queue latencies, waker milestones, and thread pool exhaustion events."
)


def _has_starnix_binder_category(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
) -> bool:
    """Checks whether the trace contains slices with the 'starnix:binder' category."""
    if "slice" not in db_objects:
        return False
    try:
        rows = tp.run_query(
            "SELECT 1 FROM slice WHERE category = 'starnix:binder' LIMIT 1"
        )
        return len(rows) > 0
    except Exception:
        return False


def _analyze_missed_wakeups(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    threshold_ns: int,
    has_category: bool = True,
) -> SectionResult:
    """Analyzes thread runnable state durations to detect scheduling delays and missed wakeups."""
    name = "Missed Wakeups (Wakeup Latencies)"
    note = CATEGORY_NOT_FOUND_NOTE if not has_category else None
    required_tables = {
        "thread_state",
        "slice",
        "thread_track",
        "thread",
        "process",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
            note=note,
        )

    if has_category:
        binder_threads_cte = """
        WITH binder_threads AS (
          SELECT DISTINCT utid
          FROM slice
          JOIN thread_track ON slice.track_id = thread_track.id
          WHERE slice.category = 'starnix:binder'
             OR slice.name = 'binder_ioctl'
        )
        """
    else:
        binder_threads_cte = """
        WITH binder_threads AS (
          SELECT DISTINCT utid
          FROM thread
          WHERE name LIKE 'binder:%' OR name LIKE 'binder_%'
          UNION
          SELECT DISTINCT utid
          FROM slice
          JOIN thread_track ON slice.track_id = thread_track.id
          WHERE slice.name = 'binder_ioctl'
        )
        """

    # Linux traces record runnable time as 'R'/'R+'. Fuchsia (FXT) traces record a woken
    # thread as 'W' until it runs, with the waker on that row, and use 'R' only for a
    # preempted thread, which has no waker.
    query = f"""
    {binder_threads_cte}
    SELECT
      ts.ts as wakeup_ts,
      ts.dur as scheduling_delay_ns,
      ts.state as state,
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
    WHERE ts.state IN ('R', 'R+', 'W') AND ts.dur > {threshold_ns}
    ORDER BY ts.dur DESC
    """
    try:
        missed_wakeups = tp.run_query(query)
        return SectionResult(
            name=name,
            results=missed_wakeups,
            note=note,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
            note=note,
        )


def _starnix_binder_delays_query(threshold_ns: int, complete_only: bool) -> str:
    """Returns the Binder Delays query for traces with the 'starnix:binder' category.

    Starnix binderfs attaches its flows to zero-duration `enqueue_*`/`dequeue_*` instants
    inside the `HandleTransaction`, `HandleThreadRead`, and `HandleReply` slices (see
    src/starnix/modules/binderfs/trace.rs). A two-way transaction is the flow chain
    `enqueue_transaction` (client) -> `dequeue_transaction` (server) -> `enqueue_reply`
    (server) -> `dequeue_reply` (client), and a oneway transaction is `enqueue_oneway` ->
    `dequeue_oneway`. Each row is one transaction, with:
      - queue_ns: enqueue to dequeue by the server.
      - server_ns: server dequeue to its reply (two-way only); this is the server's own
        work on one thread, not queueing.
      - reply_ns: reply enqueue to dequeue by the client (two-way only).
    A step missing at the end of the trace makes the transaction incomplete; the stage it
    is stuck in is measured to the end of the trace. Rows are kept if any stage exceeds
    the threshold, and are ordered by transport time (queue plus reply delivery).
    """
    complete_filter = ""
    if complete_only:
        complete_filter = """
          AND ((kind = 'oneway' AND dequeue_ts IS NOT NULL)
               OR reply_dequeue_ts IS NOT NULL)"""
    return f"""
    WITH
      steps AS MATERIALIZED (
        SELECT s.id, s.ts, s.name, s.arg_set_id, tt.utid
        FROM slice s
        JOIN thread_track tt ON s.track_id = tt.id
        WHERE s.category = 'starnix:binder'
          AND (s.name GLOB 'enqueue_*' OR s.name GLOB 'dequeue_*')
      ),
      next_step AS (
        SELECT f.slice_out AS from_id, st.*
        FROM flow f
        JOIN steps st ON st.id = f.slice_in
      ),
      txns AS (
        SELECT
          CASE WHEN e.name = 'enqueue_oneway' THEN 'oneway' ELSE 'two-way' END AS kind,
          e.ts AS enqueue_ts,
          e.utid AS client_utid,
          e.arg_set_id AS arg_set_id,
          d.ts AS dequeue_ts,
          d.utid AS server_utid,
          r.ts AS reply_ts,
          r.name AS reply_name,
          c.ts AS reply_dequeue_ts
        FROM steps e
        LEFT JOIN next_step d
          ON d.from_id = e.id AND d.name IN ('dequeue_transaction', 'dequeue_oneway')
        -- A reply follows the server's dequeue, or the client's enqueue when the driver
        -- fails the transaction before any server dequeues it.
        LEFT JOIN next_step r
          ON e.name = 'enqueue_transaction'
          AND r.from_id = COALESCE(d.id, e.id)
          AND r.name GLOB 'enqueue_*reply'
        LEFT JOIN next_step c
          ON c.from_id = r.id AND c.name GLOB 'dequeue_*reply'
        WHERE e.name IN ('enqueue_transaction', 'enqueue_oneway')
      ),
      stages AS (
        SELECT
          txns.*,
          (SELECT end_ts FROM trace_bounds) AS end_ts,
          CASE
            WHEN reply_name IS NOT NULL AND reply_name != 'enqueue_reply'
              THEN SUBSTR(reply_name, 9)
            WHEN dequeue_ts IS NULL THEN 'Incomplete (queued)'
            WHEN kind = 'oneway' THEN 'Completed'
            WHEN reply_ts IS NULL THEN 'Incomplete (server)'
            WHEN reply_dequeue_ts IS NULL THEN 'Incomplete (reply)'
            ELSE 'Completed'
          END AS status
        FROM txns
      ),
      timed AS (
        SELECT
          stages.*,
          COALESCE(dequeue_ts, CASE WHEN reply_ts IS NULL THEN end_ts END) - enqueue_ts
            AS queue_ns,
          CASE
            WHEN kind = 'two-way' AND dequeue_ts IS NOT NULL
              THEN COALESCE(reply_ts, end_ts) - dequeue_ts
          END AS server_ns,
          CASE
            WHEN reply_ts IS NOT NULL THEN COALESCE(reply_dequeue_ts, end_ts) - reply_ts
          END AS reply_ns
        FROM stages
      ),
      slow AS (
        SELECT *
        FROM timed
        WHERE MAX(COALESCE(queue_ns, 0), COALESCE(server_ns, 0), COALESCE(reply_ns, 0))
            > {threshold_ns}{complete_filter}
      )
    SELECT
      slow.enqueue_ts AS send_ts,
      slow.kind,
      slow.status,
      slow.queue_ns,
      slow.server_ns,
      slow.reply_ns,
      COALESCE(
        CASE WHEN slow.kind = 'oneway' THEN slow.dequeue_ts ELSE slow.reply_dequeue_ts END,
        slow.end_ts
      ) - slow.enqueue_ts AS total_ns,
      tc.name AS client_thread,
      tc.tid AS client_tid,
      pc.name AS client_process,
      ts_.name AS server_thread,
      ts_.tid AS server_tid,
      ps.name AS server_process,
      COALESCE(
        (
          -- The server's AIDL method runs on the server thread after it dequeues the
          -- transaction, and before its reply or its next dequeue.
          SELECT a.name
          FROM slice a
          JOIN thread_track att ON a.track_id = att.id
          WHERE att.utid = slow.server_utid
            AND a.name GLOB 'AIDL::*::server'
            AND a.ts >= slow.dequeue_ts
            AND a.ts <= COALESCE(
              slow.reply_ts,
              (
                SELECT MIN(n.ts)
                FROM steps n
                WHERE n.utid = slow.server_utid
                  AND n.name GLOB 'dequeue_*'
                  AND n.ts > slow.dequeue_ts
              ),
              slow.end_ts
            )
          ORDER BY a.ts
          LIMIT 1
        ),
        (SELECT 'code: ' || int_value FROM args WHERE arg_set_id = slow.arg_set_id AND key = 'code')
      ) AS method
    FROM slow
    JOIN thread tc ON tc.utid = slow.client_utid
    LEFT JOIN process pc ON pc.upid = tc.upid
    LEFT JOIN thread ts_ ON ts_.utid = slow.server_utid
    LEFT JOIN process ps ON ps.upid = ts_.upid
    ORDER BY COALESCE(slow.queue_ns, 0) + COALESCE(slow.reply_ns, 0) DESC, slow.server_ns DESC
    """


def _analyze_binder_delays(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    threshold_ns: int,
    complete_only: bool,
    has_category: bool = True,
) -> SectionResult:
    """Analyzes binder transaction latencies to identify delayed or incomplete transactions."""
    name = "Binder Delays (Transaction Queue Latencies)"
    note = CATEGORY_NOT_FOUND_NOTE if not has_category else None
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
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
            note=note,
        )

    if has_category:
        query = _starnix_binder_delays_query(threshold_ns, complete_only)
        try:
            binder_delays = tp.run_query(query)
            return SectionResult(name=name, results=binder_delays, note=note)
        except Exception as e:
            return SectionResult(
                name=name, error=f"Query execution failed: {e}", note=note
            )

    completed_filter = f"""
    WHERE (s_out.name = 'binder_ioctl'
           OR t_out.name LIKE 'binder:%' OR t_out.name LIKE 'binder_%'
           OR t_in.name LIKE 'binder:%' OR t_in.name LIKE 'binder_%')
      AND (s_in.ts - s_out.ts) > {threshold_ns}
    """
    incomplete_filter = f"""
    WHERE (s.name = 'binder_ioctl' OR s.name = 'Transaction'
           OR t.name LIKE 'binder:%' OR t.name LIKE 'binder_%')
      AND s.id NOT IN (SELECT slice_out FROM flow WHERE slice_out IS NOT NULL)
      AND s.id NOT IN (SELECT slice_in FROM flow WHERE slice_in IS NOT NULL)
      AND ((SELECT end_ts FROM trace_bounds) - s.ts) > {threshold_ns}
    """

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
          COALESCE(
            (SELECT string_value FROM args WHERE arg_set_id = s_out.arg_set_id AND key = 'cmd'),
            (SELECT 'code: ' || int_value FROM args WHERE arg_set_id = s_out.arg_set_id AND key = 'code'),
            s_out.name
          ) as cmd
        FROM flow
        JOIN slice s_out ON flow.slice_out = s_out.id
        JOIN slice s_in ON flow.slice_in = s_in.id
        JOIN thread_track track_out ON s_out.track_id = track_out.id
        JOIN thread t_out ON track_out.utid = t_out.utid
        JOIN thread_track track_in ON s_in.track_id = track_in.id
        JOIN thread t_in ON track_in.utid = t_in.utid
        {completed_filter}
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
          COALESCE(
            (SELECT string_value FROM args WHERE arg_set_id = s.arg_set_id AND key = 'cmd'),
            (SELECT 'code: ' || int_value FROM args WHERE arg_set_id = s.arg_set_id AND key = 'code'),
            s.name
          ) as cmd
        FROM slice s
        JOIN thread_track tr ON s.track_id = tr.id
        JOIN thread t USING(utid)
        {incomplete_filter}
        """
        query = f"{completed_query}\n        UNION ALL\n{incomplete_query}\n        ORDER BY queue_latency_ns DESC"

    try:
        binder_delays = tp.run_query(query)
        return SectionResult(
            name=name,
            results=binder_delays,
            note=note,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
            note=note,
        )


def _analyze_spawn_loopers(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    has_category: bool = True,
) -> SectionResult:
    """Analyzes SpawnLooper instant events to identify thread pool exhaustion and late-spawned wakers."""
    name = "Spawn Looper Events (Late-Spawned Wakers)"
    note = CATEGORY_NOT_FOUND_NOTE if not has_category else None
    required_tables = {
        "slice",
        "thread_track",
        "thread",
        "process",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
            note=note,
        )

    if not has_category:
        return SectionResult(
            name=name,
            results=[],
            note=note,
        )

    query = """
    SELECT
      s.ts AS ts,
      p.name AS process_name,
      p.pid AS pid,
      t.name AS thread_name,
      t.tid AS tid,
      parent.name AS parent_slice_name,
      parent.dur AS parent_dur_ns
    FROM slice s
    JOIN thread_track tt ON s.track_id = tt.id
    JOIN thread t ON tt.utid = t.utid
    JOIN process p ON t.upid = p.upid
    LEFT JOIN slice parent ON s.parent_id = parent.id
    WHERE s.category = 'starnix:binder' AND s.name = 'SpawnLooper'
    ORDER BY s.ts ASC
    """
    try:
        spawn_loopers = tp.run_query(query)
        return SectionResult(
            name=name,
            results=spawn_loopers,
            note=note,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
            note=note,
        )


def _analyze_no_available_threads(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    has_category: bool = True,
) -> SectionResult:
    """Analyzes no_available_threads instant events indicating binder thread pool exhaustion."""
    name = "No Available Threads Events (Thread Pool Exhaustion)"
    note = (
        NOT_EMITTED_BY_STARNIX_NOTE.format(
            event="'no_available_threads' events"
        )
        if has_category
        else CATEGORY_NOT_FOUND_NOTE
    )
    required_tables = {
        "slice",
        "thread_track",
        "thread",
        "process",
        "args",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
            note=note,
        )

    if not has_category:
        return SectionResult(
            name=name,
            results=[],
            note=note,
        )

    if "process_track" in db_objects:
        process_join = """
        LEFT JOIN process_track pt ON s.track_id = pt.id
        LEFT JOIN process p_proc ON pt.upid = p_proc.upid
        """
        proc_name_expr = "COALESCE(p_proc.name, p_thread.name)"
        pid_expr = "COALESCE(p_proc.pid, p_thread.pid)"
    else:
        process_join = ""
        proc_name_expr = "p_thread.name"
        pid_expr = "p_thread.pid"

    query = f"""
    SELECT
      s.ts AS ts,
      {proc_name_expr} AS process_name,
      {pid_expr} AS pid,
      (SELECT int_value FROM args WHERE arg_set_id = s.arg_set_id AND key = 'target_pid') AS target_pid,
      (SELECT string_value FROM args WHERE arg_set_id = s.arg_set_id AND key = 'command') AS command
    FROM slice s
    {process_join}
    LEFT JOIN thread_track tt ON s.track_id = tt.id
    LEFT JOIN thread t ON tt.utid = t.utid
    LEFT JOIN process p_thread ON t.upid = p_thread.upid
    WHERE s.category = 'starnix:binder' AND s.name = 'no_available_threads'
    ORDER BY s.ts ASC
    """
    try:
        events = tp.run_query(query)
        return SectionResult(
            name=name,
            results=events,
            note=note,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
            note=note,
        )


def _analyze_process_queue_depth(
    tp: PerfettoTraceProcessor,
    db_objects: Set[str],
    has_category: bool = True,
) -> SectionResult:
    """Analyzes process_queue_depth counter tracks to quantify transaction queue backlog."""
    name = "Binder Process Queue Depth (Counter Summary)"
    note = (
        NOT_EMITTED_BY_STARNIX_NOTE.format(
            event="a 'process_queue_depth' counter"
        )
        if has_category
        else CATEGORY_NOT_FOUND_NOTE
    )
    required_tables = {
        "counter",
        "process_counter_track",
        "process",
    }
    if missing := required_tables - db_objects:
        return SectionResult(
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
            note=note,
        )

    if not has_category:
        return SectionResult(
            name=name,
            results=[],
            note=note,
        )

    query = """
    SELECT
      p.name AS process_name,
      p.pid AS pid,
      MAX(c.value) AS max_queue_depth,
      AVG(c.value) AS avg_queue_depth,
      COUNT(*) AS sample_count
    FROM counter c
    JOIN process_counter_track pct ON c.track_id = pct.id
    JOIN process p ON pct.upid = p.upid
    WHERE pct.name = 'process_queue_depth'
    GROUP BY p.name, p.pid
    HAVING MAX(c.value) > 0
    ORDER BY max_queue_depth DESC
    """
    try:
        queue_depths = tp.run_query(query)
        return SectionResult(
            name=name,
            results=queue_depths,
            note=note,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
            note=note,
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
            has_category = _has_starnix_binder_category(tp, db_objects)

            results: list[SectionResult] = []
            if not has_category:
                results.append(
                    SectionResult(
                        name="Trace Category Status",
                        note=CATEGORY_NOT_FOUND_NOTE,
                        results=[
                            {
                                "category": "starnix:binder",
                                "status": "Not Found (using fallback heuristics)",
                                "recommendation": (
                                    "Including 'starnix:binder' in the trace will"
                                    " strengthen binder performance analysis."
                                ),
                            }
                        ],
                    )
                )

            results.extend(
                [
                    _analyze_missed_wakeups(
                        tp, db_objects, threshold_ns, has_category
                    ),
                    _analyze_binder_delays(
                        tp,
                        db_objects,
                        threshold_ns,
                        args.complete_only,
                        has_category,
                    ),
                    _analyze_spawn_loopers(tp, db_objects, has_category),
                    _analyze_no_available_threads(tp, db_objects, has_category),
                    _analyze_process_queue_depth(tp, db_objects, has_category),
                ]
            )
            return results
