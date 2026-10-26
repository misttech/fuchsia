#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Linux procfs (/proc) parsing and process inspection utilities."""

import os
from collections.abc import Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

DEFAULT_PROC_ROOT: Path = Path("/proc")


@dataclass(frozen=True, slots=True)
class ProcessStat:
    """Parsed attributes from /proc/<pid>/stat."""

    pid: int
    comm: str
    state: str
    ppid: int
    pgrp: int
    utime: int = 0
    stime: int = 0


@dataclass(frozen=True, slots=True)
class ProcessInfo:
    """Snapshot of process metadata read from /proc/<pid>/."""

    pid: int
    ppid: int
    comm: str
    state: str
    pgrp: int | None = None
    cmdline: list[str] = field(default_factory=list)
    wchan: str | None = None
    utime: int = 0
    stime: int = 0

    def cmdline_str(self) -> str:
        """Returns the command line as a single line string, falling back to [comm]."""
        return format_cmdline(self.cmdline, self.comm)


def parse_stat_file(content: str) -> ProcessStat | None:
    """Parses /proc/<pid>/stat file content.

    The executable name (comm) is enclosed in parentheses, which may themselves
    contain spaces or nested parentheses.

    Returns:
        ProcessStat instance, or None if parsing failed.
    """
    lparen = content.find("(")
    rparen = content.rfind(")")
    if lparen == -1 or rparen == -1 or rparen < lparen:
        return None

    try:
        pid = int(content[:lparen].strip())
    except ValueError:
        return None

    comm = content[lparen + 1 : rparen]
    rest = content[rparen + 1 :].strip().split()
    if len(rest) < 3:
        return None

    state = rest[0]
    try:
        ppid = int(rest[1])
        pgrp = int(rest[2])
    except ValueError:
        return None

    utime = 0
    stime = 0
    if len(rest) >= 13:
        try:
            utime = int(rest[11])
            stime = int(rest[12])
        except ValueError:
            pass

    return ProcessStat(
        pid=pid,
        comm=comm,
        state=state,
        ppid=ppid,
        pgrp=pgrp,
        utime=utime,
        stime=stime,
    )


def parse_cmdline_file(
    cmdline_bytes: bytes, fallback_comm: str = ""
) -> list[str]:
    """Parses NUL-separated argument bytes from /proc/<pid>/cmdline.

    Args:
        cmdline_bytes: Raw bytes read from cmdline file.
        fallback_comm: Optional fallback process name to use if cmdline is empty (e.g. kernel threads).

    Returns:
        List of decoded argument strings.
    """
    trimmed = cmdline_bytes.rstrip(b"\0")
    if not trimmed:
        return [f"[{fallback_comm}]"] if fallback_comm else []
    return [
        arg.decode("utf-8", errors="replace") for arg in trimmed.split(b"\0")
    ]


def format_cmdline(cmdline: Sequence[str], fallback_comm: str = "") -> str:
    """Formats a command line argument list into a clean single line string.

    Embedded newlines and carriage returns are escaped so output lines
    remain on a single line per process. Falls back to '[comm]' if empty.
    """
    if cmdline:
        sanitized = [
            arg.replace("\n", "\\n").replace("\r", "\\r") for arg in cmdline
        ]
        return " ".join(sanitized)
    return f"[{fallback_comm}]" if fallback_comm else ""


def read_process_stat(
    pid: int, proc_root: Path = DEFAULT_PROC_ROOT
) -> ProcessStat | None:
    """Reads and parses /proc/<pid>/stat for a given PID.

    Args:
        pid: The process ID to inspect.
        proc_root: Path to the procfs filesystem (default: DEFAULT_PROC_ROOT).

    Returns:
        ProcessStat instance, or None if the process does not exist or is inaccessible.
    """
    proc_dir = proc_root / str(pid)
    try:
        stat_text = (proc_dir / "stat").read_text(
            encoding="utf-8", errors="replace"
        )
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return None

    return parse_stat_file(stat_text)


def read_process_comm(
    pid: int, proc_root: Path = DEFAULT_PROC_ROOT
) -> str | None:
    """Reads the process command name from /proc/<pid>/comm."""
    try:
        return (
            (proc_root / str(pid) / "comm")
            .read_text(encoding="utf-8", errors="replace")
            .rstrip("\n")
        )
    except OSError:
        return None


