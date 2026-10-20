# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""App launch critical path phase breakdown and diagnostic analysis plugin.

This plugin divides the critical path of an Android/Starnix application launch
into distinct, contiguous chronological segments across processes and threads:
  S1 (Pre-Fork Delay): Launch intent start -> Start proc in system_server
  S2a (Process Fork): Start proc -> Child process PostFork / ActivityThreadMain
  S2b (Attach Application): PostFork -> bindApplication start on app main thread
  S3 (Bind Application): bindApplication duration (DEX load, dlopen, Providers, Application.onCreate)
  S4 (Activity Lifecycle): bindApplication end -> First Choreographer#doFrame start
  S5 (First Frame UI): First Choreographer#doFrame start -> First RenderThread DrawFrame start
  S6 (First Frame Render & GPU): First RenderThread DrawFrame start -> Launch completion (TTID)
  S7 (Display Present Tail): Launch completion (TTID) -> Fuchsia Display::Client::CommitConfig

Warm and hot launches have no S1-S3. For them, a Pre-Activity segment covers the launch
start -> first activityStart/activityResume on the app main thread, and S4 starts there.

It also provides:
  - CUJ sub-stage breakdown (init, draw, render, traversal, animation, scenic), counting
    the self time of nested slices so that no time is counted twice on a thread.
  - Intra-segment sub-operation breakdowns (OpenDexFilesFromOat, dlopen, VerifyClass, inflate,
    dequeueBuffer, shader_compile, QueuePresentKHR).
  - Per-segment thread state breakdowns (Running on-CPU vs. Runnable runqueue starvation vs.
    Sleeping/Blocked on IPC, locks, or I/O).
  - Cross-layer contention and Fuchsia subsystem attribution (Binder IPC, monitor locks,
    native mutexes, Fxfs storage page-ins, Sysmem buffer allocations, and Starnix SELinux hooks).
