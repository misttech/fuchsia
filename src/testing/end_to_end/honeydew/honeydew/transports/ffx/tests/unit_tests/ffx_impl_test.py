# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ffx_impl.py."""

import ipaddress
import json
import re
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller
from parameterized import param, parameterized

from honeydew import affordances_capable, errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx_impl
from honeydew.transports.ffx.types import (
    MachineFormat,
    MonitorTargetInfo,
    TargetInfoData,
)
from honeydew.typing import custom_types
from honeydew.utils import host_shell

# pylint: disable=protected-access
_TARGET_NAME: str = "fuchsia-emulator"

_IPV6: str = "fe80::4fce:3102:ef13:888c%qemu"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)

_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_SSH_ADDRESS = custom_types.TargetSshAddress(
    ip=_SSH_ADDRESS, port=_SSH_PORT
)

_ISOLATE_DIR: str = "/tmp/isolate"
_LOGS_DIR: str = "/tmp/logs"
_BINARY_PATH: str = "ffx"
_LOGS_LEVEL: str = "debug"
_MDNS_ENABLED: bool = False
_SUBTOOLS_SEARCH_PATH: str = "/subtools"
_PROXY_TIMEOUT_SECS: int = 30
_SSH_KEEPALIVE_TIMEOUT: int = 60

_FFX_TARGET_SHOW_JSON: dict[str, Any] = {
    "target": {
        "name": _TARGET_NAME,
        "ssh_address": {"host": f"{_SSH_ADDRESS}", "port": _SSH_PORT},
        "compatibility_state": "supported",
        "compatibility_message": "",
        "last_reboot_graceful": "false",
        "last_reboot_reason": None,
        "uptime_nanos": -1,
    },
    "board": {
        "name": "default-board",
        "revision": None,
        "instruction_set": "x64",
    },
    "device": {
        "serial_number": "1234321",
        "retail_sku": None,
        "retail_demo": None,
        "device_id": None,
    },
    "product": {
        "audio_amplifier": None,
        "build_date": None,
        "build_name": None,
        "colorway": None,
        "display": None,
        "emmc_storage": None,
        "language": None,
        "regulatory_domain": None,
        "locale_list": None,
        "manufacturer": None,
        "microphone": None,
        "model": None,
        "name": None,
        "nand_storage": None,
        "memory": None,
        "sku": None,
    },
    "update": {"current_channel": None, "next_channel": None},
    "build": {
        "version": "2023-02-01T17:26:40+00:00",
        "product": "workstation_eng",
        "board": "x64",
        "commit": "2023-02-01T17:26:40+00:00",
    },
}

_FFX_TARGET_SHOW_OUTPUT: str = json.dumps(_FFX_TARGET_SHOW_JSON)
_FFX_TARGET_SHOW_INFO = TargetInfoData(**_FFX_TARGET_SHOW_JSON)

_FFX_TARGET_LIST_OUTPUT: str = (
    '[{"nodename":"fuchsia-emulator","rcs_state":"Y","serial":"<unknown>",'
    '"target_type":"workstation_eng.x64","target_state":"Product",'
    '"addresses":["fe80::6a47:a931:1e84:5077%qemu"],"is_default":true}]\n'
)

_FFX_TARGET_INFO: dict[str, Any] = {
    "nodename": _TARGET_NAME,
    "rcs_state": "Y",
    "serial": "<unknown>",
    "target_type": "workstation_eng.x64",
    "target_state": "Product",
    "addresses": ["fe80::6a47:a931:1e84:5077%qemu"],
    "is_default": True,
}

_FFX_TARGET_LIST_JSON: list[dict[str, Any]] = [_FFX_TARGET_INFO]

_FFX_TARGET_STATUS_OUTPUT: str = (
    r'\[✓\] Device resolved to node: "fuchsia-emulator".*'
    r".*"
    r"\[✓\] Connected"
    r".*"
    r"\[✓\] All checks passed\."
)

_FFX_TARGET_STATUS_FULL_OUTPUT: str = (
    '[✓] Device resolved to node: "fuchsia-emulator" in product '
    "state (addrs: [fe80::6bab:2908:a0c9:7e6d%brqemu])\n"
    "[✓] Connected\n"
    "[✓] Success\n"
    "[✓] All checks passed.\n"
)

_FFX_TARGET_WAIT_MACHINE_RAW: str = ""

