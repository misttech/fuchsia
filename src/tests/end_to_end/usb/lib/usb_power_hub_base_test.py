# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Base test class for Mobly tests requiring or optionally using a USB power hub fixture."""

import logging
import typing

import fuchsia_base_test
from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from mobly import signals

from . import linux_virtual_usb_hub

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbPowerHubBaseTest(fuchsia_base_test.FuchsiaBaseTest):
    """Base test class for Mobly tests that bind to a USB power hub fixture.

    Attributes:
        USB_POWER_HUB_REQUIRED: If True, failure to acquire the USB power hub fixture
            or power on the port will abort the test class (signals.TestAbortClass).
            If False, failures are logged as warnings and the test proceeds with
            both _usb_power_hub and _usb_port set to None.
    """

    USB_POWER_HUB_REQUIRED: bool = True

    _usb_power_hub: usb_power_hub.UsbPowerHub | None = None
    _usb_port: int | None = None

    def require_usb_power_hub(self) -> usb_power_hub.UsbPowerHub:
        """Returns the bound hub, raising if it was not acquired."""
        if self._usb_power_hub is None:
            raise errors.HoneydewError(
                "USB power hub fixture is required but was not acquired."
            )
        return self._usb_power_hub

    def _lookup_usb_power_hub(
        self, fx_device: fuchsia_device.FuchsiaDevice
    ) -> tuple[usb_power_hub.UsbPowerHub | None, int | None]:
        """Returns the USB power hub and port to use for the given device.

        Selected in this order:
          1. The hub described by `usb_power_hub_hw` + `usb_power_hub_impl`
             in the testbed config, when both are present.
          2. The host side virtual hub, when the `use_virtual_usb_hub` param
             is set. It toggles the device USB `authorized` attribute instead
             of cutting power, so it needs no testbed config nor physical
             hub. Intended for at-desk runs; see
             //src/tests/end_to_end/usb/lib/disconnect/udev.sh for the host
             permissions it requires.
          3. DMC, for devices power cycled in infra.

        Returns:
            The USB power hub and its port, or `(None, None)` if none apply.
        """
        device_config: dict[str, object] = self._get_device_config(
            controller_type="FuchsiaDevice",
            identifier_key="name",
            identifier_value=fx_device.device_name,
        )
        if not (
            device_config.get("usb_power_hub_hw")
            and device_config.get("usb_power_hub_impl")
        ) and self.user_params.get("use_virtual_usb_hub"):
            # Disambiguates discovery when more than one Fuchsia USB device is
            # attached to the same host.
            target_serial = typing.cast(
                str | None, device_config.get("device_serial", None)
            )
            return (
                linux_virtual_usb_hub.LinuxVirtualUsbPowerHub(
                    target_serial=target_serial,
                    # Retry settings for the bus ID lookup in power_off().
                    bus_id_lookup_attempts=self.user_params.get(
                        "bus_id_lookup_attempts",
                        linux_virtual_usb_hub.DEFAULT_BUS_ID_LOOKUP_ATTEMPTS,
                    ),
                    bus_id_lookup_timeout_sec=self.user_params.get(
                        "bus_id_lookup_timeout_sec",
                        linux_virtual_usb_hub.DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC,
                    ),
                ),
                None,
            )
        return super()._lookup_usb_power_hub(fx_device)

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        hub: usb_power_hub.UsbPowerHub | None = self.dut.usb_power_hub
        port: int | None = self.dut.usb_power_hub_port
        if hub is None:
            if self.USB_POWER_HUB_REQUIRED:
                raise signals.TestAbortClass(
                    f"USB power hub is not configured for {self.dut.device_name}."
                )
            _LOGGER.info(
                "No USB power hub configured for %s. Proceeding without one.",
                self.dut.device_name,
            )
            self._usb_power_hub = None
            self._usb_port = None
        else:
            self._usb_power_hub = hub
            self._usb_port = port
            self._usb_power_hub.power_on(port=self._usb_port)
            _LOGGER.info(
                "Successfully bound USB Power Hub fixture on port %s",
                self._usb_port,
            )

        # Pre-cache PersistentProperty values (board, product) while the device is online
        # in Fuchsia OS. Because they are lazily evaluated, if left un-evaluated and the test
        # later fails while in Fastboot mode or offline, Mobly's post-test metadata collection
        # (test_summary.yaml) would trigger an FFX query against the offline device.
        _ = self.dut.board
        _ = self.dut.product
