# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for system_power_state_controller_using_usb_cli.py."""

import signal
import subprocess
import unittest
from copy import deepcopy
from typing import Any, cast
from unittest import mock

import fuchsia_inspect
from honeydew import affordances_capable, errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_usb_cli,
)
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)

_DEVICE_NAME: str = "fuchsia-usb-device"
_CAPABILITY_OUTPUT: str = (
    "`core/usb-policy` declared capability `fuchsia.usb.policy.Configuration`"
)

_SAG_INSPECT_DATA_BEFORE: list[dict[str, Any]] = [
    {
        "data_source": "Inspect",
        "metadata": {
            "component_url": "fuchsia-boot:///system-activity-governor#meta/system-activity-governor.cm",
            "timestamp": 372140515750,
        },
        "moniker": "bootstrap/system-activity-governor",
        "payload": {
            "root": {
                "suspend_stats": {
                    "success_count": 0,
                    "fail_count": 0,
                    "last_failed_error": 0,
                    "last_time_in_suspend_ns": -1,
                    "last_time_in_suspend_operations": -1,
                },
            }
        },
        "version": 1,
    }
]

_SAG_SUSPEND_STATS_AFTER: dict[str, int] = {
    "fail_count": 0,
    "last_failed_error": 0,
    "last_time_in_suspend_ns": 3053743875,
    "last_time_in_suspend_operations": 188959,
    "success_count": 1,
}
_SAG_INSPECT_DATA_AFTER: list[dict[str, Any]] = deepcopy(
    _SAG_INSPECT_DATA_BEFORE
)
_SAG_INSPECT_DATA_AFTER[0]["payload"]["root"][
    "suspend_stats"
] = _SAG_SUSPEND_STATS_AFTER

_SAG_INSPECT_DATA_AFTER_LONG_DURATION: list[dict[str, Any]] = deepcopy(
    _SAG_INSPECT_DATA_BEFORE
)
_SAG_INSPECT_DATA_AFTER_LONG_DURATION[0]["payload"]["root"]["suspend_stats"] = {
    "fail_count": 0,
    "last_failed_error": 0,
    "last_time_in_suspend_ns": 10_000_000_000,
    "last_time_in_suspend_operations": 188959,
    "success_count": 1,
}


