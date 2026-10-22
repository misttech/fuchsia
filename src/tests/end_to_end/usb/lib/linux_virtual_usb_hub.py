# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Linux Virtual UsbPower auxiliary device implementation."""

import glob
import logging
import os
import platform
import re
import time

from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.utils import common, host_shell

_LOGGER: logging.Logger = logging.getLogger(__name__)

DEFAULT_BUS_ID_LOOKUP_ATTEMPTS: int = 5
DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC: float = 10.0
_BUS_ID_LOOKUP_RETRY_INTERVAL_SEC: float = 1.0


class LinuxVirtualUsbPowerHub(usb_power_hub.UsbPowerHub):
    """LinuxVirtualUsbPowerHub auxiliary device implementation.

    This class enables virtual USB plug/unplug (authorization control)
    on a Linux host. It is intended for local at-desk developer testing
    and virtual/emulated testbeds where physical power hubs are not available.

    Note on permissions:
        Writing to `/sys/bus/usb/devices/<bus_id>/authorized` requires write
        access that a regular user does not have by default. Run
        //src/tests/end_to_end/usb/lib/disconnect/udev.sh (or the at-desk
        runner //src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh,
        which invokes `udev.sh`) to install the host udev rules (`sudo` is only
        prompted on first-time setup) so `power_off()`/`power_on()` can toggle
        `authorized` at test time without `sudo`.

    Args:
        target_serial: The serial number of the Fuchsia device. Used for
            discovery to match against connected USB devices.
        bus_id_lookup_attempts: Maximum number of times `power_off()` looks up
            the bus ID before giving up, to tolerate the DUT still
            re-enumerating right after a previous reconnect.
        bus_id_lookup_timeout_sec: Maximum total time in seconds `power_off()`
            spends retrying the bus ID lookup. Retries stop at whichever of
            the attempts or this timeout is reached first.
    """

    def __init__(
        self,
        target_serial: str | None = None,
        bus_id_lookup_attempts: int = DEFAULT_BUS_ID_LOOKUP_ATTEMPTS,
        bus_id_lookup_timeout_sec: float = DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC,
    ) -> None:
        super().__init__()
        if platform.system() != "Linux":
            raise usb_power_hub.UsbPowerHubError(
                "LinuxVirtualUsbPowerHub is only supported on Linux hosts."
            )

        self._target_serial = target_serial
        self._bus_id_lookup_attempts = bus_id_lookup_attempts
        self._bus_id_lookup_timeout_sec = bus_id_lookup_timeout_sec
        self._usb_bus_id: str = self._find_usb_bus_id()

        if not common.is_infra() and not os.access(
            f"/sys/bus/usb/devices/{self._usb_bus_id}/authorized", os.W_OK
        ):
            msg = (
                "Host udev rules for virtual USB disconnect are not installed "
                f"or /sys/bus/usb/devices/{self._usb_bus_id}/authorized is not "
                "writable. Run //src/tests/end_to_end/usb/lib/disconnect/udev.sh "
                "or //src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh "
                "first."
            )
            _LOGGER.error(msg)
            raise usb_power_hub.UsbPowerHubError(msg)

    def power_off(self, port: int | None = None) -> None:
        """Deauthorizes (virtually unplugs) the USB device.

        The bus ID is looked up again before each unplug, since the DUT may
        have re-enumerated on a different port path (e.g. moved to another
        port or a hub renumbered) since the last call. The lookup is retried
        per `bus_id_lookup_attempts` and `bus_id_lookup_timeout_sec` in case
        the DUT is still re-enumerating. `power_on()` reuses the bus ID
        resolved here.

        Args:
            port: None. Not used by this implementation.
        """
        end_time = time.monotonic() + self._bus_id_lookup_timeout_sec
        for attempt in range(1, self._bus_id_lookup_attempts + 1):
            try:
                self._usb_bus_id = self._find_usb_bus_id()
                break
            except ValueError as err:
                if (
                    attempt == self._bus_id_lookup_attempts
                    or time.monotonic() >= end_time
                ):
                    raise usb_power_hub.UsbPowerHubError(
                        f"USB bus ID lookup failed after {attempt} attempt(s): "
                        f"{err}"
                    ) from err
                _LOGGER.warning(
                    "USB bus ID lookup attempt %d/%d failed: %s. Retrying...",
                    attempt,
                    self._bus_id_lookup_attempts,
                    err,
                )
                time.sleep(_BUS_ID_LOOKUP_RETRY_INTERVAL_SEC)
        _LOGGER.info("Virtually unplugging USB device %s...", self._usb_bus_id)
        cmd: list[str] = [
            "sh",
            "-c",
            f"echo 0 > /sys/bus/usb/devices/{self._usb_bus_id}/authorized",
        ]
        try:
            host_shell.run(cmd=cmd)
        except errors.HostCmdError as err:
            raise usb_power_hub.UsbPowerHubError(err) from err
        _LOGGER.info(
            "Successfully virtually unplugged USB device %s.", self._usb_bus_id
        )

    def power_on(self, port: int | None = None) -> None:
        """Authorizes (virtually plugs in) the USB device.

        Args:
            port: None. Not used by this implementation.
        """
        _LOGGER.info("Virtually plugging in USB device %s...", self._usb_bus_id)
        cmd: list[str] = [
            "sh",
            "-c",
            f"echo 1 > /sys/bus/usb/devices/{self._usb_bus_id}/authorized",
        ]
        try:
            host_shell.run(cmd=cmd)
        except errors.HostCmdError as err:
            raise usb_power_hub.UsbPowerHubError(err) from err
        _LOGGER.info(
            "Successfully virtually plugged in USB device %s.", self._usb_bus_id
        )

    def _find_usb_bus_id(self) -> str:
        """Finds the USB bus ID for the target device."""
        vendor_id = "18d1"
        product_ids = {p.lower() for p in ["a02b", "a025", "d00d"]}

        serial = self._target_serial

        matching_devices = []
        for dev_path in glob.glob("/sys/bus/usb/devices/*"):
            vendor_file = os.path.join(dev_path, "idVendor")
            product_file = os.path.join(dev_path, "idProduct")
            serial_file = os.path.join(dev_path, "serial")

            if not (
                os.path.exists(vendor_file) and os.path.exists(product_file)
            ):
                continue

            try:
                with open(vendor_file, "r", encoding="utf-8") as f_v, open(
                    product_file, "r", encoding="utf-8"
                ) as f_p:
                    v_id = f_v.read().strip().lower()
                    p_id = f_p.read().strip().lower()

                if v_id != vendor_id.lower() or p_id not in product_ids:
                    continue

                if serial:
                    if not os.path.exists(serial_file):
                        continue
                    with open(serial_file, "r", encoding="utf-8") as f_s:
                        dev_serial = f_s.read().strip()
                    if dev_serial != serial:
                        continue

                matching_devices.append(os.path.basename(dev_path))
            except OSError:
                pass

        if not matching_devices:
            raise ValueError(
                f"No USB device with vendor {vendor_id} and products {product_ids} (serial: {serial}) found."
            )
        if len(matching_devices) > 1:
            raise ValueError(
                f"Multiple USB devices found: {matching_devices}. "
                "Please specify target_serial explicitly."
            )
        usb_bus_id = matching_devices[0]
        if not re.match(r"^[a-zA-Z0-9.-]+$", usb_bus_id):
            raise ValueError(f"Invalid usb_bus_id format: {usb_bus_id}")
        return usb_bus_id
