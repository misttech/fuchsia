# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import math
from datetime import timedelta

import fuchsia_base_test
from honeydew import errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.utils import control_flows, power
from honeydew.utils.deadline import Deadline
from mobly.asserts import assert_equal, assert_less

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Timeout for automated suspend/resume runs (e.g. with DMC in infra or usb-cli).
_SUSPEND_RESUME_TIMEOUT: timedelta = timedelta(minutes=1)

# Initial duration for on-device USB disconnect via `usb-cli cable-breaker`.
# Doubled on every retry up to `power.SUSPEND_RESUME_MAX_IDLE_DURATION`. Must
# leave enough time for the host to notice that the device went offline before
# the wake alarm reconnects USB.
_USB_CLI_BASE_IDLE_DURATION: timedelta = timedelta(seconds=15)

# More forgiving timeouts for manual at-desk testing where a user physically
# unplugs and replugs the USB cable.
_MANUAL_SUSPEND_RESUME_TIMEOUT: timedelta = timedelta(minutes=5)
_MANUAL_BASE_IDLE_DURATION: timedelta = timedelta(seconds=10)


class _ManualUsbPowerHub(usb_power_hub.UsbPowerHub):
    """Prompts the user to manually unplug and replug the USB cable.

    Used during local at-desk testing when neither an infra-provided USB
    disconnector (such as DMC, a programmable USB power hub, or a virtual USB
    hub) nor on-device `usb-cli` disconnect support is available.
    """

    def __init__(self, device_name: str) -> None:
        super().__init__()
        self._device_name = device_name

    def power_off(self, port: int | None = None) -> None:
        """Logs instructions for the user to unplug the USB cable."""
        del port
        _LOGGER.info(
            "[ACTION REQUIRED] Please UNPLUG the USB cable from %s now so the "
            "device can suspend (waiting for device to go offline)...",
            self._device_name,
        )

    def power_on(self, port: int | None = None) -> None:
        """Logs instructions for the user to replug the USB cable."""
        del port
        _LOGGER.info(
            "[ACTION REQUIRED] Please PLUG the USB cable back into %s now to "
            "resume the device (waiting for device to come back online)...",
            self._device_name,
        )


