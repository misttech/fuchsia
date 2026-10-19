# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.__init__.py."""

import importlib
import inspect
import pkgutil
import unittest
from collections.abc import Callable
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller
import honeydew
from honeydew import device_classes, errors
from honeydew.device_classes import (
    android_device,
    smart_display,
    workbench,
)
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fuchsia_controller_transport,
)
from honeydew.typing import custom_types
from parameterized import param, parameterized

_TARGET_NAME: str = "fuchsia-emulator"

_REMOTE_TARGET_IP_PORT: str = "[::1]:8088"
_REMOTE_TARGET_IP_PORT_OBJ: custom_types.IpPort = (
    custom_types.IpPort.create_using_ip_and_port(_REMOTE_TARGET_IP_PORT)
)

_SHARED_DATA: str = "/tmp/shared_data"

_INPUT_ARGS: dict[str, Any] = {
    "ffx_config_data": ffx_config.FfxConfigData(
        isolate_dir=fuchsia_controller.IsolateDir("/tmp/isolate"),
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
    "target_name": _TARGET_NAME,
    "target_ip_port": _REMOTE_TARGET_IP_PORT_OBJ,
}


class _CustomAndroidDevice(android_device.AndroidDevice):
    """Dummy custom device class for testing."""


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom name function method."""
    test_func_name: str = testcase_func.__name__

    params_dict: dict[str, Any] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(params_dict["label"])

    return f"{test_func_name}_with_{test_label}"


# pylint: disable=protected-access
class InitTests(unittest.TestCase):
    """Unit tests for honeydew.__init__.py."""

    def setUp(self) -> None:
        super().setUp()
        honeydew._REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT.clear()

    def tearDown(self) -> None:
        honeydew._REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT.clear()
        super().tearDown()

    # List all the tests related to public methods
    @mock.patch.object(
        honeydew,
        "_get_device_class",
        return_value=FuchsiaDevice,
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    @mock.patch.object(
        fuchsia_controller_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch("fuchsia_controller_py.Context", autospec=True)
    def test_create_device_return(
        self,
        mock_fc_context: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
        mock_get_device_class: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device()."""
        self.assertIsInstance(
            honeydew.create_device(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    serial_number=None,
                    ip_port=None,
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            ),
            FuchsiaDevice,
        )

        mock_get_device_class.assert_called_once()
        mock_fc_context.assert_called_once_with(
            config={
                "log.level": "debug",
                "log.dir": "/tmp/logs",
                "shared_data": _SHARED_DATA,
                "connectivity.enable_usb": "false",
                "connectivity.usb_driver_autostart": "false",
            },
            isolate_dir=_INPUT_ARGS["ffx_config_data"].isolate_dir,
            target=_INPUT_ARGS["target_name"],
        )
        mock_fc_check_connection.assert_called()
        mock_ffx_check_connection.assert_called()

    @mock.patch.object(
        honeydew,
        "_get_device_class",
        return_value=FuchsiaDevice,
        autospec=True,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    @mock.patch.object(
        fuchsia_controller_transport.FuchsiaController,
        "check_connection",
        autospec=True,
    )
    @mock.patch("fuchsia_controller_py.Context", autospec=True)
    def test_create_device_using_device_ip_port(
        self,
        mock_fc_context: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
        mock_get_device_class: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device() where it returns a device
        from an IpPort."""
        self.assertIsInstance(
            honeydew.create_device(
                custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    serial_number=None,
                    ip_port=_INPUT_ARGS["target_ip_port"],
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            ),
            FuchsiaDevice,
        )

        mock_get_device_class.assert_called_once()
        mock_fc_context.assert_called_once_with(
            config={
                "log.level": "debug",
                "log.dir": "/tmp/logs",
                "shared_data": _SHARED_DATA,
                "connectivity.enable_usb": "false",
                "connectivity.usb_driver_autostart": "false",
            },
            isolate_dir=_INPUT_ARGS["ffx_config_data"].isolate_dir,
            target=str(_INPUT_ARGS["target_ip_port"]),
        )
        mock_fc_check_connection.assert_called()
        mock_ffx_check_connection.assert_called()

    @mock.patch.object(
        honeydew,
        "_get_device_class",
        return_value=FuchsiaDevice,
        autospec=True,
    )
    @mock.patch.object(
        FuchsiaDevice,
        "__init__",
        side_effect=fc_errors.FuchsiaControllerConnectionError("Error"),
        autospec=True,
    )
    def test_create_device_using_device_ip_port_throws_error(
        self,
        mock_fc_fuchsia_device: mock.Mock,
        mock_get_device_class: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device() where it raises an error."""
        with self.assertRaises(errors.FuchsiaDeviceError):
            honeydew.create_device(
                custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    serial_number=None,
                    ip_port=_INPUT_ARGS["target_ip_port"],
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            )

        mock_get_device_class.assert_called_once()
        mock_fc_fuchsia_device.assert_called()

    def test_all_device_classes_are_registered(self) -> None:
        """Verifies all concrete device classes in honeydew.device_classes are in _DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT."""
        discovered_classes: set[type[FuchsiaDevice]] = set()
        for _, module_name, ispkg in pkgutil.walk_packages(
            device_classes.__path__, "honeydew.device_classes."
        ):
            if ispkg or "tests" in module_name.split("."):
                continue
            module = importlib.import_module(module_name)
            for _, obj in inspect.getmembers(module, inspect.isclass):
                if (
                    issubclass(obj, FuchsiaDevice)
                    and obj is not FuchsiaDevice
                    and not inspect.isabstract(obj)
                    and obj.__module__.startswith("honeydew.device_classes.")
                ):
                    discovered_classes.add(obj)

        registered_classes: set[type[FuchsiaDevice]] = set(
            honeydew._DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT.values()
        )
        self.assertEqual(
            discovered_classes,
            registered_classes,
            "All concrete device classes in honeydew.device_classes must be "
            "registered in honeydew._DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT.",
        )

    def test_register_device_classes(self) -> None:
        """Test case for honeydew.register_device_classes()."""
        honeydew.register_device_classes(
            product_to_device_class_dict={"sorrel": _CustomAndroidDevice},
        )
        self.assertEqual(
            honeydew._REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT.get("sorrel"),
            _CustomAndroidDevice,
        )

    @parameterized.expand(
        [
            (
                {
                    "label": "default_fuchsia_device_for_minimal",
                    "product_type": "minimal",
                    "registered_dict": None,
                    "expected_class": FuchsiaDevice,
                },
            ),
            (
                {
                    "label": "regex_match_smart_display",
                    "product_type": "smart_display",
                    "registered_dict": None,
                    "expected_class": smart_display.SmartDisplay,
                },
            ),
            (
                {
                    "label": "regex_match_smart_display_m3_eng",
                    "product_type": "smart_display_m3_eng",
                    "registered_dict": None,
                    "expected_class": smart_display.SmartDisplay,
                },
            ),
            (
                {
                    "label": "smart_display_recovery_fallback_to_fuchsia_device",
                    "product_type": "smart_display_recovery",
                    "registered_dict": None,
                    "expected_class": FuchsiaDevice,
                },
            ),
            (
                {
                    "label": "regex_match_iris_eng",
                    "product_type": "iris_eng",
                    "registered_dict": None,
                    "expected_class": android_device.AndroidDevice,
                },
            ),
            (
                {
                    "label": "iris_recovery_fallback_to_fuchsia_device",
                    "product_type": "iris_recovery",
                    "registered_dict": None,
                    "expected_class": FuchsiaDevice,
                },
            ),
            (
                {
                    "label": "regex_match_sorrel_eng",
                    "product_type": "sorrel_eng",
                    "registered_dict": None,
                    "expected_class": android_device.AndroidDevice,
                },
            ),
            (
                {
                    "label": "regex_match_starnix_eng",
                    "product_type": "starnix_eng",
                    "registered_dict": None,
                    "expected_class": android_device.AndroidDevice,
                },
            ),
            (
                {
                    "label": "sorrel_recovery_fallback_to_fuchsia_device",
                    "product_type": "sorrel_recovery",
                    "registered_dict": None,
                    "expected_class": FuchsiaDevice,
                },
            ),
            (
                {
                    "label": "regex_match_workbench_eng",
                    "product_type": "workbench_eng",
                    "registered_dict": None,
                    "expected_class": workbench.Workbench,
                },
            ),
            (
                {
                    "label": "workbench_recovery_fallback_to_fuchsia_device",
                    "product_type": "workbench_recovery",
                    "registered_dict": None,
                    "expected_class": FuchsiaDevice,
                },
            ),
            (
                {
                    "label": "custom_registered_dict_overrides_default_with_different_regex",
                    "product_type": "sorrel_eng",
                    "registered_dict": {"sorrel": _CustomAndroidDevice},
                    "expected_class": _CustomAndroidDevice,
                },
            ),
            (
                {
                    "label": "registered_dict_no_match_falls_back_to_default_dict",
                    "product_type": "smart_display",
                    "registered_dict": {"sorrel": _CustomAndroidDevice},
                    "expected_class": smart_display.SmartDisplay,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(ffx.FFX, "check_connection", autospec=True)
    @mock.patch.object(ffx.FFX, "get_target_product", autospec=True)
    def test_get_device_class(
        self,
        parameterized_dict: dict[str, Any],
        mock_get_target_product: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
    ) -> None:
        """Test case for honeydew._get_device_class()."""
        mock_get_target_product.return_value = parameterized_dict[
            "product_type"
        ]
        if parameterized_dict["registered_dict"]:
            honeydew.register_device_classes(
                product_to_device_class_dict=parameterized_dict[
                    "registered_dict"
                ],
            )

        device_class = honeydew._get_device_class(
            device_info=custom_types.DeviceInfo(
                name=_INPUT_ARGS["target_name"],
                serial_number=None,
                ip_port=_INPUT_ARGS["target_ip_port"],
                serial_socket=None,
            ),
            ffx_config_data=_INPUT_ARGS["ffx_config_data"],
        )
        self.assertEqual(device_class, parameterized_dict["expected_class"])
        mock_get_target_product.assert_called_once()
        mock_ffx_check_connection.assert_called_once()


if __name__ == "__main__":
    unittest.main()
