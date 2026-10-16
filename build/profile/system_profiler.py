#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Linux-specific system-wide and process-tree resource profiler.

Outputs a consolidated JSON trace file compatible with chrome://tracing.
"""

import argparse
import datetime
import json
import signal
import sys
import time
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from pathlib import Path
from typing import Any

# Ensure fuchsia root is in sys.path
_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.profile import procfs

# Type alias representing a Chrome trace event dictionary
TraceEvent = dict[str, Any]


# --- Pure, Module-Scope Utility Functions ---


def make_counter_event(
    name: str, category: str, ts_us: int, unit: str, value: Any
) -> TraceEvent:
    """Helper to construct a standardized Chrome trace counter event dictionary.

    Args:
        name: The metric name (e.g., 'cpu.user').
        category: The category string (e.g., 'system', 'network').
        ts_us: Timestamp in microseconds since the start of profiling.
        unit: The unit name of the value (e.g., 'percent', 'bytes', 'count').
        value: The numerical metric value.

    Returns:
        A dictionary matching the Chrome Trace Counter Event schema.
    """
    return {
        "name": name,
        "cat": category,
        "ph": "C",
        "pid": 1,
        "tid": 1,
        "ts": ts_us,
        "args": {unit: value},
    }


# --- Pure, Module-Scope Parsing Functions (Decoupled from I/O and self-state) ---


# --- Pluggable Metric Sampler Base Interface ---


class MetricSampler:
    """Interface for isolated, state-encapsulated metric pollers."""

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        """Polls raw performance stats and yields Chrome Trace counter events.

        Args:
            ts_us: Microseconds since start of session.
            elapsed: Seconds elapsed since previous polling loop.
        """
        raise NotImplementedError("Subclasses must implement sample")


# --- Concrete, Pluggable Sampler Implementations ---


class CpuAndSystemSampler(MetricSampler):
    """Sampler for CPU utilization, context switches, interrupts, and task counts.

    Polls '/proc/stat' and tracks internal state to compute relative CPU state
    percentages, system-wide context switches per second, and interrupts per second.

    Metrics yielded:
        - cpu.user, cpu.system, cpu.idle, cpu.wait_io, cpu.stolen, cpu.kvm_guest (percent)
        - processes.running, processes.blocked (count)
        - system.interrupts (count_per_second)
        - system.context_switches (count_per_second)
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir
        self._prev_cpu_ticks: list[int] | None = None
        self._prev_intr: int | None = None
        self._prev_ctxt: int | None = None

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "stat", "r") as f:
                lines = f.readlines()
        except IOError:
            return

        data = procfs.parse_proc_stat(lines)

        # 1. CPU utilisation tick percentages
        if data.cpu_ticks:
            ticks = data.cpu_ticks
            if elapsed and self._prev_cpu_ticks:
                deltas = [t - p for t, p in zip(ticks, self._prev_cpu_ticks)]
                total_delta = sum(deltas)
                if total_delta > 0:
                    yield make_counter_event(
                        "cpu.user",
                        "system",
                        ts_us,
                        "percent",
                        (deltas[0] + deltas[1]) / total_delta * 100,
                    )
                    yield make_counter_event(
                        "cpu.system",
                        "system",
                        ts_us,
                        "percent",
                        deltas[2] / total_delta * 100,
                    )
                    yield make_counter_event(
                        "cpu.idle",
                        "system",
                        ts_us,
                        "percent",
                        deltas[3] / total_delta * 100,
                    )
                    yield make_counter_event(
                        "cpu.wait_io",
                        "system",
                        ts_us,
                        "percent",
                        deltas[4] / total_delta * 100,
                    )
                    if len(deltas) > 7:
                        yield make_counter_event(
                            "cpu.stolen",
                            "system",
                            ts_us,
                            "percent",
                            deltas[7] / total_delta * 100,
                        )
                    if len(deltas) > 8:
                        yield make_counter_event(
                            "cpu.kvm_guest",
                            "system",
                            ts_us,
                            "percent",
                            deltas[8] / total_delta * 100,
                        )
            self._prev_cpu_ticks = ticks

        # 2. Running/Blocked processes
        if data.procs_running is not None:
            yield make_counter_event(
                "processes.running",
                "system",
                ts_us,
                "count",
                data.procs_running,
            )
        if data.procs_blocked is not None:
            yield make_counter_event(
                "processes.blocked",
                "system",
                ts_us,
                "count",
                data.procs_blocked,
            )

        # 3. System interrupts
        if data.intr is not None:
            total_intr = data.intr
            if elapsed and self._prev_intr is not None:
                yield make_counter_event(
                    "system.interrupts",
                    "system",
                    ts_us,
                    "count_per_second",
                    (total_intr - self._prev_intr) / elapsed,
                )
            self._prev_intr = total_intr

        # 4. Context switches
        if data.ctxt is not None:
            total_ctxt = data.ctxt
            if elapsed and self._prev_ctxt is not None:
                yield make_counter_event(
                    "system.context_switches",
                    "system",
                    ts_us,
                    "count_per_second",
                    (total_ctxt - self._prev_ctxt) / elapsed,
                )
            self._prev_ctxt = total_ctxt


