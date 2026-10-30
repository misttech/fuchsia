# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for adb.py."""

import asyncio
import concurrent.futures
import io
import os
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
from importlib import resources
from pathlib import Path
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.transports.adb import adb, adb_server
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import host_shell
from mobly import signals

_DEVICE_NAME = "fuchsia-mock-device"
_SERIAL_NUMBER = "12345678"


# pylint: disable=protected-access
class AdbTests(unittest.TestCase):
    """Unit tests for ADB transport."""

    def setUp(self) -> None:
        super().setUp()

        self._verify_supported_patcher = mock.patch.object(
            adb.Adb,
            "verify_supported",
            return_value=None,
        )
        self._verify_supported_patcher.start()

        self._check_connection_patcher = mock.patch.object(
            adb.Adb,
            "check_connection",
            return_value=None,
        )
        self._check_connection_patcher.start()

        self._env_patcher = mock.patch.dict(
            os.environ,
            {
                "HONEYDEW_ADB_OVERRIDE": "/custom/adb",
                "ADB_VENDOR_KEYS": tempfile.gettempdir(),
            },
        )
        self._env_patcher.start()

        self.mock_ffx = mock.Mock()
        with mock.patch.object(adb.Adb, "_cache_adbd_pid"):
            self.adb_obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )

    def tearDown(self) -> None:
        self.adb_obj.close()
        self._env_patcher.stop()
        self._check_connection_patcher.stop()
        self._verify_supported_patcher.stop()
        super().tearDown()

    @mock.patch("glob.glob", autospec=True)
    def test_check_adb_sysfs_success(self, mock_glob: mock.Mock) -> None:
        """Test _check_adb_sysfs success path."""
        mock_glob.side_effect = lambda pattern: {
            "/sys/bus/usb/devices/*": ["/sys/bus/usb/devices/1-1"],
            "/sys/bus/usb/devices/1-1/*:*.*/bInterfaceClass": [
                "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass"
            ],
        }.get(pattern, [])

        mock_files = {
            "/sys/bus/usb/devices/1-1/idVendor": "18d1\n",
            "/sys/bus/usb/devices/1-1/serial": f"{_SERIAL_NUMBER}\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass": "ff\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceSubClass": "42\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceProtocol": "01\n",
        }

        def mock_open_file(
            path: str, mode: str = "r", *args: Any, **kwargs: Any
        ) -> mock.MagicMock:
            if path in mock_files:
                return mock.mock_open(read_data=mock_files[path])()
            raise OSError(f"File not found: {path}")

        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock_open_file
        ):
            res = adb._check_adb_sysfs(_SERIAL_NUMBER)
            self.assertTrue(res)

    @mock.patch("glob.glob", autospec=True)
    def test_check_adb_sysfs_without_serial_success(
        self, mock_glob: mock.Mock
    ) -> None:
        """Test _check_adb_sysfs success path when target_serial is None."""
        mock_glob.side_effect = lambda pattern: {
            "/sys/bus/usb/devices/*": ["/sys/bus/usb/devices/1-1"],
            "/sys/bus/usb/devices/1-1/*:*.*/bInterfaceClass": [
                "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass"
            ],
        }.get(pattern, [])

        mock_files = {
            "/sys/bus/usb/devices/1-1/idVendor": "18d1\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass": "ff\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceSubClass": "42\n",
            "/sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceProtocol": "01\n",
        }

        def mock_open_file(
            path: str, mode: str = "r", *args: Any, **kwargs: Any
        ) -> mock.MagicMock:
            if path in mock_files:
                return mock.mock_open(read_data=mock_files[path])()
            raise OSError(f"File not found: {path}")

        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock_open_file
        ):
            res = adb._check_adb_sysfs(None)
            self.assertTrue(res)

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=True)
    def test_verify_supported(self, mock_check: mock.Mock) -> None:
        """Test verify_supported calls _check_adb_sysfs with serial number."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj.verify_supported()
            mock_check.assert_called_once_with(_SERIAL_NUMBER)
        finally:
            self._verify_supported_patcher.start()

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=False)
    def test_verify_supported_not_supported(
        self, mock_check: mock.Mock
    ) -> None:
        """Test verify_supported raises NotSupportedError when ADB is not supported."""
        self._verify_supported_patcher.stop()
        try:
            with self.assertRaises(errors.NotSupportedError):
                self.adb_obj.verify_supported()
            mock_check.assert_called_once_with(_SERIAL_NUMBER)
        finally:
            self._verify_supported_patcher.start()

    def test_is_tcp_serial(self) -> None:
        """Test is_tcp_serial identifies TCP host:port serials."""
        self.assertTrue(adb.is_tcp_serial("127.0.0.1:5555"))
        self.assertTrue(adb.is_tcp_serial("localhost:5556"))
        self.assertTrue(adb.is_tcp_serial("[::1]:5555"))
        self.assertTrue(
            adb.is_tcp_serial("[fe80::5054:ff:fe63:5e7a%qemu]:5555")
        )
        self.assertFalse(adb.is_tcp_serial(None))
        self.assertFalse(adb.is_tcp_serial(""))
        self.assertFalse(adb.is_tcp_serial(_SERIAL_NUMBER))
        self.assertFalse(adb.is_tcp_serial(":5555"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:0"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:70000"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1:¹²³"))
        self.assertFalse(adb.is_tcp_serial("fe80::1"))
        self.assertFalse(adb.is_tcp_serial("[127.0.0.1]:5555"))
        self.assertFalse(adb.is_tcp_serial("[127.0.0.1:5555"))
        self.assertFalse(adb.is_tcp_serial("127.0.0.1]:5555"))
        self.assertFalse(adb.is_tcp_serial("[]:5555"))

    @mock.patch.object(
        adb, "_check_adb_sysfs", return_value=False, autospec=True
    )
    def test_verify_supported_tcp_serial_skips_sysfs(
        self, mock_check: mock.Mock
    ) -> None:
        """Test verify_supported skips sysfs check when serial is a TCP address."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj._serial_number = "127.0.0.1:5555"
            with self.assertLogs(level="INFO") as log:
                self.adb_obj.verify_supported()
            mock_check.assert_not_called()
            self.assertTrue(
                any(
                    "TCP ADB serial '127.0.0.1:5555' detected" in msg
                    for msg in log.output
                )
            )
        finally:
            self._verify_supported_patcher.start()

    @mock.patch(
        "socket.getaddrinfo",
        side_effect=socket.gaierror("Name or service not known"),
        autospec=True,
    )
    @mock.patch.object(
        adb, "_check_adb_sysfs", return_value=False, autospec=True
    )
    def test_verify_supported_tcp_serial_unresolvable_host_raises(
        self, mock_check: mock.Mock, mock_getaddrinfo: mock.Mock
    ) -> None:
        """Test verify_supported raises NotSupportedError on unresolvable TCP host."""
        self._verify_supported_patcher.stop()
        try:
            self.adb_obj._serial_number = "[fe80::1%qemu]:5555"
            with self.assertRaisesRegex(
                errors.NotSupportedError, "could not be resolved"
            ):
                self.adb_obj.verify_supported()
            mock_getaddrinfo.assert_called_once_with(
                "fe80::1", 5555, type=socket.SOCK_STREAM
            )
            mock_check.assert_not_called()
        finally:
            self._verify_supported_patcher.start()

    def test_public_properties(self) -> None:
        """Test serial_number, binary_path, and server_host_port properties."""
        self.assertEqual(self.adb_obj.serial_number, _SERIAL_NUMBER)
        self.assertEqual(self.adb_obj.binary_path, "/custom/adb")
        self.assertIsNone(self.adb_obj.server_host_port)

        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 54321
        self.adb_obj._adb_server = mock_server
        self.assertEqual(self.adb_obj.server_host_port, ("127.0.0.1", 54321))

    @mock.patch.object(resources, "files", autospec=True)
    def test_get_adb_binary_with_env_override(
        self, mock_files: mock.Mock
    ) -> None:
        """Test _get_adb_binary when environment variable override is provided."""
        with mock.patch.dict(
            os.environ, {"HONEYDEW_ADB_OVERRIDE": "/env/path/adb"}, clear=True
        ):
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "/env/path/adb")
            mock_files.assert_not_called()

    @mock.patch.object(resources, "files", autospec=True)
    @mock.patch.object(resources, "as_file", autospec=True)
    @mock.patch("atexit.register", autospec=True)
    @mock.patch("shutil.copy2", autospec=True)
    @mock.patch("tempfile.NamedTemporaryFile", autospec=True)
    def test_get_adb_binary_with_resource_success(
        self,
        mock_tmp_file: mock.Mock,
        mock_copy: mock.Mock,
        *unused_args: Any,
    ) -> None:
        """Test _get_adb_binary when adb data resource exists."""
        with (
            mock.patch.dict(sys.modules, {"honeydew.data": mock.Mock()}),
            mock.patch.dict(os.environ, {}, clear=True),
        ):
            mock_fd = mock.Mock()
            mock_fd.name = "tmpadb"
            mock_tmp_file.return_value = mock_fd
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "tmpadb")
            mock_fd.close.assert_called_once()
            mock_copy.assert_called_with(mock.ANY, bin_name)

    @mock.patch("shutil.which", return_value="/which/adb", autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_get_adb_binary_fallback_to_path_success(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test _get_adb_binary falls back to PATH when resource is not available."""
        with mock.patch.dict(os.environ, {}, clear=True):
            bin_name = adb._get_adb_binary()
            self.assertEqual(bin_name, "/which/adb")
            mock_which.assert_called_once_with("adb")

    @mock.patch("shutil.which", return_value=None, autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_get_adb_binary_not_found_fail(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test _get_adb_binary raises InitializationError when binary is not found anywhere."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(adb_errors.InitializationError):
                adb._get_adb_binary()
            mock_which.assert_called_once_with("adb")

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("shutil.which", return_value="/usr/bin/adb", autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_init_with_serial(
        self,
        mock_as_file: mock.Mock,
        mock_which: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init sets adb binary and serial number."""
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(adb._LOGGER, "warning") as mock_log_warning,
        ):
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )
            mock_log_warning.assert_not_called()
            self.assertEqual(obj._adb_binary, "/usr/bin/adb")
            self.assertEqual(obj._serial_number, _SERIAL_NUMBER)
            mock_which.assert_called_once_with("adb")
            mock_cache_pid.assert_called_once_with(obj)

    @mock.patch("shutil.which", return_value=None, autospec=True)
    @mock.patch.object(
        resources, "as_file", side_effect=FileNotFoundError, autospec=True
    )
    def test_init_binary_not_found(
        self, mock_as_file: mock.Mock, mock_which: mock.Mock
    ) -> None:
        """Test init raises InitializationError when adb binary is not found anywhere."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(adb_errors.InitializationError):
                adb.Adb(
                    device_name=_DEVICE_NAME,
                    serial_number=_SERIAL_NUMBER,
                    ffx_transport=self.mock_ffx,
                    run_isolated_server=False,
                )

    @mock.patch.object(adb, "_check_adb_sysfs", return_value=False)
    def test_init_not_supported(self, mock_check: mock.Mock) -> None:
        """Test init raises NotSupportedError when ADB is not supported."""
        self._verify_supported_patcher.stop()
        try:
            with self.assertRaises(errors.NotSupportedError):
                adb.Adb(
                    device_name=_DEVICE_NAME,
                    serial_number=_SERIAL_NUMBER,
                    ffx_transport=self.mock_ffx,
                    run_isolated_server=False,
                )
        finally:
            self._verify_supported_patcher.start()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init starts isolated server by default and warns when output_dir is None."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        with self.assertLogs(adb._LOGGER, level="WARNING") as cm:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                vendor_keys_path="/keys",
            )
        self.assertTrue(
            any("No output_dir provided" in msg for msg in cm.output)
        )
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=None,
            vendor_keys_path="/keys",
        )
        mock_server.start.assert_called_once()
        mock_cache_pid.assert_called_once_with(obj)
        self.assertIsNone(obj._output_dir)
        self.assertIs(obj._adb_server, mock_server)
        obj.close()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_with_output_dir(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init passes specified output_dir to AdbServer without warning."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        custom_output_dir = "/tmp/custom_output_dir"
        with mock.patch.object(adb._LOGGER, "warning") as mock_log_warning:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                output_dir=custom_output_dir,
                vendor_keys_path="/keys",
            )
            mock_log_warning.assert_not_called()
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=Path(custom_output_dir),
            vendor_keys_path="/keys",
        )
        self.assertEqual(obj._output_dir, Path(custom_output_dir))
        mock_server.start.assert_called_once()
        mock_cache_pid.assert_called_once_with(obj)
        self.assertIs(obj._adb_server, mock_server)
        obj.close()

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch("os.path.exists", return_value=True)
    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_with_empty_output_dir(
        self,
        mock_adb_server_cls: mock.Mock,
        mock_exists: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test init treats empty output_dir as unspecified and logs a warning."""
        mock_server = mock.Mock()
        mock_adb_server_cls.return_value = mock_server

        with self.assertLogs(adb._LOGGER, level="WARNING") as cm:
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                output_dir="",
                vendor_keys_path="/keys",
            )
        self.assertTrue(
            any("No output_dir provided" in msg for msg in cm.output)
        )
        mock_adb_server_cls.assert_called_once_with(
            adb_binary_path="/custom/adb",
            serial_id=_SERIAL_NUMBER,
            output_dir=None,
            vendor_keys_path="/keys",
        )
        self.assertIsNone(obj._output_dir)
        obj.close()

    @mock.patch.object(adb, "AdbServer")
    def test_init_isolated_server_error(
        self, mock_adb_server_cls: mock.Mock
    ) -> None:
        """Test init raises InitializationError when isolated server fails to start."""
        mock_server = mock.Mock()
        mock_server.start.side_effect = adb_errors.AdbServerError(
            "Failed to start server"
        )
        mock_adb_server_cls.return_value = mock_server

        with self.assertRaises(adb_errors.InitializationError):
            adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
            )

    def test_init_calls_check_connection(self) -> None:
        """Test init calls check_connection and _cache_adbd_pid."""
        mock_check = mock.Mock()
        mock_cache = mock.Mock()
        with (
            mock.patch.object(adb.Adb, "check_connection", mock_check),
            mock.patch.object(adb.Adb, "_cache_adbd_pid", mock_cache),
        ):
            adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
            )
        mock_check.assert_called_once()
        mock_cache.assert_called_once()

    def test_on_device_boot(self) -> None:
        """Test on_device_boot resets root state, checks connection, and caches adbd PID."""
        self.adb_obj._is_root = True
        self.adb_obj._rooted_by_context = True
        self.adb_obj._root_ref_count = 2
        self.adb_obj._cached_adbd_pid = "1111"

        with (
            mock.patch.object(adb.Adb, "check_connection") as mock_check,
            mock.patch.object(adb.Adb, "_cache_adbd_pid") as mock_cache,
        ):
            self.adb_obj.on_device_boot()
            mock_check.assert_called_once_with()
            mock_cache.assert_called_once_with()

        self.assertFalse(self.adb_obj.is_root)
        self.assertFalse(self.adb_obj._rooted_by_context)
        self.assertEqual(self.adb_obj._root_ref_count, 0)

    @mock.patch("time.time", side_effect=[100.0, 103.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_success(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection deducts elapsed time before calling wait_for_boot_complete."""
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection(timeout=10.0)
            mock_run.assert_called_once_with(
                self.adb_obj,
                ["wait-for-device"],
                timeout=10.0,
                attempts=adb._DEFAULT_RUN_ATTEMPTS,
            )
            mock_wait_for_boot.assert_called_once_with(
                self.adb_obj, timeout=7.0
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 100.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_default_timeout(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection uses default timeout of 300.0s when not specified."""
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection()
            mock_run.assert_called_once_with(
                self.adb_obj,
                ["wait-for-device"],
                timeout=300.0,
                attempts=adb._DEFAULT_RUN_ATTEMPTS,
            )
            mock_wait_for_boot.assert_called_once_with(
                self.adb_obj, timeout=300.0
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_timeout_expired(
        self,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection raises AdbConnectionError if timeout expires during wait-for-device."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaisesRegex(
                adb_errors.AdbConnectionError, "wait-for-device"
            ):
                self.adb_obj.check_connection(timeout=10.0)
            mock_wait_for_boot.assert_not_called()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_failure_wait_for_device(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection raises AdbConnectionError when wait-for-device fails."""
        self._check_connection_patcher.stop()
        try:
            mock_run.side_effect = adb_errors.AdbCommandError(
                "wait-for-device failed"
            )
            with self.assertRaises(adb_errors.AdbConnectionError):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_failure_wait_for_boot_complete(
        self, mock_run: mock.Mock, mock_wait_for_boot: mock.Mock
    ) -> None:
        """Test check_connection raises AdbConnectionError when wait_for_boot_complete fails."""
        self._check_connection_patcher.stop()
        try:
            mock_wait_for_boot.side_effect = adb_errors.AdbTimeoutError(
                "boot timeout"
            )
            with self.assertRaises(adb_errors.AdbConnectionError):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch("time.time", side_effect=[100.0, 102.0, 105.0], autospec=True)
    @mock.patch.object(adb.Adb, "wait_for_boot_complete", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    def test_check_connection_tcp_serial(
        self,
        mock_connect_tcp: mock.Mock,
        mock_run: mock.Mock,
        mock_wait_for_boot: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test check_connection connects over TCP before wait-for-device."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        self._check_connection_patcher.stop()
        try:
            self.adb_obj.check_connection(timeout=10.0)
            mock_connect_tcp.assert_called_once_with(self.adb_obj, timeout=10.0)
            mock_run.assert_called_once_with(
                self.adb_obj,
                ["wait-for-device"],
                timeout=8.0,
                attempts=adb._DEFAULT_RUN_ATTEMPTS,
            )
            mock_wait_for_boot.assert_called_once_with(
                self.adb_obj, timeout=5.0
            )
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_success(self, mock_host_shell_run: mock.Mock) -> None:
        """Test _connect_tcp succeeds when adb connect reports connected."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_host_shell_run.return_value = "connected to 127.0.0.1:5555\n"

        self.adb_obj._connect_tcp(timeout=5.0)

        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "connect", "127.0.0.1:5555"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=5.0,
            env=mock.ANY,
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_polls_until_connected(
        self, mock_host_shell_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test _connect_tcp polls when initial connect attempt fails."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_host_shell_run.side_effect = [
            "failed to connect to '127.0.0.1:5555': Connection refused\n",
            "unable to connect to 127.0.0.1:5555: No route to host\n",
            errors.HostCmdError("adb connect failed"),
            "already connected to 127.0.0.1:5555\n",
        ]

        self.adb_obj._connect_tcp(timeout=5.0, poll_interval=0.5)

        self.assertEqual(mock_host_shell_run.call_count, 4)
        mock_sleep.assert_has_calls(
            [mock.call(0.5), mock.call(0.5), mock.call(0.5)]
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", side_effect=[0.0, 1.0, 10.0], autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_timeout(
        self,
        mock_host_shell_run: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _connect_tcp raises AdbConnectionError with last error on timeout."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_host_shell_run.return_value = (
            "failed to connect to '127.0.0.1:5555': Connection refused\n"
        )

        with self.assertRaisesRegex(
            adb_errors.AdbConnectionError, "Connection refused"
        ):
            self.adb_obj._connect_tcp(timeout=5.0)

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_non_positive_timeout_raises(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _connect_tcp raises AdbConnectionError immediately when timeout <= 0."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        for bad_timeout in (0.0, -1.0):
            with self.subTest(timeout=bad_timeout):
                with self.assertRaises(adb_errors.AdbConnectionError):
                    self.adb_obj._connect_tcp(timeout=bad_timeout)
        mock_host_shell_run.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_connect_tcp_unauthorized_fails_fast(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _connect_tcp raises AdbUnauthorizedError immediately when unauthorized or auth fails."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        for bad_output in (
            "error: device unauthorized.\n",
            "failed to authenticate to 127.0.0.1:5555\n",
        ):
            with self.subTest(output=bad_output):
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.return_value = bad_output
                with self.assertRaises(adb_errors.AdbUnauthorizedError):
                    self.adb_obj._connect_tcp(timeout=5.0)
                mock_host_shell_run.assert_called_once()

        for bad_err in (
            "error: device unauthorized.",
            "failed to authenticate to 127.0.0.1:5555",
        ):
            with self.subTest(err=bad_err):
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = errors.HostCmdError(bad_err)
                with self.assertRaises(adb_errors.AdbUnauthorizedError):
                    self.adb_obj._connect_tcp(timeout=5.0)
                mock_host_shell_run.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_success(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run success path."""
        mock_host_shell_run.return_value = "output_text"

        output = self.adb_obj.run(["shell", "echo", "hello"])

        expected_env = os.environ.copy()
        expected_env["ADB_VENDOR_KEYS"] = tempfile.gettempdir()
        self.assertEqual(output, "output_text")
        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "-s", _SERIAL_NUMBER, "shell", "echo", "hello"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=None,
            env=expected_env,
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_without_serial(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run with include_serial=False."""
        mock_host_shell_run.return_value = "output_text"

        output = self.adb_obj.run(["devices"], include_serial=False)

        expected_env = os.environ.copy()
        expected_env["ADB_VENDOR_KEYS"] = tempfile.gettempdir()
        self.assertEqual(output, "output_text")
        mock_host_shell_run.assert_called_once_with(
            cmd=["/custom/adb", "devices"],
            capture_output=True,
            capture_error_in_output=True,
            timeout=None,
            env=expected_env,
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error_text"),
        autospec=True,
    )
    def test_run_failure(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run failure path (non-zero exit code)."""
        with self.assertRaises(adb_errors.AdbCommandError) as context:
            self.adb_obj.run(["shell", "bad_command"])

        self.assertIn("error_text", str(context.exception))

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_run_does_not_wrap_unexpected_exceptions(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run propagates non-host-command exceptions (e.g. Mobly signals) as-is."""
        with self.assertRaises(signals.TestAbortAll):
            self.adb_obj.run(["shell", "id"])

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_cache_adbd_pid_does_not_swallow_unexpected_exceptions(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _cache_adbd_pid propagates non-host-command exceptions."""
        with self.assertRaises(signals.TestAbortAll):
            self.adb_obj._cache_adbd_pid()

    @mock.patch.object(
        adb.Adb, "run", side_effect=signals.TestAbortAll("abort"), autospec=True
    )
    def test_check_connection_does_not_wrap_unexpected_exceptions(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection propagates non-host-command exceptions as-is."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(signals.TestAbortAll):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error: device unauthorized."),
        autospec=True,
    )
    def test_run_unauthorized_error(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run raises AdbUnauthorizedError immediately without restarting server."""
        mock_server = mock.Mock()
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        with self.assertRaises(adb_errors.AdbUnauthorizedError) as context:
            self.adb_obj.run(["shell", "id"])

        self.assertIn("is unauthorized", str(context.exception))
        self.assertIn("ADB_VENDOR_KEYS", str(context.exception))
        mock_host_shell_run.assert_called_once()
        mock_server.restart.assert_not_called()

    @mock.patch.object(
        adb.Adb,
        "run",
        side_effect=adb_errors.AdbUnauthorizedError("Device unauthorized"),
        autospec=True,
    )
    def test_check_connection_unauthorized_error(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection propagates AdbUnauthorizedError."""
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(adb_errors.AdbUnauthorizedError):
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HoneydewTimeoutError("timed out"),
        autospec=True,
    )
    def test_run_timeout(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run timeout path divides timeout by attempts."""
        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.run(["shell", "long_running"], timeout=30.0)

        mock_host_shell_run.assert_called_once_with(
            cmd=[
                "/custom/adb",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "long_running",
            ],
            capture_output=True,
            capture_error_in_output=True,
            timeout=10.0,
            env=mock.ANY,
        )

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HostCmdError("error: device not found"),
        autospec=True,
    )
    def test_run_custom_attempts(self, mock_host_shell_run: mock.Mock) -> None:
        """Test run with custom attempts=1 does not retry and uses full timeout."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.run(["shell", "some_cmd"], timeout=15.0, attempts=1)

        mock_host_shell_run.assert_called_once_with(
            cmd=[
                "/custom/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "12345",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "some_cmd",
            ],
            capture_output=True,
            capture_error_in_output=True,
            timeout=15.0,
            env=mock.ANY,
        )
        mock_server.restart.assert_not_called()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_success(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on connection error and succeeds."""
        # Set a mocked ADB server
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. First attempt: fail with "device not found"
        # 2. _recover_adb_server -> _cache_adbd_pid() runs "shell pidof adbd"
        # 3. Second attempt: succeed
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            "5555\n",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 3)
        mock_server.restart.assert_called_once()
        mock_sleep.assert_any_call(10)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retries_tcp_drop_errors(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on TCP connection drop error messages."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        for err_msg in (
            "error: closed",
            "error: protocol fault (couldn't read status)",
            "Connection reset by peer",
            "No route to host",
            "Network is unreachable",
            "Connection timed out",
        ):
            with self.subTest(err_msg=err_msg):
                mock_server.reset_mock()
                mock_host_shell_run.reset_mock()
                mock_host_shell_run.side_effect = [
                    errors.HostCmdError(err_msg),
                    "5555\n",
                    "recovered_output",
                ]
                output = self.adb_obj.run(["shell", "some_cmd"])
                self.assertEqual(output, "recovered_output")
                mock_server.restart.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_fail_max_attempts(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run retries on connection error but fails after max attempts."""
        # Set a mocked ADB server
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # All 3 command attempts fail with "device not found", with 2 _cache_adbd_pid() calls in between
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
            errors.HostCmdError("error: device not found"),
        ]

        with self.assertRaises(adb_errors.AdbCommandError):
            self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(mock_host_shell_run.call_count, 5)
        self.assertEqual(
            mock_server.restart.call_count, 2
        )  # Restarted on 1st and 2nd failure
        retry_calls = [
            c for c in mock_sleep.call_args_list if c == mock.call(10)
        ]
        self.assertEqual(len(retry_calls), 2)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_reconnect_success(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run attempts lightweight 'adb reconnect offline' on first offline error."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Recovery -> calls "adb reconnect offline"
        # 3. Main command attempt 2 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            "reconnecting offline",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 3)
        self.assertEqual(
            mock_host_shell_run.call_args_list[1].kwargs["cmd"],
            [
                "/custom/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "12345",
                "reconnect",
                "offline",
            ],
        )
        mock_server.restart.assert_not_called()
        mock_sleep.assert_called_once_with(2)

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_escalates_to_server_restart(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run escalates from 'adb reconnect offline' to full server restart on 2nd offline error."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Attempt 1 recovery -> "adb reconnect offline"
        # 3. Main command attempt 2 -> still fails with "error: device offline"
        # 4. Attempt 2 recovery -> _recover_adb_server() restarts server & runs _cache_adbd_pid()
        # 5. Main command attempt 3 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            "reconnecting offline",
            errors.HostCmdError("error: device offline"),
            "5555\n",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 5)
        mock_server.restart.assert_called_once()

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_run_retry_offline_reconnect_failure_escalates_to_server_restart(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test run still retries and escalates to server restart when 'adb reconnect offline' fails."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # 1. Main command attempt 1 -> fails with "error: device offline"
        # 2. Attempt 1 recovery -> "adb reconnect offline" itself fails
        # 3. Main command attempt 2 -> still fails with "error: device offline"
        # 4. Attempt 2 recovery -> _recover_adb_server() restarts server & runs _cache_adbd_pid()
        # 5. Main command attempt 3 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device offline"),
            errors.HostCmdError("error: reconnect failed"),
            errors.HostCmdError("error: device offline"),
            "5555\n",
            "success_output",
        ]

        output = self.adb_obj.run(["shell", "some_cmd"])

        self.assertEqual(output, "success_output")
        self.assertEqual(mock_host_shell_run.call_count, 5)
        mock_server.restart.assert_called_once()

    def test_run_chains_underlying_error(self) -> None:
        """Test run chains the underlying error as __cause__ of the raised ADB error."""
        cases: list[tuple[Exception, type[adb_errors.AdbError]]] = [
            (errors.HostCmdError("error_text"), adb_errors.AdbCommandError),
            (
                errors.HostCmdError("error: device unauthorized."),
                adb_errors.AdbUnauthorizedError,
            ),
            (
                errors.HoneydewTimeoutError("timed out"),
                adb_errors.AdbTimeoutError,
            ),
        ]
        for underlying_err, expected_err_type in cases:
            with self.subTest(underlying_err=underlying_err):
                with mock.patch.object(
                    host_shell, "run", side_effect=underlying_err, autospec=True
                ):
                    with self.assertRaises(expected_err_type) as context:
                        self.adb_obj.run(["shell", "id"])

                self.assertIs(context.exception.__cause__, underlying_err)

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_check_connection_chains_underlying_error(
        self, mock_run: mock.Mock
    ) -> None:
        """Test check_connection chains the underlying error as __cause__ of AdbConnectionError."""
        underlying_err = adb_errors.AdbCommandError("wait-for-device failed")
        mock_run.side_effect = underlying_err
        self._check_connection_patcher.stop()
        try:
            with self.assertRaises(adb_errors.AdbConnectionError) as context:
                self.adb_obj.check_connection()
        finally:
            self._check_connection_patcher.start()

        self.assertIs(context.exception.__cause__, underlying_err)

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_concurrent_calls_return_own_output(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test concurrent run calls from multiple threads each get their own output."""
        num_threads = 2
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server

        # Hold every call until all threads are inside host_shell.run so the
        # calls overlap.
        barrier = threading.Barrier(num_threads)

        def _fake_run(cmd: list[str], **unused_kwargs: Any) -> str:
            barrier.wait(timeout=5)
            return f"output_{cmd[-1]}"

        mock_host_shell_run.side_effect = _fake_run

        with concurrent.futures.ThreadPoolExecutor(num_threads) as executor:
            futures = {
                i: executor.submit(self.adb_obj.run, ["shell", "echo", str(i)])
                for i in range(num_threads)
            }
            outputs = {i: f.result(timeout=10) for i, f in futures.items()}

        self.assertEqual(
            outputs, {i: f"output_{i}" for i in range(num_threads)}
        )
        self.assertEqual(mock_host_shell_run.call_count, num_threads)
        mock_server.restart.assert_not_called()
        self.assertEqual(
            mock_server.reset_restart_count.call_count, num_threads
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_with_ffx_and_root_restoration(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _recover_adb_server kills adbd via ffx, restarts server, restores root, and re-caches PID."""
        mock_server = mock.Mock()
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        mock_ffx = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj._ffx = mock_ffx
        self.adb_obj._is_root = True

        # Cache initial adbd PID
        mock_host_shell_run.return_value = "4321\n"
        self.adb_obj._cache_adbd_pid()
        self.assertEqual(self.adb_obj._cached_adbd_pid, "4321")

        # Now run a command that fails on attempt 1 with a connection error:
        # 1. Main command attempt 1 -> fails with "error: device not found"
        # 2. _recover_adb_server -> runs ["root"] then ["wait-for-device"]
        # 3. _recover_adb_server -> calls _cache_adbd_pid() which runs ["shell", "pidof", "adbd"]
        # 4. Main command attempt 2 -> succeeds
        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device not found"),
            "restarting adbd as root",
            "",
            "8765\n",
            "cmd_output",
        ]

        output = self.adb_obj.run(["shell", "id"])

        self.assertEqual(output, "cmd_output")
        mock_ffx.run.assert_called_once_with(
            ["starnix", "kill", "-p", "4321", "-s", "9"],
            timeout=10.0,
        )
        mock_server.restart.assert_called_once()
        self.assertTrue(self.adb_obj.is_root)
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")

    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_tcp_serial_reconnects(
        self, mock_sleep: mock.Mock, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _recover_adb_server reconnects over TCP after server restart and root."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        mock_server.host.return_value = "127.0.0.1"
        mock_server.port.return_value = 12345
        self.adb_obj._adb_server = mock_server
        self.adb_obj._is_root = True

        mock_host_shell_run.side_effect = [
            "connected to 127.0.0.1:5555\n",
            "restarting adbd as root",
            "connected to 127.0.0.1:5555\n",
            "",
            "8765\n",
        ]

        self.adb_obj._recover_adb_server()

        mock_server.restart.assert_called_once()
        self.assertEqual(self.adb_obj._cached_adbd_pid, "8765")
        connect_calls = [
            c
            for c in mock_host_shell_run.call_args_list
            if c.kwargs["cmd"][-2:] == ["connect", "127.0.0.1:5555"]
        ]
        self.assertEqual(len(connect_calls), 2)

    @mock.patch.object(adb.Adb, "_cache_adbd_pid", autospec=True)
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    @mock.patch("time.sleep", autospec=True)
    def test_recover_adb_server_tcp_serial_connect_failure_and_unauthorized(
        self,
        mock_sleep: mock.Mock,
        mock_host_shell_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
        mock_cache_pid: mock.Mock,
    ) -> None:
        """Test _recover_adb_server propagates AdbConnectionError and AdbUnauthorizedError on TCP connect failure."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_server = mock.create_autospec(adb_server.AdbServer, instance=True)
        self.adb_obj._adb_server = mock_server
        self.adb_obj._is_root = True

        # 1. Connection error -> propagates immediately without calling root or _cache_adbd_pid
        mock_connect_tcp.side_effect = adb_errors.AdbConnectionError(
            "connect timed out"
        )
        with self.assertRaises(adb_errors.AdbConnectionError):
            self.adb_obj._recover_adb_server()
        mock_host_shell_run.assert_not_called()
        mock_cache_pid.assert_not_called()

        # 2. Unauthorized error -> propagates immediately
        mock_connect_tcp.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            self.adb_obj._recover_adb_server()
        mock_host_shell_run.assert_not_called()
        mock_cache_pid.assert_not_called()

    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(host_shell, "run", autospec=True)
    def test_run_retries_tcp_drop_without_isolated_server(
        self,
        mock_host_shell_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
    ) -> None:
        """Test run reconnects over TCP and retries on drop even when _adb_server is None."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        self.adb_obj._adb_server = None

        mock_host_shell_run.side_effect = [
            errors.HostCmdError("error: device '127.0.0.1:5555' not found"),
            "recovered_output",
        ]

        output = self.adb_obj.run(["shell", "id"])
        self.assertEqual(output, "recovered_output")
        mock_connect_tcp.assert_called_once_with(
            self.adb_obj, timeout=adb._TCP_RECONNECT_TIMEOUT_SECS
        )
        self.assertEqual(mock_host_shell_run.call_count, 2)

        # Verify AdbUnauthorizedError during TCP reconnect propagates immediately
        mock_connect_tcp.reset_mock()
        mock_connect_tcp.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        mock_host_shell_run.side_effect = errors.HostCmdError("error: closed")
        with self.assertRaises(adb_errors.AdbUnauthorizedError):
            self.adb_obj.run(["shell", "id"])

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_reconnect_offline_tcp_serial(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test _reconnect_offline runs adb connect after reconnect offline for TCP."""
        self.adb_obj._serial_number = "127.0.0.1:5555"
        mock_host_shell_run.side_effect = [
            "reconnecting offline",
            "connected to 127.0.0.1:5555\n",
        ]

        self.adb_obj._reconnect_offline()

        self.assertEqual(mock_host_shell_run.call_count, 2)
        self.assertEqual(
            mock_host_shell_run.call_args_list[0].kwargs["cmd"],
            ["/custom/adb", "reconnect", "offline"],
        )
        self.assertEqual(
            mock_host_shell_run.call_args_list[1].kwargs["cmd"],
            ["/custom/adb", "connect", "127.0.0.1:5555"],
        )

    @mock.patch(
        "time.time",
        side_effect=[100.0, 102.0, 104.0, 110.0, 112.0, 115.0],
        autospec=True,
    )
    @mock.patch.object(adb.Adb, "_connect_tcp", autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_and_unroot_tcp_serial(
        self,
        mock_run: mock.Mock,
        mock_connect_tcp: mock.Mock,
        mock_time: mock.Mock,
    ) -> None:
        """Test root and unroot reconnect over TCP before wait-for-device."""
        self.adb_obj._serial_number = "127.0.0.1:5555"

        self.adb_obj.root(timeout=10.0)
        self.adb_obj.unroot(timeout=10.0)

        mock_connect_tcp.assert_has_calls(
            [
                mock.call(self.adb_obj, timeout=8.0),
                mock.call(self.adb_obj, timeout=8.0),
            ]
        )
        mock_run.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    ["root"],
                    timeout=10.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=6.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["unroot"],
                    timeout=10.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=5.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
            ]
        )

    def test_close(self) -> None:
        """Test close stops the adb server if running and drops the reference."""
        mock_server = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj.close()
        mock_server.stop.assert_called_once()
        self.assertIsNone(self.adb_obj._adb_server)

    def test_close_is_idempotent(self) -> None:
        """Test calling close multiple times only cleans up resources once."""
        mock_server = mock.Mock()
        mock_temp_dir = mock.Mock()
        self.adb_obj._adb_server = mock_server
        self.adb_obj._temp_vendor_keys_dir = mock_temp_dir

        self.adb_obj.close()
        self.adb_obj.close()

        mock_server.stop.assert_called_once()
        mock_temp_dir.cleanup.assert_called_once()

    @mock.patch("atexit.unregister", autospec=True)
    def test_close_unregisters_atexit_handler(
        self, mock_unregister: mock.Mock
    ) -> None:
        """Test close unregisters its atexit handler."""
        self.adb_obj.close()
        mock_unregister.assert_called_once_with(self.adb_obj.close)

    def test_close_cleans_up_vendor_keys(self) -> None:
        """Test close cleans up temp vendor keys dir if present."""
        mock_temp_dir = mock.Mock()
        self.adb_obj._temp_vendor_keys_dir = mock_temp_dir
        self.adb_obj.close()
        mock_temp_dir.cleanup.assert_called_once()
        self.assertIsNone(self.adb_obj._temp_vendor_keys_dir)

    def test_is_root_default(self) -> None:
        """Test is_root returns False by default."""
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch("time.time", side_effect=[100.0, 103.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root(self, mock_run: mock.Mock, mock_time: mock.Mock) -> None:
        """Test root runs root and wait-for-device commands, deducting elapsed timeout."""
        self.adb_obj.root(timeout=10.0)

        mock_run.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    ["root"],
                    timeout=10.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=7.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
            ]
        )
        self.assertTrue(self.adb_obj.is_root)

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_root_timeout_expired(
        self, mock_run: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Test root raises AdbTimeoutError if timeout expires during root command."""
        with self.assertRaisesRegex(adb_errors.AdbTimeoutError, "root"):
            self.adb_obj.root(timeout=10.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["root"],
            timeout=10.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch("time.time", side_effect=[100.0, 104.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_unroot(self, mock_run: mock.Mock, mock_time: mock.Mock) -> None:
        """Test unroot runs unroot and wait-for-device commands, deducting elapsed timeout."""
        self.adb_obj._is_root = True

        self.adb_obj.unroot(timeout=10.0)

        mock_run.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    ["unroot"],
                    timeout=10.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
                mock.call(
                    self.adb_obj,
                    ["wait-for-device"],
                    timeout=6.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                ),
            ]
        )
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch("time.time", side_effect=[100.0, 110.0], autospec=True)
    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_unroot_timeout_expired(
        self, mock_run: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Test unroot raises AdbTimeoutError if timeout expires during unroot command."""
        self.adb_obj._is_root = True

        with self.assertRaisesRegex(adb_errors.AdbTimeoutError, "unroot"):
            self.adb_obj.unroot(timeout=10.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["unroot"],
            timeout=10.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )
        self.assertTrue(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_not_root(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root roots on enter and unroots on exit when not root initially."""
        self.assertFalse(self.adb_obj.is_root)

        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_root.assert_called_once_with(
                self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
            )
            mock_unroot.assert_not_called()

        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_when_already_root(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root does not root or unroot when already root."""
        self.adb_obj._is_root = True

        with self.adb_obj.use_adb_root(timeout=5.0):
            mock_root.assert_not_called()
            mock_unroot.assert_not_called()

        mock_root.assert_not_called()
        mock_unroot.assert_not_called()

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_async_context(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test use_adb_root works as an async context manager."""
        self.assertFalse(self.adb_obj.is_root)

        async def run_async() -> None:
            async with self.adb_obj.use_adb_root(timeout=5.0):
                mock_root.assert_called_once_with(
                    self.adb_obj,
                    timeout=5.0,
                    attempts=adb._DEFAULT_RUN_ATTEMPTS,
                )
                mock_unroot.assert_not_called()

        asyncio.run(run_async())
        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch.object(adb.Adb, "unroot", autospec=True)
    @mock.patch.object(adb.Adb, "root", autospec=True)
    def test_use_adb_root_concurrent_overlapping(
        self, mock_root: mock.Mock, mock_unroot: mock.Mock
    ) -> None:
        """Test overlapping use_adb_root contexts only unroot when last task exits."""
        self.assertFalse(self.adb_obj.is_root)

        def side_effect_root(
            inst: adb.Adb,
            timeout: float | None = None,
            attempts: int = adb._DEFAULT_RUN_ATTEMPTS,
        ) -> None:
            inst._is_root = True

        def side_effect_unroot(
            inst: adb.Adb,
            timeout: float | None = None,
            attempts: int = adb._DEFAULT_RUN_ATTEMPTS,
        ) -> None:
            inst._is_root = False

        mock_root.side_effect = side_effect_root
        mock_unroot.side_effect = side_effect_unroot

        ctx_a = self.adb_obj.use_adb_root(timeout=5.0)
        ctx_b = self.adb_obj.use_adb_root(timeout=5.0)

        # Task A enters -> roots device (depth 1)
        ctx_a.__enter__()
        mock_root.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )
        mock_unroot.assert_not_called()

        # Task B enters while A is active -> does not re-root (depth 2)
        ctx_b.__enter__()
        mock_root.assert_called_once()
        mock_unroot.assert_not_called()

        # Task A exits first -> must NOT unroot because Task B is still active (depth 1)
        ctx_a.__exit__(None, None, None)
        mock_unroot.assert_not_called()
        self.assertTrue(self.adb_obj.is_root)

        # Task B exits -> unroots device as depth returns to 0
        ctx_b.__exit__(None, None, None)
        mock_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )
        self.assertFalse(self.adb_obj.is_root)

    @mock.patch.object(adb.Adb, "su_unroot", autospec=True)
    @mock.patch.object(adb.Adb, "su_root", autospec=True)
    def test_use_su_root_when_not_root(
        self, mock_su_root: mock.Mock, mock_su_unroot: mock.Mock
    ) -> None:
        """Test use_su_root roots on enter and unroots on exit when not root initially."""
        self.assertFalse(self.adb_obj.is_su_root)

        with self.adb_obj.use_su_root(timeout=5.0):
            mock_su_root.assert_called_once_with(
                self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
            )
            mock_su_unroot.assert_not_called()

        mock_su_unroot.assert_called_once_with(
            self.adb_obj, timeout=5.0, attempts=adb._DEFAULT_RUN_ATTEMPTS
        )

    @mock.patch.object(adb.Adb, "su_unroot", autospec=True)
    @mock.patch.object(adb.Adb, "su_root", autospec=True)
    def test_use_su_root_when_already_root(
        self, mock_su_root: mock.Mock, mock_su_unroot: mock.Mock
    ) -> None:
        """Test use_su_root does not root or unroot when already root."""
        self.adb_obj._is_su_root = True

        with self.adb_obj.use_su_root(timeout=5.0):
            mock_su_root.assert_not_called()
            mock_su_unroot.assert_not_called()

        mock_su_root.assert_not_called()
        mock_su_unroot.assert_not_called()

    def test_su_root_and_su_unroot(self) -> None:
        """Test su_root and su_unroot toggle su root injection in _build_adb_cmd."""
        cmd = ["shell", "setprop", "foo", "bar"]
        self.assertFalse(self.adb_obj.is_su_root)

        self.adb_obj.su_root(timeout=10.0)
        self.assertTrue(self.adb_obj.is_su_root)
        self.assertEqual(
            self.adb_obj._build_adb_cmd(cmd),
            [
                "/custom/adb",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "su",
                "root",
                "setprop",
                "foo",
                "bar",
            ],
        )
        # Ensure original cmd list was not mutated
        self.assertEqual(cmd, ["shell", "setprop", "foo", "bar"])

        self.adb_obj.su_unroot(timeout=10.0)
        self.assertFalse(self.adb_obj.is_su_root)
        self.assertEqual(
            self.adb_obj._build_adb_cmd(cmd),
            [
                "/custom/adb",
                "-s",
                _SERIAL_NUMBER,
                "shell",
                "setprop",
                "foo",
                "bar",
            ],
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_setprop(self, mock_run: mock.Mock) -> None:
        """Test setprop runs adb shell setprop command."""
        self.adb_obj.setprop("persist.test.prop", "val123", timeout=5.0)

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "setprop", "persist.test.prop", "val123"],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_getprop(self, mock_run: mock.Mock) -> None:
        """Test getprop runs adb shell getprop command and strips result."""
        mock_run.return_value = "  val123 \n"

        result = self.adb_obj.getprop("persist.test.prop", timeout=5.0)

        self.assertEqual(result, "val123")
        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "getprop", "persist.test.prop"],
            timeout=5.0,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_setprop_quotes_special_characters(
        self, mock_run: mock.Mock
    ) -> None:
        """Test setprop quotes prop_name with spaces/metacharacters and empty value."""
        self.adb_obj.setprop("debug.foo bar; echo $HOME", "")

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "setprop", "'debug.foo bar; echo $HOME'", "''"],
            timeout=None,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "run", autospec=True)
    def test_getprop_quotes_special_characters(
        self, mock_run: mock.Mock
    ) -> None:
        """Test getprop quotes prop_name with spaces and shell metacharacters."""
        mock_run.return_value = ""

        self.adb_obj.getprop("debug.foo bar; echo $HOME")

        mock_run.assert_called_once_with(
            self.adb_obj,
            ["shell", "getprop", "'debug.foo bar; echo $HOME'"],
            timeout=None,
            attempts=adb._DEFAULT_RUN_ATTEMPTS,
        )

    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_immediately(
        self, mock_getprop: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete succeeds immediately when sys.boot_completed is 1."""
        mock_getprop.return_value = "1"

        self.adb_obj.wait_for_boot_complete()

        mock_getprop.assert_called_once_with(
            self.adb_obj,
            "sys.boot_completed",
            timeout=adb._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
            attempts=1,
        )

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_after_polling(
        self, mock_getprop: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete polls until sys.boot_completed is 1."""
        mock_getprop.side_effect = ["0", "", "1"]

        self.adb_obj.wait_for_boot_complete(poll_interval=0.5)

        self.assertEqual(mock_getprop.call_count, 3)
        mock_sleep.assert_has_calls([mock.call(0.5), mock.call(0.5)])

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_success_after_adb_errors(
        self, mock_getprop: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test wait_for_boot_complete ignores AdbErrors during polling and succeeds."""
        mock_getprop.side_effect = [
            adb_errors.AdbCommandError("device offline"),
            "0",
            "1",
        ]

        self.adb_obj.wait_for_boot_complete(poll_interval=0.5)

        self.assertEqual(mock_getprop.call_count, 3)
        mock_sleep.assert_has_calls([mock.call(0.5), mock.call(0.5)])

    @mock.patch("time.sleep", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_retries_after_getprop_timeout(
        self, mock_getprop: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test a timed-out getprop poll is treated as not booted and polling continues."""
        mock_getprop.side_effect = [
            adb_errors.AdbTimeoutError("getprop timed out"),
            "1",
        ]

        self.adb_obj.wait_for_boot_complete(poll_interval=0.5)

        mock_getprop.assert_has_calls(
            [
                mock.call(
                    self.adb_obj,
                    "sys.boot_completed",
                    timeout=adb._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
                    attempts=1,
                )
            ]
            * 2
        )
        mock_sleep.assert_called_once_with(0.5)

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", autospec=True)
    @mock.patch.object(adb.Adb, "getprop", autospec=True)
    def test_wait_for_boot_complete_timeout(
        self,
        mock_getprop: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test wait_for_boot_complete raises AdbTimeoutError when timeout expires."""
        mock_time.side_effect = [0.0, 1.0, 10.0]
        mock_getprop.return_value = "0"

        with self.assertRaises(adb_errors.AdbTimeoutError):
            self.adb_obj.wait_for_boot_complete(timeout=5.0)

        mock_getprop.assert_called_once_with(
            self.adb_obj,
            "sys.boot_completed",
            timeout=adb._BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
            attempts=1,
        )
        mock_sleep.assert_called_once_with(1.0)

    @mock.patch.object(
        host_shell,
        "run",
        side_effect=errors.HoneydewTimeoutError("timeout"),
        autospec=True,
    )
    def test_custom_default_attempts_in_init(
        self, mock_host_run: mock.Mock
    ) -> None:
        """Test Adb(..., attempts=1) defaults commands to 1 attempt without retries."""
        with mock.patch.object(adb.Adb, "_cache_adbd_pid"):
            obj = adb.Adb(
                device_name=_DEVICE_NAME,
                serial_number=_SERIAL_NUMBER,
                ffx_transport=self.mock_ffx,
                run_isolated_server=False,
                attempts=1,
            )
        try:
            self.assertEqual(obj._attempts, 1)
            with self.assertRaises(adb_errors.AdbTimeoutError):
                obj.run(["shell", "echo", "hi"], timeout=10.0)
            mock_host_run.assert_called_once()
        finally:
            obj.close()


class ResolveVendorKeysPathTests(unittest.TestCase):
    """Unit tests for _resolve_vendor_keys_path."""

    def test_explicit_path_non_tar(self) -> None:
        """Test explicit path that is not a tar file is returned as-is."""
        with mock.patch("os.path.exists", return_value=True):
            path, temp_dir = adb._resolve_vendor_keys_path("/custom/keys/dir")
            self.assertEqual(path, "/custom/keys/dir")
            self.assertIsNone(temp_dir)

    def test_env_var_fallback(self) -> None:
        """Test fallback to ADB_VENDOR_KEYS environment variable."""
        with (
            mock.patch("os.path.exists", return_value=True),
            mock.patch.dict(os.environ, {"ADB_VENDOR_KEYS": "/env/keys"}),
        ):
            path, temp_dir = adb._resolve_vendor_keys_path(None)
            self.assertEqual(path, "/env/keys")
            self.assertIsNone(temp_dir)

    def test_nonexistent_path_falls_back_to_bundled(self) -> None:
        """Test nonexistent vendor_keys_path logs warning and falls back to bundled keys."""
        mock_temp_dir = mock.MagicMock(spec=tempfile.TemporaryDirectory)
        mock_temp_dir.name = "/bundled/extracted/keys"
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(
                adb, "_get_bundled_keys_tar", return_value=mock_temp_dir
            ) as mock_bundled,
        ):
            path, temp_dir = adb._resolve_vendor_keys_path(
                "/nonexistent/keys.tar"
            )
            mock_bundled.assert_called_once()
            self.assertEqual(path, "/bundled/extracted/keys")
            self.assertEqual(temp_dir, mock_temp_dir)

    def test_tar_extraction_does_not_mutate_env_var(self) -> None:
        """Test extracting a valid tar archive without modifying os.environ."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                key_content = b"test_key_content"
                info = tarfile.TarInfo(name="test.adb_key")
                info.size = len(key_content)
                tar.addfile(info, io.BytesIO(key_content))

            with mock.patch.dict(os.environ, {}, clear=True):
                path, temp_dir = adb._resolve_vendor_keys_path(tar_path)
                try:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_file = os.path.join(temp_dir.name, "test.adb_key")
                    self.assertTrue(os.path.exists(extracted_file))
                    with open(extracted_file, "rb") as f:
                        self.assertEqual(f.read(), key_content)
                finally:
                    if temp_dir:
                        temp_dir.cleanup()

    def test_tar_with_symlink_raises(self) -> None:
        """Test that tar with symlink raises AdbError."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "symlink_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                info = tarfile.TarInfo(name="symlink_key")
                info.type = tarfile.SYMTYPE
                info.linkname = "/etc/passwd"
                tar.addfile(info)

            with self.assertRaises(adb_errors.AdbError):
                adb._resolve_vendor_keys_path(tar_path)

    def test_tar_with_traversal_raises(self) -> None:
        """Test that tar with path traversal raises AdbError."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "traversal_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                info = tarfile.TarInfo(name="../escape.key")
                info.size = 0
                tar.addfile(info, io.BytesIO(b""))

            with self.assertRaises(adb_errors.AdbError):
                adb._resolve_vendor_keys_path(tar_path)

    def test_python_data_resources_fallback(self) -> None:
        """Test fallback to Python data resources (honeydew.data/adb_keys.tar)."""
        with tempfile.TemporaryDirectory() as test_dir:
            tar_path = os.path.join(test_dir, "adb_keys.tar")
            with tarfile.open(tar_path, "w") as tar:
                key_content = b"bundled_key_content"
                info = tarfile.TarInfo(name="bundled.adb_key")
                info.size = len(key_content)
                tar.addfile(info, io.BytesIO(key_content))

            mock_resource = mock.MagicMock()
            mock_resource.joinpath.return_value = Path(tar_path)

            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    resources, "files", return_value=mock_resource
                ),
            ):
                path, temp_dir = adb._resolve_vendor_keys_path(None)
                try:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_file = os.path.join(
                        temp_dir.name, "bundled.adb_key"
                    )
                    self.assertTrue(os.path.exists(extracted_file))
                    with open(extracted_file, "rb") as f:
                        self.assertEqual(f.read(), key_content)
                finally:
                    if temp_dir:
                        temp_dir.cleanup()

    def test_real_bundled_keys_tar_end_to_end(self) -> None:
        """Test end-to-end extraction of the actual bundled honeydew.data/adb_keys.tar."""
        has_bundled_tar = False
        try:
            from honeydew import data  # type: ignore[attr-defined]

            has_bundled_tar = (
                resources.files(data).joinpath("adb_keys.tar").is_file()
            )
        except (ImportError, FileNotFoundError, AttributeError, TypeError):
            pass

        with mock.patch.dict(os.environ, {}, clear=True):
            path, temp_dir = adb._resolve_vendor_keys_path(None)
            try:
                if has_bundled_tar:
                    self.assertIsNotNone(temp_dir)
                    assert temp_dir is not None
                    self.assertEqual(path, temp_dir.name)
                    self.assertNotIn("ADB_VENDOR_KEYS", os.environ)
                    extracted_keys = [
                        f
                        for f in os.listdir(temp_dir.name)
                        if f.endswith(".adb_key")
                    ]
                    self.assertGreater(len(extracted_keys), 0)
                else:
                    self.assertIsNone(path)
                    self.assertIsNone(temp_dir)
            finally:
                if temp_dir:
                    temp_dir.cleanup()


class AdbServerTests(unittest.TestCase):
    """Unit tests for AdbServer."""

    def test_check_for_conflicting_adb_servers_kills_conflicting_pids(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers kills conflicting server PIDs."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        server._process = mock.Mock(pid=1000)
        pgrep_output = "\n".join(
            [
                f"{os.getpid()} adb -P 5037 fork-server server",
                "1000 adb -P 5037 fork-server server",
                "1001 adb -P 5037 fork-server server",
                "1002 adb -P 60855 --one-device target_serial server nodaemon",
                "1003 adb -P 60856 --one-device other_serial server nodaemon",
                "1004 adb -s target_serial wait-for-device",
            ]
        )
        with (
            mock.patch.object(
                subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    args=["pgrep", "-a", "adb"],
                    returncode=0,
                    stdout=pgrep_output,
                    stderr="",
                ),
            ),
            mock.patch.object(
                os,
                "kill",
                side_effect=[
                    None,  # SIGKILL 1001
                    None,  # SIGKILL 1002
                    None,  # Poll 1: 1001 still alive
                    ProcessLookupError(),  # Poll 1: 1002 exited
                    ProcessLookupError(),  # Poll 2: 1001 exited
                ],
            ) as mock_kill,
            mock.patch.object(time, "sleep") as mock_sleep,
        ):
            server._check_for_conflicting_adb_servers()
            self.assertEqual(
                mock_kill.call_args_list,
                [
                    mock.call(1001, signal.SIGKILL),
                    mock.call(1002, signal.SIGKILL),
                    mock.call(1001, 0),
                    mock.call(1002, 0),
                    mock.call(1001, 0),
                ],
            )
            mock_sleep.assert_called_once_with(0.05)

    def test_check_for_conflicting_adb_servers_ignores_pgrep_failure(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers skips the check when pgrep
        cannot be run or its output cannot be decoded."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        for err in [
            FileNotFoundError("pgrep"),
            UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid start byte"),
        ]:
            with (
                self.subTest(err=err),
                mock.patch.object(
                    subprocess,
                    "run",
                    side_effect=err,
                ) as mock_run,
                mock.patch.object(os, "kill") as mock_kill,
            ):
                server._check_for_conflicting_adb_servers()

                mock_run.assert_called_once()
                mock_kill.assert_not_called()

    def test_check_for_conflicting_adb_servers_propagates_unexpected_exceptions(
        self,
    ) -> None:
        """Test _check_for_conflicting_adb_servers propagates unexpected
        exceptions (e.g. Mobly signals) raised while running pgrep."""
        server = adb_server.AdbServer(
            adb_binary_path="/custom/adb",
            serial_id="target_serial",
        )
        with (
            mock.patch.object(
                subprocess,
                "run",
                side_effect=signals.TestAbortAll("abort"),
            ) as mock_run,
            mock.patch.object(os, "kill") as mock_kill,
        ):
            with self.assertRaises(signals.TestAbortAll):
                server._check_for_conflicting_adb_servers()

            mock_run.assert_called_once()
            mock_kill.assert_not_called()

    def test_set_pdeathsig_on_linux(self) -> None:
        """Test _set_pdeathsig sets PR_SET_PDEATHSIG to SIGKILL on Linux."""
        mock_libc = mock.Mock()
        with mock.patch.object(adb_server, "_LIBC", mock_libc):
            adb_server._set_pdeathsig()
            mock_libc.prctl.assert_called_once_with(
                adb_server._PR_SET_PDEATHSIG, signal.SIGKILL
            )


if __name__ == "__main__":
    unittest.main()