class SuspendResumeTestCases(fuchsia_base_test.FuchsiaTestCases):
    """Test cases for suspend and resume."""

    _suspend_resume_timeout: timedelta = _SUSPEND_RESUME_TIMEOUT
    _usb_cli_base_idle_duration: timedelta = _USB_CLI_BASE_IDLE_DURATION
    _usb_power_hub_base_idle_duration: timedelta = (
        power.SUSPEND_RESUME_BASE_IDLE_DURATION
    )
    _max_idle_duration: timedelta = power.SUSPEND_RESUME_MAX_IDLE_DURATION
    _manual_suspend_resume_timeout: timedelta = _MANUAL_SUSPEND_RESUME_TIMEOUT
    _manual_base_idle_duration: timedelta = _MANUAL_BASE_IDLE_DURATION
    _min_suspend_duration: timedelta = timedelta(seconds=0)

    def _set_display_power(self, device: FuchsiaDevice, power_on: bool) -> None:
        """Power on/off the display panel.

        Args:
            device: Fuchsia device object.
            power_on: True to power on the display, False to power off.
        """
        state = "on" if power_on else "off"
        _LOGGER.info(
            "Setting display panel power to %s on %s...",
            state,
            device.device_name,
        )
        try:
            device.ffx.run_ssh_cmd(f"display-tweak panel --power {state}")
        except Exception as e:  # pylint: disable=broad-except
            _LOGGER.warning("Failed to power %s display panel: %s", state, e)

    async def _suspend_resume_with_controller(
        self,
        controller: system_power_state_controller_interface.SystemPowerStateController,
        deadline: Deadline,
    ) -> None:
        """Suspends and resumes the device using SystemPowerStateController with retries."""
        before_sag_stats = (
            await power.get_sag_suspend_stats(self.dut)
            if self._min_suspend_duration > timedelta(seconds=0)
            else None
        )
        attempt = -1
        while True:
            attempt += 1
            if deadline.is_due():
                raise power.DeviceDidNotSuspendError(
                    "SAG did not suspend during idle."
                )

            idle_duration = min(
                self._usb_cli_base_idle_duration * (2**attempt),
                max(
                    self._usb_cli_base_idle_duration,
                    self._max_idle_duration,
                ),
            )
            duration_sec = math.ceil(idle_duration.total_seconds())
            _LOGGER.info(
                "Suspension attempt %s using system_power_state_controller "
                "(duration=%ss)...",
                attempt + 1,
                duration_sec,
            )
            try:
                await controller.idle_suspend_timer_based_resume(
                    duration=duration_sec,
                    verify_duration=False,
                )
            except (
                system_power_state_controller_interface.SystemPowerStateControllerError
            ) as err:
                if "Based on SAG inspect data" in str(err):
                    _LOGGER.warning(
                        "SAG did not suspend during idle (%s). Retrying...",
                        err,
                    )
                    continue
                raise

            if before_sag_stats is None:
                return

            current_sag_delta = (
                await power.get_sag_suspend_stats(self.dut) - before_sag_stats
            )
            if (
                current_sag_delta.total_time_in_suspend
                >= self._min_suspend_duration
            ):
                return
            _LOGGER.info(
                "Cumulative suspend time is %.2fs (< %.1fs); running "
                "another suspend cycle...",
                current_sag_delta.total_time_in_suspend.total_seconds(),
                self._min_suspend_duration.total_seconds(),
            )

    async def test_suspend_resume(self) -> None:
        """Must run on workbench products."""
        # TODO(https://fxbug.dev/573221590): Move the external USB power hub and
        # manual USB suspend/resume flows into SystemPowerStateController
        # implementations so this test can uniformly call
        # `self.dut.system_power_state_controller`.
        if self.dut.usb_power_hub is None:
            try:
                controller = self.dut.system_power_state_controller
            except errors.NotSupportedError:
                _LOGGER.info(
                    "No USB disconnector (e.g. DMC) or usb-cli support is "
                    "available on %s; falling back to manual USB cable "
                    "unplug/replug.",
                    self.dut.device_name,
                )
                self.dut.set_usb_power_hub(
                    _ManualUsbPowerHub(self.dut.device_name)
                )
            else:
                _LOGGER.info(
                    "No external USB power hub (e.g. DMC) is configured for %s; "
                    "using system_power_state_controller (usb-cli) to "
                    "disconnect and reconnect USB on-device.",
                    self.dut.device_name,
                )
                await self._suspend_resume_with_controller(
                    controller=controller,
                    deadline=Deadline.from_timeout(
                        self._suspend_resume_timeout
                    ),
                )
                return

        is_manual_usb = isinstance(self.dut.usb_power_hub, _ManualUsbPowerHub)
        timeout = (
            self._manual_suspend_resume_timeout
            if is_manual_usb
            else self._suspend_resume_timeout
        )
        base_idle_duration = (
            self._manual_base_idle_duration
            if is_manual_usb
            else self._usb_power_hub_base_idle_duration
        )

        # TODO(https://fxbug.dev/519249679): Find a better way to
        # separate out product-specific test logic.
        #
        # On workbench, the display panel must be manually powered off
        # before suspend. Otherwise, the device will not suspend.
        deadline = Deadline.from_timeout(timeout)
        before_sag_stats = (
            await power.get_sag_suspend_stats(self.dut)
            if self._min_suspend_duration > timedelta(seconds=0)
            else None
        )
        self._set_display_power(self.dut, power_on=False)
        try:
            while True:
                await power.suspend_resume(
                    self.dut,
                    deadline=deadline,
                    base_idle_duration=base_idle_duration,
                    max_idle_duration=self._max_idle_duration,
                )
                if before_sag_stats is None:
                    break
                current_sag_delta = (
                    await power.get_sag_suspend_stats(self.dut)
                    - before_sag_stats
                )
                if (
                    current_sag_delta.total_time_in_suspend
                    >= self._min_suspend_duration
                ):
                    break
                _LOGGER.info(
                    "Cumulative suspend time is %.2fs (< %.1fs); running "
                    "another suspend cycle...",
                    current_sag_delta.total_time_in_suspend.total_seconds(),
                    self._min_suspend_duration.total_seconds(),
                )
        finally:
            # On workbench, the display panel must be manually powered on after
            # resume.
            self._set_display_power(self.dut, power_on=True)


class NoSuspendOnUsbTestCases(fuchsia_base_test.FuchsiaTestCases):
    """Test cases for verifying the device does not suspend while on USB."""

    async def test_no_suspend_on_usb(self) -> None:
        before_on_usb_idle_stats = await power.get_sag_suspend_stats(self.dut)

        # Then, idle a bit while plugged in to make sure we _don't_ suspend.
        await control_flows.sleep_for_duration(timedelta(seconds=60))

        while_on_usb_stats = (
            await power.get_sag_suspend_stats(self.dut)
            - before_on_usb_idle_stats
        )

        _LOGGER.info(
            f"Suspend stats during on-charger idle: \n{while_on_usb_stats}"
        )
        assert_equal(
            while_on_usb_stats.success_count,
            0,
            "SAG must not suspend during idle",
        )

        # NOTE(hjfreyer): These checks are meant to detect situations where the device sits in a
        # suspend attempt loop, but doesn't actually suspend. Checking that there were *no* attempts
        # to suspend seems like it could be too harsh and lead to flakes... but the threshold here
        # hasn't been tuned at all.
        assert_less(
            while_on_usb_stats.fail_count,
            10,
            "SAG attempted to suspend too many times while on USB",
        )
