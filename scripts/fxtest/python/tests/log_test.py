# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import contextlib
import io
import json
import unittest
import unittest.mock as mock

import event
import log


class TestLogOutput(unittest.IsolatedAsyncioTestCase):
    async def _write_test_logs(self) -> io.StringIO:
        """Write out test logs

        Returns:
            io.StringIO: A buffer containing test logs.
        """
        recorder = event.EventRecorder()
        output = io.StringIO()
        log_task = asyncio.create_task(log.writer(recorder, output))
        recorder.emit_init()
        id = recorder.emit_build_start(["//test"])
        recorder.emit_end(id=id)
        recorder.emit_info_message("Done testing")
        recorder.emit_end()

        await log_task

        return output

    async def test_logs_json(self) -> None:
        """Test that logs are properly serialized to JSON."""
        output = await self._write_test_logs()

        events: list[event.Event] = [
            event.Event.from_dict(json.loads(line))  # type:ignore
            for line in output.getvalue().splitlines()
        ]

        self.assertEqual(len(events), 5)
        payloads = [e.payload for e in events if e.payload is not None]
        self.assertEqual(len(payloads), 3)
        self.assertIsNotNone(payloads[0].start_timestamp)
        self.assertEqual(payloads[1].build_targets, ["//test"])
        self.assertEqual(
            payloads[2].user_message,
            event.Message("Done testing", event.MessageLevel.INFO),
        )

    async def test_pretty_print(self) -> None:
        output = await self._write_test_logs()
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            log.pretty_print(log.LogSource.from_stream(output))
        self.assertEqual(stdout.getvalue(), "0 tests were run\n")


class TestPreviousStats(unittest.IsolatedAsyncioTestCase):
    async def _write_test_logs(self) -> io.StringIO:
        """Write out test logs with specific hierarchy to test filtering."""
        output = io.StringIO()

        # Mock time.monotonic to control duration and avoid sleeping
        with mock.patch("event.time.monotonic") as mock_time:
            mock_time.return_value = 1000.0
            recorder = event.EventRecorder()
            log_task = asyncio.create_task(log.writer(recorder, output))
            recorder.emit_init()

            id_a = recorder.emit_event_group("Group A")
            id_b = recorder.emit_program_start("prog_b", [], parent=id_a)
            recorder.emit_program_termination(id_b, 0)
            id_c = recorder.emit_program_start(
                "prog_c", ["dldist"], parent=id_a
            )
            recorder.emit_program_termination(id_c, 0)
            recorder.emit_end(id=id_a)

            id_d = recorder.emit_build_start(["//build_target_123"])
            mock_time.return_value += 3.0
            id_e = recorder.emit_program_start("prog_e", [], parent=id_d)
            mock_time.return_value += 1.0
            recorder.emit_program_termination(id_e, 0)
            recorder.emit_end(id=id_d)

            id_f = recorder.emit_test_group(1)
            id_g = recorder.emit_test_suite_started(
                "suite_g", False, parent=id_f
            )
            id_h = recorder.emit_program_start("prog_h", [], parent=id_g)
            mock_time.return_value += 2.0
            recorder.emit_program_termination(id_h, 0)
            recorder.emit_test_suite_ended(
                id_g, event.TestSuiteStatus.PASSED, None
            )

            id_i = recorder.emit_test_suite_started(
                "suite_i", False, parent=id_f
            )
            id_j = recorder.emit_program_start("prog_j", [], parent=id_i)
            mock_time.return_value += 3.0
            recorder.emit_program_termination(id_j, 0)
            recorder.emit_test_suite_ended(
                id_i, event.TestSuiteStatus.PASSED, None
            )
            recorder.emit_end(id=id_f)

            id_k = recorder.emit_start_file_parsing("file", "path")
            recorder.emit_end(id=id_k)

            recorder.emit_end()
            await log_task

        output.seek(0)
        return output

    async def test_compute_stats(self) -> None:
        output = await self._write_test_logs()
        stats = log.compute_stats(log.LogSource.from_stream(output))

        top_n_list = stats.top_n
        self.assertEqual(len(top_n_list), 3)

        self.assertEqual(
            top_n_list[0].category, event.EventStatCategory.BUILDING
        )
        self.assertIn("Building 1 targets", top_n_list[0].label)
        self.assertEqual(
            top_n_list[2].category, event.EventStatCategory.TESTING
        )
        self.assertEqual(top_n_list[1].label, "Running TestSuite suite_i")
        self.assertEqual(
            top_n_list[1].category, event.EventStatCategory.TESTING
        )
        self.assertEqual(top_n_list[2].label, "Running TestSuite suite_g")

        summary = stats.summary
        self.assertIn(event.EventStatCategory.BUILDING, summary)
        self.assertEqual(summary[event.EventStatCategory.BUILDING].count, 1)
        self.assertIn(event.EventStatCategory.TESTING, summary)
        self.assertEqual(summary[event.EventStatCategory.TESTING].count, 2)
        self.assertIn(event.EventStatCategory.SEARCHING, summary)
        self.assertEqual(summary[event.EventStatCategory.SEARCHING].count, 1)
        self.assertIn(event.EventStatCategory.OTHERS, summary)
        self.assertEqual(summary[event.EventStatCategory.OTHERS].count, 1)
        self.assertIn(event.EventStatCategory.PARSING, summary)
        self.assertEqual(summary[event.EventStatCategory.PARSING].count, 1)