"""

from typing import Sequence

from plugins import (
    AnalyzePlugin,
    PluginArgumentError,
    PluginArgumentParser,
    SectionResult,
)
from tp_shell import PerfettoTraceProcessor


def _perform_analysis_query(
    name: str,
    tp: PerfettoTraceProcessor,
    query: str,
    required_tables: set[str],
    db_objects: set[str],
) -> SectionResult:
    """Validates required tables and executes a diagnostic SQL query."""
    if missing := required_tables - db_objects:
        return SectionResult(
            name=name,
            error=f"Required schema tables/views missing: {', '.join(sorted(missing))}",
        )
    try:
        results = tp.run_query(query)
        return SectionResult(
            name=name,
            results=results,
        )
    except Exception as e:
        return SectionResult(
            name=name,
            error=f"Query execution failed: {e}",
        )


def _build_launch_cte(package_filter: str | None) -> str:
    """Builds the shared CTE that resolves app launches, target processes, and milestones.

    Handles:
      1. Android SDK 33+ 'launchingActivity#<id>' + 'launchingActivity#<id>:completed*:<pkg>' slices.
      2. Legacy 'launching: <pkg>' slices.
      3. Fallback cold launches anchored on 'bindApplication' when launchingActivity slices are absent.
      4. Starnix process name truncation (matching both full package names, 15-character
         suffixes/prefixes such as 'rformanceLaunch' or 'arable.settings', and longer
         prefixes such as 'com.google.android.apps.nexusla').
    """
    filter_clause = ""
    if package_filter:
        escaped_pkg = package_filter.replace("'", "''")
        filter_clause = (
            f"WHERE (l.package GLOB '*{escaped_pkg}*' "
            f"OR (l.process_name IS NOT NULL AND l.process_name GLOB '*{escaped_pkg}*'))"
        )

    return f"""
    WITH
      sdk33_async AS (
        SELECT
          CAST(SUBSTR(name, 19) AS INT) AS startup_id,
          ts,
          dur,
          ts + dur AS ts_end
        FROM slice
        WHERE name GLOB 'launchingActivity#*'
          AND dur > 0
          AND INSTR(name, ':') = 0
      ),
      sdk33_complete AS (
        SELECT
          CAST(STR_SPLIT(SUBSTR(name, 19), ':', 0) AS INT) AS startup_id,
          STR_SPLIT(SUBSTR(name, 19), ':', 2) AS package,
          CASE
            WHEN STR_SPLIT(SUBSTR(name, 19), ':', 1) = 'completed-cold' THEN 'cold'
            WHEN STR_SPLIT(SUBSTR(name, 19), ':', 1) = 'completed-warm' THEN 'warm'
            WHEN STR_SPLIT(SUBSTR(name, 19), ':', 1) = 'completed-hot' THEN 'hot'
            ELSE 'unknown'
          END AS startup_type,
          MIN(ts) AS complete_ts
        FROM slice
        WHERE dur = 0
          AND name GLOB 'launchingActivity#*:completed*:*'
          AND NOT (name GLOB '*:completed-same-process:*')
        GROUP BY 1, 2, 3
      ),
      sdk33_launches AS (
        SELECT
          a.startup_id,
          c.package,
          c.startup_type,
          a.ts AS launch_ts,
          a.ts_end AS launch_ts_end,
          a.dur AS launch_dur
        FROM sdk33_async a
        JOIN sdk33_complete c
          ON a.startup_id = c.startup_id
          AND c.complete_ts >= a.ts
          AND c.complete_ts <= a.ts_end + 100000000
      ),
      legacy_launches AS (
        SELECT
          ROW_NUMBER() OVER (ORDER BY s.ts) AS startup_id,
          STR_SPLIT(s.name, ': ', 1) AS package,
          NULL AS startup_type,
          s.ts AS launch_ts,
          s.ts + s.dur AS launch_ts_end,
          s.dur AS launch_dur
        FROM slice s
        WHERE s.name GLOB 'launching: *'
          AND s.dur > 0
          AND NOT EXISTS (SELECT 1 FROM sdk33_launches)
      ),
      fallback_bind_launches AS (
        SELECT
          ROW_NUMBER() OVER (ORDER BY s_bind.ts) AS startup_id,
          p.name AS package,
          'cold' AS startup_type,
          COALESCE(
            (
              SELECT MAX(sp.ts)
              FROM slice sp
              WHERE sp.name GLOB 'Start proc:*'
                AND (
                  sp.name GLOB '*' || p.name || '*'
                  OR sp.ts >= s_bind.ts - 1000000000
                )
                AND sp.ts <= s_bind.ts
            ),
            s_bind.ts
          ) AS launch_ts,
          COALESCE(
            (
              SELECT MIN(df.ts + df.dur)
              FROM slice df
              JOIN thread_track tt_df ON df.track_id = tt_df.id
              JOIN thread t_df ON tt_df.utid = t_df.utid
              WHERE t_df.upid = p.upid
                AND df.name GLOB 'DrawFrame*'
                AND df.ts >= s_bind.ts
                AND df.ts <= s_bind.ts + 10000000000
            ),
            (
              SELECT MIN(cf.ts + cf.dur)
              FROM slice cf
              JOIN thread_track tt_cf ON cf.track_id = tt_cf.id
              WHERE tt_cf.utid = t.utid
                AND cf.name GLOB 'Choreographer#doFrame*'
                AND cf.ts >= s_bind.ts
                AND cf.ts <= s_bind.ts + 10000000000
            ),
            s_bind.ts + s_bind.dur
          ) AS launch_ts_end
        FROM slice s_bind
        JOIN thread_track tt ON s_bind.track_id = tt.id
        JOIN thread t ON tt.utid = t.utid
        JOIN process p ON t.upid = p.upid
        WHERE s_bind.name = 'bindApplication'
          AND NOT (p.name GLOB '*:sandboxed_process*' OR p.name GLOB 'ocessService*')
          AND NOT EXISTS (SELECT 1 FROM sdk33_launches)
          AND NOT EXISTS (SELECT 1 FROM legacy_launches)
      ),
      raw_launches AS MATERIALIZED (
        SELECT * FROM sdk33_launches
        UNION ALL
        SELECT * FROM legacy_launches
        UNION ALL
        SELECT
          startup_id,
          package,
          startup_type,
          launch_ts,
          launch_ts_end,
          launch_ts_end - launch_ts AS launch_dur
        FROM fallback_bind_launches
      ),
      launches_with_proc AS MATERIALIZED (
        SELECT
          r.startup_id,
          r.package,
          r.startup_type AS raw_startup_type,
          r.launch_ts,
          r.launch_ts_end,
          r.launch_dur,
          COALESCE(
            (
              SELECT p.upid
              FROM slice s
              JOIN thread_track tt ON s.track_id = tt.id
              JOIN thread t ON tt.utid = t.utid
              JOIN process p ON t.upid = p.upid
              WHERE s.ts >= r.launch_ts AND s.ts < r.launch_ts_end
                AND s.name IN ('bindApplication', 'activityStart', 'activityResume')
                AND (
                  p.name = r.package
                  OR p.name = SUBSTR(r.package, -15)
                  OR p.name = SUBSTR(r.package, 1, 15)
                  OR p.name GLOB r.package || '*'
                  OR r.package GLOB '*' || p.name
                  OR (LENGTH(p.name) >= 15 AND r.package GLOB p.name || '*')
                  OR p.name IN ('zygote64', 'zygote', '<pre-initialized>')
                  OR p.name IS NULL
                )
              ORDER BY
                (p.name IS NOT NULL AND p.name NOT IN ('zygote64', 'zygote', '<pre-initialized>')) DESC,
                (s.name = 'bindApplication') DESC,
                s.ts ASC
              LIMIT 1
            ),
            (
              SELECT p.upid
              FROM process p
              WHERE p.name = r.package
                 OR p.name = SUBSTR(r.package, -15)
                 OR p.name = SUBSTR(r.package, 1, 15)
                 OR (LENGTH(p.name) >= 15 AND r.package GLOB p.name || '*')
              ORDER BY p.upid DESC
              LIMIT 1
            )
          ) AS upid
        FROM raw_launches r
      ),
      unfiltered_launches AS MATERIALIZED (
        SELECT
          lp.startup_id,
          lp.package,
          p.name AS process_name,
          lp.upid,
          lp.launch_ts AS t0_launch_start,
          lp.launch_ts_end AS t6_launch_end,
          lp.launch_dur AS total_launch_dur,
          COALESCE(
            (
              SELECT t.utid
              FROM slice s
              JOIN thread_track tt ON s.track_id = tt.id
              JOIN thread t ON tt.utid = t.utid
              WHERE t.upid = lp.upid
                AND s.ts >= lp.launch_ts AND s.ts < lp.launch_ts_end
                AND s.name IN ('bindApplication', 'activityStart', 'activityResume')
              ORDER BY COALESCE(t.is_main_thread, 0) DESC, s.ts ASC
              LIMIT 1
            ),
            (
              SELECT t.utid
              FROM thread t
              WHERE t.upid = lp.upid
                AND (t.is_main_thread = 1 OR t.name = p.name)
              ORDER BY COALESCE(t.is_main_thread, 0) DESC, t.utid ASC
              LIMIT 1
            )
          ) AS main_utid,
          COALESCE(
            lp.raw_startup_type,
            CASE
              WHEN EXISTS (
                SELECT 1 FROM slice s
                JOIN thread_track tt ON s.track_id = tt.id
                JOIN thread t ON tt.utid = t.utid
                WHERE t.upid = lp.upid
                  AND s.ts >= lp.launch_ts AND s.ts < lp.launch_ts_end
                  AND s.name = 'bindApplication'
              ) THEN 'cold'
              WHEN EXISTS (
                SELECT 1 FROM slice s
                JOIN thread_track tt ON s.track_id = tt.id
                JOIN thread t ON tt.utid = t.utid
                WHERE t.upid = lp.upid
                  AND s.ts >= lp.launch_ts AND s.ts < lp.launch_ts_end
                  AND s.name = 'activityStart'
              ) THEN 'warm'
              ELSE 'hot'
            END
          ) AS startup_type
        FROM launches_with_proc lp
        LEFT JOIN process p ON p.upid = lp.upid
      ),
      resolved_launches AS MATERIALIZED (
        SELECT *
        FROM unfiltered_launches l
        {filter_clause}
      ),
      milestones AS MATERIALIZED (
        SELECT
          l.*,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            JOIN process p ON t.upid = p.upid
            WHERE p.name = 'system_server'
              AND s.name GLOB 'Start proc:*'
              AND NOT (s.name GLOB '*:sandboxed_process*')
              AND (
                s.name GLOB '*' || l.package || '*'
                OR (l.process_name IS NOT NULL AND s.name GLOB '*' || l.process_name || '*')
              )
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ) AS t1_start_proc_ts,
          (
            SELECT s.dur
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            JOIN process p ON t.upid = p.upid
            WHERE p.name = 'system_server'
              AND s.name GLOB 'Start proc:*'
              AND NOT (s.name GLOB '*:sandboxed_process*')
              AND (
                s.name GLOB '*' || l.package || '*'
                OR (l.process_name IS NOT NULL AND s.name GLOB '*' || l.process_name || '*')
              )
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
            ORDER BY s.ts ASC
            LIMIT 1
          ) AS start_proc_dur,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name IN ('PostFork', 'ActivityThreadMain')
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ) AS t1b_post_fork_ts,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name = 'bindApplication'
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ) AS t2_bind_app_start,
          (
            SELECT
              CASE
                WHEN s.dur >= 1000000 THEN s.ts + s.dur
                ELSE COALESCE(
                  (
                    SELECT MIN(s2.ts)
                    FROM slice s2
                    JOIN thread_track tt2 ON s2.track_id = tt2.id
                    WHERE tt2.utid = l.main_utid
                      AND s2.ts > s.ts
                      AND s2.ts < l.t6_launch_end
                      AND (
                        s2.name IN ('clientTransactionExecuted', 'activityStart', 'activityResume')
                        OR s2.name GLOB 'broadcastReceiveComp*'
                      )
                  ),
                  s.ts + s.dur
                )
              END
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name = 'bindApplication'
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
            ORDER BY s.ts ASC
            LIMIT 1
          ) AS t3_bind_app_end,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name IN ('activityStart', 'activityResume')
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ) AS t3b_activity_start_ts,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name = 'activityResume'
              AND s.ts >= l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ) AS t3c_activity_resume_ts,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid = l.main_utid
              AND s.name GLOB 'Choreographer#doFrame*'
              -- The first frame after the app starts handling the launch. A warm or
              -- hot app can draw frames (e.g. animations) before its activity resumes.
              AND s.ts >= COALESCE(
                (
                  SELECT MIN(a.ts)
                  FROM slice a
                  JOIN thread_track att ON a.track_id = att.id
                  WHERE att.utid = l.main_utid
                    AND a.name IN ('bindApplication', 'activityStart', 'activityResume')
                    AND a.ts >= l.t0_launch_start
                    AND a.ts < l.t6_launch_end
                ),
                l.t0_launch_start
              )
              AND s.ts < l.t6_launch_end
          ) AS t4_do_frame_start
        FROM resolved_launches l
      ),
      full_milestones AS MATERIALIZED (
        SELECT
          m.*,
          COALESCE(
            (
              SELECT t.utid
              FROM slice s
              JOIN thread_track tt ON s.track_id = tt.id
              JOIN thread t ON tt.utid = t.utid
              WHERE t.upid = m.upid
                AND s.name GLOB 'DrawFrame*'
                AND s.ts >= COALESCE(m.t4_do_frame_start, m.t0_launch_start)
                AND s.ts <= m.t6_launch_end
              ORDER BY (t.name GLOB 'RenderThread*') DESC, s.ts ASC
              LIMIT 1
            ),
            (
              SELECT t.utid
              FROM thread t
              WHERE t.upid = m.upid
                AND t.name GLOB 'RenderThread*'
              ORDER BY t.utid ASC
              LIMIT 1
            )
          ) AS render_utid,
          (
            SELECT MIN(s.ts)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            WHERE t.upid = m.upid
              AND s.name GLOB 'DrawFrame*'
              AND s.ts >= COALESCE(m.t4_do_frame_start, m.t0_launch_start)
              AND s.ts <= m.t6_launch_end
          ) AS t5_draw_frame_start,
          (
            SELECT MIN(s.ts + s.dur)
            FROM slice s
            WHERE s.name = 'Display::Client::CommitConfig'
              AND s.ts >= m.t6_launch_end
              AND s.ts <= m.t6_launch_end + 100000000
          ) AS t7_display_commit_end,
          COALESCE(
            -- ActivityTaskManager's START: the last resolveIntent before the background
            -- activity start check, on the thread that runs the start. logcat `Displayed`
            -- times are measured from here, while launchingActivity can begin earlier.
            -- Without that check in the trace (it is missing for some hot launches), the
            -- launch start stands in for START.
            (
            SELECT COALESCE(
              (
                SELECT MAX(ri.ts)
                FROM slice ri
                WHERE ri.track_id = sab.track_id
                  AND ri.name = 'resolveIntent'
                  AND ri.ts >= m.t0_launch_start
                  AND ri.ts <= sab.ts
              ),
              sab.ts
            )
            FROM slice sab
            JOIN thread_track tt ON sab.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            JOIN process p ON t.upid = p.upid
            WHERE p.name = 'system_server'
              AND sab.name = 'shouldAbortBackgroundActivityStart'
              AND sab.ts >= m.t0_launch_start
              AND sab.ts < m.t6_launch_end
            ORDER BY sab.ts
            LIMIT 1
            ),
            m.t0_launch_start
          ) AS t0b_activity_start_request_ts
        FROM milestones m
      )
    """


def _analyze_critical_path_segments(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    package_filter: str | None,
    limit: int,
) -> SectionResult:
    """Divides each app launch critical path into sequential, contiguous phase segments.

    `total_launch_ms` is the launchingActivity slice, which starts when system_server
    receives the intent. `displayed_ms` starts at ActivityTaskManager's START instead,
    which is where logcat `Displayed` times (and AtmDisplayTime) start. `pre_start_ms` is
    the time between the two, e.g. resolving the intent or waiting for a lock. If the
    trace has no START marker, `displayed_ms` equals `total_launch_ms` and
    `pre_start_ms` is 0.

    The segments before `display_present_tail_ms` tile the launch from its start to its
    end, so they sum to `total_launch_ms`:
      - Cold launches use `pre_start_ms` through `bind_app_ms`.
      - Warm and hot launches have no `Start proc:` or `bindApplication`, so those
        columns are NULL and `pre_activity_ms` covers START to the first
        `activityStart`/`activityResume` on the app main thread.
      - `unattributed_ms` is the time no segment covers because a milestone is missing.
    """
    required_tables = {"slice", "thread_track", "thread", "process"}
    cte = _build_launch_cte(package_filter)
    query = f"""
    {cte},
      raw_segments AS (
        SELECT
          startup_id,
          package,
          process_name,
          startup_type,
          t0_launch_start,
          total_launch_dur,
          (t6_launch_end - t0b_activity_start_request_ts) AS displayed_dur,
          (t0b_activity_start_request_ts - t0_launch_start) AS pre_start_dur,
          (t1_start_proc_ts - t0b_activity_start_request_ts) AS pre_fork_dur,
          (COALESCE(t1b_post_fork_ts, t1_start_proc_ts + start_proc_dur) - t1_start_proc_ts) AS process_fork_dur,
          (t2_bind_app_start - COALESCE(t1b_post_fork_ts, t1_start_proc_ts + start_proc_dur)) AS attach_app_dur,
          (t3_bind_app_end - t2_bind_app_start) AS bind_app_dur,
          CASE
            WHEN t2_bind_app_start IS NULL
              THEN t3b_activity_start_ts - t0b_activity_start_request_ts
          END AS pre_activity_dur,
          (t4_do_frame_start - COALESCE(t3_bind_app_end, t3b_activity_start_ts)) AS activity_lifecycle_dur,
          (t5_draw_frame_start - t4_do_frame_start) AS first_frame_ui_dur,
          (t6_launch_end - t5_draw_frame_start) AS first_frame_render_dur,
          (t7_display_commit_end - t6_launch_end) AS display_present_tail_dur
        FROM full_milestones
      ),
      segments_with_sum AS (
        SELECT
          *,
          (
            COALESCE(pre_start_dur, 0)
            + COALESCE(pre_fork_dur, 0)
            + COALESCE(process_fork_dur, 0)
            + COALESCE(attach_app_dur, 0)
            + COALESCE(bind_app_dur, 0)
            + COALESCE(pre_activity_dur, 0)
            + COALESCE(activity_lifecycle_dur, 0)
            + COALESCE(first_frame_ui_dur, 0)
            + COALESCE(first_frame_render_dur, 0)
          ) AS seg_sum
        FROM raw_segments
      )
    SELECT
      startup_id,
      package,
      process_name,
      startup_type,
      ROUND(total_launch_dur / 1e6, 2) AS total_launch_ms,
      ROUND(displayed_dur / 1e6, 2) AS displayed_ms,
      ROUND(pre_start_dur / 1e6, 2) AS pre_start_ms,
      ROUND(pre_fork_dur / 1e6, 2) AS pre_fork_ms,
      ROUND(process_fork_dur / 1e6, 2) AS process_fork_ms,
      ROUND(attach_app_dur / 1e6, 2) AS attach_app_ms,
      ROUND(bind_app_dur / 1e6, 2) AS bind_app_ms,
      ROUND(pre_activity_dur / 1e6, 2) AS pre_activity_ms,
      ROUND(activity_lifecycle_dur / 1e6, 2) AS activity_lifecycle_ms,
      ROUND(first_frame_ui_dur / 1e6, 2) AS first_frame_ui_ms,
      ROUND(first_frame_render_dur / 1e6, 2) AS first_frame_render_ms,
      ROUND((total_launch_dur - seg_sum) / 1e6, 2) AS unattributed_ms,
      ROUND(display_present_tail_dur / 1e6, 2) AS display_present_tail_ms
    FROM segments_with_sum
    ORDER BY t0_launch_start
    LIMIT {limit};
    """
    return _perform_analysis_query(
        "Critical Path Phase Breakdown (Sequential Segments)",
        tp,
        query,
        required_tables,
        db_objects,
    )


def _substage_case(name: str, is_main: str) -> str:
    """Returns a SQL expression naming the CUJ sub-stage of a slice, or NULL if it has none.

    Args:
        name: SQL expression for the slice name.
        is_main: SQL expression that is true if the slice is on the app main thread.
    """
    return f"""
      CASE
        WHEN ({is_main})
          AND {name} IN ('ActivityThreadMain', 'bindApplication', 'activityResume')
          THEN 'init'
        WHEN {name} IN ('traversal', 'performTraversals') THEN 'traversal'
        WHEN {name} IN ('performDraw', 'Record View#draw()')
          OR {name} GLOB 'draw-VRI*'
          OR {name} GLOB 'DMSCVRENDERING*'
          THEN 'render'
        WHEN {name} GLOB 'DrawFrame*' OR {name} GLOB 'Choreographer$FrameHandler*' THEN 'draw'
        WHEN {name} = 'animation'
          OR {name} GLOB 'animator*'
          OR {name} GLOB 'transition_animation*'
          THEN 'animation'
      END"""


def _analyze_cuj_substages(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    package_filter: str | None,
    limit: int,
    scaling: str = "raw",
) -> SectionResult:
    """Computes the CUJ sub-stage breakdown for each launch.

    Sub-stage columns and the slices attributed to each:
      - init: ActivityThreadMain, bindApplication, activityResume (app main thread)
      - draw: DrawFrame*, Choreographer$FrameHandler*
      - render: performDraw, Record View#draw(), draw-VRI*, DMSCVRENDERING*
      - traversal: traversal, performTraversals
      - animation: animation, animator*, transition_animation*
      - scenic: Flatland::Present, top-level Scenic* slices

    These slices nest (e.g. `traversal` > `draw-VRI[...]` > `Record View#draw()`), so
    each app slice counts only its self time: its duration, clipped to the launch, minus
    the time of the nearest nested slices that also belong to a sub-stage. Each moment
    on a thread then counts toward at most one sub-stage. Sub-stages on different
    threads run in parallel, so they can sum to more than the launch duration, and
    `--scaling proportional` reports each sub-stage's share of the attributed work
    scaled to `total_launch_ms`, not critical-path time.
    """
    required_tables = {"slice", "thread_track", "thread", "process"}
    cte = _build_launch_cte(package_filter)
    if scaling == "proportional":
        stage_select_exprs = """
          ROUND(
            CASE WHEN stage_sum > 0 THEN (init_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS init,
          ROUND(
            CASE WHEN stage_sum > 0 THEN (draw_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS draw,
          ROUND(
            CASE WHEN stage_sum > 0 THEN (render_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS render,
          ROUND(
            CASE WHEN stage_sum > 0 THEN (traversal_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS traversal,
          ROUND(
            CASE WHEN stage_sum > 0 THEN (animation_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS animation,
          ROUND(
            CASE WHEN stage_sum > 0 THEN (scenic_dur * 1.0 / stage_sum) * (total_launch_dur / 1e6) ELSE 0.0 END,
            2
          ) AS scenic
        """
    else:
        stage_select_exprs = """
          ROUND(init_dur / 1e6, 2) AS init,
          ROUND(draw_dur / 1e6, 2) AS draw,
          ROUND(render_dur / 1e6, 2) AS render,
          ROUND(traversal_dur / 1e6, 2) AS traversal,
          ROUND(animation_dur / 1e6, 2) AS animation,
          ROUND(scenic_dur / 1e6, 2) AS scenic
        """

    query = f"""
    {cte},
      app_stage_slices AS MATERIALIZED (
        SELECT
          m.startup_id,
          m.t0_launch_start,
          s.id AS slice_id,
          s.name AS slice_name,
          s.ts AS slice_ts,
          MIN(s.ts + s.dur, m.t6_launch_end) - s.ts AS clipped_dur,
          (tt.utid = m.main_utid) AS is_main,
          {_substage_case("s.name", "tt.utid = m.main_utid")} AS stage
        FROM full_milestones m
        JOIN thread t ON t.upid = m.upid
        JOIN thread_track tt ON tt.utid = t.utid
        JOIN slice s ON s.track_id = tt.id
        WHERE s.ts >= m.t0_launch_start
          AND s.ts < m.t6_launch_end
          AND s.dur > 0
      ),
      staged_slices AS MATERIALIZED (
        SELECT
          a.*,
          (
            SELECT anc.id
            FROM ancestor_slice(a.slice_id) anc
            WHERE {_substage_case("anc.name", "a.is_main")} IS NOT NULL
            ORDER BY anc.depth DESC
            LIMIT 1
          ) AS stage_parent_id
        FROM app_stage_slices a
        WHERE a.stage IS NOT NULL
      ),
      stage_self AS (
        SELECT
          p.*,
          p.clipped_dur - COALESCE(
            (
              SELECT SUM(c.clipped_dur)
              FROM staged_slices c
              WHERE c.startup_id = p.startup_id
                AND c.t0_launch_start = p.t0_launch_start
                AND c.stage_parent_id = p.slice_id
            ),
            0
          ) AS self_dur
        FROM staged_slices p
      ),
      raw_substages AS (
        SELECT
          m.startup_id,
          m.package,
          m.total_launch_dur,
          MAX(
            COALESCE(SUM(CASE WHEN a.stage = 'init' THEN a.self_dur END), 0),
            CASE
              WHEN m.t3_bind_app_end IS NOT NULL
                AND COALESCE(
                  SUM(CASE WHEN a.stage = 'init' AND a.slice_name = 'bindApplication' THEN a.clipped_dur END),
                  0
                ) < 1000000
              THEN COALESCE(m.t3_bind_app_end - m.t2_bind_app_start, 0)
              WHEN m.t3_bind_app_end IS NULL
                AND COALESCE(
                  SUM(CASE WHEN a.stage = 'init' AND a.slice_name = 'activityResume' THEN a.clipped_dur END),
                  0
                ) < 1000000
              THEN COALESCE(m.t4_do_frame_start - m.t3c_activity_resume_ts, 0)
              ELSE 0
            END
          ) AS init_dur,
          MAX(
            COALESCE(SUM(CASE WHEN a.stage = 'traversal' THEN a.self_dur END), 0),
            CASE
              WHEN COALESCE(SUM(CASE WHEN a.stage = 'traversal' THEN a.clipped_dur END), 0) < 500000
              THEN COALESCE(
                COALESCE(
                  MIN(
                    CASE
                      WHEN a.slice_name IN ('performDraw', 'Record View#draw()')
                        AND a.slice_ts >= m.t4_do_frame_start
                        AND a.slice_ts <= m.t5_draw_frame_start
                      THEN a.slice_ts
                    END
                  ),
                  m.t5_draw_frame_start
                ) - m.t4_do_frame_start,
                0
              )
              ELSE 0
            END
          ) AS traversal_dur,
          MAX(
            COALESCE(SUM(CASE WHEN a.stage = 'render' THEN a.self_dur END), 0),
            CASE
              WHEN COALESCE(SUM(CASE WHEN a.stage = 'render' THEN a.clipped_dur END), 0) < 100000
              THEN COALESCE(
                m.t5_draw_frame_start - MIN(
                  CASE
                    WHEN a.slice_name IN ('performDraw', 'Record View#draw()')
                      AND a.slice_ts >= m.t4_do_frame_start
                      AND a.slice_ts <= m.t5_draw_frame_start
                    THEN a.slice_ts
                  END
                ),
                0
              )
              ELSE 0
            END
          ) AS render_dur,
          MAX(
            COALESCE(SUM(CASE WHEN a.stage = 'draw' THEN a.self_dur END), 0),
            CASE
              WHEN COALESCE(
                SUM(CASE WHEN a.slice_name GLOB 'DrawFrame*' THEN a.clipped_dur END),
                0
              ) < 500000
              THEN COALESCE(m.t6_launch_end - m.t5_draw_frame_start, 0)
              ELSE 0
            END
          ) AS draw_dur,
          COALESCE(SUM(CASE WHEN a.stage = 'animation' THEN a.self_dur END), 0) AS animation_dur,
          COALESCE(
            (
              SELECT SUM(MIN(m.t6_launch_end, s.ts + s.dur) - MAX(m.t0_launch_start, s.ts))
              FROM slice s
              JOIN thread_track tt ON s.track_id = tt.id
              JOIN thread t ON tt.utid = t.utid
              JOIN process p ON t.upid = p.upid
              WHERE (p.name GLOB '*scenic*' OR s.name = 'Flatland::Present')
                AND (s.name = 'Flatland::Present' OR (s.name GLOB 'Scenic*' AND s.depth = 0))
                AND s.ts + s.dur > m.t0_launch_start
                AND s.ts < m.t6_launch_end
                AND NOT EXISTS (
                  SELECT 1
                  FROM ancestor_slice(s.id) anc
                  WHERE anc.name = 'Flatland::Present'
                    OR (anc.name GLOB 'Scenic*' AND anc.depth = 0)
                )
            ),
            0
          ) AS scenic_dur
        FROM full_milestones m
        LEFT JOIN stage_self a USING (startup_id, t0_launch_start)
        GROUP BY
          m.startup_id,
          m.package,
          m.total_launch_dur,
          m.t0_launch_start,
          m.t2_bind_app_start,
          m.t3_bind_app_end,
          m.t3c_activity_resume_ts,
          m.t4_do_frame_start,
          m.t5_draw_frame_start,
          m.t6_launch_end
        ORDER BY m.t0_launch_start
      ),
      substages_with_sum AS (
        SELECT
          *,
          (
            MAX(init_dur, 0)
            + MAX(draw_dur, 0)
            + MAX(render_dur, 0)
            + MAX(traversal_dur, 0)
            + MAX(animation_dur, 0)
            + MAX(scenic_dur, 0)
          ) AS stage_sum
        FROM raw_substages
      )
    SELECT
      startup_id,
      package,
      ROUND(total_launch_dur / 1e6, 2) AS total_launch_ms,
      {stage_select_exprs}
    FROM substages_with_sum
    LIMIT {limit};
    """
    return _perform_analysis_query(
        "CUJ Sub-Stage Breakdown",
        tp,
        query,
        required_tables,
        db_objects,
    )


def _analyze_intra_segment_operations(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    package_filter: str | None,
    limit: int,
) -> SectionResult:
    """Quantifies key sub-operations inside app binding, lifecycle, and rendering segments."""
    required_tables = {"slice", "thread_track", "thread", "process"}
    cte = _build_launch_cte(package_filter)
    query = f"""
    {cte},
      app_slices AS (
        SELECT
          l.startup_id,
          l.t0_launch_start,
          s.name AS slice_name,
          s.dur AS slice_dur,
          (tt.utid = l.main_utid) AS is_main
        FROM resolved_launches l
        JOIN thread t ON t.upid = l.upid
        JOIN thread_track tt ON tt.utid = t.utid
        JOIN slice s ON s.track_id = tt.id
        WHERE s.ts >= l.t0_launch_start
          AND s.ts < l.t6_launch_end
          AND s.dur > 0
      )
    SELECT
      l.startup_id,
      l.package,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'OpenDexFilesFromOat*' THEN a.slice_dur END), 0) / 1e6, 2) AS open_dex_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'dlopen*' AND NOT a.slice_name GLOB '* - loading and linking' THEN a.slice_dur END), 0) / 1e6, 2) AS dlopen_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'VerifyClass*' AND NOT a.slice_name GLOB 'VerifyClassLoader*' THEN a.slice_dur END), 0) / 1e6, 2) AS verify_class_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'L*/*;' THEN a.slice_dur END), 0) / 1e6, 2) AS class_init_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'ResourcesManager#getResources*' THEN a.slice_dur END), 0) / 1e6, 2) AS get_resources_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name = 'inflate' THEN a.slice_dur END), 0) / 1e6, 2) AS inflate_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.is_main AND a.slice_name GLOB 'relayoutWindow*' THEN a.slice_dur END), 0) / 1e6, 2) AS relayout_window_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.slice_name = 'dequeueBuffer' OR a.slice_name GLOB 'allocateBuffers*' THEN a.slice_dur END), 0) / 1e6, 2) AS dequeue_buffer_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.slice_name GLOB '*shader_compile*' OR a.slice_name GLOB '*compile_shader*' THEN a.slice_dur END), 0) / 1e6, 2) AS shader_compile_ms,
      ROUND(COALESCE(SUM(CASE WHEN a.slice_name GLOB 'QueuePresentKHR*' OR a.slice_name GLOB 'eglSwapBuffers*' OR a.slice_name GLOB 'Vulkan finish frame*' THEN a.slice_dur END), 0) / 1e6, 2) AS queue_present_ms
    FROM resolved_launches l
    LEFT JOIN app_slices a USING (startup_id, t0_launch_start)
    GROUP BY l.startup_id, l.t0_launch_start, l.package
    ORDER BY l.t0_launch_start
    LIMIT {limit};
    """
    return _perform_analysis_query(
        "Intra-Segment Sub-Operation Breakdown",
        tp,
        query,
        required_tables,
        db_objects,
    )


def _analyze_segment_thread_states(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    package_filter: str | None,
    limit: int,
) -> SectionResult:
    """Breaks down critical-path segments by kernel thread_state (Running vs Runnable vs Blocked)."""
    required_tables = {
        "slice",
        "thread_track",
        "thread",
        "process",
        "thread_state",
    }
    cte = _build_launch_cte(package_filter)
    query = f"""
    {cte},
      segment_windows AS (
        -- S2b: AttachApplication on App Main Thread
        SELECT
          m.startup_id,
          m.t0_launch_start,
          m.package,
          'S2b_AttachApplication' AS segment,
          t.utid,
          t.name AS thread_name,
          COALESCE(m.t1b_post_fork_ts, m.t1_start_proc_ts + m.start_proc_dur) AS seg_start,
          m.t2_bind_app_start AS seg_end
        FROM full_milestones m
        JOIN thread t ON t.utid = m.main_utid
        WHERE COALESCE(m.t1b_post_fork_ts, m.t1_start_proc_ts + m.start_proc_dur) IS NOT NULL
          AND m.t2_bind_app_start > COALESCE(m.t1b_post_fork_ts, m.t1_start_proc_ts + m.start_proc_dur)

        UNION ALL

        -- S3: BindApplication on App Main Thread
        SELECT
          m.startup_id,
          m.t0_launch_start,
          m.package,
          'S3_BindApplication' AS segment,
          t.utid,
          t.name AS thread_name,
          m.t2_bind_app_start AS seg_start,
          m.t3_bind_app_end AS seg_end
        FROM full_milestones m
        JOIN thread t ON t.utid = m.main_utid
        WHERE m.t2_bind_app_start IS NOT NULL
          AND m.t3_bind_app_end > m.t2_bind_app_start

        UNION ALL

        -- S4: Activity Lifecycle on App Main Thread
        SELECT
          m.startup_id,
          m.t0_launch_start,
          m.package,
          'S4_ActivityLifecycle' AS segment,
          t.utid,
          t.name AS thread_name,
          COALESCE(m.t3_bind_app_end, m.t3b_activity_start_ts) AS seg_start,
          m.t4_do_frame_start AS seg_end
        FROM full_milestones m
        JOIN thread t ON t.utid = m.main_utid
        WHERE COALESCE(m.t3_bind_app_end, m.t3b_activity_start_ts) IS NOT NULL
          AND m.t4_do_frame_start > COALESCE(m.t3_bind_app_end, m.t3b_activity_start_ts)

        UNION ALL

        -- S5: First Frame UI Traversal on App Main Thread
        SELECT
          m.startup_id,
          m.t0_launch_start,
          m.package,
          'S5_FirstFrameUi' AS segment,
          t.utid,
          t.name AS thread_name,
          m.t4_do_frame_start AS seg_start,
          m.t5_draw_frame_start AS seg_end
        FROM full_milestones m
        JOIN thread t ON t.utid = m.main_utid
        WHERE m.t4_do_frame_start IS NOT NULL
          AND m.t5_draw_frame_start > m.t4_do_frame_start

        UNION ALL

        -- S6: First Frame Render & GPU on App RenderThread
        SELECT
          m.startup_id,
          m.t0_launch_start,
          m.package,
          'S6_FirstFrameRender' AS segment,
          t.utid,
          t.name AS thread_name,
          m.t5_draw_frame_start AS seg_start,
          m.t6_launch_end AS seg_end
        FROM full_milestones m
        JOIN thread t ON t.utid = m.render_utid
        WHERE m.t5_draw_frame_start IS NOT NULL
          AND m.t6_launch_end > m.t5_draw_frame_start
      ),
      segment_states AS (
        SELECT
          w.startup_id,
          w.t0_launch_start,
          w.package,
          w.segment,
          w.thread_name,
          w.seg_end - w.seg_start AS seg_dur,
          st.state,
          SUM(MIN(w.seg_end, st.ts + st.dur) - MAX(w.seg_start, st.ts)) AS state_dur
        FROM segment_windows w
        JOIN thread_state st
          ON st.utid = w.utid
          AND st.ts + st.dur > w.seg_start
          AND st.ts < w.seg_end
        WHERE st.dur > 0
        GROUP BY w.startup_id, w.t0_launch_start, w.package, w.segment, w.thread_name, w.seg_start, w.seg_end, st.state
      )
    SELECT
      startup_id,
      package,
      segment,
      thread_name,
      ROUND(seg_dur / 1e6, 2) AS segment_dur_ms,
      ROUND(COALESCE(SUM(CASE WHEN state = 'Running' THEN state_dur END), 0) / 1e6, 2) AS running_cpu_ms,
      ROUND(COALESCE(SUM(CASE WHEN state IN ('R', 'R+', 'W') THEN state_dur END), 0) / 1e6, 2) AS runnable_starved_ms,
      ROUND(COALESCE(SUM(CASE WHEN state IN ('S', 'D') THEN state_dur END), 0) / 1e6, 2) AS sleeping_blocked_ms,
      ROUND(
        COALESCE(SUM(CASE WHEN state = 'Running' THEN state_dur END), 0) * 100.0 / CAST(seg_dur AS DOUBLE),
        1
      ) AS running_pct
    FROM segment_states
    GROUP BY startup_id, t0_launch_start, package, segment, thread_name, seg_dur
    ORDER BY t0_launch_start, startup_id, segment
    LIMIT {limit};
    """
    return _perform_analysis_query(
        "Critical Path Thread State Breakdown",
        tp,
        query,
        required_tables,
        db_objects,
    )


def _top_level_thread_slice_ms(
    utids: str, name_match: str, launch: str = "l"
) -> str:
    """Returns SQL for the time of matching slices on the given threads during a launch.

    Only slices with no matching ancestor count, so nested matches are not counted
    twice, and each slice is clipped to the launch.

    Args:
        utids: SQL list of thread utids, e.g. "(l.main_utid, l.render_utid)".
        name_match: SQL predicate on a slice alias `{a}`, formatted with `a`.
        launch: alias of the launch row.
    """
    return f"""
      ROUND(
        COALESCE(
          (
            SELECT SUM(MIN({launch}.t6_launch_end, s.ts + s.dur) - MAX({launch}.t0_launch_start, s.ts))
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            WHERE tt.utid IN {utids}
              AND s.ts + s.dur > {launch}.t0_launch_start
              AND s.ts < {launch}.t6_launch_end
              AND {name_match.format(a="s")}
              AND NOT EXISTS (
                SELECT 1 FROM ancestor_slice(s.id) anc WHERE {name_match.format(a="anc")}
              )
          ),
          0
        ) / 1e6,
        2
      )"""


_BINDER_CALL = (
    "({a}.name = 'BinderIoctl' OR {a}.name GLOB 'binder transaction*'"
    " OR {a}.name = 'security.hooks.binder_transaction')"
)
_MONITOR_CONTENTION = "({a}.name GLOB 'monitor contention*' OR {a}.name GLOB 'Lock contention on a monitor*')"
_NATIVE_LOCK_CONTENTION = (
    "({a}.name GLOB '*Mutex::AcquireContended*' OR {a}.name GLOB '*ContendedReadAcquire*'"
    " OR {a}.name GLOB '*ContendedWriteAcquire*'"
    " OR ({a}.name GLOB 'Lock contention*' AND {a}.name NOT GLOB 'Lock contention on a monitor*'))"
)


def _analyze_subsystem_contention(
    tp: PerfettoTraceProcessor,
    db_objects: set[str],
    package_filter: str | None,
    limit: int,
) -> SectionResult:
    """Quantifies cross-layer contention and Fuchsia platform subsystem costs during each launch.

    Binder and lock columns cover only the app main thread (binder) or the main thread
    and RenderThread (locks), count only outermost matching slices, and clip them to the
    launch, so they never exceed the launch duration:
      - main_binder_calls / main_binder_ms: outermost binder calls on the main thread
        (`BinderIoctl` on Starnix, `binder transaction` on Linux).
      - monitor_contention_ms: Java monitor contention.
      - native_lock_contention_ms: ART and native mutex contention other than monitors.
    The remaining columns are system-wide and can overlap the app's own work:
      - fxfs_page_in_ms: Fxfs page-in time, clipped to the launch.
      - sysmem_busy_ms: time sysmem spent in top-level work on its own threads, clipped
        to the launch. It is not necessarily on the app's critical path.
      - selinux_hooks_ms: Starnix SELinux hook time in the app process.
    """
    required_tables = {"slice", "thread_track", "thread", "process"}
    cte = _build_launch_cte(package_filter)
    main_thread = "(l.main_utid)"
    ui_threads = "(l.main_utid, l.render_utid)"
    query = f"""
    {cte}
    SELECT
      l.startup_id,
      l.package,
      (
        SELECT COUNT(*)
        FROM slice s
        JOIN thread_track tt ON s.track_id = tt.id
        WHERE tt.utid = l.main_utid
          AND s.ts >= l.t0_launch_start AND s.ts < l.t6_launch_end
          AND {_BINDER_CALL.format(a="s")}
          AND NOT EXISTS (
            SELECT 1 FROM ancestor_slice(s.id) anc WHERE {_BINDER_CALL.format(a="anc")}
          )
      ) AS main_binder_calls,
      {_top_level_thread_slice_ms(main_thread, _BINDER_CALL)} AS main_binder_ms,
      {_top_level_thread_slice_ms(ui_threads, _MONITOR_CONTENTION)} AS monitor_contention_ms,
      {_top_level_thread_slice_ms(ui_threads, _NATIVE_LOCK_CONTENTION)} AS native_lock_contention_ms,
      ROUND(
        COALESCE(
          (
            SELECT SUM(MIN(l.t6_launch_end, s.ts + s.dur) - MAX(l.t0_launch_start, s.ts))
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            JOIN process p ON t.upid = p.upid
            WHERE p.name GLOB '*fxfs*'
              AND s.name IN ('page_in_chunk', 'start-page-in')
              AND s.ts + s.dur > l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ),
          0
        ) / 1e6,
        2
      ) AS fxfs_page_in_ms,
      ROUND(
        COALESCE(
          (
            SELECT SUM(MIN(l.t6_launch_end, s.ts + s.dur) - MAX(l.t0_launch_start, s.ts))
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            JOIN process p ON t.upid = p.upid
            WHERE p.name GLOB '*sysmem*'
              AND s.depth = 0
              AND s.name NOT GLOB 'Mutex::*'
              AND s.ts + s.dur > l.t0_launch_start
              AND s.ts < l.t6_launch_end
          ),
          0
        ) / 1e6,
        2
      ) AS sysmem_busy_ms,
      ROUND(
        COALESCE(
          (
            SELECT SUM(s.dur)
            FROM slice s
            JOIN thread_track tt ON s.track_id = tt.id
            JOIN thread t ON tt.utid = t.utid
            WHERE t.upid = l.upid
              AND s.ts >= l.t0_launch_start AND s.ts < l.t6_launch_end
              AND (
                s.name GLOB 'security.hooks*'
                OR (s.name GLOB 'security.selinux*' AND s.depth = 0)
              )
          ),
          0
        ) / 1e6,
        2
      ) AS selinux_hooks_ms
    FROM full_milestones l
    ORDER BY l.t0_launch_start
    LIMIT {limit};
    """
    return _perform_analysis_query(
        "Cross-Layer Contention & Fuchsia Subsystem Attribution",
        tp,
        query,
        required_tables,
        db_objects,
    )


class AppLaunchPlugin(AnalyzePlugin):
    """Analysis plugin for decomposing app launch critical path segments and bottlenecks."""

    name: str = "app-launch"
    description: str = (
        "Decompose app launch critical path into sequential phase segments, "
        "sub-operations, thread states, and Fuchsia subsystem bottlenecks"
    )

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
            "--package",
            type=str,
            default=None,
            help="Optional substring filter for package or process name",
        )
        parser.add_argument(
            "--limit",
            type=int,
            default=20,
            help="Maximum number of rows to return per section (default: 20)",
        )
        parser.add_argument(
            "--scaling",
            choices=["raw", "proportional"],
            default="raw",
            help=(
                "Report CUJ sub-stage durations as raw milliseconds ('raw') or "
                "as each sub-stage's share of the attributed work, scaled to "
                "total_launch_ms ('proportional')"
            ),
        )
        args = parser.parse_args(remaining_args)

        if args.limit <= 0:
            raise PluginArgumentError("Limit must be a positive integer.")

        with PerfettoTraceProcessor(trace_path, cache=cache) as tp:
            db_objects = tp.get_tables()
            return [
                _analyze_critical_path_segments(
                    tp, db_objects, args.package, args.limit
                ),
                _analyze_cuj_substages(
                    tp, db_objects, args.package, args.limit, args.scaling
                ),
                _analyze_intra_segment_operations(
                    tp, db_objects, args.package, args.limit
                ),
                _analyze_segment_thread_states(
                    tp, db_objects, args.package, args.limit
                ),
                _analyze_subsystem_contention(
                    tp, db_objects, args.package, args.limit
                ),
            ]
