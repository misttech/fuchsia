# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Linux Virtual UsbPower auxiliary device implementations."""

import abc
import glob
import json
import logging
import os
import platform
import re
import time
from importlib import resources

from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.utils import common, host_shell
from testusb import ioctl

from . import usb_reset

_LOGGER: logging.Logger = logging.getLogger(__name__)

DEFAULT_BUS_ID_LOOKUP_ATTEMPTS: int = 5
DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC: float = 10.0
_BUS_ID_LOOKUP_RETRY_INTERVAL_SEC: float = 1.0

# Shared with //src/tests/end_to_end/usb/lib/disconnect/udev.sh.
_PID_LIST_FILE: str = "fuchsia_usb_pids.json"

_SYSFS_USB_DEVICES_DIR: str = "/sys/bus/usb/devices"


class _LinuxVirtualUsbPowerHubBase(usb_power_hub.UsbPowerHub):
    """Shared base of the Linux virtual USB power hubs.

    Finds the sysfs USB bus ID of the Fuchsia device on a Linux host and
    checks, outside infra, that the host file the subclass writes to is
    writable. Intended for local at-desk developer testing and
    virtual/emulated testbeds where physical power hubs are not available.

    Note on permissions:
        The files the subclasses write to require write access that a regular
        user does not have by default. Run
        //src/tests/end_to_end/usb/lib/disconnect/udev.sh (or the at-desk
        runner //src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh,
        which invokes `udev.sh`) to install the host udev rules (`sudo` is only
        prompted on first-time setup) so they can be written at test time
        without `sudo`.

    Args:
        target_serial: The serial number of the Fuchsia device. Used for
            discovery to match against connected USB devices.
        bus_id_lookup_attempts: Maximum number of times the bus ID is looked
            up before each virtual disconnect, to tolerate the DUT still
            re-enumerating right after a previous reconnect.
        bus_id_lookup_timeout_sec: Maximum total time in seconds spent
            retrying the bus ID lookup. Retries stop at whichever of the
            attempts or this timeout is reached first.
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
                f"{type(self).__name__} is only supported on Linux hosts."
            )

        self._target_serial = target_serial
        self._bus_id_lookup_attempts = bus_id_lookup_attempts
        self._bus_id_lookup_timeout_sec = bus_id_lookup_timeout_sec

        # The PID list is shared with udev.sh and shipped as usb_lib data
        # (see `data_sources` in this directory's BUILD.gn), so it is only
        # present when usb_lib is built with GN.
        try:
            # pylint: disable-next=import-outside-toplevel
            from . import data  # type: ignore[attr-defined,unused-ignore]

            pid_entries = json.loads(
                resources.files(data)
                .joinpath(_PID_LIST_FILE)
                .read_text(encoding="utf-8")
            )
        # KeyError (a LookupError) is raised instead of OSError when the
        # resource is missing from a zip/wheel package.
        except (ImportError, OSError, LookupError) as err:
            raise usb_power_hub.UsbPowerHubError(
                f"usb_lib data resource {_PID_LIST_FILE} not found; it is "
                "only bundled when usb_lib is built with GN "
                "(fx build / fx test)."
            ) from err
        self._product_ids: set[str] = {
            entry["pid"].lower() for entry in pid_entries
        }

        self._usb_bus_id: str = self._find_usb_bus_id()

        if not common.is_infra():
            try:
                path = self._host_write_path()
            except (OSError, ValueError) as err:
                raise usb_power_hub.UsbPowerHubError(
                    f"Failed to resolve the host file of USB device "
                    f"{self._usb_bus_id}: {err}"
                ) from err
            if not os.access(path, os.W_OK):
                msg = (
                    "Host udev rules for virtual USB disconnect are not "
                    f"installed or {path} is not writable. Run "
                    "//src/tests/end_to_end/usb/lib/disconnect/udev.sh "
                    "or //src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh "
                    "first."
                )
                _LOGGER.error(msg)
                raise usb_power_hub.UsbPowerHubError(msg)

    @abc.abstractmethod
    def _host_write_path(self) -> str:
        """Returns the host file written to virtually disconnect the device."""

    def _refresh_usb_bus_id(self) -> None:
        """Looks up the bus ID again, retrying while the DUT re-enumerates.

        The DUT may have re-enumerated on a different port path (e.g. moved
        to another port or a hub renumbered) since the last lookup. The lookup
        is retried per `bus_id_lookup_attempts` and
        `bus_id_lookup_timeout_sec` in case the DUT is still re-enumerating.

        Raises:
            usb_power_hub.UsbPowerHubError: If every lookup attempt fails.
        """
        end_time = time.monotonic() + self._bus_id_lookup_timeout_sec
        for attempt in range(1, self._bus_id_lookup_attempts + 1):
            try:
                self._usb_bus_id = self._find_usb_bus_id()
                return
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

    def _find_usb_bus_id(self) -> str:
        """Finds the USB bus ID for the target device."""
        vendor_id = "18d1"

        serial = self._target_serial

        matching_devices = []
        for dev_path in glob.glob(f"{_SYSFS_USB_DEVICES_DIR}/*"):
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

                if v_id != vendor_id.lower() or p_id not in self._product_ids:
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
                f"No USB device with vendor {vendor_id} and products {sorted(self._product_ids)} (serial: {serial}) found."
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


class LinuxVirtualUsbAuthorizePowerHub(_LinuxVirtualUsbPowerHubBase):
    """Virtually unplugs and plugs in the device by USB authorization control.

    `power_off()` and `power_on()` toggle the sysfs
    `/sys/bus/usb/devices/<bus_id>/authorized` attribute of the device on a
    Linux host.

    Args:
        target_serial: See `_LinuxVirtualUsbPowerHubBase`.
        bus_id_lookup_attempts: See `_LinuxVirtualUsbPowerHubBase`.
        bus_id_lookup_timeout_sec: See `_LinuxVirtualUsbPowerHubBase`.
    """

    def _host_write_path(self) -> str:
        return f"{_SYSFS_USB_DEVICES_DIR}/{self._usb_bus_id}/authorized"

    def power_off(self, port: int | None = None) -> None:
        """Deauthorizes (virtually unplugs) the USB device.

        The bus ID is looked up again before each unplug. `power_on()` reuses
        the bus ID resolved here.

        Args:
            port: None. Not used by this implementation.
        """
        self._refresh_usb_bus_id()
        _LOGGER.info("Virtually unplugging USB device %s...", self._usb_bus_id)
        self._write_authorized("0")
        _LOGGER.info(
            "Successfully virtually unplugged USB device %s.", self._usb_bus_id
        )

    def power_on(self, port: int | None = None) -> None:
        """Authorizes (virtually plugs in) the USB device.

        Args:
            port: None. Not used by this implementation.
        """
        _LOGGER.info("Virtually plugging in USB device %s...", self._usb_bus_id)
        self._write_authorized("1")
        _LOGGER.info(
            "Successfully virtually plugged in USB device %s.", self._usb_bus_id
        )

    def _write_authorized(self, value: str) -> None:
        """Writes the sysfs `authorized` attribute of the USB device."""
        cmd: list[str] = [
            "sh",
            "-c",
            f"echo {value} > {self._host_write_path()}",
        ]
        try:
            host_shell.run(cmd=cmd)
        except errors.HostCmdError as err:
            raise usb_power_hub.UsbPowerHubError(err) from err


class LinuxVirtualUsbResetPowerHub(
    _LinuxVirtualUsbPowerHubBase, usb_reset.UsbResetCapable
):
    """Resets the device's USB link with the USBDEVFS_RESET ioctl.

    `reset()` issues USBDEVFS_RESET on the usbfs device node
    `/dev/bus/usb/<busnum>/<devnum>` of the device on a Linux host. The host
    drives a bus reset on the port and re-enumerates the device without
    cutting VBUS, so there is no "off" phase: `power_off()` is not supported,
    and `power_on()` is a no-op since this hub never leaves the link down.

    Args:
        target_serial: See `_LinuxVirtualUsbPowerHubBase`.
        bus_id_lookup_attempts: See `_LinuxVirtualUsbPowerHubBase`.
        bus_id_lookup_timeout_sec: See `_LinuxVirtualUsbPowerHubBase`.
    """

    def _host_write_path(self) -> str:
        return self._usb_device_node()

    def reset(self, port: int | None = None) -> None:
        """Resets the USB link of the device.

        The bus ID and the device node are looked up again before each
        reset, as the device number changes whenever the device re-enumerates.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: In case of failure.
        """
        self._refresh_usb_bus_id()
        try:
            node = self._usb_device_node()
            _LOGGER.info(
                "Resetting USB device %s (%s)...", self._usb_bus_id, node
            )
            reenumerated = ioctl.reset_usb_device(node)
        except (OSError, ValueError) as err:
            raise usb_power_hub.UsbPowerHubError(
                f"Failed to reset USB device {self._usb_bus_id}: {err}"
            ) from err
        _LOGGER.info(
            "Successfully reset USB device %s%s.",
            self._usb_bus_id,
            "; it is being re-enumerated" if reenumerated else "",
        )

    def power_off(self, port: int | None = None) -> None:
        """Not supported, as a USB reset has no "off" phase.

        Args:
            port: None. Not used by this implementation.

        Raises:
            usb_power_hub.UsbPowerHubError: Always.
        """
        raise usb_power_hub.UsbPowerHubError(
            f"{type(self).__name__} cannot power off the USB link; use reset()."
        )

    def power_on(self, port: int | None = None) -> None:
        """No-op, as this hub never leaves the USB link down.

        Args:
            port: None. Not used by this implementation.
        """
        _LOGGER.debug(
            "USB device %s is never left disconnected by %s; nothing to do.",
            self._usb_bus_id,
            type(self).__name__,
        )

    def _usb_device_node(self) -> str:
        """Returns the usbfs device node path of the USB device.

        Raises:
            OSError: If the sysfs `busnum` or `devnum` cannot be read.
            ValueError: If `busnum` or `devnum` is not an integer.
        """
        dev_dir = f"{_SYSFS_USB_DEVICES_DIR}/{self._usb_bus_id}"
        with open(f"{dev_dir}/busnum", "r", encoding="utf-8") as f:
            busnum = int(f.read().strip())
        with open(f"{dev_dir}/devnum", "r", encoding="utf-8") as f:
            devnum = int(f.read().strip())
        return f"/dev/bus/usb/{busnum:03d}/{devnum:03d}"
