#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path

import procfs


class ParseStatFileTest(unittest.TestCase):
    def test_parse_normal_stat(self) -> None:
        content = (
            "123 (ninja) S 456 123 123 0 0 0 0 0 0 0 15 25 0 0 20 0 1 0 100"
        )
        stat = procfs.parse_stat_file(content)
        self.assertIsNotNone(stat)
        assert stat is not None
        self.assertEqual(stat.pid, 123)
        self.assertEqual(stat.comm, "ninja")
        self.assertEqual(stat.state, "S")
        self.assertEqual(stat.ppid, 456)
        self.assertEqual(stat.pgrp, 123)
        self.assertEqual(stat.utime, 15)
        self.assertEqual(stat.stime, 25)

    def test_parse_stat_comm_with_spaces_and_parens(self) -> None:
        content = "999 (worker (sub) proc) R 1 999 999 0 0 0 0 0 0 0 5 10 0 0"
        stat = procfs.parse_stat_file(content)
        self.assertIsNotNone(stat)
        assert stat is not None
        self.assertEqual(stat.pid, 999)
        self.assertEqual(stat.comm, "worker (sub) proc")
        self.assertEqual(stat.state, "R")
        self.assertEqual(stat.ppid, 1)
        self.assertEqual(stat.pgrp, 999)
        self.assertEqual(stat.utime, 5)
        self.assertEqual(stat.stime, 10)

    def test_parse_stat_malformed(self) -> None:
        self.assertIsNone(procfs.parse_stat_file("invalid"))
        self.assertIsNone(procfs.parse_stat_file("123 no_parens"))
        self.assertIsNone(procfs.parse_stat_file(")reversed( 1 2 3"))
        self.assertIsNone(procfs.parse_stat_file("not_int (cmd) S 1 2"))
        self.assertIsNone(procfs.parse_stat_file("123 (cmd) S not_int 2"))


class ParseCmdlineFileTest(unittest.TestCase):
    def test_parse_normal_cmdline(self) -> None:
        raw = b"python3\0-c\0print('hello')\0"
        args = procfs.parse_cmdline_file(raw)
        self.assertEqual(args, ["python3", "-c", "print('hello')"])

    def test_parse_empty_cmdline_with_fallback(self) -> None:
        self.assertEqual(
            procfs.parse_cmdline_file(b"", fallback_comm="kthreadd"),
            ["[kthreadd]"],
        )
        self.assertEqual(
            procfs.parse_cmdline_file(b"\0\0", fallback_comm="kworker"),
            ["[kworker]"],
        )

    def test_parse_empty_cmdline_without_fallback(self) -> None:
        self.assertEqual(procfs.parse_cmdline_file(b""), [])


def make_fake_process(
    proc_root: Path,
    pid: int,
    *,
    comm: str = "test",
    ppid: int = 1,
    pgrp: int | None = None,
    state: str = "S",
    utime: int = 0,
    stime: int = 0,
    cmdline: Sequence[str] | None = None,
    wchan: str | None = None,
    fds: int = 0,
) -> Path:
    """Creates a mock /proc/<pid> directory populated with virtual files."""
    pid_dir = proc_root / str(pid)
    pid_dir.mkdir(parents=True, exist_ok=True)

    actual_pgrp = pgrp if pgrp is not None else pid
    stat_content = f"{pid} ({comm}) {state} {ppid} {actual_pgrp} 0 0 0 0 0 0 0 0 {utime} {stime} 0 0\n"
    (pid_dir / "stat").write_text(stat_content, encoding="utf-8")

    if cmdline is not None:
        (pid_dir / "cmdline").write_bytes(
            b"\0".join(arg.encode("utf-8") for arg in cmdline)
            + (b"\0" if cmdline else b"")
        )

    if wchan is not None:
        (pid_dir / "wchan").write_text(f"{wchan}\n", encoding="utf-8")

    if fds > 0:
        fd_dir = pid_dir / "fd"
        fd_dir.mkdir(parents=True, exist_ok=True)
        for fd in range(fds):
            (fd_dir / str(fd)).touch()

    return pid_dir


