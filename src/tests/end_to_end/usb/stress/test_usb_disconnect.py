# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB Disconnect stress tests (handles both physical and virtual)."""

import asyncio
import json
import logging

import usb_lib
from honeydew import errors
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.typing import custom_types
from mobly import asserts, signals, test_runner
from usb_lib.dwc3_irq_metrics import (
    Dwc3IrqSnapshot,
    assert_no_dwc3_unhandled_irq_storm,
    collect_dwc3_irq_metrics,
)
from usb_lib.irq_storm import (
    IrqStormSnapshot,
    assert_no_irq_storm,
    take_irq_storm_snapshot,
)
from usb_lib.link_speed import assert_link_speed

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_RECONNECT_TIMEOUT_SEC: int = 60
_DEFAULT_TEST_CASE_PREFIX: str = "test_usb_disconnect"
_DISCONNECT_METHOD_POWER_CYCLE: str = "power_cycle"
_DISCONNECT_METHOD_RESET: str = "reset"


class UsbDisconnectTest(usb_lib.UsbPowerHubBaseTest):
    """Mobly test for testing USB disconnects.

    Supports both physical disconnect (using hardware PDU/power hub) and virtual
    disconnect (using software authorization control).

    Required Mobly Test Params:
        num_iterations (int, optional): Number of times to execute the test.
            Defaults to 10 (or uses num_usb_disconnects if provided).
        num_usb_disconnects (int, optional): Alias for num_iterations.
        disconnect_duration_sec (int, optional): How long to stay disconnected.
            Defaults to 10. Not used by the "reset" disconnect method.
        disconnect_method (str, optional): How to disconnect the device:
            - "power_cycle" (default): power the USB port off, wait for the
              device to go offline and `disconnect_duration_sec`, then power
              it back on.
            - "reset": reset the USB link in a single step, e.g. with
              `virtual_usb_hub_mode: reset`. The link has no "off" phase, so
              there is no offline wait nor disconnect duration. Hubs that
              cannot reset fall back to a power cycle without the offline
              wait.
        reconnect_timeout_sec (int, optional): How long to wait for the device
            to come back online after USB power is restored. Defaults to 60.
        verify_mdns (bool, optional): Verify that the device is discoverable
            over mDNS before the first disconnect and after every reconnect.
            Defaults to False.
        mdns_timeout_sec (int, optional): How long each mDNS check waits for
            the device to be discovered. Defaults to 12, which allows one
            retry, since ffx re-sends its mDNS query every 10s.
        expected_usb_link_speed (str, optional): When set, e.g. to "high",
            assert the USB link negotiated this speed after every reconnect.
        verify_no_irq_storm (bool, optional): Verify that no driver dispatcher
            spin-loop or unhandled IRQ storm occurred across each disconnect
            cycle (checked across all drivers on the DUT, not only USB
            drivers). Defaults to False.
        test_case_prefix (str, optional): Prefix of the generated test case
            names, which end in the iteration number. Defaults to
            "test_usb_disconnect".
    """

    USB_POWER_HUB_REQUIRED: bool = True
    _reconnect_failed: bool = False

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
        if self.user_params.get("verify_mdns", False):
            # Confirm mDNS works before any disconnect, so that a failure in
            # an iteration can be attributed to the hotplug.
            self._verify_mdns_advertisement("baseline")

    async def setup_test(self) -> None:
        """setup_test is called once before running each test."""
        self._reconnect_failed = False
        await super().setup_test()

    async def teardown_test(self) -> None:
        """teardown_test is called once after running each test."""
        if self._reconnect_failed:
            _LOGGER.warning(
                "Skipping teardown health check because device failed to reconnect over USB."
            )
            return
        await super().teardown_test()

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that disconnects the USB from a fuchsia device."""
        _LOGGER.info(
            "Starting the Usb Disconnect test iteration# %s", iteration
        )

        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 10)
        )
        disconnect_method = self.user_params.get(
            "disconnect_method", _DISCONNECT_METHOD_POWER_CYCLE
        )
        if disconnect_method not in (
            _DISCONNECT_METHOD_POWER_CYCLE,
            _DISCONNECT_METHOD_RESET,
        ):
            raise signals.TestAbortAll(
                f"Unsupported disconnect_method {disconnect_method!r}; "
                f"expected {_DISCONNECT_METHOD_POWER_CYCLE!r} or "
                f"{_DISCONNECT_METHOD_RESET!r}."
            )
        reconnect_timeout = int(
            self.user_params.get(
                "reconnect_timeout_sec", _DEFAULT_RECONNECT_TIMEOUT_SEC
            )
        )

        power_hub = self.require_usb_power_hub()

        try:
            await asyncio.wait_for(
                self.dut.wait_for_online(),
                timeout=reconnect_timeout,
            )
        except asyncio.TimeoutError as e:
            self._reconnect_failed = True
            raise signals.TestAbortAll(
                f"Device {self.dut.device_name} failed to come online before "
                f"starting iteration {iteration} within {reconnect_timeout}s."
            ) from e

        pre_disconnect_boot_id = await self.dut.boot_id()
        _LOGGER.info("Pre-disconnect Boot ID: %s", pre_disconnect_boot_id)
        verify_no_irq_storm = bool(
            self.user_params.get("verify_no_irq_storm", False)
        )
        pre_disconnect_irq_snapshot: IrqStormSnapshot | None = None
        pre_disconnect_dwc3_metrics: dict[str, Dwc3IrqSnapshot] | None = None
        if verify_no_irq_storm:
            pre_disconnect_irq_snapshot = await take_irq_storm_snapshot(
                self.dut
            )
            pre_disconnect_dwc3_metrics = collect_dwc3_irq_metrics(self.dut)
        self.dut.fuchsia_controller.before_usb_disconnect()
        try:
            self.dut.ffx.notify_intentional_disconnect()
            if disconnect_method == _DISCONNECT_METHOD_RESET:
                _LOGGER.info("Resetting the USB link...")
                await asyncio.to_thread(
                    usb_lib.reset_usb_link,
                    power_hub,
                    self._usb_port,
                    disconnect_duration,
                )
                await asyncio.to_thread(self._restart_host_adb_server)
            else:
                power_hub.power_off(port=self._usb_port)
                _LOGGER.info("Waiting for the device to go offline...")
                await asyncio.to_thread(self.dut.wait_for_offline)
                _LOGGER.info("Device is successfully offline.")

                if disconnect_duration > 0:
                    _LOGGER.info(
                        "Sleeping for %d seconds...", disconnect_duration
                    )
                    await asyncio.sleep(disconnect_duration)
        finally:
            if disconnect_method == _DISCONNECT_METHOD_POWER_CYCLE:
                power_hub.power_on(port=self._usb_port)
            _LOGGER.info("Waiting for the device to go online...")
            try:
                await asyncio.wait_for(
                    self.dut.wait_for_online(),
                    timeout=reconnect_timeout,
                )
            except asyncio.TimeoutError as e:
                self._reconnect_failed = True
                raise signals.TestAbortAll(
                    f"Device {self.dut.device_name} failed to reconnect over USB "
                    f"network within {reconnect_timeout}s after USB power-on."
                ) from e
            self.dut.fuchsia_controller.after_usb_reconnect()
            post_reconnect_boot_id = await self.dut.boot_id()
            _LOGGER.info("Post-reconnect Boot ID: %s", post_reconnect_boot_id)
            if pre_disconnect_boot_id != post_reconnect_boot_id:
                raise errors.FuchsiaDeviceError(
                    f"Unexpected reboot detected during USB disconnect for {self.dut.device_name}. "
                    f"Boot ID before: {pre_disconnect_boot_id} != after: {post_reconnect_boot_id}"
                )
            _LOGGER.info("Device is successfully back online.")
            self.dut.health_check()

        # Outside the `finally` block so that an mDNS, link speed, or IRQ storm
        # failure cannot mask a disconnect failure.
        if (
            pre_disconnect_irq_snapshot is not None
            and pre_disconnect_dwc3_metrics is not None
        ):
            # Note: `assert_no_irq_storm` checks for driver dispatcher
            # spin-loops and hardware IRQ storms across all drivers on the DUT
            # (not only USB drivers).
            post_reconnect_irq_snapshot = await take_irq_storm_snapshot(
                self.dut
            )
            post_reconnect_dwc3_metrics = collect_dwc3_irq_metrics(self.dut)
            assert_no_irq_storm(
                pre_disconnect_irq_snapshot,
                post_reconnect_irq_snapshot,
                phase=f"iteration {iteration}",
            )
            assert_no_dwc3_unhandled_irq_storm(
                pre_disconnect_dwc3_metrics,
                post_reconnect_dwc3_metrics,
                before_irq_snapshot=pre_disconnect_irq_snapshot,
                after_irq_snapshot=post_reconnect_irq_snapshot,
                phase=f"iteration {iteration}",
            )
        if self.user_params.get("verify_mdns", False):
            self._verify_mdns_advertisement(f"iteration {iteration}")
        if expected_speed := self.user_params.get("expected_usb_link_speed"):
            assert_link_speed(
                self.dut,
                expected=expected_speed,
                phase=f"iteration {iteration}",
            )

        _LOGGER.info(
            "Successfully ended the Usb Disconnect test iteration# %s",
            iteration,
        )

    def _restart_host_adb_server(self) -> None:
        """Restarts the isolated host ADB server of the DUT, if any.

        An in-place USB reset keeps the device number, so a running host ADB
        server keeps its stale USB handle and loses the device for good.
        Restarting the host server is enough; adbd on the DUT is unaffected.
        """
        # TODO(b/562031693): Use a public Honeydew API once one exists.
        adb_server = getattr(
            getattr(self.dut, "adb", None), "_adb_server", None
        )
        if adb_server is None:
            return
        _LOGGER.info("Restarting the host ADB server after the USB reset...")
        adb_server.restart()

    def _verify_mdns_advertisement(self, phase: str) -> None:
        """Asserts that the device is discoverable over mDNS.

        In infra, Honeydew reconnects to the device by static IP, and ffx can
        also find it over USB, so a broken mDNS responder would otherwise go
        unnoticed. `--no-usb` limits discovery to mDNS, and `--no-probe` skips
        connecting to the device since only discovery is under test.

        Args:
            phase: Label used in log and failure messages, e.g. "iteration 3".
        """
        timeout_sec = int(self.user_params.get("mdns_timeout_sec", 12))
        try:
            output = self.dut.ffx.run(
                cmd=[
                    "-c",
                    f"discovery.timeout={timeout_sec * 1000}",
                    "target",
                    "list",
                    "--no-usb",
                    "--no-probe",
                    self.dut.device_name,
                ],
                include_target=False,
            )
        except ffx_errors.FfxCommandError as e:
            # When the named device is not discovered, ffx exits with an error
            # rather than returning an empty list.
            raise signals.TestFailure(
                f"[{phase}] Device {self.dut.device_name} not discovered via "
                f"mDNS within {timeout_sec}s: {e}"
            ) from e

        targets = json.loads(output)
        asserts.assert_true(
            bool(targets), f"[{phase}] No targets returned by ffx target list"
        )
        discovered_ips = [
            str(address["ip"]).split("%")[0]
            for address in targets[0]["addresses"]
        ]
        _LOGGER.info(
            "[%s] Device discovered via mDNS at %s", phase, discovered_ips
        )

        target_addr = self.dut.ffx.get_target_address()
        if isinstance(target_addr, custom_types.IpPort):
            expected_ip = str(target_addr.ip).split("%")[0]
            asserts.assert_in(
                expected_ip,
                discovered_ips,
                f"[{phase}] Expected DUT IP {expected_ip} in mDNS addresses "
                f"{discovered_ips}",
            )

    def _name_func(self, iteration: int) -> str:
        """This function generates the names of each test case based on each
        argument set.

        The name function should have the same signature as the actual test
        logic function.

        Returns:
            Test case name
        """
        prefix = self.user_params.get(
            "test_case_prefix", _DEFAULT_TEST_CASE_PREFIX
        )
        return f"{prefix}_{iteration}"


if __name__ == "__main__":
    test_runner.main()
