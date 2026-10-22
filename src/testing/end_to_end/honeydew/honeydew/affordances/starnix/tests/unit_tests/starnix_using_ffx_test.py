# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.starnix.starnix_using_ffx.py."""

import json
import pathlib
import tempfile
import unittest
from typing import Any
from unittest import mock

import fuchsia_inspect
from honeydew import affordances_capable, errors
from honeydew.affordances.starnix import errors as starnix_errors
from honeydew.affordances.starnix import starnix_using_ffx
from honeydew.transports.ffx import ffx as ffx_transport

_DEVICE_NAME: str = "device"

_SAMPLE_KERNEL: dict[str, Any] = {
    "data_source": "Inspect",
    "metadata": {
        "component_url": "fuchsia-pkg://fuchsia.com/starnix_kernel#meta/starnix_kernel.cm",
        "errors": None,
        "file_name": "fuchsia.inspect.Tree",
        "timestamp": 123456789,
    },
    "moniker": f"{starnix_using_ffx._STARNIX_KERNEL_PREFIX}kernel_1",
    "payload": {"root": {"kernel": {"pid": 1}}},
    "version": 1,
}

_SECOND_KERNEL: dict[str, Any] = {
    **_SAMPLE_KERNEL,
    "moniker": f"{starnix_using_ffx._STARNIX_KERNEL_PREFIX}kernel_2",
    "payload": {"root": {"kernel": {"pid": 2}}},
}


