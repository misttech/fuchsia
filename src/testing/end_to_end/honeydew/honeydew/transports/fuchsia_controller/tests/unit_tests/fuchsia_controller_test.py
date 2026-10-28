# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.transports.fuchsia_controller.py."""

import ipaddress
import unittest
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller
from honeydew import affordances_capable, errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types
from mobly import signals

_TARGET_NAME: str = "fuchsia-emulator"

_IPV6: str = "fe80::4fce:3102:ef13:888c%1"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)

_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_IP_PORT = custom_types.IpPort(ip=_SSH_ADDRESS, port=_SSH_PORT)


_INPUT_ARGS: dict[str, Any] = {
    "target_name": _TARGET_NAME,
    "target_ip_port": _TARGET_IP_PORT,
    "BuildInfo": custom_types.FidlEndpoint(
        "/core/build-info", "fuchsia.buildinfo.Provider"
    ),
}

_SHARED_DATA: str = "/tmp/shared_data"

_MOCK_ARGS: dict[str, Any] = {
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
        shared_data=_SHARED_DATA,
    ),
}


# pylint: disable=protected-access
class FuchsiaControllerTests(unittest.TestCase):
    """Unit tests for honeydew.transports.fuchsia_controller.py."""

    def setUp(self) -> None:
        super().setUp()

        with mock.patch.object(
            fuchsia_controller.Context,
            "target_wait",
            autospec=True,
        ) as mock_target_wait:
            self.fuchsia_controller_obj_wo_device_ip = (
                fc_transport.FuchsiaController(
                    target_name=_INPUT_ARGS["target_name"],
                    ffx_config_data=_MOCK_ARGS["ffx_config_data"],
                )
            )
        mock_target_wait.assert_called()

        mock_target_wait.reset_mock()

        with (
            mock.patch.object(
                fuchsia_controller.Context,
                "target_wait",
                autospec=True,
            ) as mock_target_wait,
        ):
            self.device_address_change = mock.MagicMock(
                spec=affordances_capable.FuchsiaDeviceAddressChange
            )

            self.fuchsia_controller_obj_with_device_ip = (
                fc_transport.FuchsiaController(
                    target_name=_INPUT_ARGS["target_name"],
                    target_addr=_INPUT_ARGS["target_ip_port"],
                    ffx_config_data=_MOCK_ARGS["ffx_config_data"],
                    device_address_change=self.device_address_change,
                )
            )
        mock_target_wait.assert_called()
        self.device_address_change.register_for_on_device_address_change.assert_called_once()

    def test_init_with_target_addr_raises_when_no_device_address_change(
        self,
    ) -> None:
        """Test case for FuchsiaController() when called with target_addr and no device_address_change."""
        with self.assertRaises(ValueError):
            fc_transport.FuchsiaController(
                target_name=_INPUT_ARGS["target_name"],
                target_addr=_INPUT_ARGS["target_ip_port"],
                ffx_config_data=_MOCK_ARGS["ffx_config_data"],
            )

    def test_create_context(self) -> None:
        """Test case for fuchsia_controller_transport.create_context()."""
        self.fuchsia_controller_obj_with_device_ip.create_context()

    @mock.patch.object(
        fc_transport.FuchsiaController,
        "create_context",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    def test_on_device_address_change(
        self,
        mock_check_connection: mock.Mock,
        mock_create_context: mock.Mock,
    ) -> None:
        """Test case for _on_device_address_change()."""
        new_ip_port = custom_types.IpPort(
            ip=ipaddress.ip_address("127.0.0.1"), port=8022
        )
        self.fuchsia_controller_obj_with_device_ip._on_device_address_change(
            new_ip_port
        )
        self.assertEqual(
            self.fuchsia_controller_obj_with_device_ip._target_addr,
            new_ip_port,
        )
        self.assertEqual(
            self.fuchsia_controller_obj_with_device_ip._target,
            str(new_ip_port),
        )
        mock_create_context.assert_called_once_with(
            self.fuchsia_controller_obj_with_device_ip
        )
        mock_check_connection.assert_called_once_with(
            self.fuchsia_controller_obj_with_device_ip
        )

    @mock.patch.object(
        fc_transport.FuchsiaController,
        "create_context",
        autospec=True,
    )
    @mock.patch.object(
        fc_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    def test_on_device_address_change_usb(
        self,
        mock_check_connection: mock.Mock,
        mock_create_context: mock.Mock,
    ) -> None:
        """Test case for _on_device_address_change() with UsbAddress."""
        new_usb = custom_types.UsbAddress(target_id=42)
        self.fuchsia_controller_obj_with_device_ip._on_device_address_change(
            new_usb
        )
        self.assertEqual(
            self.fuchsia_controller_obj_with_device_ip._target_addr,
            new_usb,
        )
        self.assertEqual(
            self.fuchsia_controller_obj_with_device_ip._target,
            "usb:cid:42",
        )
        mock_create_context.assert_called_once_with(
            self.fuchsia_controller_obj_with_device_ip
        )
        mock_check_connection.assert_called_once_with(
            self.fuchsia_controller_obj_with_device_ip
        )

    @mock.patch.object(
        fuchsia_controller,
        "Context",
        autospec=True,
    )
    def test_create_context_with_ssh_auth_sock(
        self, mock_fc_context: mock.Mock
    ) -> None:
        """Test case for fuchsia_controller_transport.create_context() with ssh_auth_sock."""
        ssh_auth_sock = "/tmp/ssh_auth_sock"
        ffx_config_data = ffx_config.FfxConfigData(
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
            ssh_auth_sock=ssh_auth_sock,
            shared_data=_SHARED_DATA,
        )
        fc_transport.FuchsiaController(
            target_name=_INPUT_ARGS["target_name"],
            ffx_config_data=ffx_config_data,
        )

        mock_fc_context.assert_called_with(
            config={
                "log.level": "debug",
                "log.dir": "/tmp/logs",
                "shared_data": _SHARED_DATA,
                "connectivity.enable_usb": "false",
                "connectivity.usb_driver_autostart": "false",
                "ssh.auth-sock": ssh_auth_sock,
            },
            target=_INPUT_ARGS["target_name"],
        )

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        autospec=True,
    )
    @mock.patch.object(
        fuchsia_controller,
        "Context",
        autospec=True,
    )
    def test_create_context_with_identities_only(
        self,
        mock_fc_context: mock.Mock,
        mock_target_wait: mock.Mock,
    ) -> None:
        """Verify create_context() with identities_only passed in ffx_config_data."""
        ffx_config_data = ffx_config.FfxConfigData(
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
            identities_only=True,
            shared_data=_SHARED_DATA,
        )
        fc_transport.FuchsiaController(
            target_name=_INPUT_ARGS["target_name"],
            ffx_config_data=ffx_config_data,
        )

        mock_fc_context.assert_called_with(
            config={
                "log.level": "debug",
                "log.dir": "/tmp/logs",
                "shared_data": _SHARED_DATA,
                "connectivity.enable_usb": "false",
                "connectivity.usb_driver_autostart": "false",
                "ssh.identities-only": "true",
            },
            target=_INPUT_ARGS["target_name"],
        )

    @mock.patch.object(
        fuchsia_controller,
        "Context",
        side_effect=fuchsia_controller.FcTransportStatus(
            fuchsia_controller.FcTransportStatus.FC_ERR_INVALID_ARGS
        ),
        autospec=True,
    )
    def test_create_context_creation_error(
        self, mock_fc_context: mock.Mock
    ) -> None:
        """Verify create_context() when the fuchsia controller Context creation
        raises an error."""
        with self.assertRaises(fc_errors.FuchsiaControllerError):
            self.fuchsia_controller_obj_with_device_ip.create_context()

        mock_fc_context.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "connect_device_proxy",
        autospec=True,
    )
    def test_connect_device_proxy(
        self, mock_fc_connect_device_proxy: mock.Mock
    ) -> None:
        """Test case for fuchsia_controller_transport.connect_device_proxy()"""
        self.fuchsia_controller_obj_with_device_ip.connect_device_proxy(
            _INPUT_ARGS["BuildInfo"]
        )

        mock_fc_connect_device_proxy.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "connect_device_proxy",
        side_effect=fuchsia_controller.FcTransportStatus(
            fuchsia_controller.FcTransportStatus.FC_ERR_INVALID_ARGS
        ),
        autospec=True,
    )
    def test_connect_device_proxy_error(
        self, mock_fc_connect_device_proxy: mock.Mock
    ) -> None:
        """Test case for fuchsia_controller_transport.connect_device_proxy()"""
        with self.assertRaises(fc_errors.FuchsiaControllerError):
            self.fuchsia_controller_obj_with_device_ip.connect_device_proxy(
                _INPUT_ARGS["BuildInfo"]
            )

        mock_fc_connect_device_proxy.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        autospec=True,
    )
    def test_check_connection(self, mock_target_wait: mock.Mock) -> None:
        """Testcase for FuchsiaController.check_connection()"""
        self.fuchsia_controller_obj_with_device_ip.check_connection()

        mock_target_wait.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        side_effect=RuntimeError("error"),
        autospec=True,
    )
    def test_check_connection_raises(self, mock_target_wait: mock.Mock) -> None:
        """Testcase for FuchsiaController.check_connection() raises
        FuchsiaControllerConnectionError"""
        with self.assertRaises(fc_errors.FuchsiaControllerConnectionError):
            self.fuchsia_controller_obj_with_device_ip.check_connection()

        mock_target_wait.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        side_effect=fuchsia_controller.FcTransportStatus(
            fuchsia_controller.FcTransportStatus.FC_ERR_INVALID_ARGS
        ),
        autospec=True,
    )
    def test_check_connection_raises_on_fc_transport_status(
        self, mock_target_wait: mock.Mock
    ) -> None:
        """Test check_connection() wraps FcTransportStatus"""
        with self.assertRaises(fc_errors.FuchsiaControllerConnectionError):
            self.fuchsia_controller_obj_with_device_ip.check_connection()

        mock_target_wait.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        side_effect=errors.HoneydewError("error"),
        autospec=True,
    )
    def test_check_connection_raises_on_honeydew_error(
        self, mock_target_wait: mock.Mock
    ) -> None:
        """Test check_connection() wraps HoneydewError"""
        with self.assertRaises(fc_errors.FuchsiaControllerConnectionError):
            self.fuchsia_controller_obj_with_device_ip.check_connection()

        mock_target_wait.assert_called()

    @mock.patch.object(
        fuchsia_controller.Context,
        "target_wait",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_check_connection_propagates_unexpected_exceptions(
        self, mock_target_wait: mock.Mock
    ) -> None:
        """Test check_connection() propagates non-FC exceptions"""
        with self.assertRaises(signals.TestAbortAll):
            self.fuchsia_controller_obj_with_device_ip.check_connection()

        mock_target_wait.assert_called()

    @mock.patch.object(
        fuchsia_controller,
        "Context",
        side_effect=RuntimeError("internal error"),
        autospec=True,
    )
    def test_create_context_raises_on_runtime_error(
        self, mock_fc_context: mock.Mock
    ) -> None:
        """Test create_context() wraps RuntimeError"""
        with self.assertRaises(fc_errors.FuchsiaControllerError):
            self.fuchsia_controller_obj_with_device_ip.create_context()

        mock_fc_context.assert_called()

    @mock.patch.object(
        fuchsia_controller,
        "Context",
        side_effect=signals.TestAbortAll("abort"),
        autospec=True,
    )
    def test_create_context_propagates_unexpected_exceptions(
        self, mock_fc_context: mock.Mock
    ) -> None:
        """Test create_context() propagates non-FC exceptions"""
        with self.assertRaises(signals.TestAbortAll):
            self.fuchsia_controller_obj_with_device_ip.create_context()

        mock_fc_context.assert_called()
