# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""UserInput affordance implementation using FuchsiaController."""

import asyncio
import json

import fidl_fuchsia_input_report as f_input_report
import fidl_fuchsia_math as f_math
import fidl_fuchsia_ui_test_input as f_test_input
import fuchsia_async_extension
import fuchsia_controller_py as fcp

from honeydew import errors
from honeydew.affordances.affordance import AsyncLazyReady, ensure_ready
from honeydew.affordances.ui.user_input import errors as user_input_errors
from honeydew.affordances.ui.user_input import types as ui_custom_types
from honeydew.affordances.ui.user_input import user_input
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types

_INPUT_HELPER_COMPONENT: str = "core/ui/input-helper"


class _FcProxies:
    INPUT_REGISTRY: custom_types.FidlEndpoint = custom_types.FidlEndpoint(
        "/core/ui", "fuchsia.ui.test.input.Registry"
    )


class AsyncTouchDeviceUsingFc(user_input.AsyncTouchDevice, AsyncLazyReady):
    """Virtual TouchDevice for testing using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        UserInputError: if failed to create virtual touch device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        touch_screen_size: ui_custom_types.Size,
    ) -> None:
        super().__init__()
        self._device_name = device_name
        self._fuchsia_controller = fuchsia_controller
        self._touch_screen_size = touch_screen_size
        self._touch_screen_proxy: f_test_input.TouchScreenClient | None = None

    async def make_ready(self) -> None:
        await super().make_ready()
        (
            channel_server,
            channel_client,
        ) = self._fuchsia_controller.channel_create()

        try:
            input_registry_proxy = f_test_input.RegistryClient(
                self._fuchsia_controller.connect_device_proxy(
                    _FcProxies.INPUT_REGISTRY
                )
            )
            await input_registry_proxy.register_touch_screen(
                device=channel_server.take(),
                coordinate_unit=f_test_input.CoordinateUnit.PHYSICAL_PIXELS,
                display_dimensions=f_test_input.DisplayDimensions(
                    0,
                    0,
                    self._touch_screen_size.width,
                    self._touch_screen_size.height,
                ),
            )
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"Failed to initialize touch device on {self._device_name}"
            ) from status

        self._touch_screen_proxy = f_test_input.TouchScreenClient(
            channel_client
        )

    @ensure_ready
    async def tap(
        self,
        location: ui_custom_types.Coordinate,
        tap_event_count: int = user_input.DEFAULTS["TAP_EVENT_COUNT"],
        duration_ms: int = user_input.DEFAULTS["TAP_DURATION_MS"],
        duration_of_one_tap_ms: int = user_input.DEFAULTS[
            "ONE_TAP_DURATION_MS"
        ],
    ) -> None:
        """Instantiates Taps at coordinates (x, y) for a touchscreen."""
        assert self._touch_screen_proxy is not None

        try:
            interval: float = duration_ms / tap_event_count

            for _ in range(tap_event_count):
                await self._touch_screen_proxy.simulate_touch_event(
                    report=f_input_report.TouchInputReport(
                        contacts=[
                            f_input_report.ContactInputReport(
                                contact_id=1,
                                position_x=location.x,
                                position_y=location.y,
                            ),
                        ],
                    ),
                )

                await asyncio.sleep(duration_of_one_tap_ms / 1000)

                await self._touch_screen_proxy.simulate_touch_event(
                    report=f_input_report.TouchInputReport(
                        contacts=[],
                    ),
                )

                await asyncio.sleep(
                    interval / 1000 - duration_of_one_tap_ms / 1000
                )  # Sleep in seconds

        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"tap operation failed on {self._device_name}"
            ) from status

    @ensure_ready
    async def swipe(
        self,
        start_location: ui_custom_types.Coordinate,
        end_location: ui_custom_types.Coordinate,
        move_event_count: int,
        duration_ms: int = user_input.DEFAULTS["SWIPE_DURATION_MS"],
    ) -> None:
        """Instantiates a swipe event sequence."""
        assert self._touch_screen_proxy is not None

        try:
            await self._touch_screen_proxy.simulate_swipe(
                start_location=f_math.Vec(
                    x=start_location.x, y=start_location.y
                ),
                end_location=f_math.Vec(x=end_location.x, y=end_location.y),
                move_event_count=move_event_count,
                duration=duration_ms * 1000000,  # milliseconds to nanoseconds
            )
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"swipe operation failed on {self._device_name}"
            ) from status


class AsyncMouseDeviceUsingFc(user_input.AsyncMouseDevice, AsyncLazyReady):
    """Virtual MouseDevice for testing using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        UserInputError: if failed to create virtual mouse device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        super().__init__()
        self._device_name = device_name
        self._fuchsia_controller = fuchsia_controller
        self._mouse_proxy: f_test_input.MouseClient | None = None

    async def make_ready(self) -> None:
        await super().make_ready()
        (
            channel_server,
            channel_client,
        ) = self._fuchsia_controller.channel_create()

        try:
            input_registry_proxy = f_test_input.RegistryClient(
                self._fuchsia_controller.connect_device_proxy(
                    _FcProxies.INPUT_REGISTRY
                )
            )
            await input_registry_proxy.register_mouse(
                device=channel_server.take(),
            )
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"Failed to initialize mouse device on {self._device_name}"
            ) from status

        self._mouse_proxy = f_test_input.MouseClient(channel_client)

    @ensure_ready
    async def scroll(
        self,
        scroll_v_detent: int = 0,
        scroll_h_detent: int = 0,
    ) -> None:
        """Instantiates a scroll event."""
        assert self._mouse_proxy is not None
        try:
            await self._mouse_proxy.simulate_mouse_event(
                scroll_v_detent=scroll_v_detent,
                scroll_h_detent=scroll_h_detent,
            )
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"scroll operation failed on {self._device_name}"
            ) from status