class ReadProcessInfoTest(unittest.TestCase):
    def test_read_process_info_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            make_fake_process(
                proc_root,
                42,
                comm="clang",
                ppid=1,
                state="R",
                utime=100,
                stime=200,
                cmdline=["clang", "-O2", "foo.c"],
                wchan="ep_poll",
            )

            info = procfs.read_process_info(42, proc_root=proc_root)
            self.assertIsNotNone(info)
            assert info is not None
            self.assertEqual(info.pid, 42)
            self.assertEqual(info.ppid, 1)
            self.assertEqual(info.comm, "clang")
            self.assertEqual(info.state, "R")
            self.assertEqual(info.pgrp, 42)
            self.assertEqual(info.cmdline, ["clang", "-O2", "foo.c"])
            self.assertEqual(info.wchan, "ep_poll")
            self.assertEqual(info.utime, 100)
            self.assertEqual(info.stime, 200)

    def test_read_process_info_missing_wchan_and_cmdline(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            make_fake_process(proc_root, 100, comm="kworker", ppid=2)

            info = procfs.read_process_info(100, proc_root=proc_root)
            self.assertIsNotNone(info)
            assert info is not None
            self.assertEqual(info.pid, 100)
            self.assertEqual(info.comm, "kworker")
            self.assertEqual(info.cmdline, ["[kworker]"])
            self.assertIsNone(info.wchan)

    def test_read_process_info_nonexistent_pid(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            self.assertIsNone(
                procfs.read_process_info(9999, proc_root=Path(tmpdir))
            )


class IterPidsTest(unittest.TestCase):
    def test_iter_pids(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            (proc_root / "1").mkdir()
            (proc_root / "42").mkdir()
            (proc_root / "100").mkdir()
            (proc_root / "not_a_pid").mkdir()
            (proc_root / "stat").write_text("cpu info", encoding="utf-8")

            pids = sorted(procfs.iter_pids(proc_root=proc_root))
            self.assertEqual(pids, [1, 42, 100])

    def test_iter_pids_missing_directory(self) -> None:
        self.assertEqual(
            list(procfs.iter_pids(proc_root=Path("/nonexistent/path"))), []
        )


class ReadAllProcessesTest(unittest.TestCase):
    def test_read_all_processes(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            for pid in (10, 20):
                make_fake_process(proc_root, pid, comm=f"proc{pid}")

            all_procs = procfs.read_all_processes(proc_root=proc_root)
            self.assertEqual(sorted(all_procs.keys()), [10, 20])
            self.assertEqual(all_procs[10].comm, "proc10")
            self.assertEqual(all_procs[20].comm, "proc20")


class ParseLoadAvgTest(unittest.TestCase):
    def test_parse_loadavg_valid(self) -> None:
        content = "0.50 0.75 1.25 2/450 12345\n"
        loads = procfs.parse_loadavg(content)
        self.assertEqual(loads, procfs.LoadAvgData(0.50, 0.75, 1.25))

    def test_parse_loadavg_invalid(self) -> None:
        self.assertIsNone(procfs.parse_loadavg("invalid"))
        self.assertIsNone(procfs.parse_loadavg("0.50"))


class ParseCpuInfoTest(unittest.TestCase):
    def test_parse_cpuinfo_valid(self) -> None:
        content = (
            "processor\t: 0\n"
            "model name\t: Intel(R) Xeon(R) CPU @ 2.20GHz\n"
            "cpu cores\t: 2\n"
            "\n"
            "processor\t: 1\n"
            "model name\t: Intel(R) Xeon(R) CPU @ 2.20GHz\n"
            "cpu cores\t: 2\n"
        )
        info = procfs.parse_cpuinfo(content)
        self.assertEqual(info.model_name, "Intel(R) Xeon(R) CPU @ 2.20GHz")
        self.assertEqual(info.logical_processors, 2)
        self.assertEqual(info.physical_cores, 2)

    def test_parse_cpuinfo_fallback_cores(self) -> None:
        content = "processor\t: 0\nprocessor\t: 1\n"
        info = procfs.parse_cpuinfo(content)
        self.assertEqual(info.model_name, "Unknown")
        self.assertEqual(info.logical_processors, 2)
        self.assertEqual(info.physical_cores, 2)


class ParseMountsTest(unittest.TestCase):
    def test_parse_mounts(self) -> None:
        content = (
            "/dev/sda1 / ext4 rw,noatime,discard,errors=remount-ro 0 0\n"
            "tmpfs /run tmpfs rw,nosuid,nodev 0 0\n"
        )
        mounts = list(procfs.parse_mounts(content))
        self.assertEqual(len(mounts), 2)
        self.assertEqual(mounts[0].device, "/dev/sda1")
        self.assertEqual(mounts[0].mount_point, "/")
        self.assertEqual(mounts[0].fstype, "ext4")
        self.assertEqual(
            mounts[0].options, "rw,noatime,discard,errors=remount-ro"
        )


class ParseMeminfoBytesTest(unittest.TestCase):
    def test_parse_meminfo_bytes_valid(self) -> None:
        content = "MemTotal:       65873920 kB\nMemFree:        4312690 kB\n"
        self.assertEqual(
            procfs.parse_meminfo_bytes(content, "MemTotal"), 65873920 * 1024
        )
        self.assertEqual(
            procfs.parse_meminfo_bytes(content, "MemFree"), 4312690 * 1024
        )

    def test_parse_meminfo_bytes_missing_key(self) -> None:
        self.assertEqual(
            procfs.parse_meminfo_bytes("MemFree: 100 kB\n", "MemTotal"), 0
        )

    def test_parse_meminfo_bytes_units(self) -> None:
        self.assertEqual(
            procfs.parse_meminfo_bytes("MemTotal: 1024 B\n", "MemTotal"), 1024
        )
        self.assertEqual(
            procfs.parse_meminfo_bytes("MemTotal: 16 MB\n", "MemTotal"),
            16 * 1024 * 1024,
        )
        self.assertEqual(
            procfs.parse_meminfo_bytes("MemTotal: 2 GB\n", "MemTotal"),
            2 * 1024 * 1024 * 1024,
        )


class ParseProcStatTest(unittest.TestCase):
    def test_parse_proc_stat(self) -> None:
        lines = [
            "cpu  100 20 30 500 10 5 2 0 0 0\n",
            "intr 1000 1 2 3\n",
            "ctxt 5000\n",
            "procs_running 2\n",
            "procs_blocked 1\n",
        ]
        data = procfs.parse_proc_stat(lines)
        expected = procfs.ProcStatData(
            cpu_ticks=[100, 20, 30, 500, 10, 5, 2, 0, 0, 0],
            procs_running=2,
            procs_blocked=1,
            intr=1000,
            ctxt=5000,
        )
        self.assertEqual(data, expected)

    def test_parse_proc_stat_malformed_lines(self) -> None:
        lines = [
            "cpu invalid values\n",
            "procs_running not_a_number\n",
            "intr\n",
        ]
        data = procfs.parse_proc_stat(lines)
        self.assertEqual(data.cpu_ticks, [])
        self.assertIsNone(data.procs_running)
        self.assertIsNone(data.intr)


class ParseProcMeminfoTest(unittest.TestCase):
    def test_parse_proc_meminfo(self) -> None:
        lines = [
            "MemFree:       4000000 kB\n",
            "Buffers:       50000 kB\n",
            "Cached:        1000000 kB\n",
            "SReclaimable:  200000 kB\n",
            "Shmem:         10000 kB\n",
            "SwapTotal:     8000000 kB\n",
            "SwapFree:      6000000 kB\n",
            "Dirty:         3000 kB\n",
            "Writeback:     100 kB\n",
        ]
        data = procfs.parse_proc_meminfo(lines)
        expected = procfs.MemInfoData(
            free=4000000 * 1024,
            buffers=50000 * 1024,
            cached=1190000 * 1024,
            swap_used=2000000 * 1024,
            dirty=3000 * 1024,
            writeback=100 * 1024,
        )
        self.assertEqual(data, expected)


class ParseProcVmstatTest(unittest.TestCase):
    def test_parse_proc_vmstat(self) -> None:
        lines = [
            "pgpgin 100\n",
            "pgpgout 200\n",
            "pswpin 5\n",
            "pswpout 10\n",
        ]
        data = procfs.parse_proc_vmstat(lines)
        expected = procfs.VmStatData(
            pgpgin=100 * 1024,
            pgpgout=200 * 1024,
            pswpin=5 * 4096,
            pswpout=10 * 4096,
        )
        self.assertEqual(data, expected)


class ParseFileNrTest(unittest.TestCase):
    def test_parse_file_nr(self) -> None:
        data = procfs.parse_file_nr("2000\t500\t100000\n")
        expected = procfs.FileNrData(allocated=2000, free_allocated=500)
        self.assertEqual(data, expected)

    def test_parse_file_nr_invalid(self) -> None:
        self.assertIsNone(procfs.parse_file_nr(""))
        self.assertIsNone(procfs.parse_file_nr("2000"))
        self.assertIsNone(procfs.parse_file_nr("invalid content"))


class ParsePressureFileTest(unittest.TestCase):
    def test_parse_pressure_file(self) -> None:
        lines = [
            "some avg10=0.15 avg60=0.10 avg300=0.05 total=1234\n",
            "full avg10=0.05 avg60=0.02 avg300=0.01 total=432\n",
        ]
        data = procfs.parse_pressure_file(lines)
        expected = procfs.PressureData(
            some=procfs.PressureLine(avg10=0.15, avg60=0.10, avg300=0.05),
            full=procfs.PressureLine(avg10=0.05, avg60=0.02, avg300=0.01),
        )
        self.assertEqual(data, expected)


class ParseNetDevTest(unittest.TestCase):
    def test_parse_net_dev(self) -> None:
        lines = [
            "Inter-|   Receive                                                |  Transmit\n",
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
            "  eth0: 100000 1000 0 0 0 0 0 0 200000 2000 0 0 0 0 0 0\n",
            "  lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n",
            "  veth123: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n",
        ]
        results = procfs.parse_net_dev(lines)
        expected = [
            procfs.InterfaceNetworkData(
                ifname="eth0",
                rx_bytes=100000,
                rx_packets=1000,
                tx_bytes=200000,
                tx_packets=2000,
            )
        ]
        self.assertEqual(results, expected)


class FormatCmdlineTest(unittest.TestCase):
    def test_format_cmdline_escapes_newlines(self) -> None:
        formatted = procfs.format_cmdline(
            ["python3", "-c", "import sys\nprint('hello\rworld')"],
            fallback_comm="python3",
        )
        self.assertEqual(
            formatted, r"python3 -c import sys\nprint('hello\rworld')"
        )

    def test_format_cmdline_fallback(self) -> None:
        self.assertEqual(
            procfs.format_cmdline([], fallback_comm="kworker"), "[kworker]"
        )
        self.assertEqual(procfs.format_cmdline([]), "")


class CountProcessFdsTest(unittest.TestCase):
    def test_count_process_fds(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            make_fake_process(proc_root, 100, fds=3)

            self.assertEqual(
                procfs.count_process_fds(100, proc_root=proc_root), 3
            )
            self.assertEqual(
                procfs.count_process_fds(999, proc_root=proc_root), 0
            )


class FindDescendantPidsTest(unittest.TestCase):
    def test_find_descendant_pids(self) -> None:
        ppid_map = {
            1: 0,
            10: 1,
            20: 10,
            21: 20,
            22: 20,
            30: 1,
        }
        self.assertEqual(
            procfs.find_descendant_pids(20, ppid_map=ppid_map), {20, 21, 22}
        )
        self.assertEqual(
            procfs.find_descendant_pids(10, ppid_map=ppid_map),
            {10, 20, 21, 22},
        )
        self.assertEqual(
            procfs.find_descendant_pids(30, ppid_map=ppid_map), {30}
        )
        self.assertEqual(
            procfs.find_descendant_pids(99, ppid_map=ppid_map), {99}
        )

    def test_read_ppid_map(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            make_fake_process(proc_root, 10, comm="bash", ppid=1)
            make_fake_process(proc_root, 20, comm="ninja", ppid=10)

            ppid_map = procfs.read_ppid_map(proc_root=proc_root)
            self.assertEqual(ppid_map, {10: 1, 20: 10})
            self.assertEqual(
                procfs.find_descendant_pids(10, proc_root=proc_root),
                {10, 20},
            )


if __name__ == "__main__":
    unittest.main()