class MemorySampler(MetricSampler):
    """Sampler for system-wide memory allocation states from /proc/meminfo.

    Polls '/proc/meminfo' on each tick and parses various memory usage categories
    in bytes. This class is stateless since it only reports absolute, current quantities.

    Metrics yielded:
        - memory.free: Available physical RAM bytes.
        - memory.buffers: Disk block buffer cache bytes.
        - memory.cache: Page cache RAM bytes (incorporating SReclaimable, subtracting Shmem).
        - memory.swap_used: Swapped memory bytes.
        - memory.dirty: RAM bytes awaiting writeback to disk.
        - memory.writeback: RAM bytes actively being written back to disk.
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "meminfo", "r") as f:
                lines = f.readlines()
        except IOError:
            return

        data = procfs.parse_proc_meminfo(lines)
        yield make_counter_event(
            "memory.free", "system", ts_us, "bytes", data.free
        )
        yield make_counter_event(
            "memory.buffers", "system", ts_us, "bytes", data.buffers
        )
        yield make_counter_event(
            "memory.cache", "system", ts_us, "bytes", data.cached
        )
        yield make_counter_event(
            "memory.swap_used", "system", ts_us, "bytes", data.swap_used
        )
        yield make_counter_event(
            "memory.dirty", "system", ts_us, "bytes", data.dirty
        )
        yield make_counter_event(
            "memory.writeback", "system", ts_us, "bytes", data.writeback
        )


class DiskIoAndPagingSampler(MetricSampler):
    """Sampler for system-wide virtual memory paging and swap rates from /proc/vmstat.

    Polls '/proc/vmstat' on each tick and computes average disk paging and swap I/O
    bandwidth in bytes per second by tracking historical counters across polls.

    Metrics yielded:
        - block.in: Disk blocks read into memory (bytes_per_second).
        - block.out: Disk blocks written out of memory (bytes_per_second).
        - swap.in: Swapped-in pages from swap space (bytes_per_second).
        - swap.out: Swapped-out pages to swap space (bytes_per_second).
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir
        self._prev_pgpg: tuple[int, int] | None = None
        self._prev_pswp: tuple[int, int] | None = None

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "vmstat", "r") as f:
                lines = f.readlines()
        except IOError:
            return

        data = procfs.parse_proc_vmstat(lines)

        if elapsed and self._prev_pgpg:
            yield make_counter_event(
                "block.in",
                "system",
                ts_us,
                "bytes_per_second",
                (data.pgpgin - self._prev_pgpg[0]) / elapsed,
            )
            yield make_counter_event(
                "block.out",
                "system",
                ts_us,
                "bytes_per_second",
                (data.pgpgout - self._prev_pgpg[1]) / elapsed,
            )
        if elapsed and self._prev_pswp:
            yield make_counter_event(
                "swap.in",
                "system",
                ts_us,
                "bytes_per_second",
                (data.pswpin - self._prev_pswp[0]) / elapsed,
            )
            yield make_counter_event(
                "swap.out",
                "system",
                ts_us,
                "bytes_per_second",
                (data.pswpout - self._prev_pswp[1]) / elapsed,
            )

        self._prev_pgpg = (data.pgpgin, data.pgpgout)
        self._prev_pswp = (data.pswpin, data.pswpout)