_INPUT_ARGS: dict[str, Any] = {
    "target_name": _TARGET_NAME,
    "target_ip_port": _TARGET_SSH_ADDRESS,
    "ffx_config_data": ffx_config.FfxConfigData(
        isolate_dir=fuchsia_controller.IsolateDir(_ISOLATE_DIR),
        logs_dir=_LOGS_DIR,
        binary_path=_BINARY_PATH,
        logs_level=_LOGS_LEVEL,
        mdns_enabled=_MDNS_ENABLED,
        subtools_search_path=_SUBTOOLS_SEARCH_PATH,
        proxy_timeout_secs=_PROXY_TIMEOUT_SECS,
        ssh_keepalive_timeout=_SSH_KEEPALIVE_TIMEOUT,
    ),
    "run_cmd": ffx_impl._FFX_CMDS["TARGET_SHOW"],
    "run_machine_cmd": ffx_impl._FFX_CMDS["TARGET_WAIT"],
}

_MOCK_ADDRESS = json.dumps(
    [
        {
            "nodename": "fuchsia-emulator",
            "rcs_state": "Y",
            "serial": "<unknown>",
            "target_type": "core.x64",
            "target_state": "Product",
            "addresses": [
                {"type": "Ip", "ip": str(_SSH_ADDRESS), "ssh_port": _SSH_PORT}
            ],
            "is_default": False,
            "is_manual": False,
        }
    ]
)

_MOCK_ARGS: dict[str, Any] = {
    "ffx_target_show_output": _FFX_TARGET_SHOW_OUTPUT,
    "ffx_target_show_json": _FFX_TARGET_SHOW_JSON,
    "ffx_target_show_object": _FFX_TARGET_SHOW_INFO,
    "ffx_target_ssh_address_output": _MOCK_ADDRESS,
    "ffx_target_list_output": _FFX_TARGET_LIST_OUTPUT,
    "ffx_target_list_json": _FFX_TARGET_LIST_JSON,
    "ffx_target_wait_machine": _FFX_TARGET_WAIT_MACHINE_RAW,
    "ffx_target_status_output": _FFX_TARGET_STATUS_FULL_OUTPUT,
}

