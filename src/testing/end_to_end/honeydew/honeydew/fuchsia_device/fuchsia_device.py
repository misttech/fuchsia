# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FuchsiaDevice abstract base class implementation."""


import inspect
import logging
from collections.abc import Callable
from typing import Any, Coroutine

import fidl_fuchsia_diagnostics_types as f_diagnostics_types
import fuchsia_async_extension
import fuchsia_inspect
from fuchsia_controller_py.wrappers import BoundAsyncMethod

from honeydew import affordances_capable
from honeydew.affordances.connectivity.bluetooth.avrcp import avrcp
from honeydew.affordances.connectivity.bluetooth.gap import gap
from honeydew.affordances.connectivity.bluetooth.le import le
from honeydew.affordances.connectivity.netstack import (
    netstack,
    netstack_using_fc,
)
from honeydew.affordances.connectivity.wlan.wlan_core import (
    wlan_core,
    wlan_core_using_fc,
)
from honeydew.affordances.connectivity.wlan.wlan_policy import wlan_policy
from honeydew.affordances.connectivity.wlan.wlan_policy_ap import wlan_policy_ap
from honeydew.affordances.device_knobs import device_knobs
from honeydew.affordances.hello_world import hello_world
from honeydew.affordances.location import location
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.rtc import rtc, rtc_using_fc
from honeydew.affordances.session import session
from honeydew.affordances.starnix import starnix
from honeydew.affordances.tracing import tracing, tracing_using_fc
from honeydew.affordances.ui.screenshot import screenshot
from honeydew.affordances.ui.user_input import user_input, user_input_using_fc
from honeydew.affordances.virtual_audio import audio
from honeydew.auxiliary_devices.power_switch import (
    power_switch as power_switch_interface,
)
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub as usb_power_hub_interface,
)
from honeydew.fuchsia_device.async_fuchsia_device import AsyncFuchsiaDevice
from honeydew.transports.fastboot import (
    fastboot as fastboot_transport_interface,
)
from honeydew.transports.ffx import ffx as ffx_transport_interface
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fuchsia_controller_transport_interface,
)
from honeydew.transports.serial import serial as serial_transport_interface
from honeydew.transports.sl4f import sl4f as sl4f_transport_interface
from honeydew.typing import custom_types
from honeydew.utils import properties

_FC_PROXIES: dict[str, custom_types.FidlEndpoint] = {
    "BuildInfo": custom_types.FidlEndpoint(
        "/core/build-info", "fuchsia.buildinfo.Provider"
    ),
    "DeviceInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Device"
    ),
    "Feedback": custom_types.FidlEndpoint(
        "/core/feedback", "fuchsia.feedback.DataProvider"
    ),
    "LastRebootInfo": custom_types.FidlEndpoint(
        "/core/feedback", "fuchsia.feedback.LastRebootInfoProvider"
    ),
    "ProductInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Product"
    ),
    "PowerAdmin": custom_types.FidlEndpoint(
        "/bootstrap/shutdown_shim", "fuchsia.hardware.power.statecontrol.Admin"
    ),
    "RemoteControl": custom_types.FidlEndpoint(
        "/core/remote-control", "fuchsia.developer.remotecontrol.RemoteControl"
    ),
}

_FFX_CMDS: dict[str, list[str]] = {
    "RESOLVE_IP": [
        "target",
        "list",
        "--format",
        "addresses",
        "--no-usb",  # do not do USB discovery
        "--no-probe",  # do not connect to targets
    ],
}

_LOG_SEVERITIES: dict[custom_types.LEVEL, f_diagnostics_types.Severity] = {
    custom_types.LEVEL.INFO: f_diagnostics_types.Severity.INFO,
    custom_types.LEVEL.WARNING: f_diagnostics_types.Severity.WARN,
    custom_types.LEVEL.ERROR: f_diagnostics_types.Severity.ERROR,
}

_LOGGER: logging.Logger = logging.getLogger(__name__)