class FileDescriptorSampler(MetricSampler):
    """Sampler for system-wide and process-tree open file descriptor counts.

    Polls '/proc/sys/fs/file-nr' to track system-wide allocated file descriptors,
    and optionally walks the descendant process tree of a given target PID to
    sum all open files under '/proc/[PID]/fd/'.

    Metrics yielded:
        - fds.system: Total allocated file descriptors system-wide (count).
        - fds.process: Sum of open file descriptors across the target PID's tree (count).
    """

    def __init__(self, proc_dir: Path, target_pid: int | None = None):
        self._proc_dir = proc_dir
        self._target_pid = target_pid

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "sys/fs/file-nr", "r") as f:
                content = f.read()
            data = procfs.parse_file_nr(content)
            if data:
                yield make_counter_event(
                    "fds.system",
                    "system",
                    ts_us,
                    "count",
                    data.allocated - data.free_allocated,
                )
        except IOError:
            pass

        if self._target_pid:
            pids = procfs.find_descendant_pids(
                self._target_pid, proc_root=self._proc_dir
            )
            if pids:
                fds_count = sum(
                    procfs.count_process_fds(pid, proc_root=self._proc_dir)
                    for pid in pids
                )
                yield make_counter_event(
                    "fds.process", "process", ts_us, "count", fds_count
                )


class LoadAverageSampler(MetricSampler):
    """Sampler for 1-minute, 5-minute, and 15-minute system load averages from /proc/loadavg.

    Polls '/proc/loadavg' on each tick to parse CPU and task load averages. This
    class is stateless since it only reports absolute averages computed by the kernel.

    Metrics yielded:
        - loadavg.1min: 1-minute load average (load).
        - loadavg.5min: 5-minute load average (load).
        - loadavg.15min: 15-minute load average (load).
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "loadavg", "r") as f:
                content = f.read()
            data = procfs.parse_loadavg(content)
            if data:
                yield make_counter_event(
                    "loadavg.1min", "system", ts_us, "load", data.load1
                )
                yield make_counter_event(
                    "loadavg.5min", "system", ts_us, "load", data.load5
                )
                yield make_counter_event(
                    "loadavg.15min", "system", ts_us, "load", data.load15
                )
        except IOError:
            pass


class PressureSampler(MetricSampler):
    """Sampler for Pressure Stall Information (PSI) averages for CPU, memory, and IO resources.

    Polls files under '/proc/pressure/' (cpu, memory, io) to capture average resource
    stalls. This provides high-fidelity signals when system execution is resource-constrained.

    Metrics yielded:
        - pressure.[resource].some.avg10: Percentage of time some tasks were stalled (percent).
        - pressure.[resource].full.avg10: Percentage of time all tasks were stalled (percent).
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        for resource in ("cpu", "memory", "io"):
            try:
                path = self._proc_dir / "pressure" / resource
                if not path.exists():
                    continue

                with open(path, "r") as f:
                    lines = f.readlines()

                data = procfs.parse_pressure_file(lines)
                if data.some:
                    yield make_counter_event(
                        f"pressure.{resource}.some.avg10",
                        "pressure",
                        ts_us,
                        "percent",
                        data.some.avg10,
                    )
                if data.full:
                    yield make_counter_event(
                        f"pressure.{resource}.full.avg10",
                        "pressure",
                        ts_us,
                        "percent",
                        data.full.avg10,
                    )
            except IOError:
                pass


