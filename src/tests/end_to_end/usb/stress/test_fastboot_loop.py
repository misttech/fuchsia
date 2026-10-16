# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for repetitive USB disconnect and reconnect cycles in
Fastboot mode.
"""

import asyncio
import logging

import usb_lib
from honeydew import errors
from mobly import expects, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbFastbootLoopTest(usb_lib.UsbPowerHubBaseTest):
    """Mobly test for stress testing USB disconnects in Fastboot mode.

    Required Mobly Test Params:
        num_iterations (int, optional): Number of times to execute the
            disconnect loop. Defaults to 10.
        disconnect_duration_sec (int, optional): How long to stay disconnected
            in seconds. Defaults to 3.
        fastboot_reconnect_timeout_sec (int, optional): Timeout waiting for
            fastboot mode re-entry after power-on. Defaults to 60.
        fuchsia_reboot_timeout_sec (int, optional): Timeout for initial boot
            to fastboot and recovery back to Fuchsia mode. Defaults to 30.
    """

    USB_POWER_HUB_REQUIRED: bool = True

    async def _run_fastboot_disconnect_loop(
        self,
        num_iterations: int,
        disconnect_duration: int,
        fastboot_reconnect_timeout: int,
    ) -> None:
        """Executes repetitive USB disconnect and reconnect cycles in Fastboot
        mode.

        Enforces a minimum 2s VBUS discharge settling delay before polling
        offline to prevent premature fastboot re-enumeration from triggering
        DWC3 bulk transfer aborts and host USB link drops (b/568894619).
        """
        power_hub = self.require_usb_power_hub()
        for iteration in range(1, num_iterations + 1):
            _LOGGER.info(
                "Starting Fastboot disconnect iteration# %d/%d",
                iteration,
                num_iterations,
            )
            try:
                power_hub.power_off(port=self._usb_port)
                # Ensure disconnect duration is at least 2s for VBUS discharge
                # settling to avoid DWC3 bulk transfer aborts (b/568894619).
                actual_disconnect_duration = max(2, disconnect_duration)
                timeout = max(actual_disconnect_duration, 5)
                _LOGGER.info(
                    "Polling up to %d seconds for the USB to disconnect",
                    timeout,
                )
                poll_end_time = asyncio.get_running_loop().time() + timeout
                while asyncio.get_running_loop().time() < poll_end_time:
                    if not self.dut.fastboot.is_in_fastboot_mode():
                        break
                    await asyncio.sleep(0.5)
                expects.expect_false(
                    self.dut.fastboot.is_in_fastboot_mode(),
                    "Fastboot device is still visible",
                )
            finally:
                power_hub.power_on(port=self._usb_port)
                _LOGGER.info("Waiting for device to re-enter fastboot...")
                try:
                    self.dut.fastboot.wait_for_fastboot_mode(
                        timeout=fastboot_reconnect_timeout
                    )
                    _LOGGER.info("Device successfully re-entered fastboot.")
                except (
                    TimeoutError,
                    errors.HoneydewTimeoutError,
                    asyncio.TimeoutError,
                ) as e:
                    raise errors.FuchsiaDeviceError(
                        "Device failed to re-enter Fastboot after power-on. "
                        "It likely booted to Fuchsia/Android automatically. "
                        "Fastboot loop aborted."
                    ) from e

    async def test_usb_fastboot_loop(self) -> None:
        """Test logic that loops disconnects entirely within Fastboot mode."""
        _LOGGER.info("Starting the Fastboot USB Disconnect Loop test")

        num_iterations = int(self.user_params.get("num_iterations", 10))
        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 3)
        )
        fastboot_reconnect_timeout = int(
            self.user_params.get("fastboot_reconnect_timeout_sec", 90)
        )
        fuchsia_reboot_timeout = int(
            self.user_params.get("fuchsia_reboot_timeout_sec", 120)
        )

        try:
            await usb_lib.reboot_to_fastboot_mode(
                self.dut,
                usb_power_hub=self._usb_power_hub,
                usb_port=self._usb_port,
                timeout_sec=fuchsia_reboot_timeout,
            )
            await self._run_fastboot_disconnect_loop(
                num_iterations, disconnect_duration, fastboot_reconnect_timeout
            )

        finally:
            await usb_lib.recover_to_fuchsia_mode(
                self.dut,
                usb_power_hub=self._usb_power_hub,
                usb_port=self._usb_port,
                timeout_sec=fuchsia_reboot_timeout,
            )


if __name__ == "__main__":
    test_runner.main()