class TouchDeviceUsingFc(user_input.TouchDevice):
    """Virtual TouchDevice wrapper."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        touch_screen_size: ui_custom_types.Size,
        inner: AsyncTouchDeviceUsingFc | None = None,
    ) -> None:
        self._inner = inner or AsyncTouchDeviceUsingFc(
            device_name=device_name,
            fuchsia_controller=fuchsia_controller,
            touch_screen_size=touch_screen_size,
        )
        if not self._inner._ready:  # pylint: disable=protected-access
            fuchsia_async_extension.get_loop().run_until_complete(
                self._inner.make_ready()
            )

    def tap(
        self,
        location: ui_custom_types.Coordinate,
        tap_event_count: int = user_input.DEFAULTS["TAP_EVENT_COUNT"],
        duration_ms: int = user_input.DEFAULTS["TAP_DURATION_MS"],
        duration_of_one_tap_ms: int = user_input.DEFAULTS[
            "ONE_TAP_DURATION_MS"
        ],
    ) -> None:
        """Instantiates Taps at coordinates (x, y) for a touchscreen."""
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.tap(
                location, tap_event_count, duration_ms, duration_of_one_tap_ms
            )
        )

    def swipe(
        self,
        start_location: ui_custom_types.Coordinate,
        end_location: ui_custom_types.Coordinate,
        move_event_count: int,
        duration_ms: int = user_input.DEFAULTS["SWIPE_DURATION_MS"],
    ) -> None:
        """Instantiates a swipe event sequence."""
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.swipe(
                start_location, end_location, move_event_count, duration_ms
            )
        )

    def as_async(self) -> AsyncTouchDeviceUsingFc:
        """Returns the async version of TouchDevice."""
        return self._inner


class AsyncKeyboardDeviceUsingFc(
    user_input.AsyncKeyboardDevice, AsyncLazyReady
):
    """Virtual KeyboardDevice for testing using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        UserInputError: if failed to create virtual keyboard device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        super().__init__()
        self._device_name = device_name
        self._fuchsia_controller = fuchsia_controller
        self._keyboard_proxy: f_test_input.KeyboardClient | None = None

    async def make_ready(self) -> None:
        await super().make_ready()
        (
            channel_server,
            channel_client,
        ) = self._fuchsia_controller.channel_create()

        try:
            input_registry_proxy = f_test_input.RegistryClient(
                self._fuchsia_controller.connect_device_proxy(
                    _FcProxies.INPUT_REGISTRY
                )
            )
            await input_registry_proxy.register_keyboard(
                device=channel_server.take(),
            )
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"Failed to initialize keyboard device on {self._device_name}"
            ) from status

        self._keyboard_proxy = f_test_input.KeyboardClient(channel_client)

    @ensure_ready
    async def key_press(
        self,
        key_code: int,
    ) -> None:
        """Instantiates key press includes down and up."""
        assert self._keyboard_proxy is not None
        try:
            await self._keyboard_proxy.simulate_key_press(key_code=key_code)
        except fcp.FcTransportStatus as status:
            raise user_input_errors.UserInputError(
                f"key press operation failed on {self._device_name}"
            ) from status


