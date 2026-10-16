# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared Fastboot lifecycle utilities for Fuchsia USB end-to-end tests."""

import asyncio
import logging

import fidl_fuchsia_hardware_power_statecontrol as fhp_statecontrol
from honeydew import errors
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.fastboot import types as fastboot_types
from honeydew.typing import custom_types as honeydew_types

_LOGGER: logging.Logger = logging.getLogger(__name__)


async def reboot_to_fastboot_mode(
    dut: fuchsia_device.FuchsiaDevice,
    usb_power_hub: usb_power_hub.UsbPowerHub | None = None,
    usb_port: int | None = None,
    timeout_sec: int = 30,
) -> None:
    """Reboots the DUT to bootloader/fastboot mode.

    Args:
        dut: Honeydew FuchsiaDevice instance.
        usb_power_hub: Optional USB Power Hub fixture.
        usb_port: Optional port number on the USB power hub.
        timeout_sec: Timeout in seconds for waiting for fastboot mode.
    """
    # Ensure USB power is turned ON before rebooting so bootloader sees VBUS
    if usb_power_hub:
        usb_power_hub.power_on(port=usb_port)

    try:
        if dut.fastboot.is_in_fastboot_mode():
            _LOGGER.info("Device is already in Fastboot mode.")
            return
    except Exception as e:
        _LOGGER.debug("Check is_in_fastboot_mode error: %s", e)

    _LOGGER.info("Rebooting device to bootloader via FIDL...")

    power_admin_endpoint = honeydew_types.FidlEndpoint(
        "/bootstrap/shutdown_shim",
        "fuchsia.hardware.power.statecontrol.Admin",
    )

    try:
        dut.ffx.notify_intentional_disconnect()
        fc_transport = dut.fuchsia_controller
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
            "Reboot command raised exception (expected if device rebooted "
            "quickly): %s",
            e,
        )

    _LOGGER.info("Waiting for device to enter fastboot mode...")
    dut.fastboot.wait_for_fastboot_mode(timeout=float(timeout_sec))


async def _boot_from_fastboot_to_fuchsia(
    dut: fuchsia_device.FuchsiaDevice,
    method: fastboot_types.BootToFuchsiaMethod,
    timeout_sec: int,
) -> None:
    """Boots DUT from Fastboot to Fuchsia, tolerating expected USB PHY reset
    link drops.
    """
    try:
        await dut.fastboot.boot_to_fuchsia_mode(method=method)
    except errors.FuchsiaDeviceError as err:
        err_msg = str(err.__cause__ or err).lower()
        if (
            "status read failed" in err_msg
            or "protocol error" in err_msg
            or "no error" in err_msg
            or "device not found" in err_msg
            or "write to device failed" in err_msg
            or "no such file or directory" in err_msg
        ):
            _LOGGER.info(
                "Ignoring expected fastboot USB link drop during %s: %s",
                method,
                err,
            )
            dut.fastboot.wait_for_fuchsia_mode(timeout=float(timeout_sec))
            await asyncio.wait_for(
                dut.wait_for_online(), timeout=float(timeout_sec)
            )
            await dut.on_device_boot()
        else:
            raise


async def recover_to_fuchsia_mode(
    dut: fuchsia_device.FuchsiaDevice,
    usb_power_hub: usb_power_hub.UsbPowerHub | None = None,
    usb_port: int | None = None,
    timeout_sec: int = 120,
) -> None:
    """Ensures DUT is rebooted out of Fastboot and restored back to Fuchsia.

    Args:
        dut: Honeydew FuchsiaDevice instance.
        usb_power_hub: Optional USB Power Hub fixture.
        usb_port: Optional port number on the USB power hub.
        timeout_sec: Timeout in seconds for waiting for Fuchsia mode.
    """
    _LOGGER.info("Ensuring device is booted back to Fuchsia...")
    if usb_power_hub:
        usb_power_hub.power_on(port=usb_port)

    # Allow 2s settling delay for USB enumeration and endpoints to stabilize.
    await asyncio.sleep(2)

    if dut.fastboot.is_in_fastboot_mode():
        _LOGGER.info("Device is in Fastboot mode; issuing fastboot continue...")
        try:
            await _boot_from_fastboot_to_fuchsia(
                dut, fastboot_types.BootToFuchsiaMethod.CONTINUE, timeout_sec
            )
            _LOGGER.info("Device is successfully back online in Fuchsia.")
            return
        except Exception as e:
            _LOGGER.warning(
                "fastboot continue failed (%s); attempting fallback reboot...",
                e,
            )
            await _boot_from_fastboot_to_fuchsia(
                dut, fastboot_types.BootToFuchsiaMethod.REBOOT, timeout_sec
            )
            _LOGGER.info("Device successfully rebooted to Fuchsia.")
            return

    try:
        await asyncio.wait_for(
            dut.wait_for_online(), timeout=float(timeout_sec)
        )
        await dut.on_device_boot()
        _LOGGER.info("Device is successfully back online in Fuchsia.")
    except (errors.HoneydewError, asyncio.TimeoutError) as e:
        _LOGGER.warning(
            "Device failed to come online or complete boot handling (%s); "
            "falling back to health check.",
            e,
        )
        dut.health_check()
