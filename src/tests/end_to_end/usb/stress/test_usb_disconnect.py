# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB Disconnect stress tests (handles both physical and virtual)."""

import asyncio
import logging

import fidl_fuchsia_hardware_power_statecontrol as fhp_statecontrol
import fuchsia_base_test
from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.typing import custom_types as honeydew_types
from mobly import expects, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbDisconnectTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for testing USB disconnects.

    Supports both physical disconnect (using hardware PDU/power hub) and virtual
    disconnect (using software authorization control).

    Required Mobly Test Params:
        num_iterations (int, optional): Number of times to execute the test.
            Defaults to 10 (or uses num_usb_disconnects if provided).
        num_usb_disconnects (int, optional): Alias for num_iterations.
        disconnect_duration_sec (int, optional): How long to stay disconnected.
            Defaults to 10.
    """

    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        fastboot_loop = bool(self.user_params.get("fastboot_loop", False))
        if fastboot_loop:
            self.generate_tests(
                test_logic=self._test_fastboot_loop_logic,
                name_func=self._fastboot_loop_name_func,
                arg_sets=[()],
            )
            return

        test_arg_tuple_list: list[tuple[int]] = []

        num_iterations = int(
            self.user_params.get(
                "num_iterations",
                self.user_params.get("num_usb_disconnects", 10),
            )
        )
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        self._usb_power_hub: usb_power_hub.UsbPowerHub
        self._usb_port: int | None
        (self._usb_power_hub, self._usb_port) = self._lookup_usb_power_hub(
            self.dut
        )

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that disconnects the USB from a fuchsia device."""
        _LOGGER.info(
            "Starting the Usb Disconnect test iteration# %s", iteration
        )

        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 10)
        )

        await self.dut.wait_for_online()
        try:
            self._usb_power_hub.power_off(port=self._usb_port)
            _LOGGER.info("Waiting for the device to go offline...")
            await asyncio.to_thread(self.dut.wait_for_offline)
            _LOGGER.info("Device is successfully offline.")

            if disconnect_duration > 0:
                _LOGGER.info("Sleeping for %d seconds...", disconnect_duration)
                await asyncio.sleep(disconnect_duration)
        finally:
            self._usb_power_hub.power_on(port=self._usb_port)
            _LOGGER.info("Waiting for the device to go online...")
            await self.dut.wait_for_online()
            await self.dut.on_device_boot()
            _LOGGER.info("Device is successfully back online.")

        _LOGGER.info(
            "Successfully ended the Usb Disconnect test iteration# %s",
            iteration,
        )

    def _name_func(self, iteration: int) -> str:
        """This function generates the names of each test case based on each
        argument set.

        The name function should have the same signature as the actual test
        logic function.

        Returns:
            Test case name
        """
        return f"test_usb_disconnect_{iteration}"

    def _fastboot_loop_name_func(self) -> str:
        return "test_usb_fastboot_loop"

    async def _test_fastboot_loop_logic(self) -> None:
        """Test logic that loops disconnects entirely within Fastboot mode."""
        _LOGGER.info("Starting the Fastboot USB Disconnect Loop test")

        num_iterations = int(self.user_params.get("num_iterations", 10))
        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 3)
        )
        fastboot_reconnect_timeout = int(
            self.user_params.get("fastboot_reconnect_timeout_sec", 60)
        )
        fuchsia_reboot_timeout = int(
            self.user_params.get("fuchsia_reboot_timeout_sec", 30)
        )

        # 1. Setup: Reboot to Fastboot (once)
        _LOGGER.info("Rebooting device to bootloader via FIDL (once)...")

        power_admin_endpoint = honeydew_types.FidlEndpoint(
            "/bootstrap/shutdown_shim",
            "fuchsia.hardware.power.statecontrol.Admin",
        )

        # TODO(b/527657910): Remove private _get_fastboot_node() call once Honeydew adds retry
        # polling inside _get_fastboot_node() when queried mid-reboot.
        try:
            await self.dut.fastboot._get_fastboot_node()
        except Exception as e:
            _LOGGER.debug(
                "Pre-populating fastboot node ID raised exception: %s", e
            )

        try:
            self.dut.ffx.notify_intentional_disconnect()
            fc_transport = self.dut.fuchsia_controller
            power_proxy = fhp_statecontrol.AdminClient(
                fc_transport.connect_device_proxy(power_admin_endpoint)
            )
            await power_proxy.shutdown(
                options=fhp_statecontrol.ShutdownOptions(
                    action=fhp_statecontrol.ShutdownAction.REBOOT_TO_BOOTLOADER,
                    reasons=[fhp_statecontrol.ShutdownReason.DEVELOPER_REQUEST],
                )
            )
        except Exception as e:
            _LOGGER.debug(
                "Reboot command raised exception (expected if device rebooted quickly): %s",
                e,
            )

        _LOGGER.info("Waiting for device to enter fastboot mode...")
        await self.dut.fastboot.wait_for_fastboot_mode()

        # 2. Loop: Disconnect/Reconnect in Fastboot
        for iteration in range(1, num_iterations + 1):
            _LOGGER.info(
                "Starting Fastboot disconnect iteration# %d/%d",
                iteration,
                num_iterations,
            )
            try:
                self._usb_power_hub.power_off(port=self._usb_port)
                _LOGGER.info(
                    "Waiting %d seconds for the USB to disconnect",
                    disconnect_duration,
                )
                await asyncio.sleep(disconnect_duration)
                for attempt in range(15):
                    if not await self.dut.fastboot.is_in_fastboot_mode():
                        await asyncio.sleep(2)
                        if not await self.dut.fastboot.is_in_fastboot_mode():
                            break
                    _LOGGER.debug(
                        "Waiting for fastboot device to settle offline (attempt %d)...",
                        attempt + 1,
                    )
                    await asyncio.sleep(1)
                expects.expect_false(
                    await self.dut.fastboot.is_in_fastboot_mode(),
                    "Fastboot device is still visible",
                )
            finally:
                self._usb_power_hub.power_on(port=self._usb_port)
                _LOGGER.info("Waiting for device to re-enter fastboot...")
                try:
                    await asyncio.wait_for(
                        self.dut.fastboot.wait_for_fastboot_mode(),
                        timeout=fastboot_reconnect_timeout,
                    )
                    _LOGGER.info("Device successfully re-entered fastboot.")
                except asyncio.TimeoutError as e:
                    raise errors.FuchsiaDeviceError(
                        "Device failed to re-enter Fastboot after power-on. "
                        "It likely booted to Fuchsia/Android automatically. "
                        "Fastboot loop aborted."
                    ) from e

        # 3. Teardown: Boot back to Fuchsia (once)
        _LOGGER.info(
            "Fastboot loop completed successfully. Booting back to Fuchsia..."
        )
        try:
            await asyncio.wait_for(
                self.dut.fastboot.boot_to_fuchsia_mode(),
                timeout=fuchsia_reboot_timeout,
            )
        except (asyncio.TimeoutError, errors.HoneydewError) as e:
            _LOGGER.warning(
                "fastboot reboot command timed out/failed (%s). Waiting for online...",
                e,
            )
            for attempt in range(30):
                try:
                    await self.dut.wait_for_online()
                    break
                except Exception as err:
                    if attempt == 29:
                        raise
                    _LOGGER.warning(
                        "Waiting for DUT to return online (%s)...", err
                    )
                    await asyncio.sleep(2)

            await self.dut.on_device_boot()
        _LOGGER.info("Device is successfully back online in Fuchsia.")


if __name__ == "__main__":
    test_runner.main()