class FuchsiaDevice(
    device_knobs.DeviceKnobs,
    affordances_capable.RebootCapableDevice,
    affordances_capable.FuchsiaDeviceLogger,
    affordances_capable.FuchsiaDeviceClose,
    affordances_capable.InspectCapableDevice,
    affordances_capable.FuchsiaDeviceIpChange,
):
    """Class that provides access to an assortment of capabilities available
    on a Fuchsia device.

    Args:
        device_info: Fuchsia device information.
        ffx_config_data: Config that need to be used while running FFX commands.
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
                        "bluetooth": {
                            "implementation": "fuchsia-controller",
                        },
                        "wlan": {
                            "implementation": "sl4f",
                        }
                    },
                }

    Raises:
        FFXCommandError: if FFX connection check fails.
        FuchsiaControllerError: if FC connection check fails.
    """

    def __init__(
        self,
        *,
        device_info: custom_types.DeviceInfo,
        ffx_config_data: FfxConfigData,
        # intentionally made this a Dict instead of dataclass to minimize the changes in remaining Lacewing stack every time we need to add a new configuration item
        config: dict[str, Any] | None = None,
    ) -> None:
        _LOGGER.debug("Initializing FuchsiaDevice")

        self._inner = AsyncFuchsiaDevice(
            _outer=self,
            device_info=device_info,
            ffx_config_data=ffx_config_data,
            config=config,
        )

        _LOGGER.debug("Initialized FuchsiaDevice")

    @properties.PersistentProperty
    def board(self) -> str:
        return self._inner.board

    @properties.PersistentProperty
    def device_name(self) -> str:
        return self._inner.device_name

    @properties.PersistentProperty
    def manufacturer(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.manufacturer()
        )

    @properties.PersistentProperty
    def model(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.model()
        )

    @properties.PersistentProperty
    def product(self) -> str:
        return self._inner.product

    @properties.PersistentProperty
    def product_name(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.product_name()
        )

    @properties.PersistentProperty
    def serial_number(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.serial_number()
        )

    @properties.DynamicProperty
    def firmware_version(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.firmware_version()
        )

    @properties.DynamicProperty
    def last_reboot_reason(self) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.last_reboot_reason()
        )

    @properties.Transport
    def ffx(self) -> ffx_transport_interface.FFX:
        return self._inner.ffx

    @properties.Transport
    def fuchsia_controller(
        self,
    ) -> fuchsia_controller_transport_interface.FuchsiaController:
        return self._inner.fuchsia_controller

    @properties.Transport
    def fastboot(self) -> fastboot_transport_interface.Fastboot:
        return self._inner.fastboot

    @properties.Transport
    def serial(self) -> serial_transport_interface.Serial:
        return self._inner.serial

    @properties.Transport
    def sl4f(self) -> sl4f_transport_interface.SL4F:
        return self._inner.sl4f

    @properties.Affordance
    def session(self) -> session.Session:
        return self._inner.session

    @properties.Affordance
    def screenshot(self) -> screenshot.Screenshot:
        return self._inner.screenshot

    @properties.Affordance
    def virtual_audio(self) -> audio.VirtualAudio:
        return self._inner.virtual_audio

    @properties.Affordance
    def starnix(self) -> starnix.Starnix:
        return self._inner.starnix

    @properties.Affordance
    def system_power_state_controller(
        self,
    ) -> system_power_state_controller_interface.SystemPowerStateController:
        return self._inner.system_power_state_controller

    @properties.Affordance
    def rtc(self) -> rtc.Rtc:
        """Returns an RTC affordance object.

        Returns:
            rtc.AsyncRtc object
        """
        return rtc_using_fc.RtcUsingFc(
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def tracing(self) -> tracing.Tracing:
        return tracing_using_fc.TracingUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def user_input(self) -> user_input.UserInput:
        return user_input_using_fc.UserInputUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            ffx_transport=self.ffx,
        )

    @properties.Affordance
    def bluetooth_avrcp(self) -> avrcp.Avrcp:
        return self._inner.bluetooth_avrcp

    @properties.Affordance
    def bluetooth_le(self) -> le.LE:
        return self._inner.bluetooth_le

    @properties.Affordance
    def bluetooth_gap(self) -> gap.Gap:
        return self._inner.bluetooth_gap

    @properties.Affordance
    def wlan_policy(self) -> wlan_policy.WlanPolicy:
        return self._inner.wlan_policy

    @properties.Affordance
    def wlan_policy_ap(self) -> wlan_policy_ap.WlanPolicyAp:
        return self._inner.wlan_policy_ap

    @properties.Affordance
    def wlan_core(self) -> wlan_core.WlanCore:
        return wlan_core_using_fc.WlanCore(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def netstack(self) -> netstack.Netstack:
        return netstack_using_fc.NetstackUsingFc(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def location(self) -> location.Location:
        return self._inner.location

    @properties.Affordance
    def hello_world(self) -> hello_world.HelloWorld:
        return self._inner.hello_world

    def close(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.close()
        )

    def health_check(self) -> None:
        self._inner.health_check()

    def get_inspect_data(
        self,
        selectors: list[str] | None = None,
        monikers: list[str] | None = None,
    ) -> fuchsia_inspect.InspectDataCollection:
        return self._inner.get_inspect_data(selectors, monikers)

    def log_message_to_device(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.log_message_to_device(message, level)
        )

    def on_device_boot(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.on_device_boot()
        )

    def power_cycle(
        self,
        power_switch: power_switch_interface.PowerSwitch,
        outlet: int | None = None,
    ) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.power_cycle(power_switch, outlet)
        )

    def register_on_device_suspend_fn(
        self,
        fn: Callable[[], None] | Callable[[], Coroutine[Any, Any, None]],
    ) -> None:
        """Register a function to be called when device is suspended.

        Args:
            fn: Function to be called when device is suspended.
        """
        if inspect.iscoroutinefunction(fn):
            self._inner.register_on_device_suspend_fn(fn)
        # TODO(https://fxbug.dev/488299605): For the simple case when the
        # outermost wrapper is @asyncmethod, this suffices.
        elif isinstance(fn, BoundAsyncMethod):
            self._inner.register_on_device_suspend_fn(
                fn.unwrap_from_asyncmethod()
            )
        else:
            self._inner.register_on_device_suspend_fn(fn)

    def register_on_device_resume_fn(
        self,
        fn: Callable[[], None] | Callable[[], Coroutine[Any, Any, None]],
    ) -> None:
        """Register a function to be called when device is resumed.

        Args:
            fn: Function to be called when device is resumed.
        """
        if inspect.iscoroutinefunction(fn):
            self._inner.register_on_device_resume_fn(fn)
        # TODO(https://fxbug.dev/488299605): For the simple case when the
        # outermost wrapper is @asyncmethod, this suffices.
        elif isinstance(fn, BoundAsyncMethod):
            self._inner.register_on_device_resume_fn(
                fn.unwrap_from_asyncmethod()
            )
        else:
            self._inner.register_on_device_resume_fn(fn)

    def set_usb_power_hub(
        self,
        usb_power_hub: usb_power_hub_interface.UsbPowerHub,
        port: int | None = None,
    ) -> None:
        """Set USB power hub for device.

        Args:
            usb_power_hub: Implementation of UsbPowerHub interface.
            port (int | None): If required by USB power hub hardware, port on
                USB power hub hardware where this fuchsia device is connected.
        """
        self._inner.set_usb_power_hub(usb_power_hub, port)

    def suspend(self) -> None:
        """Suspend the device by disconnecting USB power.

        Requires USB power hub to be set using set_usb_power_hub. This will
        run all registered on_device_suspend_fns before disconnecting USB
        power. Note that this does not guarantee the device actually
        suspends, just that it will have the opportunity to.

        Raises:
            NotSupportedError: If USB power hub not set.
        """
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.suspend()
        )

    def resume(self) -> None:
        """Resume the device by reconnecting USB power.

        Requires USB power hub to be set using set_usb_power_hub. This will
        run all registered on_device_resume_fns after reconnecting USB power.

        Raises:
            NotSupportedError: If USB power hub not set.
        """
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.resume()
        )

    def reboot(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.reboot()
        )

    def register_for_on_device_boot(
        self, fn: Callable[[], None] | Callable[[], Coroutine[Any, Any, None]]
    ) -> None:
        if inspect.iscoroutinefunction(fn):
            self._inner.register_for_on_device_boot(fn)
        # TODO(https://fxbug.dev/488299605): For the simple case when the
        # outermost wrapper is @asyncmethod, this suffices.
        elif isinstance(fn, BoundAsyncMethod):
            self._inner.register_for_on_device_boot(
                fn.unwrap_from_asyncmethod()
            )
        else:
            self._inner.register_for_on_device_boot(fn)

    def register_for_on_device_close(
        self, fn: Callable[[], None] | Callable[[], Coroutine[Any, Any, None]]
    ) -> None:
        if inspect.iscoroutinefunction(fn):
            self._inner.register_for_on_device_close(fn)
        # TODO(https://fxbug.dev/488299605): For the simple case when the
        # outermost wrapper is @asyncmethod, this suffices.
        elif isinstance(fn, BoundAsyncMethod):
            self._inner.register_for_on_device_close(
                fn.unwrap_from_asyncmethod()
            )
        else:
            self._inner.register_for_on_device_close(fn)

    def resolve_device_ip(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.resolve_device_ip()
        )

    def register_for_on_device_ip_change(
        self,
        fn: Callable[[custom_types.IpPort], None]
        | Callable[[custom_types.IpPort], Coroutine[Any, Any, None]],
    ) -> None:
        if inspect.iscoroutinefunction(fn):
            self._inner.register_for_on_device_ip_change(fn)
        # TODO(https://fxbug.dev/488299605): For the simple case when the
        # outermost wrapper is @asyncmethod, this suffices.
        elif isinstance(fn, BoundAsyncMethod):
            self._inner.register_for_on_device_ip_change(
                fn.unwrap_from_asyncmethod()
            )
        else:
            self._inner.register_for_on_device_ip_change(fn)

    def snapshot(self, directory: str, snapshot_file: str | None = None) -> str:
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.snapshot(directory, snapshot_file)
        )

    def wait_for_offline(self) -> None:
        self._inner.wait_for_offline()

    def wait_for_online(self) -> None:
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.wait_for_online()
        )

    def is_starnix_device(self) -> bool:
        return self._inner.is_starnix_device()

    def as_async(self) -> "AsyncFuchsiaDevice":
        return self._inner