def read_process_cwd(
    pid: int, proc_root: Path = DEFAULT_PROC_ROOT
) -> Path | None:
    """Resolves the working directory symlink /proc/<pid>/cwd."""
    try:
        return (proc_root / str(pid) / "cwd").resolve(strict=True)
    except OSError:
        return None


def read_process_environ(
    pid: int, proc_root: Path = DEFAULT_PROC_ROOT
) -> dict[str, str] | None:
    """Reads and parses NUL-separated KEY=VALUE entries from /proc/<pid>/environ."""
    try:
        text = (proc_root / str(pid) / "environ").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return None
    env: dict[str, str] = {}
    for entry in text.split("\0"):
        if "=" in entry:
            k, _, v = entry.partition("=")
            env[k] = v
    return env


def read_process_info(
    pid: int, proc_root: Path = DEFAULT_PROC_ROOT
) -> ProcessInfo | None:
    """Reads process metadata for a given PID from procfs.

    Args:
        pid: The process ID to inspect.
        proc_root: Path to the procfs filesystem (default: DEFAULT_PROC_ROOT).

    Returns:
        ProcessInfo instance, or None if the process does not exist or is inaccessible.
    """
    proc_dir = proc_root / str(pid)
    parsed_stat = read_process_stat(pid, proc_root=proc_root)
    if not parsed_stat:
        return None

    cmdline_args: list[str] = []
    try:
        cmdline_bytes = (proc_dir / "cmdline").read_bytes()
        cmdline_args = parse_cmdline_file(cmdline_bytes, parsed_stat.comm)
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        cmdline_args = [f"[{parsed_stat.comm}]"]

    wchan: str | None = None
    try:
        wchan_text = (
            (proc_dir / "wchan")
            .read_text(encoding="utf-8", errors="replace")
            .strip()
        )
        if wchan_text and wchan_text != "0":
            wchan = wchan_text
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        pass

    return ProcessInfo(
        pid=parsed_stat.pid,
        ppid=parsed_stat.ppid,
        comm=parsed_stat.comm,
        state=parsed_stat.state,
        pgrp=parsed_stat.pgrp,
        cmdline=cmdline_args,
        wchan=wchan,
        utime=parsed_stat.utime,
        stime=parsed_stat.stime,
    )


def iter_pids(proc_root: Path = DEFAULT_PROC_ROOT) -> Iterator[int]:
    """Yields integer process IDs discovered under proc_root."""
    try:
        with os.scandir(proc_root) as it:
            for entry in it:
                try:
                    # In /proc, only numeric directory names correspond to processes.
                    # Ephemeral processes may exit concurrently mid-traversal, so check
                    # entry.is_dir() and safely skip missing or non-directory entries.
                    if entry.name.isdigit() and entry.is_dir():
                        yield int(entry.name)
                except (OSError, ValueError):
                    continue
    except (FileNotFoundError, PermissionError):
        return


def read_all_processes(
    proc_root: Path = DEFAULT_PROC_ROOT,
) -> dict[int, ProcessInfo]:
    """Reads and returns a mapping of pid to ProcessInfo for all discovered processes."""
    processes: dict[int, ProcessInfo] = {}
    for pid in iter_pids(proc_root):
        info = read_process_info(pid, proc_root=proc_root)
        if info:
            processes[info.pid] = info
    return processes


def read_ppid_map(proc_root: Path = DEFAULT_PROC_ROOT) -> dict[int, int]:
    """Reads a mapping of PID to parent PID (PPID) for all active processes.

    Only parses lightweight /proc/[PID]/stat files, avoiding reading
    cmdline or wchan for every process on the system.
    """
    ppid_map: dict[int, int] = {}
    for pid in iter_pids(proc_root):
        stat = read_process_stat(pid, proc_root=proc_root)
        if stat:
            ppid_map[pid] = stat.ppid
    return ppid_map


def count_process_fds(pid: int, proc_root: Path = DEFAULT_PROC_ROOT) -> int:
    """Counts the number of open file descriptors for a given process PID."""
    try:
        # Avoid materializing the full directory list in memory when processes
        # have thousands of open file descriptors.
        return sum(1 for _ in (proc_root / str(pid) / "fd").iterdir())
    except (
        FileNotFoundError,
        PermissionError,
        ProcessLookupError,
        NotADirectoryError,
    ):
        # A process may exit or close its file descriptors concurrently while
        # we are counting, in which case returning 0 is the correct fallback.
        return 0


