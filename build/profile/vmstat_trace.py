#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Converts vmstat output to various trace formats.

Expects vmstat to have been invoked with -t for the timestamp column.

Usage:
  vmstat.py [options] INPUT > OUTPUT
  INPUT can be '-' to operate as a streamed pipe.
"""

import argparse
import dataclasses
import datetime
import json
import logging
import sys
from pathlib import Path
from typing import Any, Dict, Iterable, Iterator, Optional, Sequence

import trace_tools

_SCRIPT_BASENAME = Path(__file__).name

_LOGGER = logging.getLogger(_SCRIPT_BASENAME)


@dataclasses.dataclass
class ProcessCounts:
    running: int
    blocked: int


@dataclasses.dataclass
class MemoryUsage:
    swap: int
    free: int
    buffers: int
    cache: int
    # inactive: int
    # active: int


@dataclasses.dataclass
class SwapRates:
    bytes_in_per_second: int
    bytes_out_per_second: int


@dataclasses.dataclass
class BlockIORates:
    received_per_second: int
    sent_per_second: int


@dataclasses.dataclass
class SystemEventRates:
    interrupts_per_second: int
    context_switches_per_second: int


@dataclasses.dataclass
class CPUUsage:
    user: int
    system: int
    idle: int
    wait_io: int
    stolen: int
    kvm_guest: int


@dataclasses.dataclass
class VmstatEntry:
    """Represents one line of output from 'vmstat -t'"""

    processes: ProcessCounts
    memory: MemoryUsage
    swap: SwapRates
    block_io: BlockIORates
    system: SystemEventRates
    cpu: CPUUsage
    timestamp: datetime.datetime

    def chrome_trace_events_json(
        self, start_time: datetime.datetime
    ) -> Iterable[trace_tools.TraceEvent]:
        """Yields a set of trace events at a single time."""
        tdelta_us = int(
            (self.timestamp - start_time) / datetime.timedelta(microseconds=1)
        )

        def event(
            name: str, value_type: str, value: Any
        ) -> trace_tools.TraceEvent:
            return trace_tools.event_json(
                name, "system", tdelta_us, value_type, value
            )

        yield event("processes.running", "count", self.processes.running)
        yield event("processes.blocked", "count", self.processes.blocked)

        yield event("memory.swap", "bytes", self.memory.swap)
        yield event("memory.free", "bytes", self.memory.free)
        yield event("memory.buffers", "bytes", self.memory.buffers)
        yield event("memory.cache", "bytes", self.memory.cache)

        yield event(
            "swap.in", "bytes_per_second", self.swap.bytes_in_per_second
        )
        yield event(
            "swap.out", "bytes_per_second", self.swap.bytes_out_per_second
        )

        yield event(
            "block.in", "bytes_per_second", self.block_io.received_per_second
        )
        yield event(
            "block.out", "bytes_per_second", self.block_io.sent_per_second
        )

        yield event(
            "system.interrupts",
            "count_per_second",
            self.system.interrupts_per_second,
        )
        yield event(
            "system.context_switches",
            "count_per_second",
            self.system.context_switches_per_second,
        )

        yield event("cpu.user", "percent", self.cpu.user)
        yield event("cpu.system", "percent", self.cpu.system)
        yield event("cpu.idle", "percent", self.cpu.idle)
        yield event("cpu.wait_io", "percent", self.cpu.wait_io)
        yield event("cpu.stolen", "percent", self.cpu.stolen)
        yield event("cpu.kvm_guest", "percent", self.cpu.kvm_guest)


def _parse_data_row(
    line: str, field_map: Dict[str, int]
) -> Optional[VmstatEntry]:
    if not line:
        return None

    columns = line.split()

    def get_int(name: str, default: int = 0) -> int:
        idx = field_map.get(name)
        if idx is not None and idx < len(columns):
            val = columns[idx]
            try:
                return int(val)
            except ValueError:
                _LOGGER.error(
                    f"Failed to parse integer for field '{name}' from value '{val}' in line: {line}"
                )
                return default
        return default

    # Timestamp handling.
    # vmstat -t output usually appends the timestamp at the end.
    # The header has "UTC" or "timestamp" as the last field name.
    # In the data row, this might span multiple words (Date and Time).
    ts_idx = field_map.get("UTC") or field_map.get("timestamp")
    if ts_idx is not None and ts_idx < len(columns):
        timestamp_text = " ".join(columns[ts_idx:])
        try:
            timestamp = datetime.datetime.strptime(
                timestamp_text, "%Y-%m-%d %H:%M:%S"
            )
        except ValueError:
            _LOGGER.error(
                f"Failed to parse timestamp '{timestamp_text}' in line: {line}"
            )
            # Maybe different format or missing?
            timestamp = datetime.datetime.min
    else:
        timestamp = datetime.datetime.min

    return VmstatEntry(
        processes=ProcessCounts(
            running=get_int("r"),
            blocked=get_int("b"),
        ),
        memory=MemoryUsage(
            swap=get_int("swpd"),
            free=get_int("free"),
            buffers=get_int("buff"),
            cache=get_int("cache"),
        ),
        swap=SwapRates(
            bytes_in_per_second=get_int("si"),
            bytes_out_per_second=get_int("so"),
        ),
        block_io=BlockIORates(
            received_per_second=get_int("bi"),
            sent_per_second=get_int("bo"),
        ),
        system=SystemEventRates(
            interrupts_per_second=get_int("in"),
            context_switches_per_second=get_int("cs"),
        ),
        cpu=CPUUsage(
            user=get_int("us"),
            system=get_int("sy"),
            idle=get_int("id"),
            wait_io=get_int("wa"),
            stolen=get_int("st"),
            kvm_guest=get_int("gu"),
        ),
        timestamp=timestamp,
    )


def _parse_vmstat_output(lines: Iterable[str]) -> Iterator[VmstatEntry]:
    # Expect that vmstat always prints a header row before any rows of
    # data.  Reuse the field_map until the next header row, because
    # vmstat only prints a header row periodically.
    field_map: Optional[Dict[str, int]] = None
    for line in lines:
        stripped_line = line.strip()
        if not stripped_line:
            continue
        if stripped_line.startswith("#"):  # comment
            continue

        fields = stripped_line.split()
        if not fields:
            continue

        if fields[0] in ("procs", "--procs--"):  # header categories
            continue

        if fields[:2] == ["r", "b"]:
            # header fields, starting with running/blocked processes
            # treat consecutive whitespace as single separator
            field_map = {name: i for i, name in enumerate(fields)}
            continue

        # In case there was some text appearing before the first header row,
        # ignore until we establish a row header.
        if field_map is None:
            _LOGGER.warning(f"Skipping line before header was found: {line}")
            continue

        # else is a line of trace data, and we have a field map established.
        entry = _parse_data_row(stripped_line, field_map)
        if entry:
            yield entry


def print_chrome_trace_json(
    trace: Iterator[VmstatEntry],
) -> Iterable[trace_tools.TraceEvent]:
    try:
        first: VmstatEntry = next(trace)
    except StopIteration:
        # if trace is empty, abort
        return

    start_time = first.timestamp
    yield from first.chrome_trace_events_json(start_time)

    # The remainder
    for t in trace:
        yield from t.chrome_trace_events_json(start_time)


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--metadata",
        type=str,
        help="Metadata in the form: KEY1:VALUE1,KEY2:VALUE2,...",
    )
    parser.add_argument(
        "--parser-log",
        type=Path,
        help="Path to a file where parser errors will be logged.",
    )
    parser.add_argument(
        # positional argument
        "input",
        type=Path,
        help="text file of 'vmstat -t' output.  Pass '-' to read from stdin.",
    )
    return parser


_MAIN_ARG_PARSER = _main_arg_parser()


def main(argv: Sequence[str]) -> int:
    args = _MAIN_ARG_PARSER.parse_args(argv)

    if args.parser_log:
        logging.basicConfig(
            filename=args.parser_log,
            filemode="w",
            format="%(asctime)s %(levelname)s: %(message)s",
            level=logging.INFO,
        )

    if args.input == Path("-"):
        vmstat_lines = sys.stdin  # is Iterable[str]
    else:
        vmstat_lines = args.input.read_text().splitlines()

    metadata = trace_tools.metadata_arg_to_dict(args.metadata)

    vmstat_entries = _parse_vmstat_output(vmstat_lines)
    trace = trace_tools.complete_trace(
        metadata, list(print_chrome_trace_json(vmstat_entries))
    )

    print(json.dumps(trace, indent=2))

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