class NetworkSampler(MetricSampler):
    """Sampler for upload/download bytes and packet bandwidth from /proc/net/dev.

    Polls '/proc/net/dev' on each tick, filters virtual/loopback interfaces, and
    calculates upload/download bandwidth and packet rates by comparing absolute
    byte/packet counters between consecutive polls.

    Metrics yielded:
        - network.[interface].rx.bytes / network.[interface].tx.bytes (bytes_per_second)
        - network.[interface].rx.packets / network.[interface].tx.packets (count_per_second)
        - network.rx.bytes / network.tx.bytes: Summed system-wide download/upload bandwidth (bytes_per_second)
        - network.rx.packets / network.tx.packets: Summed system-wide download/upload packet rate (count_per_second)
    """

    def __init__(self, proc_dir: Path):
        self._proc_dir = proc_dir
        self._prev_net: dict[str, tuple[int, int, int, int]] = {}

    def sample(self, ts_us: int, elapsed: float | None) -> Iterator[TraceEvent]:
        try:
            with open(self._proc_dir / "net/dev", "r") as f:
                lines = f.readlines()
        except IOError:
            return

        parsed_interfaces = procfs.parse_net_dev(lines)
        total_rx_bytes, total_rx_pkts = 0.0, 0.0
        total_tx_bytes, total_tx_pkts = 0.0, 0.0

        for item in parsed_interfaces:
            ifname = item.ifname
            rx_bytes, rx_pkts = item.rx_bytes, item.rx_packets
            tx_bytes, tx_pkts = item.tx_bytes, item.tx_packets

            if elapsed and ifname in self._prev_net:
                prev_rx_b, prev_rx_p, prev_tx_b, prev_tx_p = self._prev_net[
                    ifname
                ]

                rx_bytes_sec = (rx_bytes - prev_rx_b) / elapsed
                rx_pkts_sec = (rx_pkts - prev_rx_p) / elapsed
                tx_bytes_sec = (tx_bytes - prev_tx_b) / elapsed
                tx_pkts_sec = (tx_pkts - prev_tx_p) / elapsed

                # Per-interface details
                yield make_counter_event(
                    f"network.{ifname}.rx.bytes",
                    "network",
                    ts_us,
                    "bytes_per_second",
                    rx_bytes_sec,
                )
                yield make_counter_event(
                    f"network.{ifname}.rx.packets",
                    "network",
                    ts_us,
                    "count_per_second",
                    rx_pkts_sec,
                )
                yield make_counter_event(
                    f"network.{ifname}.tx.bytes",
                    "network",
                    ts_us,
                    "bytes_per_second",
                    tx_bytes_sec,
                )
                yield make_counter_event(
                    f"network.{ifname}.tx.packets",
                    "network",
                    ts_us,
                    "count_per_second",
                    tx_pkts_sec,
                )

                # Add to aggregate summary
                total_rx_bytes += rx_bytes_sec
                total_rx_pkts += rx_pkts_sec
                total_tx_bytes += tx_bytes_sec
                total_tx_pkts += tx_pkts_sec

            self._prev_net[ifname] = (rx_bytes, rx_pkts, tx_bytes, tx_pkts)

        if elapsed and (total_rx_bytes > 0 or total_tx_bytes > 0):
            # Consolidated System-Wide Ingress (download) / Egress (upload) Summary
            yield make_counter_event(
                "network.rx.bytes",
                "network",
                ts_us,
                "bytes_per_second",
                total_rx_bytes,
            )
            yield make_counter_event(
                "network.rx.packets",
                "network",
                ts_us,
                "count_per_second",
                total_rx_pkts,
            )
            yield make_counter_event(
                "network.tx.bytes",
                "network",
                ts_us,
                "bytes_per_second",
                total_tx_bytes,
            )
            yield make_counter_event(
                "network.tx.packets",
                "network",
                ts_us,
                "count_per_second",
                total_tx_pkts,
            )


# --- Pluggable Coordinator Profiler Engine ---


