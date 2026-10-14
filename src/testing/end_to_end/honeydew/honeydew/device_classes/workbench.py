# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Workbench device class implementation."""

from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_usb_cli,
)
from honeydew.fuchsia_device import fuchsia_device
from honeydew.utils import properties


class Workbench(fuchsia_device.FuchsiaDevice):
    """Workbench device class implementation."""

    @properties.Affordance
    def system_power_state_controller(
        self,
    ) -> system_power_state_controller_interface.SystemPowerStateController:
        """Returns a system_power_state_controller affordance object.

        Returns:
            system_power_state_controller.SystemPowerStateController object

        Raises:
            errors.NotSupportedError: If Workbench device does not support
                usb-cli.
        """
        return system_power_state_controller_using_usb_cli.SystemPowerStateControllerUsingUsbCli(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            device_logger=self,
            inspect=self,
            reboot_affordance=self,
        )
