#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import dataclasses
import datetime
import io
import unittest
from pathlib import Path
from unittest import mock

import vmstat_trace

_TEST_START_TIME = datetime.datetime(
    year=1984,
    month=11,
    day=5,
    hour=9,
    minute=30,
    second=0,
)

# this value matches _SAMPLE_VMSTAT_ENTRY
_SAMPLE_VMSTAT_DATA_LINE = (
    " 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 1984-11-05 9:30:00"
)

# This is not the only possible field map produced by vmstat output.
_SAMPLE_FIELD_MAP = {
    "r": 0,
    "b": 1,
    "swpd": 2,
    "free": 3,
    "buff": 4,
    "cache": 5,
    "si": 6,
    "so": 7,
    "bi": 8,
    "bo": 9,
    "in": 10,
    "cs": 11,
    "us": 12,
    "sy": 13,
    "id": 14,
    "wa": 15,
    "st": 16,
    "gu": 17,
    "UTC": 18,
}

# this value matches _SAMPLE_VMSTAT_DATA_LINE
_SAMPLE_VMSTAT_ENTRY = vmstat_trace.VmstatEntry(
    processes=vmstat_trace.ProcessCounts(
        running=1,
        blocked=2,
    ),
    memory=vmstat_trace.MemoryUsage(
        swap=3,
        free=4,
        buffers=5,
        cache=6,
    ),
    swap=vmstat_trace.SwapRates(
        bytes_in_per_second=7,
        bytes_out_per_second=8,
    ),
    block_io=vmstat_trace.BlockIORates(
        received_per_second=9,
        sent_per_second=10,
    ),
    system=vmstat_trace.SystemEventRates(
        interrupts_per_second=11,
        context_switches_per_second=12,
    ),
    cpu=vmstat_trace.CPUUsage(
        user=13,
        system=14,
        idle=15,
        wait_io=16,
        stolen=17,
        kvm_guest=18,
    ),
    timestamp=_TEST_START_TIME,
)


class VmstatEntryTests(unittest.TestCase):
    def test_chrome_trace_events_json(self) -> None:
        events = list(
            _SAMPLE_VMSTAT_ENTRY.chrome_trace_events_json(_TEST_START_TIME)
        )
        # There are 18 fields of vmstat output (not counting timestamp).
        self.assertEqual(len(events), 18)

    def test_parse_data_row(self) -> None:
        entry = vmstat_trace._parse_data_row(
            _SAMPLE_VMSTAT_DATA_LINE, _SAMPLE_FIELD_MAP
        )
        self.assertEqual(entry, _SAMPLE_VMSTAT_ENTRY)

    def test_parse_data_row_wide(self) -> None:
        # Test the wide format -w
        line = (
            " 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 1984-11-05 09:30:00"
        )
        entry = vmstat_trace._parse_data_row(line, _SAMPLE_FIELD_MAP)
        self.assertEqual(entry, _SAMPLE_VMSTAT_ENTRY)

    def test_parse_data_row_missing_gu(self) -> None:
        # Test case where 'gu' is missing (as reported in https://fxbug.dev/491318546)
        line = " 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 1984-11-05 09:30:00"
        field_map = {
            "r": 0,
            "b": 1,
            "swpd": 2,
            "free": 3,
            "buff": 4,
            "cache": 5,
            "si": 6,
            "so": 7,
            "bi": 8,
            "bo": 9,
            "in": 10,
            "cs": 11,
            "us": 12,
            "sy": 13,
            "id": 14,
            "wa": 15,
            "st": 16,
            "UTC": 17,  # UTC is at index 17 if 'gu' is missing
        }
        entry = vmstat_trace._parse_data_row(line, field_map)
        expected = dataclasses.replace(
            _SAMPLE_VMSTAT_ENTRY,
            cpu=dataclasses.replace(_SAMPLE_VMSTAT_ENTRY.cpu, kvm_guest=0),
        )
        self.assertEqual(entry, expected)

    def test_parse_vmstat_output(self) -> None:
        lines = [
            "# Remember, remember, the 5th of November",  # ignore comment
            "",  # ignore blank
            "procs -----------memory---------- ---swap-- -----io---- -system-- -------cpu------- -----timestamp-----",
            " r  b   swpd   free   buff  cache   si   so    bi    bo   in   cs us sy id wa st gu                 UTC",
            _SAMPLE_VMSTAT_DATA_LINE,
        ]
        entries = list(vmstat_trace._parse_vmstat_output(iter(lines)))
        self.assertEqual(entries, [_SAMPLE_VMSTAT_ENTRY])

    def test_parse_vmstat_output_wide_prefix(self) -> None:
        lines = [
            "--procs-- -----------------------memory---------------------- ---swap-- -----io---- -system-- ----------cpu---------- -----timestamp-----",
            "   r    b         swpd         free         buff        cache   si   so    bi    bo   in   cs  us  sy  id  wa  st  gu                 UTC",
            _SAMPLE_VMSTAT_DATA_LINE,
        ]
        entries = list(vmstat_trace._parse_vmstat_output(iter(lines)))
        self.assertEqual(entries, [_SAMPLE_VMSTAT_ENTRY])

    def test_parse_vmstat_output_single_space(self) -> None:
        lines = [
            "r b swpd free buff cache si so bi bo in cs us sy id wa st gu UTC",
            _SAMPLE_VMSTAT_DATA_LINE,
        ]
        entries = list(vmstat_trace._parse_vmstat_output(iter(lines)))
        self.assertEqual(entries, [_SAMPLE_VMSTAT_ENTRY])

    def test_print_chrome_trace_json_empty(self) -> None:
        events = list(vmstat_trace.print_chrome_trace_json(iter([])))
        self.assertEqual(events, [])

    def test_print_chrome_trace_json_nonempty(self) -> None:
        events = list(
            vmstat_trace.print_chrome_trace_json(
                iter([_SAMPLE_VMSTAT_ENTRY] * 2)
            )
        )
        self.assertEqual(len(events), 18 * 2)  # one per field


class MainTests(unittest.TestCase):
    def test_parse_and_print(self) -> None:
        argv = ["vmstat.log"]
        # Include a header in the mock data so _parse_vmstat_output works
        mock_data = (
            "r  b swpd free buff cache si so bi bo in cs us sy id wa st gu UTC\n"
            + _SAMPLE_VMSTAT_DATA_LINE
        )
        out = io.StringIO()
        with mock.patch.object(
            Path, "read_text", return_value=mock_data
        ) as mock_read:
            with contextlib.redirect_stdout(out):
                returncode = vmstat_trace.main(argv)
        self.assertEqual(returncode, 0)
        mock_read.assert_called_once_with()
        self.assertIn('"traceEvents"', out.getvalue())

    def test_main_with_parser_log(self) -> None:
        argv = ["--parser-log", "parser.err", "vmstat.log"]
        mock_data = (
            "r  b swpd free buff cache si so bi bo in cs us sy id wa st gu UTC\n"
            + _SAMPLE_VMSTAT_DATA_LINE
        )
        out = io.StringIO()
        with mock.patch.object(Path, "read_text", return_value=mock_data):
            with mock.patch("logging.basicConfig") as mock_logging_config:
                with contextlib.redirect_stdout(out):
                    returncode = vmstat_trace.main(argv)
        self.assertEqual(returncode, 0)
        mock_logging_config.assert_called_once()
        _, kwargs = mock_logging_config.call_args
        self.assertEqual(kwargs["filename"], Path("parser.err"))


if __name__ == "__main__":
    unittest.main()