class KeyboardDeviceUsingFc(user_input.KeyboardDevice):
    """Virtual KeyboardDevice wrapper."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        inner: AsyncKeyboardDeviceUsingFc | None = None,
    ) -> None:
        self._inner = inner or AsyncKeyboardDeviceUsingFc(
            device_name=device_name, fuchsia_controller=fuchsia_controller
        )
        if not self._inner._ready:  # pylint: disable=protected-access
            fuchsia_async_extension.get_loop().run_until_complete(
                self._inner.make_ready()
            )

    def key_press(
        self,
        key_code: int,
    ) -> None:
        """Instantiates key press includes down and up."""
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.key_press(key_code)
        )

    def as_async(self) -> AsyncKeyboardDeviceUsingFc:
        """Returns the async version of KeyboardDevice."""
        return self._inner


class MouseDeviceUsingFc(user_input.MouseDevice):
    """Virtual MouseDevice wrapper."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        inner: AsyncMouseDeviceUsingFc | None = None,
    ) -> None:
        self._inner = inner or AsyncMouseDeviceUsingFc(
            device_name=device_name, fuchsia_controller=fuchsia_controller
        )
        if not self._inner._ready:  # pylint: disable=protected-access
            fuchsia_async_extension.get_loop().run_until_complete(
                self._inner.make_ready()
            )

    def scroll(
        self,
        scroll_v_detent: int = 0,
        scroll_h_detent: int = 0,
    ) -> None:
        """Instantiates a scroll event."""
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.scroll(scroll_v_detent, scroll_h_detent)
        )

    def as_async(self) -> AsyncMouseDeviceUsingFc:
        """Returns the async version of MouseDevice."""
        return self._inner


class AsyncUserInputUsingFc(user_input.AsyncUserInput):
    """Async UserInput affordance implementation using FuchsiaController."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        ffx_transport: ffx.FFX,
    ) -> None:
        self._device_name = device_name
        self._fc_transport: fc_transport.FuchsiaController = fuchsia_controller
        self._ffx_transport: ffx.FFX = ffx_transport

        self.verify_supported()

    def _is_moniker_present(self, target_moniker: str) -> bool:
        """Determines if a target moniker is present."""
        components_json = self._ffx_transport.run(["component", "list"])
        data = json.loads(components_json)
        instances = data.get("instances", [])
        return any(
            instance.get("moniker") == target_moniker for instance in instances
        )

    def verify_supported(self) -> None:
        """Check if User Input affordance is supported on the DUT."""
        if not self._is_moniker_present(_INPUT_HELPER_COMPONENT):
            raise errors.NotSupportedError(
                f"{_INPUT_HELPER_COMPONENT} is not available in device {self._device_name}"
            )

    def create_touch_device(
        self,
        touch_screen_size: ui_custom_types.Size = user_input.DEFAULTS[
            "TOUCH_SCREEN_SIZE"
        ],
    ) -> AsyncTouchDeviceUsingFc:
        """Create a virtual touch device for testing touch input."""
        return AsyncTouchDeviceUsingFc(
            device_name=self._device_name,
            fuchsia_controller=self._fc_transport,
            touch_screen_size=touch_screen_size,
        )

    def create_keyboard_device(self) -> AsyncKeyboardDeviceUsingFc:
        """Create a virtual keyboard device for testing keyboard input."""
        return AsyncKeyboardDeviceUsingFc(
            device_name=self._device_name, fuchsia_controller=self._fc_transport
        )

    def create_mouse_device(self) -> AsyncMouseDeviceUsingFc:
        """Create a virtual mouse device for testing mouse input."""
        return AsyncMouseDeviceUsingFc(
            device_name=self._device_name, fuchsia_controller=self._fc_transport
        )


class UserInputUsingFc(user_input.UserInput):
    """UserInput affordance implementation using FuchsiaController."""

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        ffx_transport: ffx.FFX,
    ) -> None:
        self._inner = AsyncUserInputUsingFc(
            device_name=device_name,
            fuchsia_controller=fuchsia_controller,
            ffx_transport=ffx_transport,
        )

    def verify_supported(self) -> None:
        """Check if User Input affordance is supported on the DUT."""
        self._inner.verify_supported()

    def create_touch_device(
        self,
        touch_screen_size: ui_custom_types.Size = user_input.DEFAULTS[
            "TOUCH_SCREEN_SIZE"
        ],
    ) -> TouchDeviceUsingFc:
        """Create a virtual touch device wrapper."""
        async_device = self._inner.create_touch_device(touch_screen_size)
        return TouchDeviceUsingFc(
            device_name=self._inner._device_name,  # pylint: disable=protected-access
            fuchsia_controller=self._inner._fc_transport,  # pylint: disable=protected-access
            touch_screen_size=touch_screen_size,
            inner=async_device,
        )

    def create_keyboard_device(self) -> KeyboardDeviceUsingFc:
        """Create a virtual keyboard device wrapper."""
        async_device = self._inner.create_keyboard_device()
        return KeyboardDeviceUsingFc(
            device_name=self._inner._device_name,  # pylint: disable=protected-access
            fuchsia_controller=self._inner._fc_transport,  # pylint: disable=protected-access
            inner=async_device,
        )

    def create_mouse_device(self) -> MouseDeviceUsingFc:
        """Create a virtual mouse device wrapper."""
        async_device = self._inner.create_mouse_device()
        return MouseDeviceUsingFc(
            device_name=self._inner._device_name,  # pylint: disable=protected-access
            fuchsia_controller=self._inner._fc_transport,  # pylint: disable=protected-access
            inner=async_device,
        )

    def as_async(self) -> AsyncUserInputUsingFc:
        """Returns the async version of UserInput."""
        return self._inner
