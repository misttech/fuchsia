# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Honeydew python module."""

import logging
import re
from collections.abc import Awaitable, Callable
from typing import Any

from honeydew import affordances_capable, errors
from honeydew.device_classes import (
    android_device,
    smart_display,
    workbench,
)
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.typing import custom_types

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Any new concrete device class added under honeydew/device_classes/ must be
# added to this dict so create_device() can resolve target products to that
# device class:
# - Key: Regex pattern matching the target's `build.product` from
#   `ffx target show --json` output (returned by `FFX.get_target_product()`).
# - Value: Device class to instantiate and return for matching products.
# LINT.IfChange
_DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT: dict[
    str, type[fuchsia_device.FuchsiaDevice]
] = {
    r"^(?!.*recovery).*(sorrel|iris|starnix)": android_device.AndroidDevice,
    r"^(?!.*recovery).*smart_display": smart_display.SmartDisplay,
    r"^(?!.*recovery).*workbench": workbench.Workbench,
}
# LINT.ThenChange(//src/testing/end_to_end/honeydew/BUILD.gn)
_REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT: dict[
    str, type[fuchsia_device.FuchsiaDevice]
] = {}


class _NoOpDeviceAddressChange(affordances_capable.FuchsiaDeviceAddressChange):
    """No-op FuchsiaDeviceAddressChange used for short-lived FFX queries prior to device creation."""

    def register_for_on_device_address_change(
        self,
        fn: Callable[[custom_types.TargetAddr], None]
        | Callable[[custom_types.TargetAddr], Awaitable[None]],
    ) -> None:
        pass


# The return type of this function can change at runtime based on target product
# and device classes registered via register_device_classes.
def create_device(
    device_info: custom_types.DeviceInfo,
    ffx_config_data: FfxConfigData,
    # intentionally made this a Dict instead of dataclass to minimize the changes in remaining Lacewing stack every time we need to add a new configuration item
    config: dict[str, Any] | None = None,
) -> fuchsia_device.FuchsiaDevice:
    """Factory method that creates and returns the device class.

    Args:
        device_info: Fuchsia device information.

        ffx_config_data: Ffx configuration that need to be used while running ffx
            commands.

        config: Honeydew device configuration, if any.
            Format:
                {
                    "transports": {
                        <transport_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                    "affordances": {
                        <affordance_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                }
            Example:
                {
                    "transports": {
                        "fuchsia_controller": {
                            "timeout": 30,
                        }
                    },
                    "affordances": {
                        "tracing": {
                            "implementation": "fuchsia-controller",
                        },
                    },
                }

    Returns:
        FuchsiaDevice or a subclass device object matching the target's product type.

    Raises:
        errors.FuchsiaDeviceError: Failed to create Fuchsia device object.
    """
    _LOGGER.debug("create_device has been called with: %s", locals())

    try:
        if device_info.address:
            _LOGGER.info(
                "CAUTION: address='%s' argument has been passed. Please "
                "make sure this value associated with the device is persistent "
                "across the reboots. Otherwise, host-target interactions will not "
                "work consistently.",
                device_info.address,
            )

        device_class: type[fuchsia_device.FuchsiaDevice] = _get_device_class(
            device_info=device_info,
            ffx_config_data=ffx_config_data,
        )
        return device_class(
            device_info=device_info,
            ffx_config_data=ffx_config_data,
            config=config,
        )
    except errors.HoneydewError as err:
        raise errors.FuchsiaDeviceError(
            f"Failed to create device for '{device_info.name}': {err}"
        ) from err


def register_device_classes(
    product_to_device_class_dict: dict[str, type[fuchsia_device.FuchsiaDevice]],
) -> None:
    """Registers custom product-to-device-class mappings with Honeydew.

    Args:
        product_to_device_class_dict: Dict mapping product regex pattern to
            device class.
    """
    _LOGGER.info(
        "Registering product to device class dict '%s' with Honeydew",
        product_to_device_class_dict,
    )
    _REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT.update(
        product_to_device_class_dict
    )


# List all the private methods in alphabetical order
def _get_device_class(
    device_info: custom_types.DeviceInfo,
    ffx_config_data: FfxConfigData,
) -> type[fuchsia_device.FuchsiaDevice]:
    """Returns device class associated with the device based on its product type.

    Device class resolution follows this priority order:
    1. Regex pattern match (`re.search`) against `_REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT`.
    2. Regex pattern match (`re.search`) against `_DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT`.
    3. Fallback to the base `FuchsiaDevice` class if no match is found.

    Args:
        device_info: Fuchsia device information.
        ffx_config_data: Ffx configuration that need to be used while running ffx
            commands.

    Returns:
        Device class type.
    """
    ffx_obj: ffx_transport.FFX = ffx_transport.FFX(
        target_name=device_info.name,
        config_data=ffx_config_data,
        target_addr=device_info.address,
        device_address_change=(
            _NoOpDeviceAddressChange() if device_info.address else None
        ),
    )
    product_type: str = ffx_obj.get_target_product()

    # Priority 1: Regex pattern match against custom registered mappings.
    if _REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT:
        _LOGGER.info(
            "Checking registered product to device class dict '%s' for '%s' (product: '%s')",
            _REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT,
            device_info.name,
            product_type,
        )
        for (
            pattern,
            device_class,
        ) in _REGISTERED_PRODUCT_TO_DEVICE_CLASS_DICT.items():
            if re.search(pattern, product_type, re.IGNORECASE):
                _LOGGER.info(
                    "Found matching registered device class implementation for '%s' (product: '%s') using pattern '%s' as '%s'",
                    device_info.name,
                    product_type,
                    pattern,
                    device_class.__name__,
                )
                return device_class

    # Priority 2: Regex pattern match against Honeydew's default mappings.
    _LOGGER.info(
        "Checking default product to device class dict '%s' for '%s' (product: '%s')",
        _DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT,
        device_info.name,
        product_type,
    )
    for (
        pattern,
        device_class,
    ) in _DEFAULT_PRODUCT_TO_DEVICE_CLASS_DICT.items():
        if re.search(pattern, product_type, re.IGNORECASE):
            _LOGGER.info(
                "Found matching default device class implementation for '%s' (product: '%s') using pattern '%s' as '%s'",
                device_info.name,
                product_type,
                pattern,
                device_class.__name__,
            )
            return device_class

    # Priority 3: Fallback to default FuchsiaDevice.
    _LOGGER.info(
        "Didn't find any matching device class implementation for '%s' (product: '%s'). "
        "Returning default '%s'",
        device_info.name,
        product_type,
        fuchsia_device.FuchsiaDevice.__name__,
    )
    return fuchsia_device.FuchsiaDevice
