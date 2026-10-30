# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.device_classes.android_device.py."""

import dataclasses
import ipaddress
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances.android_diagnostics import (
    android_diagnostics as android_diagnostics_affordance,
)
from honeydew.affordances.android_package import (
    android_package as android_package_affordance,
)
from honeydew.affordances.android_session import (
    android_session as android_session_affordance,
)
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.device_classes import android_device
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types
from mobly import signals

# pylint: disable=protected-access

_IPV6: str = "fe80::4fce:3102:ef13:888c%qemu"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)
_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_SSH_ADDRESS = custom_types.IpPort(ip=_SSH_ADDRESS, port=_SSH_PORT)

_INPUT_ARGS: dict[str, Any] = {
    "device_name": "fuchsia-android-device",
    "device_ip": _TARGET_SSH_ADDRESS,
    "device_serial_socket": "/tmp/socket",
    "ffx_config_data": ffx_config.FfxConfigData(
        logs_dir="/tmp/logs",
        binary_path="/bin/ffx",
        logs_level="debug",
        enable_usb=False,
        usb_socket_path=None,
        usb_driver_autostart=False,
        subtools_search_path=None,
        proxy_timeout_secs=None,
        ssh_keepalive_timeout=None,
        emu_instance_dir=None,
        ssh_private_keys=None,
        ssh_public_keys=None,
        shared_data="/tmp/shared_data",
    ),
}


class TestAndroidDevice(android_device.AndroidDevice):
    """Concrete test subclass with device-specific defaults configured."""

    sysui_package = "com.test.sysui"
    _default_screen_size = ui_types.Size(600, 800)


class AndroidDeviceTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for honeydew.device_classes.android_device.py."""

    def setUp(self) -> None:
        adb_binary_patcher = mock.patch.object(
            adb_transport,
            "_get_adb_binary",
            return_value="/bin/adb",
            autospec=True,
        )
        adb_binary_patcher.start()
        self.addCleanup(adb_binary_patcher.stop)

        adb_server_patcher = mock.patch.object(
            adb_transport,
            "AdbServer",
            autospec=True,
        )
        adb_server_patcher.start()
        self.addCleanup(adb_server_patcher.stop)

        adb_cache_pid_patcher = mock.patch.object(
            adb_transport.Adb,
            "_cache_adbd_pid",
            autospec=True,
        )
        adb_cache_pid_patcher.start()
        self.addCleanup(adb_cache_pid_patcher.stop)

        ffx_monitor_patcher = mock.patch.object(
            ffx.FFX,
            "_check_running_monitor",
            return_value=False,
            autospec=True,
        )
        ffx_monitor_patcher.start()
        self.addCleanup(ffx_monitor_patcher.stop)

        with (
            mock.patch.object(
                fc_transport.FuchsiaController,
                "create_context",
                autospec=True,
            ),
            mock.patch.object(
                ffx.FFX,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                adb_transport.Adb,
                "verify_supported",
                autospec=True,
            ),
            mock.patch.object(
                adb_transport.Adb,
                "check_connection",
                autospec=True,
            ),
        ):
            self.ad_obj = android_device.AndroidDevice(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["device_name"],
                    serial_number="12345678",
                    address=_INPUT_ARGS["device_ip"],
                    serial_socket=_INPUT_ARGS["device_serial_socket"],
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            )
            self.ad_obj.__dict__.pop("adb", None)

    def test_device_hierarchy(self) -> None:
        """Test case to make sure AndroidDevice inherits properly."""
        self.assertIsInstance(self.ad_obj, fuchsia_device.FuchsiaDevice)
        self.assertIsInstance(self.ad_obj, android_device.AndroidDevice)

    def test_adb_transport_disabled(self) -> None:
        """Test case to make sure AndroidDevice raises NotEnabledError when adb is disabled."""
        config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        with (
            mock.patch.object(
                ffx.FFX,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "check_connection",
                autospec=True,
            ),
            mock.patch.object(
                fc_transport.FuchsiaController,
                "create_context",
                autospec=True,
            ),
        ):
            ad_obj = android_device.AndroidDevice(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["device_name"],
                    serial_number=None,
                    address=_INPUT_ARGS["device_ip"],
                    serial_socket=_INPUT_ARGS["device_serial_socket"],
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
                config=config,
            )
            with self.assertRaises(errors.NotEnabledError):
                _ = ad_obj.adb

    def test_is_pontis_target_device_info_address(self) -> None:
        """Test case for _is_pontis_target() using device_info.address."""
        orig_device_info = self.ad_obj._device_info
        try:
            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="12345678",
                address=custom_types.IpPort(
                    ip=ipaddress.IPv6Address(
                        "fe80::bcd3:bbce:3ee9:4d87%pontis"
                    ),
                    port=8022,
                ),
                serial_socket=_INPUT_ARGS["device_serial_socket"],
            )
            self.assertTrue(self.ad_obj._is_pontis_target())

            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="12345678",
                address=custom_types.IpPort(
                    ip=ipaddress.IPv4Address("192.168.1.2"),
                    port=8022,
                ),
                serial_socket=_INPUT_ARGS["device_serial_socket"],
            )
            self.assertFalse(self.ad_obj._is_pontis_target())
        finally:
            self.ad_obj._device_info = orig_device_info

    def test_is_pontis_target_ffx_fallback(self) -> None:
        """Test case for _is_pontis_target() falling back to FFX get_target_address."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=None,
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        try:
            with mock.patch.object(
                ffx.FFX,
                "get_target_address",
                return_value=custom_types.IpPort(
                    ip=ipaddress.IPv6Address(
                        "fe80::bcd3:bbce:3ee9:4d87%pontis"
                    ),
                    port=None,
                ),
                autospec=True,
            ):
                self.assertTrue(self.ad_obj._is_pontis_target())

            with mock.patch.object(
                ffx.FFX,
                "get_target_address",
                side_effect=ffx_errors.FfxCommandError("FFX error"),
                autospec=True,
            ):
                self.assertFalse(self.ad_obj._is_pontis_target())
        finally:
            self.ad_obj._device_info = orig_device_info

    def test_adb_transport_disabled_on_pontis(self) -> None:
        """Test case to make sure AndroidDevice raises NotEnabledError on Pontis targets."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            with self.assertRaises(errors.NotEnabledError):
                _ = self.ad_obj.adb
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    def test_adb_transport(
        self,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice supports adb transport."""
        self.assertIsInstance(
            self.ad_obj.adb,
            adb_transport.Adb,
        )
        mock_verify_supported.assert_called_once()
        mock_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_derives_output_dir_from_ffx_logs_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb derives output_dir from ffx_config_data.logs_dir."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/logs"
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/logs/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_derives_output_dir_when_logs_dir_ends_with_ffx(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb derives output_dir when logs_dir ends with ffx."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/test_run/ffx"
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/test_run/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_uses_configured_output_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb uses output_dir from adb_config when provided."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir="/tmp/logs"
            )
            self.ad_obj._config = {
                "transports": {
                    "adb": {
                        "output_dir": "/custom/adb_logs",
                    }
                }
            }

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir="/custom/adb_logs",
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_output_dir_none_when_no_logs_dir(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test adb passes output_dir=None when logs_dir is not set."""
        orig_ffx_config_data = self.ad_obj._ffx_config_data
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._ffx_config_data = dataclasses.replace(
                orig_ffx_config_data, logs_dir=""
            )
            self.ad_obj._config = None

            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=None,
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=adb_transport._DEFAULT_RUN_ATTEMPTS,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._ffx_config_data = orig_ffx_config_data
            self.ad_obj._config = orig_config

    @mock.patch.object(adb_transport, "Adb", autospec=True)
    def test_adb_transport_custom_attempts_config(
        self, mock_adb_cls: mock.Mock
    ) -> None:
        """Test case to verify honeydew_config.transports.adb.attempts is passed to Adb."""
        orig_config = self.ad_obj._config
        try:
            self.ad_obj._config = {"transports": {"adb": {"attempts": 1}}}
            _ = self.ad_obj.adb

            mock_adb_cls.assert_called_once_with(
                device_name=self.ad_obj.device_name,
                serial_number=self.ad_obj._device_info.serial_number,
                run_isolated_server=True,
                output_dir=Path("/tmp/logs/adb"),
                vendor_keys_path=None,
                ffx_transport=self.ad_obj.ffx,
                attempts=1,
            )
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._config = orig_config

    # TODO(b/564946370): Remove this test once 'adb root' is stable on USB.
    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    async def test_adb_routes_root_methods_to_su_root(
        self,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Verifies AndroidDevice.adb routes root methods to su_root."""
        mock_su_root = mock.Mock()
        mock_su_unroot = mock.Mock()
        mock_use_su_root = mock.MagicMock()

        with (
            mock.patch.object(adb_transport.Adb, "su_root", mock_su_root),
            mock.patch.object(adb_transport.Adb, "su_unroot", mock_su_unroot),
            mock.patch.object(
                adb_transport.Adb, "use_su_root", mock_use_su_root
            ),
        ):
            self.ad_obj.adb.root(timeout=5.0)
            mock_su_root.assert_called_once_with(timeout=5.0)

            self.ad_obj.adb.unroot(timeout=5.0)
            mock_su_unroot.assert_called_once_with(timeout=5.0)

            with self.ad_obj.adb.use_adb_root(timeout=5.0):
                mock_use_su_root.assert_called_once_with(timeout=5.0)
            mock_use_su_root.return_value.__enter__.assert_called_once()
            mock_use_su_root.return_value.__exit__.assert_called_once()

            mock_use_su_root.reset_mock()
            async with self.ad_obj.adb.use_adb_root(timeout=5.0):
                mock_use_su_root.assert_called_once_with(timeout=5.0)
            mock_use_su_root.return_value.__enter__.assert_called_once()
            mock_use_su_root.return_value.__exit__.assert_called_once()

        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("Not supported"),
        autospec=True,
    )
    def test_adb_transport_not_supported(
        self, mock_verify_supported: mock.Mock
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when
        ADB is not supported on the target."""
        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb
        mock_verify_supported.assert_called_once()

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        return_value="ffx-serial-1234",
    )
    def test_adb_transport_fallback_ffx_serial(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice falls back to FFX serial_number
        when serial_number is not in device_info."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )

        adb_inst: adb_transport.Adb = self.ad_obj.adb
        self.assertIsInstance(adb_inst, adb_transport.Adb)
        mock_ffx_serial_number.assert_called_once()
        mock_verify_supported.assert_called_once()
        mock_check_connection.assert_called_once()

        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "get_target_address",
        side_effect=ffx_errors.FfxCommandError("ffx error"),
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
    )
    def test_adb_transport_use_adb_usb_false_defaults_to_tcp_serial(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_get_target_address: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test AndroidDevice.adb uses default TCP serial when use_adb_usb is False
        without mutating _device_info.serial_number."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}
        self.ad_obj.__dict__.pop("adb", None)

        try:
            adb_inst: adb_transport.Adb = self.ad_obj.adb
            self.assertIsInstance(adb_inst, adb_transport.Adb)
            self.assertEqual(adb_inst.serial_number, "127.0.0.1:5555")
            self.assertIsNone(self.ad_obj._device_info.serial_number)
            mock_get_target_address.assert_called()
            mock_ffx_serial_number.assert_not_called()
            mock_verify_supported.assert_called_once()
            mock_check_connection.assert_called_once()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "get_target_address",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
    )
    def test_adb_transport_use_adb_usb_false_ffx_target_address_and_usb_serial_override(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_get_target_address: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test use_adb_usb=False overrides USB serial via FFX target address."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}

        try:
            mock_get_target_address.return_value = (
                custom_types.IpPort.create_using_ip_and_port("10.0.0.42:8022")
            )
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = custom_types.DeviceInfo(
                name=_INPUT_ARGS["device_name"],
                serial_number="USB-HW-SERIAL-123",
                address=None,
                serial_socket=None,
            )
            adb_inst: adb_transport.Adb = self.ad_obj.adb
            self.assertEqual(adb_inst.serial_number, "10.0.0.42:5555")
            mock_get_target_address.assert_called()
            mock_ffx_serial_number.assert_not_called()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
    )
    def test_adb_transport_use_adb_usb_false_uses_device_info_ip(
        self,
        mock_ffx_serial_number: mock.Mock,
        mock_verify_supported: mock.Mock,
        mock_check_connection: mock.Mock,
    ) -> None:
        """Test AndroidDevice.adb derives TCP serial from device_info.address IP when use_adb_usb is False."""
        orig_device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        orig_config = self.ad_obj._config
        self.ad_obj._config = {"transports": {"adb": {"use_adb_usb": False}}}

        try:
            for addr_str, expected_serial in (
                ("192.168.42.10:8022", "192.168.42.10:5555"),
                ("[fe80::1%eth0]:8022", "[fe80::1%eth0]:5555"),
            ):
                with self.subTest(addr=addr_str):
                    self.ad_obj.__dict__.pop("adb", None)
                    self.ad_obj._device_info = custom_types.DeviceInfo(
                        name=_INPUT_ARGS["device_name"],
                        serial_number=None,
                        address=custom_types.IpPort.create_using_ip_and_port(
                            addr_str
                        ),
                        serial_socket=None,
                    )
                    adb_inst: adb_transport.Adb = self.ad_obj.adb
                    self.assertEqual(adb_inst.serial_number, expected_serial)
                    mock_ffx_serial_number.assert_not_called()
        finally:
            self.ad_obj.__dict__.pop("adb", None)
            self.ad_obj._device_info = orig_device_info
            self.ad_obj._config = orig_config

    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        return_value=None,
    )
    def test_adb_transport_ffx_serial_none(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when
        FFX serial_number is None."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )

        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb

        mock_ffx_serial_number.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        side_effect=ffx_errors.FfxCommandError("FFX error"),
    )
    def test_adb_transport_error(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test case to make sure AndroidDevice raises NotSupportedError when we try to
        access "adb" transport without serial_number and FFX fails."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )

        with self.assertRaises(errors.NotSupportedError):
            _: adb_transport.Adb = self.ad_obj.adb

        mock_ffx_serial_number.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        ffx.FFX,
        "serial_number",
        new_callable=mock.PropertyMock,
        side_effect=signals.TestAbortAll("abort"),
    )
    def test_adb_transport_ffx_serial_propagates_unexpected_exceptions(
        self,
        mock_ffx_serial_number: mock.Mock,
    ) -> None:
        """Test accessing "adb" transport propagates unexpected exceptions
        (e.g. Mobly signals) raised while getting serial_number from FFX."""
        device_info: custom_types.DeviceInfo = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number=None,
            address=None,
            serial_socket=None,
        )

        with self.assertRaises(signals.TestAbortAll):
            _: adb_transport.Adb = self.ad_obj.adb

        mock_ffx_serial_number.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)
        self.ad_obj._device_info = device_info

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_supported(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when ADB is supported."""
        _ = self.ad_obj.adb
        mock_adb_check_connection.reset_mock()

        self.ad_obj.health_check()

        mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
        mock_fc_check_connection.assert_called_once_with(
            self.ad_obj.fuchsia_controller
        )
        mock_adb_check_connection.assert_called_once_with(self.ad_obj.adb)

        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("ADB not supported"),
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_not_supported(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising NotSupportedError when ADB is not supported."""
        with self.assertRaises(errors.NotSupportedError):
            self.ad_obj.health_check()

        mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
        mock_fc_check_connection.assert_called_once_with(
            self.ad_obj.fuchsia_controller
        )
        mock_adb_verify_supported.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_disabled_on_pontis(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when target is connected via Pontis."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            self.ad_obj.health_check()

            mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
            mock_fc_check_connection.assert_called_once_with(
                self.ad_obj.fuchsia_controller
            )
            self.assertNotIn("adb", self.ad_obj.__dict__)
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_disabled(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() when ADB is disabled in config."""
        orig_config = self.ad_obj._config
        self.ad_obj._config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        self.ad_obj.__dict__.pop("adb", None)
        try:
            self.ad_obj.health_check()

            mock_ffx_check_connection.assert_called_once_with(self.ad_obj.ffx)
            mock_fc_check_connection.assert_called_once_with(
                self.ad_obj.fuchsia_controller
            )
        finally:
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        side_effect=adb_errors.AdbConnectionError("ADB connection error"),
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    def test_health_check_adb_connection_error(
        self,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.health_check() raising HealthCheckError on ADB failure."""
        with self.assertRaises(errors.HealthCheckError):
            self.ad_obj.health_check()

        mock_adb_check_connection.assert_called_once()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        autospec=True,
    )
    async def test_on_device_boot(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_adb_check_connection: mock.Mock,
        mock_adb_on_device_boot: mock.Mock,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot()."""
        _ = self.ad_obj.adb
        await self.ad_obj.on_device_boot()
        mock_adb_on_device_boot.assert_called_once_with(self.ad_obj.adb)
        mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    @mock.patch.object(
        adb_transport.Adb,
        "verify_supported",
        side_effect=errors.NotSupportedError("ADB not supported"),
        autospec=True,
    )
    async def test_on_device_boot_adb_not_supported(
        self,
        mock_adb_verify_supported: mock.Mock,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() raising NotSupportedError when ADB is not supported."""
        with self.assertRaises(errors.NotSupportedError):
            await self.ad_obj.on_device_boot()
        mock_adb_verify_supported.assert_called_once()
        mock_super_on_device_boot.assert_not_called()
        self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    async def test_on_device_boot_adb_disabled_on_pontis(
        self,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() when target is connected via Pontis."""
        orig_device_info = self.ad_obj._device_info
        self.ad_obj._device_info = custom_types.DeviceInfo(
            name=_INPUT_ARGS["device_name"],
            serial_number="12345678",
            address=custom_types.IpPort(
                ip=ipaddress.IPv6Address("fe80::bcd3:bbce:3ee9:4d87%pontis"),
                port=8022,
            ),
            serial_socket=_INPUT_ARGS["device_serial_socket"],
        )
        self.ad_obj.__dict__.pop("adb", None)
        try:
            await self.ad_obj.on_device_boot()
            mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
            self.assertNotIn("adb", self.ad_obj.__dict__)
        finally:
            self.ad_obj._device_info = orig_device_info
            self.ad_obj.__dict__.pop("adb", None)

    @mock.patch.object(
        fuchsia_device.FuchsiaDevice,
        "on_device_boot",
        autospec=True,
    )
    async def test_on_device_boot_adb_disabled(
        self,
        mock_super_on_device_boot: mock.Mock,
    ) -> None:
        """Testcase for AndroidDevice.on_device_boot() when ADB is disabled in config."""
        orig_config = self.ad_obj._config
        self.ad_obj._config = {
            "transports": {
                "adb": {
                    "enabled": False,
                }
            }
        }
        self.ad_obj.__dict__.pop("adb", None)
        try:
            await self.ad_obj.on_device_boot()
            mock_super_on_device_boot.assert_called_once_with(self.ad_obj)
        finally:
            self.ad_obj._config = orig_config
            self.ad_obj.__dict__.pop("adb", None)


class AndroidDeviceBaseCapabilitiesTest(unittest.IsolatedAsyncioTestCase):
    """Unit tests for base AndroidDevice profile, geometry, and diagnostics."""

    def setUp(self) -> None:
        super().setUp()
        self.dev_info = custom_types.DeviceInfo(
            name="fuchsia-emulator",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        self.ffx_cfg = mock.MagicMock()
        self.mock_ffx = mock.Mock()
        self.mock_screenshot = mock.Mock()
        self.mock_adb: Any = mock.MagicMock()
        self.mock_adb.run.return_value = ""

        for patcher in (
            mock.patch.object(
                fuchsia_device.FuchsiaDevice, "health_check", autospec=True
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "ffx",
                new_callable=mock.PropertyMock,
                return_value=self.mock_ffx,
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "screenshot",
                new_callable=mock.PropertyMock,
                return_value=self.mock_screenshot,
            ),
            mock.patch.object(
                android_device.AndroidDevice,
                "adb",
                new_callable=mock.PropertyMock,
                return_value=self.mock_adb,
            ),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

        self.device = TestAndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
        )

    def test_init_via_super_fuchsia_device(self) -> None:
        """Verifies initializing AndroidDevice via FuchsiaDevice.__init__."""
        self.assertEqual(self.device.device_name, "fuchsia-emulator")
        self.assertIsNone(self.device._device_info.serial_number)
        self.assertIsNone(self.device._screen_size_override)

    def test_init_use_adb_usb_config(self) -> None:
        """Verifies use_adb_usb=False in config does not mutate DeviceInfo.serial_number."""
        dev_info = custom_types.DeviceInfo(
            name="fuchsia-device",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        dev = TestAndroidDevice(
            device_info=dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"transports": {"adb": {"use_adb_usb": True}}},
        )
        self.assertIsNone(dev._device_info.serial_number)

        dev_tcp = TestAndroidDevice(
            device_info=dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"transports": {"adb": {"use_adb_usb": False}}},
        )
        self.assertIsNone(dev_tcp._device_info.serial_number)

    def test_initialize_called_in_init_for_product_subclass(self) -> None:
        """Verifies __init__ calls initialize() and _product_setup()."""
        with mock.patch.object(
            TestAndroidDevice, "_product_setup", autospec=True
        ) as mock_prod_setup:
            dev = TestAndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
                config={"screen_size": "640x480"},
            )
            self.assertEqual(dev._screen_size_override, ui_types.Size(640, 480))
            self.assertEqual(dev.screen_size(), ui_types.Size(640, 480))
            mock_prod_setup.assert_called_once_with(dev)

    def test_initialize_skips_without_product_profile(self) -> None:
        """Verifies initialize() is a no-op when sysui_package is empty."""
        with mock.patch.object(
            android_device.AndroidDevice, "_product_setup", autospec=True
        ) as mock_prod_setup:
            bare_device = android_device.AndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
            )
            bare_device.initialize()
            mock_prod_setup.assert_not_called()

    def test_initialize_skips_when_adb_disabled(self) -> None:
        """Verifies initialize() skips ADB calls when ADB is disabled."""
        with (
            mock.patch.object(
                TestAndroidDevice,
                "screen_size",
                side_effect=errors.NotEnabledError("adb disabled"),
            ),
            mock.patch.object(
                TestAndroidDevice, "_product_setup", autospec=True
            ) as mock_prod_setup,
        ):
            dev = TestAndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
            )
            dev.initialize()
            mock_prod_setup.assert_not_called()

    def test_screen_size_default_and_config(self) -> None:
        """Verifies screen_size() default and config parsing/validation."""
        self.assertEqual(self.device.screen_size(), ui_types.Size(600, 800))
        dev = TestAndroidDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
            config={"screen_size": "1234x5678"},
        )
        self.assertEqual(dev.screen_size(), ui_types.Size(1234, 5678))

        with self.assertRaises(ValueError):
            TestAndroidDevice(
                device_info=self.dev_info,
                ffx_config_data=self.ffx_cfg,
                config={"screen_size": "invalid_geometry"},
            )

    def test_screen_size_query_and_precedence(self) -> None:
        """Verifies wm size Physical/Override query and fallback."""
        self.mock_adb.run.return_value = (
            "Physical size: 500x1000\nOverride size: 400x800"
        )
        self.assertEqual(self.device.screen_size(), ui_types.Size(400, 800))
        self.assertEqual(
            self.device._screen_size_cache, ui_types.Size(400, 800)
        )

    def test_screen_size_error_fallback(self) -> None:
        """Verifies screen_size falls back on ADB failure."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError(
            "ADB query failure"
        )
        with self.assertLogs(level="WARNING") as log:
            size = self.device.screen_size()
        self.assertEqual(size, ui_types.Size(600, 800))
        self.assertIn("Failed to query screen size via wm size", log.output[0])

    def test_android_session_affordance(self) -> None:
        """Verifies android_session returns a cached AndroidSession."""
        session: Any = self.device.android_session
        self.assertIsInstance(
            session, android_session_affordance.AndroidSession
        )
        self.assertIs(self.device.android_session, session)
        self.assertIs(session._adb, self.mock_adb)

    def test_diagnostics_affordance(self) -> None:
        """Verifies diagnostics returns a cached AndroidDiagnostics."""
        diagnostics: Any = self.device.diagnostics
        self.assertIsInstance(
            diagnostics,
            android_diagnostics_affordance.AndroidDiagnostics,
        )
        self.assertIs(self.device.diagnostics, diagnostics)
        self.assertIs(diagnostics._adb, self.mock_adb)

    def test_android_package_affordance(self) -> None:
        """Verifies android_package returns a cached AndroidPackage."""
        package: Any = self.device.android_package
        self.assertIsInstance(
            package, android_package_affordance.AndroidPackage
        )
        self.assertIs(self.device.android_package, package)
        self.assertIs(package._adb, self.mock_adb)


if __name__ == "__main__":
    unittest.main()
