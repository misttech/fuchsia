# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""SystemPowerStateController affordance implementation using usb-cli."""

import asyncio
import logging
import os
import signal
import subprocess

from honeydew import affordances_capable, errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_common,
)
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)

_REQUIRED_CAPABILITY: str = "fuchsia.usb.policy.Configuration"
_USB_POLICY_MONIKER: str = "/core/usb-policy"

# How long to wait for `usb-cli cable-breaker` to fail before assuming that it
# broke the Type-C cable connection, which leaves its ffx session hanging until
# the connection is closed.
_USB_CLI_ERROR_WINDOW_SEC: float = 5.0

# How long to wait for the output of a killed `usb-cli cable-breaker` session.
_USB_CLI_OUTPUT_TIMEOUT_SEC: float = 10.0


class _FuchsiaCmds:
    """Class to hold Fuchsia commands."""

    DROP_POWER_LEASE: list[str] = [
        "session",
        "drop-power-lease",
        "--allow-missing",
    ]

    @staticmethod
    def display_power_ssh_command(power_on: bool) -> str:
        state = "on" if power_on else "off"
        return f"display-tweak panel --power {state}"

    @staticmethod
    def cable_breaker_command(duration: int) -> list[str]:
        return [
            "component",
            "explore",
            _USB_POLICY_MONIKER,
            "-c",
            f"usb-cli cable-breaker --duration {duration}s",
        ]