# pylint: disable=protected-access
class SystemPowerStateControllerUsingUsbCliTests(
    unittest.IsolatedAsyncioTestCase
):
    """Unit tests for SystemPowerStateControllerUsingUsbCli."""

    async def asyncSetUp(self) -> None:
        await super().asyncSetUp()

        self.mock_ffx = mock.MagicMock(spec=ffx_transport.FFX)
        self.mock_ffx.run.return_value = _CAPABILITY_OUTPUT
        self.mock_fc = mock.MagicMock(spec=fc_transport.FuchsiaController)
        self.mock_device_logger = mock.AsyncMock(
            spec=affordances_capable.FuchsiaDeviceLogger
        )
        self.mock_inspect = mock.MagicMock(
            spec=affordances_capable.InspectCapableDevice
        )
        self.mock_reboot_affordance = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice
        )
        self.mock_reboot_affordance.wait_for_online = mock.AsyncMock()

        self.controller = system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli(
            device_name=_DEVICE_NAME,
            ffx=self.mock_ffx,
            fuchsia_controller=self.mock_fc,
            device_logger=self.mock_device_logger,
            inspect=self.mock_inspect,
            reboot_affordance=self.mock_reboot_affordance,
        )

    def test_verify_supported_missing_capability(self) -> None:
        """Test verify_supported raises NotSupportedError when capability is missing."""
        self.mock_ffx.run.return_value = ""
        with self.assertRaises(errors.NotSupportedError):
            system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli(
                device_name=_DEVICE_NAME,
                ffx=self.mock_ffx,
                fuchsia_controller=self.mock_fc,
                device_logger=self.mock_device_logger,
                inspect=self.mock_inspect,
                reboot_affordance=self.mock_reboot_affordance,
            )

    def test_verify_supported_ffx_error(self) -> None:
        """Test verify_supported raises NotSupportedError when ffx.run fails."""
        self.mock_ffx.run.side_effect = ffx_errors.FfxCommandError("FFX error")
        with self.assertRaises(errors.NotSupportedError):
            system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli(
                device_name=_DEVICE_NAME,
                ffx=self.mock_ffx,
                fuchsia_controller=self.mock_fc,
                device_logger=self.mock_device_logger,
                inspect=self.mock_inspect,
                reboot_affordance=self.mock_reboot_affordance,
            )

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "suspend_resume",
        autospec=True,
    )
    async def test_idle_suspend_timer_based_resume(
        self, mock_suspend_resume: mock.AsyncMock
    ) -> None:
        """Test idle_suspend_timer_based_resume delegates to suspend_resume."""
        await self.controller.idle_suspend_timer_based_resume(
            duration=15,
            verify_duration=False,
        )
        mock_suspend_resume.assert_called_once_with(
            self.controller,
            suspend_state=system_power_state_controller_interface.IdleSuspend(),
            resume_mode=system_power_state_controller_interface.TimerResume(
                duration=15,
                verify_duration=False,
            ),
        )

    async def test_suspend_resume_with_not_supported_suspend_mode(self) -> None:
        """Test suspend_resume raises NotSupportedError for invalid suspend state."""
        with self.assertRaises(errors.NotSupportedError):
            await self.controller.suspend_resume(
                suspend_state=cast(Any, "invalid"),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=3
                ),
            )

    async def test_suspend_resume_with_not_supported_resume_mode(self) -> None:
        """Test suspend_resume raises NotSupportedError for ButtonPressResume."""
        with self.assertRaises(errors.NotSupportedError):
            await self.controller.suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.ButtonPressResume(),
            )

    async def test_suspend_resume_with_invalid_duration(self) -> None:
        """Test suspend_resume raises ValueError for non-positive duration."""
        with self.assertRaises(ValueError):
            await self.controller.suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=0
                ),
            )

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_perform_suspend_resume",
        autospec=True,
    )
    async def test_suspend_resume_success(
        self, mock_perform_suspend_resume: mock.AsyncMock
    ) -> None:
        """Test suspend_resume succeeds when SAG inspect success_count increases."""
        self.mock_inspect.get_inspect_data.side_effect = [
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_BEFORE
            ),
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_AFTER
            ),
        ]
        await self.controller.suspend_resume(
            suspend_state=system_power_state_controller_interface.IdleSuspend(),
            resume_mode=system_power_state_controller_interface.TimerResume(
                duration=3
            ),
        )
        mock_perform_suspend_resume.assert_called_once()
        self.mock_ffx.run_ssh_cmd.assert_has_calls(
            [
                mock.call("display-tweak panel --power off"),
                mock.call("display-tweak panel --power on"),
            ]
        )

    def test_pre_post_suspend_helpers_log_warnings_on_error(self) -> None:
        """Test display, battery_manager, and session lease helpers log warnings on FFX errors."""
        self.mock_ffx.run_ssh_cmd.side_effect = ffx_errors.FfxCommandError(
            "ssh error"
        )
        self.mock_ffx.run.side_effect = ffx_errors.FfxCommandError("ffx error")

        self.controller._set_display_power(power_on=False)
        self.controller._set_battery_manager_running(running=False)
        self.controller._drop_session_power_lease()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_perform_suspend_resume",
        autospec=True,
    )
    async def test_suspend_resume_sag_inspect_fail(
        self, mock_perform_suspend_resume: mock.AsyncMock
    ) -> None:
        """Test suspend_resume fails when SAG inspect success_count does not increase."""
        self.mock_inspect.get_inspect_data.side_effect = [
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_BEFORE
            ),
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_BEFORE
            ),
        ]
        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "Based on SAG inspect data",
        ):
            await self.controller.suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=3
                ),
            )
        mock_perform_suspend_resume.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_perform_suspend_resume",
        autospec=True,
    )
    async def test_suspend_resume_duration_exceeded_fail(
        self, mock_perform_suspend_resume: mock.AsyncMock
    ) -> None:
        """Test suspend_resume fails when suspend duration exceeds expected duration."""
        self.mock_inspect.get_inspect_data.side_effect = [
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_BEFORE
            ),
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_AFTER_LONG_DURATION
            ),
        ]
        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "Expected it to not take more than",
        ):
            await self.controller.suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=3,
                    verify_duration=True,
                ),
            )
        mock_perform_suspend_resume.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_perform_suspend_resume",
        autospec=True,
    )
    async def test_suspend_resume_verify_duration_false(
        self, mock_perform_suspend_resume: mock.AsyncMock
    ) -> None:
        """Test suspend_resume skips duration check when verify_duration=False."""
        self.mock_inspect.get_inspect_data.side_effect = [
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_BEFORE
            ),
            fuchsia_inspect.InspectDataCollection.from_list(
                _SAG_INSPECT_DATA_AFTER_LONG_DURATION
            ),
        ]
        await self.controller.suspend_resume(
            suspend_state=system_power_state_controller_interface.IdleSuspend(),
            resume_mode=system_power_state_controller_interface.TimerResume(
                duration=3,
                verify_duration=False,
            ),
        )
        mock_perform_suspend_resume.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_cleanup_usb_cli_process",
        autospec=True,
    )
    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_start_usb_cli_cable_breaker",
        autospec=True,
    )
    async def test_perform_suspend_resume_success(
        self,
        mock_start_cable_breaker: mock.Mock,
        mock_cleanup_proc: mock.Mock,
    ) -> None:
        """Test _perform_suspend_resume orchestrates disconnect, wait_for_offline, and wait_for_online."""
        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_start_cable_breaker.return_value = mock_proc

        await self.controller._perform_suspend_resume(
            suspend_state=system_power_state_controller_interface.IdleSuspend(),
            resume_mode=system_power_state_controller_interface.TimerResume(
                duration=15
            ),
        )

        self.mock_fc.before_usb_disconnect.assert_called_once()
        self.mock_ffx.notify_intentional_disconnect.assert_called_once()
        mock_start_cable_breaker.assert_called_once_with(
            self.controller, duration=15
        )
        self.mock_reboot_affordance.wait_for_offline.assert_called_once()
        mock_cleanup_proc.assert_called_once_with(
            self.controller, proc=mock_proc
        )
        self.mock_reboot_affordance.wait_for_online.assert_awaited_once()
        self.mock_fc.after_usb_reconnect.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_cleanup_usb_cli_process",
        autospec=True,
    )
    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_start_usb_cli_cable_breaker",
        autospec=True,
        return_value=None,
    )
    async def test_perform_suspend_resume_offline_failure(
        self,
        mock_start_cable_breaker: mock.Mock,
        mock_cleanup_proc: mock.Mock,
    ) -> None:
        """Test _perform_suspend_resume wraps wait_for_offline failure and still waits for online."""
        self.mock_reboot_affordance.wait_for_offline.side_effect = (
            errors.FuchsiaDeviceError("Failed to go offline")
        )

        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "failed to go offline",
        ):
            await self.controller._perform_suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=15
                ),
            )

        mock_start_cable_breaker.assert_called_once()
        mock_cleanup_proc.assert_called_once()
        self.mock_reboot_affordance.wait_for_online.assert_awaited_once()
        self.mock_fc.after_usb_reconnect.assert_called_once()

    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_cleanup_usb_cli_process",
        autospec=True,
    )
    @mock.patch.object(
        system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli,
        "_start_usb_cli_cable_breaker",
        autospec=True,
        return_value=None,
    )
    async def test_perform_suspend_resume_online_failure(
        self,
        mock_start_cable_breaker: mock.Mock,
        mock_cleanup_proc: mock.Mock,
    ) -> None:
        """Test _perform_suspend_resume wraps wait_for_online failure."""
        self.mock_reboot_affordance.wait_for_online.side_effect = (
            errors.FuchsiaDeviceError("Failed to come back online")
        )

        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "failed to come back online",
        ):
            await self.controller._perform_suspend_resume(
                suspend_state=system_power_state_controller_interface.IdleSuspend(),
                resume_mode=system_power_state_controller_interface.TimerResume(
                    duration=15
                ),
            )

        mock_start_cable_breaker.assert_called_once()
        mock_cleanup_proc.assert_called_once()

    def test_start_usb_cli_cable_breaker_timeout_returns_proc(self) -> None:
        """Test _start_usb_cli_cable_breaker returns proc when communicate times out."""
        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_proc.communicate.side_effect = subprocess.TimeoutExpired(
            cmd="ffx", timeout=5.0
        )
        self.mock_ffx.popen.return_value = mock_proc

        result = self.controller._start_usb_cli_cable_breaker(duration=15)
        self.assertIs(result, mock_proc)

    def test_start_usb_cli_cable_breaker_clean_exit_returns_none(self) -> None:
        """Test _start_usb_cli_cable_breaker returns None when proc exits cleanly."""
        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_proc.communicate.return_value = (
            "Breaking Type-C cable connection and scheduling reconnect...",
            None,
        )
        mock_proc.returncode = 0
        self.mock_ffx.popen.return_value = mock_proc

        result = self.controller._start_usb_cli_cable_breaker(duration=15)
        self.assertIsNone(result)

    def test_start_usb_cli_cable_breaker_command_failure(self) -> None:
        """Test _start_usb_cli_cable_breaker raises error on early failure."""
        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_proc.communicate.return_value = (
            "Error: Failed to break cable",
            None,
        )
        mock_proc.returncode = 1
        self.mock_ffx.popen.return_value = mock_proc

        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "usb-cli cable-breaker failed",
        ):
            self.controller._start_usb_cli_cable_breaker(duration=15)

    def test_start_usb_cli_cable_breaker_popen_failure(self) -> None:
        """Test _start_usb_cli_cable_breaker raises error when popen fails."""
        self.mock_ffx.popen.side_effect = RuntimeError("popen failed")

        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "Failed to start usb-cli cable-breaker",
        ):
            self.controller._start_usb_cli_cable_breaker(duration=15)

    @mock.patch("os.killpg", autospec=True)
    def test_cleanup_usb_cli_process(self, mock_killpg: mock.Mock) -> None:
        """Test _cleanup_usb_cli_process kills running process group and reaps output."""
        self.controller._cleanup_usb_cli_process(proc=None)
        mock_killpg.assert_not_called()

        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_proc.poll.return_value = None
        mock_proc.pid = 12345
        mock_proc.communicate.return_value = ("output", None)

        self.controller._cleanup_usb_cli_process(proc=mock_proc)
        mock_killpg.assert_called_once_with(12345, signal.SIGKILL)
        mock_proc.communicate.assert_called_once()

    @mock.patch("os.killpg", side_effect=ProcessLookupError, autospec=True)
    def test_cleanup_usb_cli_process_timeout(
        self, mock_killpg: mock.Mock
    ) -> None:
        """Test _cleanup_usb_cli_process handles ProcessLookupError and TimeoutExpired."""
        mock_proc = mock.MagicMock(spec=subprocess.Popen)
        mock_proc.poll.return_value = None
        mock_proc.pid = 12345
        mock_proc.communicate.side_effect = subprocess.TimeoutExpired(
            cmd="ffx", timeout=10.0
        )

        self.controller._cleanup_usb_cli_process(proc=mock_proc)
        mock_killpg.assert_called_once_with(12345, signal.SIGKILL)
        mock_proc.communicate.assert_called_once()

    async def test_get_suspend_stats_from_sag_inspect_data_fail(self) -> None:
        """Test _get_suspend_stats_from_sag_inspect_data raises error on InspectError."""
        self.mock_inspect.get_inspect_data.side_effect = errors.InspectError(
            "Inspect operation failed"
        )
        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "Failed to read SAG inspect data",
        ):
            await self.controller._get_suspend_stats_from_sag_inspect_data()

    async def test_get_suspend_stats_from_sag_inspect_data_empty(self) -> None:
        """Test _get_suspend_stats_from_sag_inspect_data raises error on empty data."""
        self.mock_inspect.get_inspect_data.return_value = (
            fuchsia_inspect.InspectDataCollection.from_list([])
        )
        with self.assertRaisesRegex(
            system_power_state_controller_interface.SystemPowerStateControllerError,
            "Failed to read SAG inspect data",
        ):
            await self.controller._get_suspend_stats_from_sag_inspect_data()


if __name__ == "__main__":
    unittest.main()