# pylint: disable=protected-access
class StarnixUsingStarnixTests(unittest.TestCase):
    """Unit tests for honeydew.affordances.starnix.starnix_using_ffx.py."""

    def setUp(self) -> None:
        super().setUp()

        self.mock_ffx = mock.MagicMock(spec=ffx_transport.FFX)
        self.mock_ffx.run.return_value = json.dumps(
            {
                "instances": [
                    {
                        "moniker": f"{starnix_using_ffx._STARNIX_KERNEL_PREFIX}kernel_1"
                    }
                ]
            }
        )
        self.mock_inspect: Any = mock.create_autospec(
            affordances_capable.InspectCapableDevice, instance=True
        )
        self.sample_collection = (
            fuchsia_inspect.InspectDataCollection.from_list([_SAMPLE_KERNEL])
        )
        self.mock_inspect.get_inspect_data.return_value = self.sample_collection

        self.starnix_obj = starnix_using_ffx.StarnixUsingFfx(
            ffx=self.mock_ffx,
            device_name=_DEVICE_NAME,
            inspect=self.mock_inspect,
        )

        # called in verify_supported()
        self.mock_ffx.run.assert_called_once_with(["component", "list"])

        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.tmp_path = pathlib.Path(temp_dir.name)

    @mock.patch(
        "os.read",
        return_value=starnix_using_ffx._RegExPatterns.STARNIX_CMD_SUCCESS.pattern.encode(),
        autospec=True,
    )
    @mock.patch(
        "pty.openpty",
        return_value=(1, 1),
        autospec=True,
    )
    async def test_run_console_shell_cmd(
        self, mock_openpty: mock.Mock, mock_os_read: mock.Mock
    ) -> None:
        """Test case for SystemPowerStateControllerUsingStarnix.run_console_shell_cmd()"""
        await self.starnix_obj.run_console_shell_cmd(cmd=["something"])
        mock_openpty.assert_called_once()
        mock_os_read.assert_called_once()

    @mock.patch(
        "os.read",
        return_value=starnix_using_ffx._RegExPatterns.STARNIX_NOT_SUPPORTED.pattern.encode(),
        autospec=True,
    )
    @mock.patch(
        "pty.openpty",
        return_value=(1, 1),
        autospec=True,
    )
    async def test_run_console_shell_cmd_exception(
        self, mock_openpty: mock.Mock, mock_os_read: mock.Mock
    ) -> None:
        """Test case for SystemPowerStateControllerUsingStarnix.run_console_shell_cmd()
        raising NotSupportedError"""
        with self.assertRaises(errors.NotSupportedError):
            await self.starnix_obj.run_console_shell_cmd(cmd=["something"])
        mock_openpty.assert_called_once()
        mock_os_read.assert_called_once()

    @mock.patch(
        "os.read",
        return_value="something".encode(),
        autospec=True,
    )
    @mock.patch(
        "pty.openpty",
        return_value=(1, 1),
        autospec=True,
    )
    async def test_run_console_shell_cmd_ffx_error(
        self, mock_openpty: mock.Mock, mock_os_read: mock.Mock
    ) -> None:
        """Test case for SystemPowerStateControllerUsingStarnix.run_console_shell_cmd()
        raising StarnixError"""
        with self.assertRaises(starnix_errors.StarnixError):
            await self.starnix_obj.run_console_shell_cmd(cmd=["something"])
        mock_openpty.assert_called_once()
        mock_os_read.assert_called_once()

    def test_get_kernel_inspect(self) -> None:
        """Verifies get_kernel_inspect() queries the Starnix kernel moniker."""
        kernels = self.starnix_obj.get_kernel_inspect()
        self.assertEqual(kernels, list(self.sample_collection.data))
        self.assertIsInstance(kernels[0], fuchsia_inspect.InspectData)
        self.mock_inspect.get_inspect_data.assert_called_once_with(
            monikers=[starnix_using_ffx._STARNIX_KERNELS_MONIKER]
        )

    def test_get_kernel_inspect_raises_when_zero_kernels(self) -> None:
        """Verifies get_kernel_inspect() raises StarnixError when 0 kernels are found."""
        self.mock_inspect.get_inspect_data.return_value = (
            fuchsia_inspect.InspectDataCollection(data=[])
        )
        with self.assertRaisesRegex(
            starnix_errors.StarnixError,
            f"No Starnix kernel inspect data found on {_DEVICE_NAME}",
        ):
            self.starnix_obj.get_kernel_inspect()

    def test_get_kernel_inspect_raises_when_more_than_one_kernel(self) -> None:
        """Verifies get_kernel_inspect() raises StarnixError when >1 kernel is found."""
        self.mock_inspect.get_inspect_data.return_value = (
            fuchsia_inspect.InspectDataCollection.from_list(
                [_SAMPLE_KERNEL, _SECOND_KERNEL]
            )
        )
        with self.assertRaisesRegex(
            starnix_errors.StarnixError,
            f"Expected 1 Starnix kernel inspect entry on {_DEVICE_NAME}, found 2",
        ):
            self.starnix_obj.get_kernel_inspect()

    def test_get_kernel_inspect_wraps_inspect_and_value_errors(self) -> None:
        """Verifies get_kernel_inspect() wraps InspectError and ValueError in StarnixError."""
        for side_effect in (
            errors.InspectError("inspect failed"),
            json.JSONDecodeError("bad json", "{", 1),
        ):
            self.mock_inspect.get_inspect_data.side_effect = side_effect
            with self.assertRaises(starnix_errors.StarnixError):
                self.starnix_obj.get_kernel_inspect()

    def test_save_kernel_inspect_default_name(self) -> None:
        """Verifies save_kernel_inspect() uses a 24h microsecond timestamped name."""
        path = pathlib.Path(self.starnix_obj.save_kernel_inspect(self.tmp_path))
        self.assertTrue(path.is_absolute())
        self.assertEqual(path.parent, self.tmp_path.absolute())
        self.assertRegex(
            path.name,
            rf"^StarnixKernelInspect_{_DEVICE_NAME}_\d{{4}}-\d{{2}}-\d{{2}}-\d{{2}}-\d{{2}}-\d{{2}}-\d{{6}}\.json$",
        )
        self.assertEqual(json.loads(path.read_text()), [_SAMPLE_KERNEL])

    def test_save_kernel_inspect_custom_name_creates_directory(self) -> None:
        """Verifies save_kernel_inspect() creates nested directories and appends .json."""
        out_dir = self.tmp_path / "new_dir"
        path = self.starnix_obj.save_kernel_inspect(
            out_dir, file="nested/kernel_inspect"
        )
        self.assertEqual(
            path, str(out_dir.absolute() / "nested" / "kernel_inspect.json")
        )
        self.assertEqual(
            json.loads(pathlib.Path(path).read_text()), [_SAMPLE_KERNEL]
        )

    def test_save_kernel_inspect_default_directory(self) -> None:
        """Verifies save_kernel_inspect() defaults to a temporary directory."""
        with mock.patch(
            "honeydew.affordances.starnix.starnix_using_ffx.tempfile.mkdtemp",
            return_value=str(self.tmp_path),
            autospec=True,
        ) as mock_mkdtemp:
            path = pathlib.Path(self.starnix_obj.save_kernel_inspect())
        mock_mkdtemp.assert_called_once_with()
        self.assertEqual(path.parent, self.tmp_path.absolute())
        self.assertEqual(json.loads(path.read_text()), [_SAMPLE_KERNEL])

    def test_save_kernel_inspect_wraps_os_and_type_error(self) -> None:
        """Verifies save_kernel_inspect() wraps OSError and TypeError in StarnixError."""
        with mock.patch.object(
            pathlib.Path, "write_text", side_effect=OSError("disk full")
        ):
            with self.assertRaises(starnix_errors.StarnixError):
                self.starnix_obj.save_kernel_inspect(self.tmp_path)

        with mock.patch(
            "honeydew.affordances.starnix.starnix_using_ffx.json.dumps",
            side_effect=TypeError("not serializable"),
            autospec=True,
        ):
            with self.assertRaises(starnix_errors.StarnixError):
                self.starnix_obj.save_kernel_inspect(self.tmp_path)
