# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Isolated Perfetto FXT direct ingestion and parsing backend.

This module handles the direct ingestion of Fuchsia Trace (FXT) files into the
in-memory trace model using PerfettoTraceProcessor.
"""

import dataclasses
from collections import defaultdict
from collections.abc import Iterator, Set
from typing import Any, NamedTuple, TypeVar

from tp_shell import PerfettoTraceProcessor
from trace_processing import trace_model, trace_time
from trace_processing.trace_importing import construct_model

_T = TypeVar("_T", bound=tuple[Any, ...])


class _SchedRow(NamedTuple):
    ts: int
    cpu: int
    dur: int
    end_state: str
    priority: int
    tid: int
    state: str


class _ArgRow(NamedTuple):
    arg_set_id: int | None
    key: str
    int_value: int | None
    real_value: float | None
    string_value: str | None


class _SliceRow(NamedTuple):
    id: int
    parent_id: int | None
    arg_set_id: int | None
    ts: int
    dur: int
    category: str
    name: str
    tid: int
    pid: int
    is_async: int


class _CounterRow(NamedTuple):
    ts: int
    value: float
    name: str
    pid: int
    tid: int
    arg_set_id: int | None
    id: int | None


class _FlowRow(NamedTuple):
    slice_out: int | None
    slice_in: int | None
    trace_id: int


class _ProcessRow(NamedTuple):
    pid: int
    name: str


class _ThreadRow(NamedTuple):
    tid: int
    pid: int
    name: str


def create_model_from_tp_session(
    session: PerfettoTraceProcessor,
    patterns: Set[str] | None = None,
    categories: Set[str] | None = None,
) -> trace_model.Model:
    """Creates a trace model directly from an active PerfettoTraceProcessor session.

    Args:
        session: Active PerfettoTraceProcessor instance.
        patterns: Optional regex patterns to filter events.
        categories: Optional categories to filter events.

    Returns:
        A Model object.
    """

    importer = _FxtImporter(session, patterns, categories)
    processes = importer.get_pid_to_name()
    threads = importer.get_threads()
    events = importer.get_events()
    scheduling = importer.get_scheduling_records()

    return construct_model(
        processes.pid_to_name,
        threads.tid_to_name,
        threads.tid_to_pid,
        events.events,
        scheduling.records,
    )


def create_model_from_fxt_path_directly(
    trace_path: str,
    patterns: Set[str] | None = None,
    categories: Set[str] | None = None,
    trace_processor_shell_path: str | None = None,
) -> trace_model.Model:
    """Create a trace model directly from a Perfetto trace file path.

    A temporary PerfettoTraceProcessor session is created for the duration of model creation
    and closed when model creation completes.

    Args:
        trace_path: Path to trace file or URL string.
        patterns: Optional regex patterns to filter events.
        categories: Optional categories to filter events.
        trace_processor_shell_path: Optional path to Perfetto trace_processor_shell executable.

    Returns:
        A trace model containing the trace data.
    """
    with PerfettoTraceProcessor(
        trace_path=trace_path, tp_shell_path=trace_processor_shell_path
    ) as session:
        return create_model_from_tp_session(session, patterns, categories)


class _FxtImporter:
    """Encapsulates Perfetto FXT ingestion session state and query parsing."""

    class _Processes(NamedTuple):
        pid_to_name: dict[int, str]

    class _Threads(NamedTuple):
        tid_to_name: dict[int, str]
        tid_to_pid: dict[int, int]

    class _Slices(NamedTuple):
        events: list[trace_model.Event]
        id_to_event: dict[int, trace_model.Event]
        parent_map: dict[int, int]

    class _Counters(NamedTuple):
        events: list[trace_model.Event]

    class _Flows(NamedTuple):
        events: list[trace_model.Event]

    class _SchedulingRecords(NamedTuple):
        records: dict[int, list[trace_model.SchedulingRecord]]

    class _Events(NamedTuple):
        events: list[trace_model.Event]

    def __init__(
        self,
        session: PerfettoTraceProcessor,
        patterns: Set[str] | None = None,
        categories: Set[str] | None = None,
    ) -> None:
        self.session = session
        self.patterns = patterns
        self.categories = categories
        self._args_map: dict[int, dict[str, Any]] | None = None

    def _query_as(self, row_type: type[_T], sql: str) -> Iterator[_T]:
        """Executes a SQL query and yields rows as instances of row_type.

        Validates that row_type fields match the SQL query column names.
        """
        cols, row_iter = self.session.query_rows(sql)
        expected_fields = getattr(row_type, "_fields", None)
        if expected_fields is not None and expected_fields != cols:
            raise ValueError(
                f"Query column mismatch for {row_type.__name__}: "
                f"expected fields {expected_fields}, got columns {cols}"
            )
        make = getattr(row_type, "_make", row_type)
        for raw_row in row_iter:
            yield make(raw_row)

    def get_pid_to_name(self) -> _Processes:
        sql = (
            "SELECT DISTINCT pid, COALESCE(name, '') AS name FROM process ORDER"
            " BY name ASC"
        )
        return self._Processes(
            {row.pid: row.name for row in self._query_as(_ProcessRow, sql)}
        )

    def get_threads(self) -> _Threads:
        sql = """
        SELECT
          tid,
          process.pid AS pid,
          COALESCE(thread.name, '') AS name
        FROM thread
        JOIN process USING (upid)
        ORDER BY name ASC
        """
        raw_tid_to_name: dict[int, str] = {}
        raw_tid_to_pid: dict[int, int] = {}
        for row in self._query_as(_ThreadRow, sql):
            raw_tid_to_name[row.tid] = row.name
            raw_tid_to_pid[row.tid] = row.pid

        tid_to_name = {k: v for k, v in raw_tid_to_name.items() if v}
        tid_to_pid = {k: v for k, v in raw_tid_to_pid.items() if k != v}
        return self._Threads(tid_to_name, tid_to_pid)

    def _get_args_map(self) -> dict[int, dict[str, Any]]:
        if self._args_map is not None:
            return self._args_map

        # Filter out serialized binary payloads emitted by Chromium navigation
        # (e.g. debug.params.page_state.data) which contain non-UTF-8 bytes that
        # cause UnicodeDecodeError during protobuf deserialization (b/573890571).
        sql = """
        SELECT
          arg_set_id,
          key,
          int_value,
          real_value,
          string_value
        FROM args
        WHERE key NOT LIKE 'debug.params.%.data'
        """
        arg_set_id_to_args: dict[int, dict[str, Any]] = defaultdict(dict)
        for row in self._query_as(_ArgRow, sql):
            if row.arg_set_id is not None:
                val: Any
                if row.int_value is not None:
                    val = row.int_value
                elif row.real_value is not None:
                    val = row.real_value
                else:
                    val = row.string_value or ""
                arg_set_id_to_args[row.arg_set_id][row.key] = val
        self._args_map = arg_set_id_to_args
        return arg_set_id_to_args

    def get_slices(self) -> _Slices:
        arg_set_id_to_args = self._get_args_map()
        where_clauses: list[str] = []
        if self.categories:
            cats_joined = ", ".join(f"'{c}'" for c in self.categories)
            where_clauses.append(
                f"COALESCE(slice.category, '') IN ({cats_joined})"
            )
        if self.patterns is not None and len(self.patterns) > 0:
            combined_pattern = "|".join(f"({p})" for p in self.patterns)
            where_clauses.append(f"slice.name REGEXP '{combined_pattern}'")

        if where_clauses:
            where_sql = " WHERE " + " OR ".join(where_clauses)
        elif self.patterns is not None and len(self.patterns) == 0:
            where_sql = " WHERE 0"
        else:
            where_sql = ""

        sql = f"""
        SELECT
          slice.id AS id,
          slice.parent_id AS parent_id,
          slice.arg_set_id AS arg_set_id,
          COALESCE(slice.ts, 0) AS ts,
          COALESCE(slice.dur, 0) AS dur,
          COALESCE(slice.category, '') AS category,
          COALESCE(slice.name, '') AS name,
          COALESCE(thread.tid, 0) AS tid,
          COALESCE(process.pid, async_process.pid, 0) AS pid,
          (thread_track.id IS NULL) AS is_async
        FROM slice
        LEFT JOIN track ON slice.track_id = track.id
        LEFT JOIN thread_track ON track.id = thread_track.id
        LEFT JOIN thread ON thread_track.utid = thread.utid
        LEFT JOIN process_track ON track.id = process_track.id
        LEFT JOIN process ON COALESCE(thread.upid, process_track.upid) = process.upid
        LEFT JOIN args ON track.source_arg_set_id = args.arg_set_id AND args.key = 'upid'
        LEFT JOIN process async_process ON args.int_value = async_process.upid
        {where_sql}
        """
        slice_events: list[trace_model.Event] = []
        slice_id_to_event: dict[int, trace_model.Event] = {}
        slice_parent_map: dict[int, int] = {}

        for row in self._query_as(_SliceRow, sql):
            dur_ns = row.dur
            ts_ns = row.ts
            arg_id = row.arg_set_id
            args = (
                arg_set_id_to_args[arg_id]
                if arg_id is not None and arg_id in arg_set_id_to_args
                else {}
            )
            pid = row.pid
            tid = row.tid
            is_async = bool(row.is_async)

            base = trace_model.Event(
                category=row.category,
                name=row.name,
                start=trace_time.TimePoint.from_epoch_delta(
                    trace_time.TimeDelta.from_nanoseconds(float(ts_ns))
                ),
                pid=pid,
                tid=0 if is_async else tid,
                args=args,
            )
            dur_delta = trace_time.TimeDelta.from_nanoseconds(float(dur_ns))
            event: trace_model.Event
            if is_async:
                event = trace_model.AsyncEvent(
                    id=0,
                    duration=dur_delta,
                    base=base,
                )
            elif dur_ns == 0:
                event = trace_model.InstantEvent(
                    scope=trace_model.InstantEventScope.THREAD,
                    base=base,
                )
            else:
                event = trace_model.DurationEvent(
                    duration=dur_delta,
                    parent=None,
                    child_durations=[],
                    child_flows=[],
                    base=base,
                )
            slice_events.append(event)
            slice_id = row.id
            slice_id_to_event[slice_id] = event
            if row.parent_id is not None:
                slice_parent_map[slice_id] = row.parent_id

        return self._Slices(slice_events, slice_id_to_event, slice_parent_map)

    def get_counter_events(self) -> _Counters:
        @dataclasses.dataclass(frozen=True)
        class _CounterKey:
            ts_ns: int
            pid: int
            tid: int
            event_name: str
            track_id: int | None

        @dataclasses.dataclass
        class _CounterGroup:
            name: str
            ts_ns: int
            pid: int
            tid: int
            id: int | None
            args: dict[str, Any]

        if self.patterns is not None and len(self.patterns) > 0:
            combined_pattern = "|".join(f"({p})" for p in self.patterns)
            where_sql = f" WHERE counter_track.name REGEXP '{combined_pattern}'"
        elif self.patterns is not None and len(self.patterns) == 0:
            where_sql = " WHERE 0"
        else:
            where_sql = ""

        sql = f"""
        SELECT
          counter.ts AS ts,
          counter.value AS value,
          counter_track.name AS name,
          COALESCE(process.pid, 0) AS pid,
          COALESCE(thread.tid, 0) AS tid,
          counter.arg_set_id AS arg_set_id,
          counter.id AS id
        FROM counter
        JOIN counter_track ON counter.track_id = counter_track.id
        LEFT JOIN process_counter_track ON counter_track.id = process_counter_track.id
        LEFT JOIN process ON process_counter_track.upid = process.upid
        LEFT JOIN thread_counter_track ON counter_track.id = thread_counter_track.id
        LEFT JOIN thread ON thread_counter_track.utid = thread.utid
        {where_sql}
        ORDER BY counter.ts ASC
        """
        arg_set_id_to_args = self._get_args_map()
        counter_groups: dict[_CounterKey, _CounterGroup] = {}

        for row in self._query_as(_CounterRow, sql):
            ts_ns = row.ts
            val = float(row.value)
            track_name = row.name
            event_name = track_name
            pid = row.pid
            tid = row.tid
            arg_id = row.arg_set_id
            counter_id = row.id
            track_id: int | None = None

            args = (
                dict(arg_set_id_to_args[arg_id])
                if arg_id is not None and arg_id in arg_set_id_to_args
                else {}
            )

            # Fuchsia counter events ingested into Perfetto are named in counter_track
            # by joining category, counter metric name, and track ID with colons (':').
            # For example, a track name like "kmem_stats_a:zram_bytes:0" is created when a
            # counter event (such as category "kmem_stats_a" and name "zram_bytes") is
            # imported into Perfetto.
            # Splitting on colons extracts the base event name (e.g. "kmem_stats_a"), assigns
            # the counter value to the metric argument key (e.g. "zram_bytes"), and extracts
            # any track ID suffix.
            if ":" in track_name:
                parts = track_name.split(":")
                event_name = parts[0]
                arg_key = parts[1] if len(parts) > 1 else "value"
                args[arg_key] = val
                for part in parts[1:]:
                    try:
                        parsed_track_id = int(part)
                        if track_id is None:
                            track_id = parsed_track_id
                        break
                    except ValueError:
                        pass
            else:
                args["value"] = val

            key = _CounterKey(ts_ns, pid, tid, event_name, track_id)
            if key not in counter_groups:
                counter_groups[key] = _CounterGroup(
                    name=event_name,
                    ts_ns=ts_ns,
                    pid=pid,
                    tid=tid,
                    id=counter_id,
                    args=args,
                )
            else:
                counter_groups[key].args.update(args)

        counter_events: list[trace_model.Event] = []
        for group in counter_groups.values():
            base = trace_model.Event(
                category="",
                name=group.name,
                start=trace_time.TimePoint.from_epoch_delta(
                    trace_time.TimeDelta.from_nanoseconds(float(group.ts_ns))
                ),
                pid=group.pid,
                tid=group.tid,
                args=group.args,
            )
            counter_events.append(trace_model.CounterEvent(id=0, base=base))

        return self._Counters(counter_events)

    def get_flow_events(
        self,
        slice_id_to_event: dict[int, trace_model.Event],
        slice_parent_map: dict[int, int],
    ) -> _Flows:
        sql = """
        SELECT
          flow.slice_out AS slice_out,
          flow.slice_in AS slice_in,
          flow.trace_id AS trace_id
        FROM flow
        WHERE flow.trace_id IS NOT NULL
          AND (flow.slice_out IS NOT NULL OR flow.slice_in IS NOT NULL)
        """
        flow_events: list[trace_model.Event] = []
        key_to_flow_event: dict[tuple[int, int], trace_model.FlowEvent] = {}

        def get_or_create_flow_event(
            slice_id: int, flow_trace_id: int, is_out: bool
        ) -> trace_model.FlowEvent | None:
            """Retrieves or creates a FlowEvent for a (slice_id, flow_trace_id) pair.

            If the event already exists, updates its phase to STEP if it acts as both
            an incoming and outgoing node in the flow graph.
            """
            key = (slice_id, flow_trace_id)
            if key in key_to_flow_event:
                existing_flow_event = key_to_flow_event[key]
                # Upgrade phase to STEP if an END event is encountered as outgoing
                # or a START event is encountered as incoming.
                if (
                    is_out
                    and existing_flow_event.phase
                    == trace_model.FlowEventPhase.END
                ):
                    existing_flow_event.phase = trace_model.FlowEventPhase.STEP
                elif (
                    not is_out
                    and existing_flow_event.phase
                    == trace_model.FlowEventPhase.START
                ):
                    existing_flow_event.phase = trace_model.FlowEventPhase.STEP
                return existing_flow_event

            # Skip creation if the underlying slice was filtered out.
            base_event = slice_id_to_event.get(slice_id)
            if base_event is None:
                return None

            # Outgoing slices start as START; incoming slices start as END.
            phase = (
                trace_model.FlowEventPhase.START
                if is_out
                else trace_model.FlowEventPhase.END
            )

            # Associate FlowEvent with its enclosing DurationEvent. If the slice is an
            # InstantEvent (dur == 0) or AsyncEvent (unlikely), walk up the parent chain
            # to locate the enclosing DurationEvent.
            enclosing = None
            curr_id = slice_id
            event = slice_id_to_event.get(curr_id)
            if isinstance(event, trace_model.DurationEvent):
                enclosing = event
            elif slice_parent_map:
                while curr_id in slice_parent_map:
                    curr_id = slice_parent_map[curr_id]
                    event = slice_id_to_event.get(curr_id)
                    if isinstance(event, trace_model.DurationEvent):
                        enclosing = event
                        break

            flow_event = trace_model.FlowEvent(
                id=str(flow_trace_id),
                phase=phase,
                enclosing_duration=enclosing,
                previous_flow=None,
                next_flow=None,
                base=base_event,
            )
            if enclosing is not None:
                enclosing.child_flows.append(flow_event)

            key_to_flow_event[key] = flow_event
            flow_events.append(flow_event)
            return flow_event

        for row in self._query_as(_FlowRow, sql):
            s_out = row.slice_out
            s_in = row.slice_in
            flow_trace_id = row.trace_id

            flow_event_out = (
                get_or_create_flow_event(s_out, flow_trace_id, is_out=True)
                if s_out is not None
                else None
            )
            flow_event_in = (
                get_or_create_flow_event(s_in, flow_trace_id, is_out=False)
                if s_in is not None
                else None
            )

            if (
                flow_event_out is not None
                and flow_event_in is not None
                and s_out != s_in
            ):
                flow_event_out.next_flow = flow_event_in
                flow_event_in.previous_flow = flow_event_out

        return self._Flows(flow_events)

    def get_scheduling_records(
        self,
    ) -> _SchedulingRecords:
        sql = """
        WITH cs AS (
          SELECT
            s.ts AS ts,
            COALESCE(s.cpu, 0) AS cpu,
            COALESCE(s.dur, 0) AS dur,
            COALESCE(s.end_state, '') AS end_state,
            COALESCE(s.priority, 0) AS priority,
            COALESCE(t.tid, 0) AS tid,
            'Running' AS state
          FROM sched s
          LEFT JOIN thread t ON s.utid = t.utid
        ),
        waking AS (
          SELECT
            ts.ts AS ts,
            COALESCE(ts.cpu, ts.ucpu, 0) AS cpu,
            0 AS dur,
            '' AS end_state,
            0 AS priority,
            COALESCE(t.tid, 0) AS tid,
            'W' AS state
          FROM thread_state ts
          JOIN thread t ON ts.utid = t.utid
          WHERE ts.state = 'W'
        )
        SELECT ts, cpu, dur, end_state, priority, tid, state
        FROM (
          SELECT * FROM cs
          UNION ALL
          SELECT * FROM waking
          ORDER BY ts
        )
        """
        scheduling_records: dict[
            int, list[trace_model.SchedulingRecord]
        ] = defaultdict(list)

        END_STATE_MAP: dict[str, trace_model.ThreadState] = {
            "R": trace_model.ThreadState.ZX_THREAD_STATE_RUNNING,
            "S": trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            "D": trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            "T": trace_model.ThreadState.ZX_THREAD_STATE_SUSPENDED,
            "Z": trace_model.ThreadState.ZX_THREAD_STATE_DYING,
            "X": trace_model.ThreadState.ZX_THREAD_STATE_DEAD,
        }
        IDLE_TID = 0
        IDLE_PRIO = trace_model.INT32_MIN

        class CpuState(NamedTuple):
            last_end_ts: int | None
            last_tid: int
            last_prio: int | None
            last_end_state: trace_model.ThreadState

        default_cpu_state = CpuState(
            last_end_ts=None,
            last_tid=IDLE_TID,
            last_prio=None,
            last_end_state=END_STATE_MAP["R"],
        )
        cpu_states: dict[int, CpuState] = {}

        def make_idle_switch(state: CpuState) -> trace_model.ContextSwitch:
            assert state.last_end_ts is not None
            return trace_model.ContextSwitch(
                start=trace_time.TimePoint.from_epoch_delta(
                    trace_time.TimeDelta.from_nanoseconds(
                        float(state.last_end_ts)
                    )
                ),
                incoming_tid=IDLE_TID,
                outgoing_tid=state.last_tid,
                incoming_prio=IDLE_PRIO,
                outgoing_prio=state.last_prio,
                outgoing_state=state.last_end_state,
                args={},
            )

        for row in self._query_as(_SchedRow, sql):
            cpu = row.cpu
            ts_ns = row.ts
            state = row.state

            records = scheduling_records[cpu]
            timestamp = trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_nanoseconds(float(ts_ns))
            )

            cpu_state = cpu_states.get(cpu, default_cpu_state)
            if (
                cpu_state.last_end_ts is not None
                and ts_ns > cpu_state.last_end_ts
                and cpu_state.last_tid != IDLE_TID
            ):
                records.append(make_idle_switch(cpu_state))
                cpu_state = cpu_state._replace(
                    last_tid=IDLE_TID,
                    last_prio=IDLE_PRIO,
                    last_end_state=END_STATE_MAP["R"],
                )
                cpu_states[cpu] = cpu_state

            if state == "W":
                records.append(
                    trace_model.Waking(
                        start=timestamp,
                        tid=row.tid,
                        prio=0,
                        args={},
                    )
                )
                continue

            raw_dur = row.dur
            tid = row.tid
            prio = row.priority

            records.append(
                trace_model.ContextSwitch(
                    start=timestamp,
                    incoming_tid=tid,
                    outgoing_tid=cpu_state.last_tid,
                    incoming_prio=prio,
                    outgoing_prio=cpu_state.last_prio,
                    outgoing_state=cpu_state.last_end_state,
                    args={},
                )
            )

            cpu_states[cpu] = CpuState(
                last_end_ts=ts_ns + raw_dur if raw_dur >= 0 else None,
                last_tid=tid,
                last_prio=prio,
                last_end_state=END_STATE_MAP.get(
                    row.end_state, END_STATE_MAP["R"]
                ),
            )

        for cpu, cpu_state in cpu_states.items():
            if (
                cpu_state.last_end_ts is not None
                and cpu_state.last_tid != IDLE_TID
            ):
                scheduling_records[cpu].append(make_idle_switch(cpu_state))

        return self._SchedulingRecords(scheduling_records)

    def _restore_slice_hierarchy(
        self,
        slice_parent_map: dict[int, int],
        slice_id_to_event: dict[int, trace_model.Event],
    ) -> None:
        """Restores parent-child relationships between DurationEvents in-place.

        Perfetto's `slice` table represents slice nesting via `parent_id` foreign
        keys. However, `trace_model.DurationEvent` objects rely on explicit object-level
        tree relationships (`child.parent` references and `parent_event.child_durations`
        lists) so that metrics processors and model utilities can traverse nested duration
        stacks and sub-events.

        This method iterates over the slice parent mappings to construct those
        bidirectional parent-child links between instanced DurationEvent objects.
        """
        for slice_id, parent_id in slice_parent_map.items():
            if slice_id in slice_id_to_event and parent_id in slice_id_to_event:
                child = slice_id_to_event[slice_id]
                parent_event = slice_id_to_event[parent_id]
                if isinstance(child, trace_model.DurationEvent) and isinstance(
                    parent_event, trace_model.DurationEvent
                ):
                    child.parent = parent_event
                    parent_event.child_durations.append(child)

    def get_events(self) -> _Events:
        slices = self.get_slices()
        counters = self.get_counter_events()
        flows = self.get_flow_events(
            slices.id_to_event,
            slices.parent_map,
        )

        result_events: list[trace_model.Event] = (
            slices.events + counters.events + flows.events
        )

        self._restore_slice_hierarchy(slices.parent_map, slices.id_to_event)

        result_events.sort(
            key=lambda e: (
                e.start,
                0
                if isinstance(e, trace_model.DurationEvent)
                and getattr(e, "duration", None) is not None
                else (
                    1
                    if isinstance(e, trace_model.FlowEvent)
                    else (2 if isinstance(e, trace_model.DurationEvent) else 3)
                ),
            )
        )

        return self._Events(result_events)