def find_descendant_pids(
    root_pid: int,
    ppid_map: Mapping[int, int] | None = None,
    proc_root: Path = DEFAULT_PROC_ROOT,
) -> set[int]:
    """Returns a set of all active descendant PIDs (including root_pid) under root_pid."""
    if ppid_map is None:
        ppid_map = read_ppid_map(proc_root=proc_root)

    descendants = {root_pid}
    children_map: dict[int, list[int]] = {}
    for pid, ppid in ppid_map.items():
        children_map.setdefault(ppid, []).append(pid)

    queue = [root_pid]
    while queue:
        curr = queue.pop()
        for child in children_map.get(curr, []):
            if child not in descendants:
                descendants.add(child)
                queue.append(child)
    return descendants


@dataclass(frozen=True, slots=True)
class LoadAvgData:
    """1-minute, 5-minute, and 15-minute system load averages."""

    load1: float
    load5: float
    load15: float


def parse_loadavg(content: str) -> LoadAvgData | None:
    """Parses 1-min, 5-min, and 15-min load averages from /proc/loadavg."""
    parts = content.strip().split()
    if len(parts) < 3:
        return None
    try:
        return LoadAvgData(float(parts[0]), float(parts[1]), float(parts[2]))
    except ValueError:
        return None


@dataclass(frozen=True, slots=True)
class CpuInfo:
    """CPU hardware information parsed from /proc/cpuinfo."""

    model_name: str
    logical_processors: int
    physical_cores: int

    def to_dict(self) -> dict[str, Any]:
        """Converts the CPU info record to a clean JSON-serializable dictionary."""
        return {
            "model_name": self.model_name,
            "logical_processors": self.logical_processors,
            "physical_cores": self.physical_cores,
        }


def parse_cpuinfo(content: str) -> CpuInfo:
    """Parses /proc/cpuinfo content and returns a CpuInfo record."""
    model_name = "Unknown"
    logical_processors = 0
    physical_cores = 0

    for line in content.splitlines():
        if ":" in line:
            key, val = [parts.strip() for parts in line.split(":", 1)]
            if key == "processor":
                logical_processors += 1
            elif key == "model name" and model_name == "Unknown":
                model_name = val
            elif key == "cpu cores" and physical_cores == 0:
                try:
                    physical_cores = int(val)
                except ValueError:
                    pass

    if physical_cores == 0:
        physical_cores = logical_processors

    return CpuInfo(
        model_name=model_name,
        logical_processors=logical_processors,
        physical_cores=physical_cores,
    )


@dataclass(frozen=True, slots=True)
class MountEntry:
    """Filesystem mount entry parsed from /proc/mounts."""

    device: str
    mount_point: str
    fstype: str
    options: str


def parse_mounts(content: str) -> Iterator[MountEntry]:
    """Parses /proc/mounts content and yields MountEntry records."""
    for line in content.splitlines():
        parts = line.split()
        if len(parts) >= 4:
            yield MountEntry(
                device=parts[0],
                mount_point=parts[1],
                fstype=parts[2],
                options=parts[3],
            )


def parse_meminfo_bytes(content: str, key: str = "MemTotal") -> int:
    """Parses a byte count for a given key from /proc/meminfo content (e.g. 'MemTotal').

    Values in /proc/meminfo typically include a unit suffix like 'kB' (1024 bytes).
    Handles 'kB', 'MB', 'GB', or raw bytes if unspecified.
    """
    target = f"{key}:"
    for line in content.splitlines():
        if line.startswith(target):
            parts = line.split()
            if len(parts) >= 2:
                try:
                    val = int(parts[1])
                    unit = parts[2].lower() if len(parts) >= 3 else "kb"
                    multipliers = {
                        "b": 1,
                        "kb": 1024,
                        "kib": 1024,
                        "mb": 1024 * 1024,
                        "gb": 1024 * 1024 * 1024,
                    }
                    return val * multipliers.get(unit, 1024)
                except ValueError:
                    pass
            break
    return 0