_EXPECTED_VALUES: dict[str, Any] = {
    "ffx_target_show_output": _FFX_TARGET_SHOW_OUTPUT,
    "ffx_target_show_object": _FFX_TARGET_SHOW_INFO,
    "ffx_target_show_json": _FFX_TARGET_SHOW_JSON,
    "ffx_target_list_json": _FFX_TARGET_LIST_JSON,
    "ffx_target_wait_machine": _FFX_TARGET_WAIT_MACHINE_RAW,
    "ffx_target_status_output": _FFX_TARGET_STATUS_OUTPUT,
}


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom name function method."""
    test_func_name: str = testcase_func.__name__

    params_dict: dict[str, Any] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(params_dict["label"])

    return f"{test_func_name}_with_{test_label}"


class FfxImplTests(unittest.TestCase):
    """Unit tests for ffx_impl.FfxImpl"""

    def setUp(self) -> None:
        super().setUp()

        with (
            mock.patch.object(
                ffx_impl.FfxImpl,
                "check_connection",
                autospec=True,
            ) as mock_ffx_check_connection,
        ):
            self.ffx_obj_wo_ip = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                config_data=_INPUT_ARGS["ffx_config_data"],
            )
        mock_ffx_check_connection.assert_called()

        mock_ffx_check_connection.reset_mock()

        self.device_ip_change = mock.MagicMock(
            spec=affordances_capable.FuchsiaDeviceIpChange
        )
        with (
            mock.patch.object(
                ffx_impl.FfxImpl,
                "check_connection",
                autospec=True,
            ) as mock_ffx_check_connection,
        ):
            self.ffx_obj_with_ip = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                target_ip_port=_INPUT_ARGS["target_ip_port"],
                config_data=_INPUT_ARGS["ffx_config_data"],
                device_ip_change=self.device_ip_change,
            )
        mock_ffx_check_connection.assert_called()

        mock_ffx_check_connection.reset_mock()

        with (
            mock.patch.object(
                ffx_impl.FfxImpl,
                "check_connection",
                autospec=True,
            ) as mock_ffx_check_connection,
            mock.patch.object(
                ffx_impl.FfxImpl,
                "_check_running_monitor",
                return_value=True,
                autospec=True,
            ) as mock_ffx_check_running_monitor,
        ):
            self.ffx_obj_with_ip_and_monitor = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                target_ip_port=_INPUT_ARGS["target_ip_port"],
                config_data=_INPUT_ARGS["ffx_config_data"],
                use_monitor_state=True,
                device_ip_change=self.device_ip_change,
            )
        mock_ffx_check_connection.assert_called()
        mock_ffx_check_running_monitor.assert_called()

    def test_ffx_init_with_ip_as_target_name(self) -> None:
        """Test case for ffx_impl.FfxImpl() when called with target_name=<ip>."""
        with self.assertRaises(ValueError):
            ffx_impl.FfxImpl(
                target_name=_IPV6,
                config_data=_INPUT_ARGS["ffx_config_data"],
            )

    def test_ffx_init_shared_data_default(self) -> None:
        """Verify shared_data defaults to logs_dir in __init__."""
        self.assertEqual(self.ffx_obj_wo_ip._shared_data, _LOGS_DIR)

    def test_ffx_init_shared_data_custom(self) -> None:
        """Verify shared_data is set to custom value in __init__."""
        shared_data = "/tmp/custom_shared_data"
        with mock.patch.object(
            ffx_impl.FfxImpl,
            "check_connection",
            autospec=True,
        ):
            ffx_obj = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                config_data=_INPUT_ARGS["ffx_config_data"],
                shared_data=shared_data,
            )
        self.assertEqual(ffx_obj._shared_data, shared_data)

    @mock.patch.object(
        ffx_impl.FfxImpl, "wait_for_rcs_connection", autospec=True
    )
    def test_check_connection(
        self, mock_wait_for_rcs_connection: mock.Mock
    ) -> None:
        """Test case for check_connection()"""
        self.ffx_obj_with_ip.check_connection()

        mock_wait_for_rcs_connection.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "wait_for_rcs_connection",
        side_effect=errors.DeviceNotConnectedError(
            ffx_impl._DEVICE_NOT_CONNECTED
        ),
        autospec=True,
    )
    def test_check_connection_raises(
        self, mock_wait_for_rcs_connection: mock.Mock
    ) -> None:
        """Test case for check_connection() raising ffx_errors.FfxConnectionError"""
        with self.assertRaises(ffx_errors.FfxConnectionError):
            self.ffx_obj_with_ip.check_connection()

        mock_wait_for_rcs_connection.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_show_output"],
        autospec=True,
    )
    def test_get_target_information(self, mock_ffx_run: mock.Mock) -> None:
        """Verify get_target_information()."""
        self.assertEqual(
            self.ffx_obj_with_ip.get_target_information(),
            _EXPECTED_VALUES["ffx_target_show_object"],
        )

        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_list_output"],
        autospec=True,
    )
    def test_get_target_info_from_target_list(
        self, mock_ffx_run: mock.Mock
    ) -> None:
        """Test case for get_target_info_from_target_list()."""
        mock_ffx_run.return_value = _MOCK_ARGS["ffx_target_list_output"]

        self.assertEqual(
            self.ffx_obj_with_ip.get_target_info_from_target_list(),
            _EXPECTED_VALUES["ffx_target_list_json"][0],
        )

        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value="[]",
        autospec=True,
    )
    def test_get_target_info_from_target_list_exception(
        self,
        mock_ffx_run: mock.Mock,
    ) -> None:
        """Test case for get_target_info_from_target_list() raising exception."""
        with self.assertRaises(ffx_errors.FfxCommandError):
            self.ffx_obj_with_ip.get_target_info_from_target_list()
        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_ssh_address_output"],
        autospec=True,
    )
    def test_get_target_ssh_address(self, mock_ffx_run: mock.Mock) -> None:
        """Verify get_target_ssh_address returns SSH information of the fuchsia
        device."""
        self.assertEqual(
            self.ffx_obj_with_ip.get_target_ssh_address(), _TARGET_SSH_ADDRESS
        )
        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_board(
        self, mock_get_target_information: mock.Mock
    ) -> None:
        """Verify ffx_impl.get_target_board returns board value of fuchsia device."""
        result: str = self.ffx_obj_with_ip.get_target_board()
        expected: str | None = _FFX_TARGET_SHOW_INFO.build.board

        self.assertEqual(result, expected)

        mock_get_target_information.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_product(
        self, mock_get_target_information: mock.Mock
    ) -> None:
        """Verify ffx_impl.get_target_product returns product value of fuchsia
        device."""
        result: str = self.ffx_obj_with_ip.get_target_product()
        expected: str | None = _FFX_TARGET_SHOW_INFO.build.product

        self.assertEqual(result, expected)

        mock_get_target_information.assert_called()

    @mock.patch.object(
        host_shell,
        "run",
        return_value=_MOCK_ARGS["ffx_target_show_output"],
        autospec=True,
    )
    def test_ffx_run(self, mock_host_shell_run: mock.Mock) -> None:
        """Test case for ffx_impl.run()"""
        self.assertEqual(
            self.ffx_obj_with_ip.run(cmd=_INPUT_ARGS["run_cmd"]),
            _EXPECTED_VALUES["ffx_target_show_output"],
        )

        mock_host_shell_run.assert_called_with(
            [
                _BINARY_PATH,
                "-t",
                str(_TARGET_SSH_ADDRESS),
                "--isolate-dir",
                _ISOLATE_DIR,
                "--machine",
                "json",
                "-o",
                str(Path(_LOGS_DIR) / "ffx.log"),
                "--direct",
                "-c",
                f"log.dir={_LOGS_DIR}",
                "-c",
                f"log.level={_LOGS_LEVEL}",
                "-c",
                f"discovery.mdns.enabled={str(_MDNS_ENABLED).lower()}",
                "-c",
                f"ffx.subtool-search-paths={_SUBTOOLS_SEARCH_PATH}",
                "-c",
                f"proxy.timeout_secs={_PROXY_TIMEOUT_SECS}",
                "-c",
                f"ssh.keepalive_timeout={_SSH_KEEPALIVE_TIMEOUT}",
                "-c",
                f"shared_data={_LOGS_DIR}",
            ]
            + ffx_impl._FFX_CMDS["TARGET_SHOW"],
            capture_output=True,
            log_output=True,
            timeout=None,
        )

    @mock.patch.object(
        host_shell,
        "run",
        return_value=_MOCK_ARGS["ffx_target_wait_machine"],
        autospec=True,
    )
    def test_ffx_run_machine(self, mock_host_shell_run: mock.Mock) -> None:
        """Test case for ffx_impl.run()"""
        self.assertEqual(
            self.ffx_obj_with_ip.run(
                cmd=_INPUT_ARGS["run_machine_cmd"], machine=MachineFormat.RAW
            ),
            _EXPECTED_VALUES["ffx_target_wait_machine"],
        )

        mock_host_shell_run.assert_called_with(
            [
                _BINARY_PATH,
                "-t",
                str(_TARGET_SSH_ADDRESS),
                "--isolate-dir",
                _ISOLATE_DIR,
                "--machine",
                "raw",
                "-o",
                str(Path(_LOGS_DIR) / "ffx.log"),
                "--direct",
                "-c",
                f"log.dir={_LOGS_DIR}",
                "-c",
                f"log.level={_LOGS_LEVEL}",
                "-c",
                f"discovery.mdns.enabled={str(_MDNS_ENABLED).lower()}",
                "-c",
                f"ffx.subtool-search-paths={_SUBTOOLS_SEARCH_PATH}",
                "-c",
                f"proxy.timeout_secs={_PROXY_TIMEOUT_SECS}",
                "-c",
                f"ssh.keepalive_timeout={_SSH_KEEPALIVE_TIMEOUT}",
                "-c",
                f"shared_data={_LOGS_DIR}",
            ]
            + ffx_impl._FFX_CMDS["TARGET_WAIT"],
            capture_output=True,
            log_output=True,
            timeout=None,
        )

    @parameterized.expand(
        [
            (
                {
                    "label": "DeviceNotConnectedError",
                    "side_effect": errors.HostCmdError(
                        ffx_impl._DEVICE_NOT_CONNECTED,
                    ),
                    "expected_error": errors.DeviceNotConnectedError,
                },
            ),
            (
                {
                    "label": "FfxCommandError",
                    "side_effect": errors.HostCmdError(
                        "command output and error",
                    ),
                    "expected_error": ffx_errors.FfxCommandError,
                },
            ),
            (
                {
                    "label": "TimeoutExpired",
                    "side_effect": errors.HoneydewTimeoutError(
                        "timed out",
                    ),
                    "expected_error": ffx_errors.FfxTimeoutError,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(
        host_shell,
        "run",
        autospec=True,
    )
    def test_ffx_run_exceptions(
        self,
        parameterized_dict: dict[str, Any],
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test case for ffx_impl.run() raising different
        exceptions."""
        mock_host_shell_run.side_effect = parameterized_dict["side_effect"]

        with self.assertRaises(parameterized_dict["expected_error"]):
            self.ffx_obj_with_ip.run(cmd=_INPUT_ARGS["run_cmd"])

        mock_host_shell_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        autospec=True,
    )
    def test_ffx_run_test_component(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.run_test_component()"""
        self.ffx_obj_with_ip.run_test_component(
            "fuchsia-pkg://fuchsia.com/testing#meta/test.cm",
            ffx_test_args=["--foo", "bar"],
            test_component_args=["baz", "--x", "2"],
            capture_output=False,
        )

        mock_ffx_run.assert_called_with(
            self.ffx_obj_with_ip,
            [
                "test",
                "run",
                "fuchsia-pkg://fuchsia.com/testing#meta/test.cm",
                "--foo",
                "bar",
                "--",
                "baz",
                "--x",
                "2",
            ],
            capture_output=False,
        )

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        autospec=True,
    )
    def test_ffx_run_ssh_cmd(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.run_ssh_cmd()"""
        self.ffx_obj_with_ip.run_ssh_cmd(
            cmd="killall iperf3",
            capture_output=True,
        )

        mock_ffx_run.assert_called_with(
            self.ffx_obj_with_ip,
            [
                "target",
                "ssh",
                "killall iperf3",
            ],
            capture_output=True,
            machine=MachineFormat.RAW,
        )

    @mock.patch.object(
        host_shell,
        "popen",
        return_value=None,
        autospec=True,
    )
    def test_ffx_popen(self, mock_host_shell_popen: mock.Mock) -> None:
        """Test case for ffx_impl.popen()"""
        self.ffx_obj_with_ip.popen(
            cmd=["a", "b", "c"],
            # Popen forwards arbitrary kvargs to subprocess.Popen
            stdout="abc",
        )

        mock_host_shell_popen.assert_called_with(
            [
                _BINARY_PATH,
                "-t",
                str(_TARGET_SSH_ADDRESS),
                "--isolate-dir",
                _ISOLATE_DIR,
                "--machine",
                "raw",
                "-o",
                str(Path(_LOGS_DIR) / "ffx.log"),
                "--direct",
                "-c",
                f"log.dir={_LOGS_DIR}",
                "-c",
                f"log.level={_LOGS_LEVEL}",
                "-c",
                f"discovery.mdns.enabled={str(_MDNS_ENABLED).lower()}",
                "-c",
                f"ffx.subtool-search-paths={_SUBTOOLS_SEARCH_PATH}",
                "-c",
                f"proxy.timeout_secs={_PROXY_TIMEOUT_SECS}",
                "-c",
                f"ssh.keepalive_timeout={_SSH_KEEPALIVE_TIMEOUT}",
                "-c",
                f"shared_data={_LOGS_DIR}",
            ]
            + ["a", "b", "c"],
            stdout="abc",
        )

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_name(
        self, mock_ffx_get_target_information: mock.Mock
    ) -> None:
        """Verify get_target_name returns the name of the fuchsia device."""
        self.assertEqual(self.ffx_obj_with_ip.get_target_name(), _TARGET_NAME)

        mock_ffx_get_target_information.assert_called()

    @mock.patch.object(ffx_impl.FfxImpl, "run", return_value="", autospec=True)
    def test_wait_for_rcs_connection(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.wait_for_rcs_connection()"""
        self.ffx_obj_with_ip.wait_for_rcs_connection()
        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "_get_target_status",
        return_value=MonitorTargetInfo(**_FFX_TARGET_INFO),
        autospec=True,
    )
    def test_wait_for_rcs_connection_use_monitor(
        self, get_target_status: mock.Mock
    ) -> None:
        """Test case for ffx_impl.wait_for_rcs_connection()"""
        self.ffx_obj_with_ip_and_monitor.wait_for_rcs_connection()
        get_target_status.assert_called()

    @mock.patch.object(
        host_shell, "run", return_value='"/tmp/pid"', autospec=True
    )
    def test_check_running_monitor_includes_shared_data(
        self, mock_host_run: mock.Mock
    ) -> None:
        """Verify _check_running_monitor includes shared_data in its command."""
        self.ffx_obj_wo_ip._check_running_monitor()
        mock_host_run.assert_called()
        cmd = mock_host_run.call_args[1]["cmd"]
        self.assertIn("-c", cmd)
        self.assertIn(f"shared_data={_LOGS_DIR}", cmd)

    @mock.patch.object(
        host_shell, "run", return_value='{"targets": []}', autospec=True
    )
    def test_get_target_status_includes_shared_data(
        self, mock_host_run: mock.Mock
    ) -> None:
        """Verify _get_target_status includes shared_data in its command."""
        self.ffx_obj_with_ip_and_monitor._get_target_status()
        mock_host_run.assert_called()
        cmd = mock_host_run.call_args[1]["cmd"]
        self.assertIn("-c", cmd)
        self.assertIn(f"shared_data={_LOGS_DIR}", cmd)

    @mock.patch.object(ffx_impl.FfxImpl, "run", return_value="", autospec=True)
    def test_wait_for_rcs_disconnection(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.wait_for_rcs_disconnection()"""
        self.ffx_obj_with_ip.wait_for_rcs_disconnection()
        self.assertEqual(mock_ffx_run.call_count, 1)

    @mock.patch.object(
        host_shell,
        "run",
        return_value=_MOCK_ARGS["ffx_target_status_output"],
        autospec=True,
    )
    def test_get_ffx_target_status_success(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test case for get_ffx_target_status() on success."""
        result = self.ffx_obj_with_ip.get_ffx_target_status()
        pattern = _EXPECTED_VALUES["ffx_target_status_output"]
        match = re.search(pattern, result, re.DOTALL)
        self.assertIsNotNone(
            match, msg=f"Pattern failed to match in output: {result}"
        )

        # Verify host_shell.run was called with correct arguments
        mock_host_shell_run.assert_called_with(
            cmd=[
                "ffx",
                "-t",
                "[fe80::4fce:3102:ef13:888c%qemu]:8022",
                "--isolate-dir",
                "/tmp/isolate",
                "--machine",
                "json",
                "-o",
                "/tmp/logs/ffx.log",
                "--direct",
                "-c",
                "log.dir=/tmp/logs",
                "-c",
                "log.level=debug",
                "-c",
                "discovery.mdns.enabled=false",
                "-c",
                "ffx.subtool-search-paths=/subtools",
                "-c",
                "proxy.timeout_secs=30",
                "-c",
                "ssh.keepalive_timeout=60",
                "-c",
                "shared_data=/tmp/logs",
                "target",
                "status",
            ],
            capture_output=True,
            log_output=False,
            timeout=None,
        )

    @mock.patch.object(
        host_shell,
        "run",
        autospec=True,
    )
    def test_get_ffx_target_status_raises_ffxtargetstatuserror(
        self, mock_host_shell_run: mock.Mock
    ) -> None:
        """Test case for get_ffx_target_status() raising FfxTargetStatusError."""
        mock_host_shell_run.side_effect = errors.HostCmdError(
            "ffx target status failed output", 1
        )

        with self.assertRaises(ffx_errors.FfxTargetStatusError) as cm:
            self.ffx_obj_with_ip.get_ffx_target_status()

        self.assertIsInstance(cm.exception.__cause__, errors.HostCmdError)
        self.assertIn("ffx target status failed output", str(cm.exception))

        mock_host_shell_run.assert_called_once()

    @mock.patch.object(ffx_impl.FfxImpl, "get_ffx_target_status")
    @mock.patch("honeydew.utils.host_shell.run")
    def test_run_with_log_status_disabled(
        self, mock_host_shell: mock.Mock, mock_triage: mock.Mock
    ) -> None:
        """Verify get_ffx_target_status is NOT called when disabled."""
        mock_host_shell.side_effect = errors.HostCmdError("Command failed")

        with self.assertRaises(ffx_errors.FfxCommandError):
            self.ffx_obj_wo_ip.run(
                cmd=["test", "cmd"], log_status_on_failure=False
            )

        mock_triage.assert_not_called()

    @mock.patch.object(ffx_impl.FfxImpl, "get_ffx_target_status")
    @mock.patch("honeydew.utils.host_shell.run")
    def test_run_with_log_status_enabled_default(
        self, mock_host_shell: mock.Mock, mock_triage: mock.Mock
    ) -> None:
        """Verify get_ffx_target_status IS called by default on failure."""
        mock_host_shell.side_effect = errors.HostCmdError("Command failed")

        with self.assertRaises(ffx_errors.FfxCommandError):
            self.ffx_obj_wo_ip.run(cmd=["test", "cmd"])

        mock_triage.assert_called_once()