class SystemProfiler:
    """Stateful sampler engine coordinating pluggable list of sub-samplers.

    This class is completely decoupled from I/O writing, making it extremely
    easy to instantiate and unit-test in memory.

    Example:
        sampler = SystemProfiler(
            samplers=[CpuAndSystemSampler(procfs.DEFAULT_PROC_ROOT)],
            metadata={"BUILD_UUID": "abc"},
            time_fn=time.time
        )
        with streaming_trace_writer(Path("system_profile.json")) as write_event:
            # Poll/sample loop
            for event in sampler.sample(ts_us=100000):
                write_event(event)
    """

    def __init__(
        self,
        samplers: Sequence[MetricSampler],
        metadata: dict[str, str] | None,
        time_fn: Callable[[], float],
    ):
        """Initializes the coordinator SystemProfiler.

        Args:
            samplers: A list of concrete MetricSamplers to coordinate.
            metadata: Optional dictionary of metadata to append to the trace file.
            time_fn: Required function to retrieve current epoch timestamp.
        """
        self._samplers = samplers
        self._metadata = metadata or {}
        self._time_fn = time_fn
        self._start_time = datetime.datetime.now(datetime.timezone.utc)
        self._is_first_sample = True

        # State to compute rates
        self._prev_time: float | None = None

    def sample(self, ts_us: int) -> Iterator[TraceEvent]:
        """Executes a single sampling loop yielding trace events across all sub-samplers.

        Calculates the time elapsed since the previous call to `sample()`
        automatically, encapsulation-safe.

        Args:
            ts_us: Timestamp in microseconds since profiling started.
        """
        now = self._time_fn()
        # Yield metadata block as the very first trace event
        if self._is_first_sample:
            self._is_first_sample = False
            elapsed = None
            self._prev_time = now
            # Derive start_time cleanly from our single time source
            start_time = datetime.datetime.fromtimestamp(
                now, datetime.timezone.utc
            )
            yield {
                "name": "system_profiler_metadata",
                "cat": "metadata",
                "ph": "M",
                "args": {
                    "version": 1,
                    **self._metadata,
                    "start_time": start_time.isoformat(),
                },
            }
        else:
            elapsed = (now - self._prev_time) if self._prev_time else None
            self._prev_time = now

        # Drive and yield events from all pluggable samplers
        for sampler in self._samplers:
            yield from sampler.sample(ts_us, elapsed)


# --- Stateful JSON File Streaming Trace Writer Context Manager ---

# Chrome Trace Format (JSON Array Schema) Documentation:
# --------------------------------------------------------
# The profiler outputs a flat JSON array containing TraceEvent dictionaries.
# This format is fully compatible with chrome://tracing and ui.perfetto.dev.
#
# File structure:
# [
#   <initial metadata block (ph: "M")>,
#   <telemetry counter event 1 (ph: "C")>,
#   <telemetry counter event 2 (ph: "C")>,
#   ...
# ]
#
# 1. Telemetry Counter Event (ph: "C") Schema:
#    Represented as a JSON object indicating a resource sample at a specific tick:
#    {
#      "name": "cpu.user",         # Metric name (e.g., 'cpu.user', 'memory.free', etc.)
#      "cat": "system",            # Metric category (e.g., 'system', 'network', 'pressure')
#      "ph": "C",                  # Event phase: 'C' stands for Counter Event
#      "pid": 1,                   # Hardcoded process ID context (arbitrary for system-wide stats)
#      "tid": 1,                   # Hardcoded thread ID context
#      "ts": 123450,               # Elapsed microseconds since profiling start (int)
#      "args": { "percent": 15.5 } # Key-value arguments representing counter name and sampled value
#    }
#
# 2. Metadata Event (ph: "M") Schema:
#    The very first element in the array containing session properties and schema versioning:
#    {
#      "name": "system_profiler_metadata",
#      "cat": "metadata",
#      "ph": "M",                  # Event phase: 'M' stands for Metadata Event
#      "args": {
#        "version": 1,             # Schema version (integer)
#        "start_time": "2026-...", # ISO-8601 utc session start string
#        "FX_BUILD_UUID": "..."    # Optional build run UUID tag
#      }
#    }


