# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ffx_with_monitor.py."""

import ipaddress
import json
import unittest
from dataclasses import asdict
from typing import Any
from unittest import mock

from honeydew import affordances_capable, errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx_with_monitor
from honeydew.transports.ffx.types import MonitorTargetInfo
from honeydew.typing import custom_types
from honeydew.utils import host_shell

# pylint: disable=protected-access
_TARGET_NAME: str = "fuchsia-emulator"

_IPV6: str = "fe80::4fce:3102:ef13:888c%1"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)

_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_IP_PORT = custom_types.IpPort(ip=_SSH_ADDRESS, port=_SSH_PORT)

_LOGS_DIR: str = "/tmp/logs"
_SHARED_DATA: str = "/tmp/shared_data"
_BINARY_PATH: str = "ffx"
_LOGS_LEVEL: str = "debug"
_ENABLE_USB: bool = False
_USB_SOCKET_PATH: str | None = None
_USB_DRIVER_AUTOSTART: bool = False
_SUBTOOLS_SEARCH_PATH: str = "/subtools"
_PROXY_TIMEOUT_SECS: int = 30
_SSH_KEEPALIVE_TIMEOUT: int = 60

_FFX_TARGET_INFO: dict[str, Any] = {
    "nodename": _TARGET_NAME,
    "rcs_state": "Y",
    "serial": "<unknown>",
    "target_type": "workstation_eng.x64",
    "target_state": "Product",
    "addresses": [_TARGET_IP_PORT],
    "is_default": True,
}

_INPUT_ARGS: dict[str, Any] = {
    "target_query": _TARGET_NAME,
    "target_ip_port": _TARGET_IP_PORT,
    "ffx_config_data": ffx_config.FfxConfigData(
        logs_dir=_LOGS_DIR,
        binary_path=_BINARY_PATH,
        logs_level=_LOGS_LEVEL,
        enable_usb=_ENABLE_USB,
        usb_socket_path=_USB_SOCKET_PATH,
        usb_driver_autostart=_USB_DRIVER_AUTOSTART,
        subtools_search_path=_SUBTOOLS_SEARCH_PATH,
        proxy_timeout_secs=_PROXY_TIMEOUT_SECS,
        ssh_keepalive_timeout=_SSH_KEEPALIVE_TIMEOUT,
        emu_instance_dir=None,
        ssh_private_keys=None,
        ssh_public_keys=None,
        shared_data=_SHARED_DATA,
    ),
}