def _tokenize_proc_lines(lines: Sequence[str]) -> dict[str, list[str]]:
    """Maps the first token (key) of each line to its remaining tokens.

    Ignores trailing colons on keys (e.g. 'MemTotal:' -> 'MemTotal') and skips empty lines.
    """
    tokens: dict[str, list[str]] = {}
    for line in lines:
        parts = line.split()
        if parts:
            tokens[parts[0].rstrip(":")] = parts[1:]
    return tokens


def _parse_proc_ints(lines: Sequence[str]) -> dict[str, int]:
    """Parses whitespace/colon-separated key-integer lines into a dictionary.

    Safely skips non-numeric lines or truncated fields.
    """
    result: dict[str, int] = {}
    for line in lines:
        parts = line.split()
        if len(parts) >= 2:
            try:
                result[parts[0].rstrip(":")] = int(parts[1])
            except ValueError:
                # Silently ignore non-numeric fields (e.g. headers or strings).
                pass
    return result


@dataclass(frozen=True, slots=True)
class ProcStatData:
    """System-wide CPU ticks and scheduler metrics parsed from /proc/stat."""

    cpu_ticks: list[int]
    procs_running: int | None
    procs_blocked: int | None
    intr: int | None
    ctxt: int | None


def parse_proc_stat(lines: list[str]) -> ProcStatData:
    """Parses /proc/stat lines into ProcStatData.

    Args:
        lines: Raw list of lines read from /proc/stat.

    Returns:
        A ProcStatData instance containing parsed CPU ticks, runnable/blocked
        process counts, context switches, and interrupt counts.
    """
    tokens = _tokenize_proc_lines(lines)

    def _get_int(key: str) -> int | None:
        values = tokens.get(key)
        if values:
            try:
                return int(values[0])
            except ValueError:
                # If /proc/stat line is corrupted or non-numeric, leave field as None.
                return None
        return None

    def _get_int_list(key: str) -> list[int]:
        values = tokens.get(key)
        if not values:
            return []
        try:
            return [int(x) for x in values]
        except ValueError:
            return []

    return ProcStatData(
        cpu_ticks=_get_int_list("cpu"),
        procs_running=_get_int("procs_running"),
        procs_blocked=_get_int("procs_blocked"),
        intr=_get_int("intr"),
        ctxt=_get_int("ctxt"),
    )


@dataclass(frozen=True, slots=True)
class MemInfoData:
    """System memory allocation metrics parsed from /proc/meminfo."""

    free: int
    buffers: int
    cached: int
    swap_used: int
    dirty: int
    writeback: int


def parse_proc_meminfo(lines: list[str]) -> MemInfoData:
    """Parses /proc/meminfo lines into MemInfoData.

    Values in /proc/meminfo are in kB (kibibytes); converted to bytes (* 1024).

    Args:
        lines: Raw list of lines read from /proc/meminfo.

    Returns:
        A MemInfoData instance with calculated values in bytes representing free,
        buffered, cached, swap used, dirty, and writeback memory.
    """
    mem_kb = _parse_proc_ints(lines)

    free = mem_kb.get("MemFree", 0) * 1024
    buffers = mem_kb.get("Buffers", 0) * 1024
    cached = (
        mem_kb.get("Cached", 0)
        + mem_kb.get("SReclaimable", 0)
        - mem_kb.get("Shmem", 0)
    ) * 1024
    swap_used = (mem_kb.get("SwapTotal", 0) - mem_kb.get("SwapFree", 0)) * 1024
    dirty = mem_kb.get("Dirty", 0) * 1024
    writeback = mem_kb.get("Writeback", 0) * 1024

    return MemInfoData(
        free=free,
        buffers=buffers,
        cached=cached,
        swap_used=swap_used,
        dirty=dirty,
        writeback=writeback,
    )


@dataclass(frozen=True, slots=True)
class VmStatData:
    """Virtual memory paging and swap statistics parsed from /proc/vmstat."""

    pgpgin: int
    pgpgout: int
    pswpin: int
    pswpout: int