@contextmanager
def streaming_trace_writer(
    output_path: Path,
) -> Iterator[Callable[[TraceEvent], None]]:
    """A completely generic streaming JSON array writer context manager.

    Creates and opens the destination file, writes the array header,
    yields a simple callback function to append events, and cleanly
    appends the closing bracket on exiting the context block.

    Args:
        output_path: Destination path for the output JSON trace file.

    Yields:
        A callable function that accepts a TraceEvent and appends it to the file.
    """
    output_path.parent.mkdir(parents=True, exist_ok=True)

    with open(output_path, "w", buffering=1, encoding="utf-8") as f:
        f.write("[\n")
        is_first_event = True

        def write_event(event: TraceEvent) -> None:
            nonlocal is_first_event
            if is_first_event:
                f.write("  " + json.dumps(event))
                is_first_event = False
            else:
                f.write(",\n  " + json.dumps(event))
            f.flush()

        try:
            yield write_event
        finally:
            f.write("\n]\n")


# --- Execution Driver ---


def run_profiler(
    interval_sec: float,
    output_path: Path,
    target_pid: int | None,
    metadata: dict[str, str],
    proc_dir: str,
    time_fn: Callable[[], float],
) -> None:
    """Coordinates signal handling, loop timing, and drives sampling data into the streamer.

    Args:
        interval_sec: Sampling interval in seconds.
        output_path: Path to the output JSON trace file.
        target_pid: Optional process PID to profile descendant FDs.
        metadata: Meta fields to include in the trace.
        proc_dir: Path to the proc filesystem root.
        time_fn: Required function to retrieve current epoch timestamp.
    """
    proc_path = Path(proc_dir)
    samplers: list[MetricSampler] = [
        CpuAndSystemSampler(proc_path),
        MemorySampler(proc_path),
        DiskIoAndPagingSampler(proc_path),
        FileDescriptorSampler(proc_path, target_pid),
        LoadAverageSampler(proc_path),
        PressureSampler(proc_path),
        NetworkSampler(proc_path),
    ]

    sampler = SystemProfiler(
        samplers=samplers,
        metadata=metadata,
        time_fn=time_fn,
    )

    running = True

    def handler(signum: int, frame: Any) -> None:
        nonlocal running
        running = False

    signal.signal(signal.SIGINT, handler)
    signal.signal(signal.SIGTERM, handler)
    signal.signal(signal.SIGHUP, handler)

    start_time_epoch = time_fn()

    with streaming_trace_writer(output_path) as write_event:
        while running:
            start_loop = time_fn()

            now = time_fn()
            ts_us = int((now - start_time_epoch) * 1_000_000)

            for event in sampler.sample(ts_us):
                write_event(event)

            elapsed_loop = time_fn() - start_loop
            sleep_time = max(0.01, interval_sec - elapsed_loop)
            time.sleep(sleep_time)


def main() -> None:
    """Core command-line execution entry point for the utility."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="Sampling interval in seconds",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="Path to write Chrome trace JSON file",
    )
    parser.add_argument(
        "--pid",
        type=int,
        help="Target root PID to profile (creates descendant FD counts)",
    )
    parser.add_argument(
        "--metadata",
        type=str,
        help="Comma-separated key:value pairs for trace metadata",
    )
    parser.add_argument(
        "--proc-dir",
        default=str(procfs.DEFAULT_PROC_ROOT),
        help="Custom path for proc filesystem (for testing)",
    )
    args = parser.parse_args()

    metadata_dict = {}
    if args.metadata:
        for item in args.metadata.split(","):
            if ":" in item:
                k, v = item.split(":", 1)
                metadata_dict[k] = v

    run_profiler(
        interval_sec=args.interval,
        output_path=args.output,
        target_pid=args.pid,
        metadata=metadata_dict,
        proc_dir=args.proc_dir,
        time_fn=time.time,
    )


if __name__ == "__main__":
    main()
