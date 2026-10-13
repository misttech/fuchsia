# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia FXT trace binary builder for generating in-memory and file-based traces."""

import hashlib
import io
import json
import os
import struct
from typing import Any, BinaryIO, Self


class Builder:
    """High-level builder for constructing Fuchsia binary FXT traces.

    Handles inline string references, 8-byte word alignments, thread table
    indexing, and kernel object registration.

    For format details, see:
    https://fuchsia.dev/fuchsia-src/reference/tracing/format/records
    """

    def __init__(self, ticks_per_second: int = 1000000000) -> None:
        """Initialize a new Builder with an FXT magic and initialization record.

        Args:
            ticks_per_second: The clock tick frequency to write into the FXT
                initialization record. Defaults to 1,000,000,000 (1 GHz,
                nanosecond resolution).
        """
        self._ticks_per_second = ticks_per_second
        self._tid_to_idx: dict[tuple[int, int], int] = {}
        self._next_thread_idx: int = 1
        self._registered_pids: set[int] = set()
        self._registered_tids: set[int] = set()

        self._buf = io.BytesIO()
        self._buf.write(Builder._pack_magic())
        self._buf.write(
            Builder._pack_initialization_record(self._ticks_per_second)
        )

    @staticmethod
    def _align(size: int) -> int:
        return (size + 7) & ~7

    @staticmethod
    def _pad_bytes(b: bytes) -> bytes:
        aligned_size = Builder._align(len(b))
        return b + b"\x00" * (aligned_size - len(b))

    @staticmethod
    def _make_string_ref(s: str) -> int:
        if not s:
            return 0
        length = len(s.encode("utf-8"))
        if length > 0x7FFF:
            raise ValueError(
                f"String too long ({length} bytes > 32767): {s[:50]}..."
            )
        return 0x8000 | length

    @staticmethod
    def _pack_word(val: int) -> bytes:
        return struct.pack("<Q", val & 0xFFFFFFFFFFFFFFFF)

    @staticmethod
    def _pack_magic() -> bytes:
        return struct.pack("<Q", 0x0016547846040010)

    @staticmethod
    def _parse_correlation_id(id_val: Any) -> int:
        if isinstance(id_val, int):
            return id_val
        if isinstance(id_val, str):
            try:
                return int(id_val, 0)
            except ValueError:
                return int(
                    hashlib.sha256(id_val.encode("utf-8")).hexdigest()[:16],
                    16,
                )
        return 0

    @staticmethod
    def _pack_initialization_record(ticks_per_second: int) -> bytes:
        header = 1 | (2 << 4)
        return Builder._pack_word(header) + Builder._pack_word(ticks_per_second)

    @staticmethod
    def _pack_kernel_object_record(
        koid: int,
        object_type: int,
        name: str,
        args: dict[str, Any] | None = None,
    ) -> bytes:
        if args is None:
            args = {}

        name_ref = Builder._make_string_ref(name)
        name_bytes = name.encode("utf-8")
        words = 2 + Builder._align(len(name_bytes)) // 8

        args_bytes = b""
        num_args = 0
        for k, v in args.items():
            if num_args >= 15:
                break
            arg_b = Builder._pack_argument(k, v)
            if arg_b:
                args_bytes += arg_b
                num_args += 1

        words += len(args_bytes) // 8
        if words > 0xFFF:
            raise ValueError(
                f"Record exceeds maximum FXT size of 4095 words: {words}"
            )

        header = (
            7
            | (words << 4)
            | (object_type << 16)
            | (name_ref << 24)
            | (num_args << 40)
        )

        res = Builder._pack_word(header)
        res += Builder._pack_word(koid)
        res += Builder._pad_bytes(name_bytes)
        res += args_bytes
        return res

    @staticmethod
    def _pack_thread_record(thread_index: int, pid: int, tid: int) -> bytes:
        header = 3 | (3 << 4) | (thread_index << 16)
        res = Builder._pack_word(header)
        res += Builder._pack_word(pid)
        res += Builder._pack_word(tid)
        return res

    @staticmethod
    def _pack_argument(name: str, value: Any) -> bytes:
        name_ref = Builder._make_string_ref(name)
        name_bytes = name.encode("utf-8")
        name_words = Builder._align(len(name_bytes)) // 8

        if isinstance(value, bool):
            words = 1 + name_words
            val_bit = 1 if value else 0
            header = 9 | (words << 4) | (name_ref << 16) | (val_bit << 32)
            return Builder._pack_word(header) + Builder._pad_bytes(name_bytes)

        if name == "process" and isinstance(value, int):
            words = 2 + name_words
            header = 8 | (words << 4) | (name_ref << 16)
            return (
                Builder._pack_word(header)
                + Builder._pad_bytes(name_bytes)
                + Builder._pack_word(value)
            )

        if isinstance(value, int) and -0x80000000 <= value <= 0x7FFFFFFF:
            words = 1 + name_words
            header = (
                1
                | (words << 4)
                | (name_ref << 16)
                | ((value & 0xFFFFFFFF) << 32)
            )
            return Builder._pack_word(header) + Builder._pad_bytes(name_bytes)
        elif isinstance(value, int):
            words = 2 + name_words
            header = 3 | (words << 4) | (name_ref << 16)
            return (
                Builder._pack_word(header)
                + Builder._pad_bytes(name_bytes)
                + Builder._pack_word(value)
            )
        elif isinstance(value, float):
            words = 2 + name_words
            header = 5 | (words << 4) | (name_ref << 16)
            return (
                Builder._pack_word(header)
                + Builder._pad_bytes(name_bytes)
                + struct.pack("<d", value)
            )
        elif isinstance(value, str):
            val_ref = Builder._make_string_ref(value)
            val_bytes = value.encode("utf-8")
            val_words = Builder._align(len(val_bytes)) // 8
            words = 1 + name_words + val_words

            header = 6 | (words << 4) | (name_ref << 16) | (val_ref << 32)
            return (
                Builder._pack_word(header)
                + Builder._pad_bytes(name_bytes)
                + Builder._pad_bytes(val_bytes)
            )
        return b""

    def _ensure_thread(self, pid: int, tid: int) -> int:
        if (pid, tid) not in self._tid_to_idx:
            if self._next_thread_idx > 255:
                raise ValueError("Exceeded maximum FXT thread index (255)")
            self._tid_to_idx[(pid, tid)] = self._next_thread_idx
            self._buf.write(
                Builder._pack_thread_record(self._next_thread_idx, pid, tid)
            )
            self._next_thread_idx += 1
        return self._tid_to_idx[(pid, tid)]

    def add_process(self, pid: int, name: str) -> Self:
        """Register a process kernel object record.

        Args:
            pid: The process ID (koid).
            name: The display name of the process.

        Returns:
            The builder instance for method chaining.
        """
        if pid not in self._registered_pids:
            self._buf.write(Builder._pack_kernel_object_record(pid, 1, name))
            self._registered_pids.add(pid)
        return self

    def add_thread(self, pid: int, tid: int, name: str) -> Self:
        """Register a thread kernel object record and assign a thread table index.

        Args:
            pid: The parent process ID (koid).
            tid: The thread ID (koid).
            name: The display name of the thread.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        self._ensure_thread(pid, tid)
        if tid not in self._registered_tids:
            self._buf.write(
                Builder._pack_kernel_object_record(
                    tid, 2, name, {"process": pid}
                )
            )
            self._registered_tids.add(tid)
        return self

    @staticmethod
    def _pack_event_record(
        event_type: int,
        timestamp_ns: int,
        thread_ref: int,
        category: str,
        name: str,
        args: dict[str, Any] | None = None,
        extra_word: int | None = None,
    ) -> bytes:
        if args is None:
            args = {}

        cat_ref = Builder._make_string_ref(category)
        name_ref = Builder._make_string_ref(name)
        cat_bytes = category.encode("utf-8")
        name_bytes = name.encode("utf-8")

        words = 2 + (1 if extra_word is not None else 0)
        words += Builder._align(len(cat_bytes)) // 8
        words += Builder._align(len(name_bytes)) // 8

        args_bytes = b""
        num_args = 0
        for k, v in args.items():
            if num_args >= 15:
                break
            arg_b = Builder._pack_argument(k, v)
            if arg_b:
                args_bytes += arg_b
                num_args += 1

        words += len(args_bytes) // 8
        if words > 0xFFF:
            raise ValueError(
                f"Record exceeds maximum FXT size of 4095 words: {words}"
            )

        header = (
            4
            | (words << 4)
            | (event_type << 16)
            | (num_args << 20)
            | (thread_ref << 24)
            | (cat_ref << 32)
            | (name_ref << 48)
        )

        res = Builder._pack_word(header)
        res += Builder._pack_word(timestamp_ns)
        res += Builder._pad_bytes(cat_bytes)
        res += Builder._pad_bytes(name_bytes)
        res += args_bytes
        if extra_word is not None:
            res += Builder._pack_word(extra_word)
        return res

    def add_duration_begin(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Duration Begin event (type 2).

        Args:
            category: The event category.
            name: The event name.
            timestamp_ns: The timestamp of the start of the duration in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                2, timestamp_ns, thread_ref, category, name, args
            )
        )
        return self

    def add_duration_end(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Duration End event (type 3).

        Args:
            category: The event category.
            name: The event name.
            timestamp_ns: The timestamp of the end of the duration in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                3, timestamp_ns, thread_ref, category, name, args
            )
        )
        return self

    def add_duration_complete(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        duration_ns: int,
        pid: int,
        tid: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Duration Complete event (type 4).

        Args:
            category: The event category.
            name: The event name.
            timestamp_ns: The start timestamp in nanoseconds.
            duration_ns: The duration in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        end_ns = timestamp_ns + duration_ns
        self._buf.write(
            Builder._pack_event_record(
                4,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=end_ns,
            )
        )
        return self

    def add_instant(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        scope: str = "t",
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit an Instant event (type 0) with a defined scope.

        Args:
            category: The event category.
            name: The event name.
            timestamp_ns: The timestamp of the instant event in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            scope: The event scope. Must be 't'/'thread', 'p'/'process', or
                'g'/'global'. Defaults to 't'.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)

        scope_val = 0  # Thread
        if scope in ("p", "process"):
            scope_val = 1  # Process
        elif scope in ("g", "global"):
            scope_val = 2  # Global

        self._buf.write(
            Builder._pack_event_record(
                0,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=scope_val,
            )
        )
        return self

    def add_counter(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        args: dict[str, Any],
        counter_id: int | None = None,
    ) -> Self:
        """Emit Counter event(s) (type 1) for one or more metric key/value pairs.

        Args:
            category: The event category.
            name: The counter track name.
            timestamp_ns: The timestamp of the counter sample in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            args: Dictionary mapping metric argument names to numeric values.
            counter_id: Optional explicit counter 64-bit ID. When None, a stable
                hash of the category and name is computed automatically.
                Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        if counter_id is None:
            hasher = hashlib.md5()
            hasher.update(category.encode("utf-8"))
            hasher.update(name.encode("utf-8"))
            counter_id = (
                struct.unpack(">Q", hasher.digest()[:8])[0] & 0x7FFFFFFFFFFFFFFF
            )

        cat_ref = Builder._make_string_ref(category)
        name_ref = Builder._make_string_ref(name)
        cat_bytes = category.encode("utf-8")
        name_bytes = name.encode("utf-8")

        for k, v in args.items():
            arg_b = Builder._pack_argument(k, v)
            if not arg_b:
                continue

            words = 3
            words += Builder._align(len(cat_bytes)) // 8
            words += Builder._align(len(name_bytes)) // 8
            words += len(arg_b) // 8
            if words > 0xFFF:
                raise ValueError(
                    f"Record exceeds maximum FXT size of 4095 words: {words}"
                )

            header = (
                4
                | (words << 4)
                | (1 << 16)  # Counter event type
                | (1 << 20)  # 1 argument
                | (thread_ref << 24)
                | (cat_ref << 32)
                | (name_ref << 48)
            )

            res = Builder._pack_word(header)
            res += Builder._pack_word(timestamp_ns)
            res += Builder._pad_bytes(cat_bytes)
            res += Builder._pad_bytes(name_bytes)
            res += arg_b
            res += Builder._pack_word(counter_id)
            self._buf.write(res)

        return self

    def add_async_begin(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        async_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit an Async Begin event (type 5).

        Args:
            category: The event category.
            name: The async slice name.
            timestamp_ns: The timestamp of the start of the async slice in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            async_id: The correlation ID for the async operation.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                5,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=async_id,
            )
        )
        return self

    def add_async_instant(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        async_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit an Async Instant event (type 6).

        Args:
            category: The event category.
            name: The async instant event name.
            timestamp_ns: The timestamp in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            async_id: The correlation ID for the async operation.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                6,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=async_id,
            )
        )
        return self

    def add_async_end(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        async_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit an Async End event (type 7).

        Args:
            category: The event category.
            name: The async slice name.
            timestamp_ns: The timestamp of the end of the async slice in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            async_id: The correlation ID for the async operation.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                7,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=async_id,
            )
        )
        return self

    def add_flow_begin(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        flow_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Flow Begin event (type 8).

        Args:
            category: The event category.
            name: The flow name.
            timestamp_ns: The timestamp in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            flow_id: The correlation ID for the flow.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                8,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=flow_id,
            )
        )
        return self

    def add_flow_step(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        flow_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Flow Step event (type 9).

        Args:
            category: The event category.
            name: The flow name.
            timestamp_ns: The timestamp in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            flow_id: The correlation ID for the flow.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                9,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=flow_id,
            )
        )
        return self

    def add_flow_end(
        self,
        category: str,
        name: str,
        timestamp_ns: int,
        pid: int,
        tid: int,
        flow_id: int,
        args: dict[str, Any] | None = None,
    ) -> Self:
        """Emit a Flow End event (type 10).

        Args:
            category: The event category.
            name: The flow name.
            timestamp_ns: The timestamp in nanoseconds.
            pid: The process ID.
            tid: The thread ID.
            flow_id: The correlation ID for the flow.
            args: Optional dictionary of event arguments. Defaults to None.

        Returns:
            The builder instance for method chaining.

        Raises:
            ValueError: If the total number of distinct threads exceeds 255.
        """
        thread_ref = self._ensure_thread(pid, tid)
        self._buf.write(
            Builder._pack_event_record(
                10,
                timestamp_ns,
                thread_ref,
                category,
                name,
                args,
                extra_word=flow_id,
            )
        )
        return self

    def add_context_switch(
        self,
        timestamp_ns: int,
        cpu_number: int,
        outgoing_thread_state: int,
        outgoing_tid: int,
        incoming_tid: int,
        outgoing_priority: int = 0,
        incoming_priority: int = 0,
    ) -> Self:
        """Emit a Context Switch Scheduler event (Record Type 8).

        Args:
            timestamp_ns: The timestamp of the context switch in nanoseconds.
            cpu_number: The CPU index where the context switch occurred.
            outgoing_thread_state: The state of the outgoing thread (e.g. 1=Ready,
                2=Suspended, 3=Blocked).
            outgoing_tid: The TID of the thread switching out.
            incoming_tid: The TID of the thread switching in.
            outgoing_priority: Priority of the outgoing thread. Defaults to 0.
            incoming_priority: Priority of the incoming thread. Defaults to 0.

        Returns:
            The builder instance for method chaining.
        """
        args_bytes = b""
        num_args = 0

        arg_b = Builder._pack_argument("incoming_weight", incoming_priority)
        if arg_b:
            args_bytes += arg_b
            num_args += 1

        arg_b = Builder._pack_argument("outgoing_weight", outgoing_priority)
        if arg_b:
            args_bytes += arg_b
            num_args += 1

        words = 4 + len(args_bytes) // 8
        header = (
            8
            | (words << 4)
            | (num_args << 16)
            | (cpu_number << 20)
            | (outgoing_thread_state << 36)
            | (1 << 60)
        )
        res = Builder._pack_word(header)
        res += Builder._pack_word(timestamp_ns)
        res += Builder._pack_word(outgoing_tid)
        res += Builder._pack_word(incoming_tid)
        res += args_bytes
        self._buf.write(res)
        return self

    def add_thread_wakeup(
        self, timestamp_ns: int, cpu_number: int, tid: int
    ) -> Self:
        """Emit a Thread Wakeup Scheduler event (Record Type 8).

        Args:
            timestamp_ns: The timestamp of the wakeup in nanoseconds.
            cpu_number: The CPU index.
            tid: The thread ID that was awakened.

        Returns:
            The builder instance for method chaining.
        """
        header = 8 | (3 << 4) | (0 << 16) | (cpu_number << 20) | (2 << 60)
        res = Builder._pack_word(header)
        res += Builder._pack_word(timestamp_ns)
        res += Builder._pack_word(tid)
        self._buf.write(res)
        return self

    def to_bytes(self) -> bytes:
        """Return the accumulated binary FXT trace data.

        Returns:
            A bytes object containing the complete serialized FXT trace.
        """
        return self._buf.getvalue()

    def write_to_stream(self, stream: BinaryIO) -> None:
        """Write the binary FXT trace data to an open binary stream.

        Args:
            stream: A writable binary I/O stream (e.g. io.BytesIO or binary file).
        """
        stream.write(self.to_bytes())

    def write_to_file(self, path: str | os.PathLike[Any]) -> None:
        """Write the binary FXT trace data to a file on disk.

        Args:
            path: Destination file path.
        """
        with open(path, "wb") as f:
            f.write(self.to_bytes())

    @classmethod
    def from_json_dict(
        cls, data: dict[str, Any], ticks_per_second: int = 1000000000
    ) -> Self:
        """Construct a Builder populated from Chrome JSON trace data.

        Args:
            data: Parsed dictionary representation of a Chrome JSON trace, containing
                optional 'systemTraceEvents' and 'traceEvents' lists.
            ticks_per_second: Clock frequency for the trace in ticks per second.
                Defaults to 1,000,000,000 (1 GHz).

        Returns:
            A Builder instance containing all translated trace records.
        """
        builder = cls(ticks_per_second=ticks_per_second)

        # 1. Process systemTraceEvents
        system_events = (data.get("systemTraceEvents") or {}).get(
            "events", []
        ) or []
        for ev in system_events:
            ph = ev.get("ph")
            ts_ns = round(ev.get("ts", 0) * 1000)
            pid = ev.get("pid", 0)
            tid = ev.get("tid", 0)

            if ph == "p":
                builder.add_process(pid, ev.get("name", ""))
            elif ph == "t":
                builder.add_thread(pid, tid, ev.get("name", ""))
            elif ph == "w":
                builder.add_thread_wakeup(ts_ns, ev.get("cpu", 0), tid)
            elif ph == "k":
                cpu = ev.get("cpu", 0)
                out_dict = ev.get("out") or {}
                in_dict = ev.get("in") or {}
                out_tid = out_dict.get("tid", 0)
                in_tid = in_dict.get("tid", 0)
                out_prio = out_dict.get("prio", 0)
                in_prio = in_dict.get("prio", 0)
                out_state = out_dict.get("state", 0)
                builder.add_context_switch(
                    ts_ns, cpu, out_state, out_tid, in_tid, out_prio, in_prio
                )

        # 2. Process traceEvents
        for ev in data.get("traceEvents") or []:
            ph = ev.get("ph")
            ts_ns = round(ev.get("ts", 0) * 1000)
            pid = ev.get("pid", 0)
            tid = ev.get("tid", 0)
            cat = ev.get("cat", "")
            name = ev.get("name", "")

            if ph == "M":
                name_attr = ev.get("name")
                args_dict = ev.get("args") or {}
                if name_attr == "process_name":
                    builder.add_process(pid, args_dict.get("name", ""))
                elif name_attr == "thread_name":
                    builder.add_thread(pid, tid, args_dict.get("name", ""))
                continue

            if ph == "B":
                builder.add_duration_begin(
                    cat, name, ts_ns, pid, tid, ev.get("args")
                )
            elif ph == "E":
                builder.add_duration_end(
                    cat, name, ts_ns, pid, tid, ev.get("args")
                )
            elif ph == "X":
                dur_ns = round(ev.get("dur", 0) * 1000)
                builder.add_duration_complete(
                    cat, name, ts_ns, dur_ns, pid, tid, ev.get("args")
                )
            elif ph in ("i", "I"):
                scope = ev.get("s", "t")
                builder.add_instant(
                    cat,
                    name,
                    ts_ns,
                    pid,
                    tid,
                    scope=scope,
                    args=ev.get("args"),
                )
            elif ph == "C":
                args = ev.get("args") or {}
                for k, v in args.items():
                    compound_name = f"{name}:{k}:0"
                    builder.add_counter(
                        cat,
                        compound_name,
                        ts_ns,
                        pid,
                        tid,
                        {"value": v},
                        counter_id=0,
                    )
            elif ph == "b":
                async_id = cls._parse_correlation_id(ev.get("id", 0))
                builder.add_async_begin(cat, name, ts_ns, pid, tid, async_id)
            elif ph == "e":
                async_id = cls._parse_correlation_id(ev.get("id", 0))
                builder.add_async_end(cat, name, ts_ns, pid, tid, async_id)
            elif ph == "s":
                flow_id = cls._parse_correlation_id(ev.get("id", 0))
                builder.add_flow_begin(cat, name, ts_ns, pid, tid, flow_id)
            elif ph == "t":
                flow_id = cls._parse_correlation_id(ev.get("id", 0))
                builder.add_flow_step(cat, name, ts_ns, pid, tid, flow_id)
            elif ph == "f":
                flow_id = cls._parse_correlation_id(ev.get("id", 0))
                builder.add_flow_end(cat, name, ts_ns, pid, tid, flow_id)

        return builder

    @classmethod
    def from_json_string(
        cls, json_str: str, ticks_per_second: int = 1000000000
    ) -> Self:
        """Construct a Builder from a JSON trace string.

        Args:
            json_str: A JSON-encoded string containing trace data.
            ticks_per_second: Clock frequency for the trace in ticks per second.
                Defaults to 1,000,000,000 (1 GHz).

        Returns:
            A Builder instance containing all translated trace records.

        Raises:
            json.JSONDecodeError: If json_str is not valid JSON.
        """
        return cls.from_json_dict(
            json.loads(json_str), ticks_per_second=ticks_per_second
        )