class SystemPowerStateControllerUsingUsbCli(
    system_power_state_controller_common.SystemPowerStateControllerCommon
):
    """Async SystemPowerStateController affordance implementation using usb-cli.

    Args:
        device_name: Device name returned by `ffx target list`.
        ffx: FFX transport implementation.
        fuchsia_controller: FuchsiaController transport implementation.
        device_logger: FuchsiaDeviceLogger implementation.
        inspect: InspectCapableDevice implementation.
        reboot_affordance: RebootCapableDevice implementation.

    Raises:
        errors.NotSupportedError: If Fuchsia device does not support usb-cli.
    """

    # Tolerance for resume duration verification (accounts for 1-second hardware
    # RTC alarm granularity and resume latency).
    _RESUME_DURATION_TOLERANCE_MILLISECONDS: float = 2000

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        device_logger: affordances_capable.FuchsiaDeviceLogger,
        inspect: affordances_capable.InspectCapableDevice,
        reboot_affordance: affordances_capable.RebootCapableDevice,
    ) -> None:
        super().__init__(
            device_name=device_name,
            ffx=ffx,
            device_logger=device_logger,
            inspect=inspect,
        )
        self._fuchsia_controller: fc_transport.FuchsiaController = (
            fuchsia_controller
        )
        self._reboot_affordance: affordances_capable.RebootCapableDevice = (
            reboot_affordance
        )

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that the usb-cli SystemPowerStateController is supported on the device.

        Raises:
            errors.NotSupportedError: If `fuchsia.usb.policy.Configuration` capability is not
                available on the device.
        """
        try:
            output = self._ffx.run(
                ["component", "capability", _REQUIRED_CAPABILITY],
                machine=ffx_types.MachineFormat.RAW,
            )
            if _REQUIRED_CAPABILITY not in output:
                raise errors.NotSupportedError(
                    f"Capability '{_REQUIRED_CAPABILITY}' is not supported on {self._device_name}"
                )
        except ffx_errors.FfxCommandError as err:
            raise errors.NotSupportedError(
                f"usb-cli SystemPowerStateController is not supported on {self._device_name}"
            ) from err

    async def suspend_resume(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> None:
        """Perform suspend-resume operation on the device.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.

        Raises:
            SystemPowerStateControllerError: In case of failure.
            errors.NotSupportedError: If any of the suspend_state or resume_mode
                is not yet supported.
            ValueError: If any of the input args are not valid.
        """
        self._validate_suspend_resume_method_args(
            suspend_state=suspend_state,
            resume_mode=resume_mode,
        )

        self._set_display_power(power_on=False)
        self._drop_session_power_lease()
        try:
            await super().suspend_resume(
                suspend_state=suspend_state,
                resume_mode=resume_mode,
            )
        finally:
            self._set_display_power(power_on=True)

    def _set_display_power(self, power_on: bool) -> None:
        """Power on/off the display panel."""
        state = "on" if power_on else "off"
        _LOGGER.info(
            "Setting display panel power to %s on '%s'...",
            state,
            self._device_name,
        )
        try:
            self._ffx.run_ssh_cmd(
                _FuchsiaCmds.display_power_ssh_command(power_on=power_on)
            )
        except errors.HoneydewError as err:
            _LOGGER.warning("Failed to power %s display panel: %s", state, err)

    def _drop_session_power_lease(self) -> None:
        """Drops the session power lease prior to suspend."""
        _LOGGER.info(
            "Dropping session power lease on '%s'...",
            self._device_name,
        )
        try:
            self._ffx.run(
                _FuchsiaCmds.DROP_POWER_LEASE,
                machine=ffx_types.MachineFormat.RAW,
            )
        except errors.HoneydewError as err:
            _LOGGER.warning("Failed to drop session power lease: %s", err)

    async def _perform_suspend_resume(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> None:
        """Disconnects USB via `usb-cli cable-breaker` and waits for offline and online.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: TimerResume configuration with duration in seconds.

        Raises:
            SystemPowerStateControllerError: In case of failure.
        """
        assert isinstance(
            resume_mode, system_power_state_controller_interface.TimerResume
        )
        _LOGGER.info(
            "Putting '%s' into '%s' with '%s' using usb-cli cable-breaker...",
            self._device_name,
            suspend_state,
            resume_mode,
        )

        self._fuchsia_controller.before_usb_disconnect()
        self._ffx.notify_intentional_disconnect()

        proc: subprocess.Popen[str] | None = self._start_usb_cli_cable_breaker(
            duration=resume_mode.duration
        )
        try:
            try:
                await asyncio.to_thread(
                    self._reboot_affordance.wait_for_offline
                )
            finally:
                self._cleanup_usb_cli_process(proc=proc)
        except errors.HoneydewError as err:
            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"'{self._device_name}' failed to go offline after usb-cli cable-breaker"
            ) from err
        finally:
            try:
                await self._reboot_affordance.wait_for_online()
                self._fuchsia_controller.after_usb_reconnect()
            except errors.HoneydewError as err:
                raise system_power_state_controller_interface.SystemPowerStateControllerError(
                    f"'{self._device_name}' failed to come back online after usb-cli cable-breaker"
                ) from err

        _LOGGER.info(
            "'%s' has been resumed from '%s'",
            self._device_name,
            suspend_state,
        )

    def _start_usb_cli_cable_breaker(
        self, duration: int
    ) -> subprocess.Popen[str] | None:
        """Starts `usb-cli cable-breaker` on the device.

        Args:
            duration: Duration in seconds before the device reconnects USB.

        Returns:
            The running subprocess if it is still open after the error window,
            or None if it already exited cleanly.

        Raises:
            SystemPowerStateControllerError: If usb-cli fails.
        """
        cmd: list[str] = _FuchsiaCmds.cable_breaker_command(duration=duration)
        _LOGGER.info(
            "Running `%s` on '%s'...",
            " ".join(cmd),
            self._device_name,
        )
        try:
            proc: subprocess.Popen[str] = self._ffx.popen(
                cmd=cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except (OSError, errors.HoneydewError) as err:
            # `OSError` if the `ffx` process cannot be started, or
            # `HoneydewError` from generating the `ffx` command.
            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"Failed to start usb-cli cable-breaker on {self._device_name}"
            ) from err

        try:
            output, _ = proc.communicate(timeout=_USB_CLI_ERROR_WINDOW_SEC)
        except subprocess.TimeoutExpired:
            # Most likely hanging on the dropped USB connection. Leave it
            # running until wait_for_offline() confirms the device disconnected.
            return proc

        if (
            proc.returncode not in (0, 255)
            and "Scheduling persistent wake alarm" not in output
            and "Breaking Type-C cable connection" not in output
        ):
            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"usb-cli cable-breaker failed on {self._device_name} with "
                f"returncode={proc.returncode}, output='{output}'"
            )

        _LOGGER.info("`usb-cli cable-breaker` output:\n%s", output)
        return None

    def _cleanup_usb_cli_process(
        self, proc: subprocess.Popen[str] | None
    ) -> None:
        """Kills and reaps the ffx session left running by _start_usb_cli_cable_breaker()."""
        if proc is None:
            return
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            output, _ = proc.communicate(timeout=_USB_CLI_OUTPUT_TIMEOUT_SEC)
        except subprocess.TimeoutExpired:
            output = "<timed out waiting for output>"
        _LOGGER.debug("`usb-cli cable-breaker` output:\n%s", output)