def parse_proc_vmstat(lines: list[str]) -> VmStatData:
    """Parses /proc/vmstat lines into VmStatData.

    Args:
        lines: Raw list of lines read from /proc/vmstat.

    Returns:
        A VmStatData instance containing page in/out and swap in/out counts in bytes.
    """
    vm_dict = _parse_proc_ints(lines)

    # pgpgin/pgpgout are in KiB, convert to Bytes
    pg_in = vm_dict.get("pgpgin", 0) * 1024
    pg_out = vm_dict.get("pgpgout", 0) * 1024
    # pswpin/pswpout are in pages (typically 4KiB), convert to Bytes
    pswp_in = vm_dict.get("pswpin", 0) * 4096
    pswp_out = vm_dict.get("pswpout", 0) * 4096

    return VmStatData(
        pgpgin=pg_in,
        pgpgout=pg_out,
        pswpin=pswp_in,
        pswpout=pswp_out,
    )


@dataclass(frozen=True, slots=True)
class FileNrData:
    """File descriptor allocation metrics parsed from /proc/sys/fs/file-nr."""

    allocated: int
    free_allocated: int


def parse_file_nr(content: str) -> FileNrData | None:
    """Parses /proc/sys/fs/file-nr content into FileNrData.

    Args:
        content: The text content of /proc/sys/fs/file-nr.

    Returns:
        A FileNrData instance containing system-wide allocated and free allocated
        file descriptors, or None if the content is malformed or empty.
    """
    parts = content.split()
    if len(parts) < 2:
        return None
    try:
        return FileNrData(allocated=int(parts[0]), free_allocated=int(parts[1]))
    except ValueError:
        # Return None rather than fabricating zero counts if format is non-numeric.
        return None


@dataclass(frozen=True, slots=True)
class PressureLine:
    """10-second, 60-second, and 300-second pressure stall averages."""

    avg10: float
    avg60: float
    avg300: float


@dataclass(frozen=True, slots=True)
class PressureData:
    """Pressure Stall Information (PSI) for 'some' and 'full' categories."""

    some: PressureLine | None
    full: PressureLine | None


def parse_pressure_file(lines: list[str]) -> PressureData:
    """Parses a pressure file (e.g. /proc/pressure/cpu) into PressureData.

    Args:
        lines: Raw list of lines read from a pressure stall information file.

    Returns:
        A PressureData instance containing 10s, 60s, 300s avg stalls for 'some'
        and 'full' categories, where applicable.
    """
    some: PressureLine | None = None
    full: PressureLine | None = None

    for line in lines:
        parts = line.split()
        if not parts:
            continue
        prefix = parts[0]
        metrics: dict[str, float] = {}
        for item in parts[1:]:
            if "=" in item:
                k, v = item.split("=", 1)
                try:
                    metrics[k] = float(v)
                except ValueError:
                    pass

        if "avg10" in metrics and "avg60" in metrics and "avg300" in metrics:
            line_data = PressureLine(
                avg10=metrics["avg10"],
                avg60=metrics["avg60"],
                avg300=metrics["avg300"],
            )
            if prefix == "some":
                some = line_data
            elif prefix == "full":
                full = line_data

    return PressureData(some=some, full=full)


@dataclass(frozen=True, slots=True)
class InterfaceNetworkData:
    """Network interface transfer statistics parsed from /proc/net/dev."""

    ifname: str
    rx_bytes: int
    rx_packets: int
    tx_bytes: int
    tx_packets: int


def parse_net_dev(lines: list[str]) -> list[InterfaceNetworkData]:
    """Parses /proc/net/dev lines into a list of InterfaceNetworkData.

    Filters out loopback, virtual, and completely inactive interfaces.

    Args:
        lines: Raw list of lines read from /proc/net/dev.

    Returns:
        A list of InterfaceNetworkData instances, one per active physical interface.
    """
    results: list[InterfaceNetworkData] = []
    for line in lines[2:]:
        parts = line.split()
        if len(parts) < 17:
            continue
        ifname = parts[0].strip(":")

        if ifname == "lo" or ifname.startswith(
            ("veth", "docker", "br-", "virbr", "dummy", "tun", "tap")
        ):
            continue

        try:
            rx_bytes, rx_packets = int(parts[1]), int(parts[2])
            tx_bytes, tx_packets = int(parts[9]), int(parts[10])
        except ValueError:
            continue

        if rx_bytes == 0 and tx_bytes == 0:
            continue

        results.append(
            InterfaceNetworkData(
                ifname=ifname,
                rx_bytes=rx_bytes,
                rx_packets=rx_packets,
                tx_bytes=tx_bytes,
                tx_packets=tx_packets,
            )
        )
    return results
