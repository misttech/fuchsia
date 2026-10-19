# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.tracing.tracing_using_ffx.py."""

import os
import subprocess
import tempfile
import unittest
from collections.abc import Callable
from typing import Any, cast
from unittest import mock

import fidl_fuchsia_tracing as f_tracing
from honeydew import affordances_capable
from honeydew.affordances.tracing import tracing_using_ffx
from honeydew.affordances.tracing.errors import TracingError, TracingStateError
from honeydew.transports.ffx import ffx
from honeydew.transports.ffx.errors import FfxCommandError
from parameterized import param, parameterized


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom name function method."""
    test_func_name: str = cast(Any, testcase_func).__name__

    params_dict: dict[str, Any] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(params_dict["label"])

    return f"{test_func_name}_{test_label}"


# pylint: disable=protected-access
class TracingFfxTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for honeydew.affordances.tracing.tracing_using_ffx.py."""

    async def asyncSetUp(self) -> None:
        super().setUp()
        self.reboot_affordance_obj = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice
        )
        self.ffx_obj = mock.MagicMock(spec=ffx.FFX)

        self.tracing_obj = tracing_using_ffx.TracingUsingFfx(
            device_name="fuchsia-emulator",
            ffx_inst=self.ffx_obj,
            reboot_affordance=self.reboot_affordance_obj,
        )

    async def test_verify_supported(self) -> None:
        """Test if verify_supported works."""
        self.tracing_obj.verify_supported()

    def test_default_categories_alignment(self) -> None:
        """Test that DEFAULT_CATEGORIES matches ffx trace config.json."""
        expected_categories = [
            "app",
            "audio",
            "benchmark",
            "blobfs",
            "driver_dispatcher",
            "dvfs",
            "fxfs",
            "gfx",
            "input",
            "kernel:meta",
            "kernel:power",
            "kernel:sched",
            "magma",
            "memory:kernel",
            "minfs",
            "modular",
            "net",
            "sdmmc",
            "starnix",
            "starnix:binder",
            "starnix:pager",
            "storage",
            "system_metrics",
            "view",
            "flutter",
            "dart",
            "dart:compiler",
            "dart:dart",
            "dart:debugger",
            "dart:embedder",
            "dart:gc",
            "dart:isolate",
            "dart:profiler",
            "dart:vm",
        ]
        self.assertEqual(
            tracing_using_ffx.DEFAULT_CATEGORIES, expected_categories
        )
        self.assertEqual(len(tracing_using_ffx.DEFAULT_CATEGORIES), 34)

    @parameterized.expand(
        [
            (
                {
                    "label": "with_no_categories_and_no_buffer_size",
                },
            ),
            (
                {
                    "label": "with_categories_and_buffer_size",
                    "categories": ["category1", "category2"],
                    "buffer_size": 1024,
                },
            ),
            (
                {
                    "label": "when_session_already_initialized",
                    "session_initialized": True,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_initialize(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.initialize() method."""
        # Perform setup based on parameters.
        if parameterized_dict.get("session_initialized"):
            self.tracing_obj.initialize()

        # Check whether an `TracingStateError` exception is raised when
        # calling `initialize()` on a session that is already initialized.
        if parameterized_dict.get("session_initialized"):
            with self.assertRaises(TracingStateError):
                self.tracing_obj.initialize()
        else:
            self.tracing_obj.initialize(
                categories=parameterized_dict.get("categories"),
                buffer_size=parameterized_dict.get("buffer_size"),
            )
            self.assertTrue(self.tracing_obj.is_session_initialized())

    @parameterized.expand(
        [
            (
                {
                    "label": "categories_none",
                    "categories": None,
                    "expected_categories": list(
                        tracing_using_ffx.DEFAULT_CATEGORIES
                    ),
                },
            ),
            (
                {
                    "label": "categories_hash_default",
                    "categories": ["#default"],
                    "expected_categories": list(
                        tracing_using_ffx.DEFAULT_CATEGORIES
                    ),
                },
            ),
            (
                {
                    "label": "categories_default",
                    "categories": ["default"],
                    "expected_categories": list(
                        tracing_using_ffx.DEFAULT_CATEGORIES
                    ),
                },
            ),
            (
                {
                    "label": "categories_hash_default_and_custom",
                    "categories": ["#default", "app", "custom_cat"],
                    "expected_categories": [
                        *tracing_using_ffx.DEFAULT_CATEGORIES,
                        "custom_cat",
                    ],
                },
            ),
            (
                {
                    "label": "categories_explicit_custom",
                    "categories": ["cat1", "cat2", "cat1"],
                    "expected_categories": ["cat1", "cat2"],
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    def test_initialize_categories_expansion(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test category expansion in TracingUsingFfx.initialize()."""
        self.tracing_obj.initialize(
            categories=parameterized_dict.get("categories")
        )
        self.assertCountEqual(
            self.tracing_obj._categories or [],
            parameterized_dict["expected_categories"],
        )

    async def test_initialize_with_compression_disabled(self) -> None:
        """Test for Tracing.initialize() with compression=False."""
        with self.assertRaises(TracingError):
            self.tracing_obj.initialize(compression=False)
        self.assertFalse(self.tracing_obj.is_session_initialized())

    @parameterized.expand(
        [
            (
                {
                    "label": "when_session_is_not_initialized",
                    "session_initialized": False,
                    "tracing_active": False,
                },
            ),
            (
                {
                    "label": "when_session_is_initialized",
                    "session_initialized": True,
                    "tracing_active": False,
                },
            ),
            (
                {
                    "label": "when_tracing_already_started",
                    "session_initialized": True,
                    "tracing_active": True,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_start(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.start() method."""
        # Perform setup based on parameters.
        if parameterized_dict.get("session_initialized"):
            self.tracing_obj.initialize()
        if parameterized_dict.get("tracing_active"):
            await self.tracing_obj.start()

        # Check whether an `TracingStateError` exception is raised when
        # state is not valid.
        if not parameterized_dict.get(
            "session_initialized"
        ) or parameterized_dict.get("tracing_active"):
            with self.assertRaises(TracingStateError):
                await self.tracing_obj.start()
        else:
            await self.tracing_obj.start()
            self.ffx_obj.run.assert_called_with(
                [
                    "trace",
                    "start",
                    "--background",
                    "--categories",
                    ",".join(tracing_using_ffx.DEFAULT_CATEGORIES),
                ]
            )
            self.assertTrue(self.tracing_obj.is_active())

    @parameterized.expand(
        [
            (
                {
                    "label": "oneshot",
                    "buffering_mode": f_tracing.BufferingMode.ONESHOT,
                    "expected_mode": "oneshot",
                },
            ),
            (
                {
                    "label": "circular",
                    "buffering_mode": f_tracing.BufferingMode.CIRCULAR,
                    "expected_mode": "circular",
                },
            ),
            (
                {
                    "label": "streaming",
                    "buffering_mode": f_tracing.BufferingMode.STREAMING,
                    "expected_mode": "streaming",
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_start_with_buffering_modes(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.start() to verify FFX arguments across all BufferingModes."""
        self.tracing_obj.initialize(
            categories=["category1"],
            buffer_size=1024,
            buffering_mode=parameterized_dict["buffering_mode"],
        )
        await self.tracing_obj.start()
        self.ffx_obj.run.assert_called_with(
            [
                "trace",
                "start",
                "--background",
                "--categories",
                "category1",
                "--buffer-size",
                "1024",
                "--buffering-mode",
                parameterized_dict["expected_mode"],
            ]
        )

    async def test_start_error_ffx_command(self) -> None:
        """Test for Tracing.start() when FFX raises FfxCommandError."""
        self.tracing_obj.initialize()
        self.ffx_obj.run.side_effect = FfxCommandError("ffx error")
        with self.assertRaises(TracingError):
            await self.tracing_obj.start()

    async def test_start_error_timeout(self) -> None:
        """Test for Tracing.start() when FFX raises subprocess.TimeoutExpired."""
        self.tracing_obj.initialize()
        self.ffx_obj.run.side_effect = subprocess.TimeoutExpired("ffx", 1)
        with self.assertRaises(subprocess.TimeoutExpired):
            await self.tracing_obj.start()

    @parameterized.expand(
        [
            (
                {
                    "label": "when_session_is_not_initialized",
                    "session_initialized": False,
                    "tracing_active": False,
                },
            ),
            (
                {
                    "label": "when_session_is_initialized",
                    "session_initialized": True,
                    "tracing_active": False,
                },
            ),
            (
                {
                    "label": "when_tracing_already_started",
                    "session_initialized": True,
                    "tracing_active": True,
                },
            ),
            (
                {
                    "label": "with_verify_trace_false",
                    "session_initialized": True,
                    "tracing_active": True,
                    "verify_trace": False,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_stop(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.stop() method."""
        # Perform setup based on parameters.
        session_initialized = parameterized_dict.get("session_initialized")
        tracing_active = parameterized_dict.get("tracing_active")
        verify_trace = parameterized_dict.get("verify_trace", True)
        if session_initialized:
            self.tracing_obj.initialize()
        if tracing_active:
            await self.tracing_obj.start()

        # Check whether an `TracingStateError` exception is raised when
        # state is not valid.
        if not session_initialized or not tracing_active:
            with self.assertRaises(TracingStateError):
                await self.tracing_obj.stop(verify_trace=verify_trace)
            return

        await self.tracing_obj.stop(verify_trace=verify_trace)

        self.assertIsNotNone(self.tracing_obj._temp_trace_file)
        expected_cmd = [
            "trace",
            "stop",
            "--output",
            self.tracing_obj._temp_trace_file,
        ]
        if not verify_trace:
            expected_cmd.append("--no-verify-trace")
        self.ffx_obj.run.assert_called_with(expected_cmd)
        self.assertFalse(self.tracing_obj.is_active())

        # Cleanup
        if self.tracing_obj._temp_trace_file and os.path.exists(
            self.tracing_obj._temp_trace_file
        ):
            os.remove(self.tracing_obj._temp_trace_file)

    @mock.patch.object(os, "close", autospec=True)
    @mock.patch.object(
        tempfile,
        "mkstemp",
        return_value=(10, "/tmp/mock_trace.fxt"),
        autospec=True,
    )
    async def test_stop_error_ffx_command(
        self,
        unused_mock_mkstemp: mock.MagicMock,
        unused_mock_close: mock.MagicMock,
    ) -> None:
        """Test for Tracing.stop() when FFX raises FfxCommandError."""
        self.tracing_obj.initialize()
        await self.tracing_obj.start()

        self.ffx_obj.run.side_effect = FfxCommandError("ffx error")
        with self.assertRaises(TracingError):
            await self.tracing_obj.stop()

    @mock.patch.object(os, "close", autospec=True)
    @mock.patch.object(
        tempfile,
        "mkstemp",
        return_value=(10, "/tmp/mock_trace.fxt"),
        autospec=True,
    )
    async def test_stop_error_timeout(
        self,
        unused_mock_mkstemp: mock.MagicMock,
        unused_mock_close: mock.MagicMock,
    ) -> None:
        """Test for Tracing.stop() when FFX raises subprocess.TimeoutExpired."""
        self.tracing_obj.initialize()
        await self.tracing_obj.start()

        self.ffx_obj.run.side_effect = subprocess.TimeoutExpired("ffx", 1)
        with self.assertRaises(subprocess.TimeoutExpired):
            await self.tracing_obj.stop()

    @parameterized.expand(
        [
            (
                {
                    "label": "when_session_is_not_initialized",
                    "session_initialized": False,
                },
            ),
            (
                {
                    "label": "when_session_is_initialized",
                    "session_initialized": True,
                },
            ),
            (
                {
                    "label": "when_session_is_active",
                    "session_initialized": True,
                    "tracing_active": True,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_terminate(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.terminate() method."""
        temp_file_path = None
        # Perform setup based on parameters.
        if parameterized_dict.get("session_initialized"):
            self.tracing_obj.initialize()
            fd, temp_file_path = tempfile.mkstemp(suffix=".fxt")
            os.close(fd)
            self.tracing_obj._temp_trace_file = temp_file_path
        if parameterized_dict.get("tracing_active"):
            await self.tracing_obj.start()

        await self.tracing_obj.terminate()

        if parameterized_dict.get("tracing_active"):
            self.ffx_obj.run.assert_called_with(["trace", "stop", "--abort"])

        if temp_file_path:
            self.assertFalse(os.path.exists(temp_file_path))

        self.assertFalse(self.tracing_obj.is_active())
        self.assertFalse(self.tracing_obj.is_session_initialized())

    async def test_reboot_handler(self) -> None:
        """Test that reboot_handler resets active tracing state and cleans up temp file."""
        self.tracing_obj.initialize(
            categories=["cat1"],
            buffer_size=512,
            buffering_mode=f_tracing.BufferingMode.CIRCULAR,
        )
        await self.tracing_obj.start()
        fd, temp_file_path = tempfile.mkstemp(suffix=".fxt")
        os.close(fd)
        self.tracing_obj._temp_trace_file = temp_file_path
        self.assertTrue(self.tracing_obj.is_session_initialized())
        self.assertTrue(self.tracing_obj.is_active())

        await self.tracing_obj.reboot_handler()

        self.assertFalse(self.tracing_obj.is_session_initialized())
        self.assertFalse(self.tracing_obj.is_active())
        self.assertIsNone(self.tracing_obj._categories)
        self.assertIsNone(self.tracing_obj._buffer_size)
        self.assertIsNone(self.tracing_obj._buffering_mode)
        self.assertIsNone(self.tracing_obj._temp_trace_file)
        self.assertFalse(os.path.exists(temp_file_path))

    @parameterized.expand(
        [
            (
                {
                    "label": "without_session_initialized",
                    "session_initialized": False,
                },
            ),
            (
                {
                    "label": "with_tracing_download_default_file_name",
                    "return_value": "samp_trace_data",
                    "session_initialized": True,
                },
            ),
            (
                {
                    "label": "with_tracing_download_given_file_name",
                    "trace_file": "trace.fxt",
                    "return_value": "samp_trace_data",
                    "session_initialized": True,
                },
            ),
            (
                {
                    "label": "when_already_stopped",
                    "trace_file": "stopped_trace.fxt",
                    "session_initialized": True,
                    "already_stopped": True,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_terminate_and_download(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.terminate_and_download() method."""
        # Perform setup based on parameters.
        if parameterized_dict.get("session_initialized"):
            self.tracing_obj.initialize()
            await self.tracing_obj.start()
            if parameterized_dict.get("already_stopped"):
                await self.tracing_obj.stop()

        with tempfile.TemporaryDirectory() as tmpdir:
            if not parameterized_dict.get("session_initialized"):
                with self.assertRaises(TracingStateError):
                    await self.tracing_obj.terminate_and_download(tmpdir)
                return

            trace_file: str = parameterized_dict.get("trace_file", "")
            trace_path: str = await self.tracing_obj.terminate_and_download(
                tmpdir, trace_file
            )
            self.assertFalse(self.tracing_obj.is_active())
            self.assertFalse(self.tracing_obj.is_session_initialized())

            # Check the return value of the terminate method.
            if trace_file:
                self.assertEqual(trace_path, f"{tmpdir}/{trace_file}")
            else:
                self.assertRegex(trace_path, f"{tmpdir}/trace_.*.fxt")

            self.assertTrue(os.path.exists(trace_path))

    async def test_terminate_and_download_relative_path_error(self) -> None:
        """Test terminate_and_download raises ValueError when directory is relative."""
        self.tracing_obj.initialize()
        with self.assertRaises(ValueError):
            await self.tracing_obj.terminate_and_download("relative/dir")

    async def test_terminate_and_download_missing_temp_file_error(self) -> None:
        """Test terminate_and_download raises TracingError when temp trace file is missing."""
        self.tracing_obj.initialize()
        with tempfile.TemporaryDirectory() as tmpdir:
            with self.assertRaises(TracingError):
                await self.tracing_obj.terminate_and_download(tmpdir)

    @parameterized.expand(
        [
            (
                {
                    "label": "when_session_is_not_initialized",
                    "session_initialized": False,
                },
            ),
            (
                {
                    "label": "when_session_is_initialized",
                    "session_initialized": True,
                },
            ),
            (
                {
                    "label": "with_tracing_download",
                    "download_trace": True,
                    "trace_file": "trace.fxt",
                    "session_initialized": False,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_trace_session(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test for Tracing.trace_session() method."""
        with tempfile.TemporaryDirectory() as tmpdir:
            trace_file: str = parameterized_dict.get("trace_file", "")
            download_trace: bool = parameterized_dict.get(
                "download_trace", False
            )

            if parameterized_dict.get("session_initialized"):
                self.tracing_obj.initialize()

            async with self.tracing_obj.trace_session(
                download=download_trace, directory=tmpdir, trace_file=trace_file
            ):
                self.assertTrue(self.tracing_obj.is_active())
            self.assertFalse(self.tracing_obj.is_active())

            if download_trace:
                trace_path: str = os.path.join(tmpdir, trace_file)
                self.assertTrue(os.path.exists(trace_path))

    @parameterized.expand(
        [
            (
                {
                    "label": "download_without_directory",
                    "directory": None,
                    "trace_file": "trace.fxt",
                },
            ),
            (
                {
                    "label": "download_with_relative_directory",
                    "directory": "relative/path",
                    "trace_file": "trace.fxt",
                },
            ),
            (
                {
                    "label": "download_without_trace_file",
                    "directory": "/tmp",
                    "trace_file": None,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_trace_session_validation_errors(
        self,
        parameterized_dict: dict[str, Any],
    ) -> None:
        """Test trace_session raises ValueError on invalid download arguments."""
        with self.assertRaises(ValueError):
            async with self.tracing_obj.trace_session(
                download=True,
                directory=parameterized_dict["directory"],
                trace_file=parameterized_dict["trace_file"],
            ):
                pass
        # Clean up temp file created by stop() before ValueError in finally block
        await self.tracing_obj.terminate()

    async def test_trace_session_with_args(self) -> None:
        """Test for Tracing.trace_session() forwarding initialization arguments."""
        self.assertFalse(self.tracing_obj.is_session_initialized())

        with tempfile.TemporaryDirectory() as tmpdir:
            async with self.tracing_obj.trace_session(
                categories=["category1"],
                buffer_size=1024,
                buffering_mode=f_tracing.BufferingMode.ONESHOT,
                compression=True,
                download=True,
                directory=tmpdir,
                trace_file="trace.fxt",
                verify_trace=False,
            ):
                self.assertTrue(self.tracing_obj.is_session_initialized())
                self.assertEqual(self.tracing_obj._categories, ["category1"])
                self.assertEqual(self.tracing_obj._buffer_size, 1024)
                self.assertEqual(
                    self.tracing_obj._buffering_mode,
                    f_tracing.BufferingMode.ONESHOT,
                )
                self.assertTrue(self.tracing_obj.is_active())

            self.assertFalse(self.tracing_obj.is_active())
            self.ffx_obj.run.assert_any_call(
                [
                    "trace",
                    "start",
                    "--background",
                    "--categories",
                    "category1",
                    "--buffer-size",
                    "1024",
                    "--buffering-mode",
                    "oneshot",
                ]
            )
            self.ffx_obj.run.assert_any_call(
                [
                    "trace",
                    "stop",
                    "--output",
                    mock.ANY,
                    "--no-verify-trace",
                ]
            )
            trace_path: str = os.path.join(tmpdir, "trace.fxt")
            self.assertTrue(os.path.exists(trace_path))

    async def test_trace_session_with_compression_disabled(self) -> None:
        """Test for Tracing.trace_session() when compression is disabled."""
        self.assertFalse(self.tracing_obj.is_session_initialized())

        with self.assertRaises(TracingError):
            async with self.tracing_obj.trace_session(
                categories=["category1"],
                buffer_size=1024,
                buffering_mode=f_tracing.BufferingMode.ONESHOT,
                compression=False,
            ):
                pass

    async def test_multi_tracing_session(self) -> None:
        """Test multiple stop/start cycles inside trace_session() context manager."""
        first_temp_file: str | None = None
        try:
            async with self.tracing_obj.trace_session():
                self.assertTrue(self.tracing_obj.is_session_initialized())
                self.assertTrue(self.tracing_obj.is_active())
                await self.tracing_obj.stop()
                first_temp_file = self.tracing_obj._temp_trace_file
                self.assertTrue(self.tracing_obj.is_session_initialized())
                self.assertFalse(self.tracing_obj.is_active())
                await self.tracing_obj.start()
                self.assertTrue(self.tracing_obj.is_session_initialized())
                self.assertTrue(self.tracing_obj.is_active())
            self.assertFalse(self.tracing_obj.is_session_initialized())
            self.assertFalse(self.tracing_obj.is_active())
        finally:
            if first_temp_file and os.path.exists(first_temp_file):
                os.remove(first_temp_file)

    async def test_trace_session_cleanup_on_exception(self) -> None:
        """Verify that trace_session cleans up even if an exception is raised."""
        with self.assertRaises(RuntimeError):
            async with self.tracing_obj.trace_session():
                raise RuntimeError("Simulated test failure")

        self.assertFalse(self.tracing_obj.is_active())


if __name__ == "__main__":
    unittest.main()