class FfxWithMonitorTests(unittest.TestCase):
    """Unit tests for ffx_with_monitor.FFXWithMonitor."""

    def setUp(self) -> None:
        super().setUp()

        self.device_ip_change = mock.MagicMock(
            spec=affordances_capable.FuchsiaDeviceIpChange
        )
        with (
            mock.patch.object(
                ffx_with_monitor.FFXWithMonitor,
                "wait_for_rcs_connection",
                autospec=True,
            ) as mock_wait_for_rcs_connection,
            mock.patch.object(
                ffx_with_monitor.FFXWithMonitor,
                "_check_running_monitor",
                return_value=True,
                autospec=True,
            ) as mock_ffx_check_running_monitor,
        ):
            self.ffx_monitor_obj = ffx_with_monitor.FFXWithMonitor(
                target_name=_INPUT_ARGS["target_query"],
                config_data=_INPUT_ARGS["ffx_config_data"],
                target_ip_port=_INPUT_ARGS["target_ip_port"],
                device_ip_change=self.device_ip_change,
            )
        mock_wait_for_rcs_connection.assert_called_once()
        mock_ffx_check_running_monitor.assert_called_once()

    def test_init_monitor_not_running_raises(self) -> None:
        """Test case for FFXWithMonitor() when no monitor is running."""
        with (
            mock.patch.object(
                ffx_with_monitor.FFXWithMonitor,
                "_check_running_monitor",
                return_value=False,
                autospec=True,
            ),
            self.assertRaises(ffx_errors.FfxMonitorNotRunningError),
        ):
            ffx_with_monitor.FFXWithMonitor(
                target_name=_INPUT_ARGS["target_query"],
                config_data=_INPUT_ARGS["ffx_config_data"],
            )

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_get_target_info_from_target_list(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_info_from_target_list returns MonitorTargetInfo as dict."""
        self.assertEqual(
            self.ffx_monitor_obj.get_target_info_from_target_list(),
            asdict(MonitorTargetInfo(**_FFX_TARGET_INFO)),
        )
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_get_target_name(self, mock_get_target_status: mock.Mock) -> None:
        """Verify get_target_name returns nodename from _get_target_status."""
        self.assertEqual(self.ffx_monitor_obj.get_target_name(), _TARGET_NAME)
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_get_target_address(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_address returns IpPort from monitor status."""
        self.assertEqual(
            self.ffx_monitor_obj.get_target_address(), _TARGET_IP_PORT
        )
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(addresses=[]),
        autospec=True,
    )
    def test_get_target_address_empty_raises(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_address raises FfxCommandError when addresses is empty."""
        with self.assertRaises(ffx_errors.FfxCommandError):
            self.ffx_monitor_obj.get_target_address()
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_get_target_board(self, mock_get_target_status: mock.Mock) -> None:
        """Verify get_target_board returns board value of fuchsia device."""
        result: str = self.ffx_monitor_obj.get_target_board()
        self.assertEqual(result, "x64")
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(rcs_state="N"),
        autospec=True,
    )
    def test_get_target_board_rcs_disconnected(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_board raises DeviceNotConnectedError when rcs_state is not Y."""
        with self.assertRaises(errors.DeviceNotConnectedError):
            self.ffx_monitor_obj.get_target_board()
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_get_target_product(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_product returns product value of fuchsia device."""
        result: str = self.ffx_monitor_obj.get_target_product()
        self.assertEqual(result, "workstation_eng")
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(rcs_state="N"),
        autospec=True,
    )
    def test_get_target_product_rcs_disconnected(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Verify get_target_product raises DeviceNotConnectedError when rcs_state is not Y."""
        with self.assertRaises(errors.DeviceNotConnectedError):
            self.ffx_monitor_obj.get_target_product()
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_wait_for_rcs_connection(
        self, mock_get_target_status: mock.Mock
    ) -> None:
        """Test case for wait_for_rcs_connection()."""
        self.ffx_monitor_obj.wait_for_rcs_connection()
        mock_get_target_status.assert_called_once()

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "_get_target_status",
        return_value=MonitorTargetInfo(),
        autospec=True,
    )
    @mock.patch.object(ffx_with_monitor, "time", autospec=True)
    def test_wait_for_rcs_connection_with_timeout_raises(
        self, mock_time: mock.Mock, mock_get_target_status: mock.Mock
    ) -> None:
        """Test case for wait_for_rcs_connection() giving up once the timeout
        expires while the target is not connected."""
        mock_time.monotonic.side_effect = [0.0, 5.0, 10.0]
        with self.assertRaises(errors.DeviceNotConnectedError):
            self.ffx_monitor_obj.wait_for_rcs_connection(timeout=10)
        self.assertEqual(mock_get_target_status.call_count, 2)

    @mock.patch.object(
        host_shell, "run", return_value='{"targets": []}', autospec=True
    )
    def test_get_target_status_includes_shared_data(
        self, mock_host_run: mock.Mock
    ) -> None:
        """Verify _get_target_status includes shared_data in its command."""
        result = self.ffx_monitor_obj._get_target_status()
        self.assertEqual(result, MonitorTargetInfo())
        mock_host_run.assert_called_once()
        cmd = mock_host_run.call_args[1]["cmd"]
        self.assertIn("-c", cmd)
        config_dict = json.loads(cmd[cmd.index("-c") + 1])
        self.assertEqual(config_dict.get("shared_data"), _SHARED_DATA)

    @mock.patch.object(
        ffx_with_monitor.FFXWithMonitor,
        "run",
        return_value=json.dumps(
            {
                "targets": [
                    {
                        "nodename": _TARGET_NAME,
                        "rcs_state": "Y",
                        "serial": "<unknown>",
                        "target_type": "workstation_eng.x64",
                        "target_state": "Product",
                        "addresses": [
                            {"ip": _IPV6, "ssh_port": _SSH_PORT},
                            {"ip": _IPV6, "ssh_port": 0},
                        ],
                        "is_default": True,
                        "is_manual": False,
                    }
                ]
            }
        ),
        autospec=True,
    )
    def test_get_target_status_parses_addresses(
        self, mock_run: mock.Mock
    ) -> None:
        """Verify _get_target_status parses target info and normalizes ssh_port 0 to None."""
        status = self.ffx_monitor_obj._get_target_status()
        self.assertEqual(status.nodename, _TARGET_NAME)
        self.assertEqual(
            status.addresses,
            [
                custom_types.IpPort(ip=_IPV6, port=_SSH_PORT),  # type: ignore[arg-type]
                custom_types.IpPort(ip=_IPV6, port=None),  # type: ignore[arg-type]
            ],
        )
        mock_run.assert_called_once()


if __name__ == "__main__":
    unittest.main()
