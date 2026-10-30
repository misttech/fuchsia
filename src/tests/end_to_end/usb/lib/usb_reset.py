# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""USB link reset support for USB power hubs."""

import abc
import logging
import time

from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbResetCapable(abc.ABC):
    """Mixin for USB power hubs that can reset a device's USB link.

    A reset is a single atomic step, unlike a power cycle: the host drives a
    bus reset on the port and re-enumerates the device without cutting VBUS,
    so the link has no "off" phase to hold or to observe.
    """

    @abc.abstractmethod
    def reset(self, port: int | None = None) -> None:
        """Resets the USB link of the device.

        Args:
            port: The USB port of the device, if the hub needs it.

        Raises:
            usb_power_hub.UsbPowerHubError: In case of failure.
        """


def reset_usb_link(
    hub: usb_power_hub.UsbPowerHub,
    port: int | None = None,
    off_duration_sec: float = 0,
) -> None:
    """Resets the USB link of the device behind the hub.

    Uses `reset()` on hubs that support it. Otherwise falls back to powering
    the port off, waiting `off_duration_sec`, and powering it back on.

    Args:
        hub: The USB power hub of the device.
        port: The USB port of the device, if the hub needs it.
        off_duration_sec: How long the port stays off in the power cycle
            fallback. Not used by hubs that support `reset()`.
    """
    if isinstance(hub, UsbResetCapable):
        hub.reset(port=port)
        return
    _LOGGER.info(
        "%s cannot reset the USB link; power cycling it instead.",
        type(hub).__name__,
    )
    hub.power_off(port=port)
    try:
        if off_duration_sec > 0:
            time.sleep(off_duration_sec)
    finally:
        hub.power_on(port=port)
