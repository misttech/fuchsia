# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB Disconnect stress tests (handles both physical and virtual)."""

import asyncio
import logging

import fuchsia_base_test
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from mobly import test_runner

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


if __name__ == "__main__":
    test_runner.main()
