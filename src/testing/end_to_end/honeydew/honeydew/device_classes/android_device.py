# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidDevice device class implementation.

This layer owns all device-facing Android interactions in Starnix, keeping
product variations isolated from test logic while reusing FuchsiaDevice
capabilities (including self.adb) directly via inheritance.
"""

import contextlib
import ipaddress
import logging
import re
import types
from pathlib import Path
from typing import Any

from honeydew import errors
from honeydew.affordances.android_diagnostics import (
    android_diagnostics as android_diagnostics_affordance,
)
from honeydew.affordances.android_package import (
    android_package as android_package_affordance,
)
from honeydew.affordances.android_session import (
    android_session as android_session_affordance,
)
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.typing import custom_types
from honeydew.utils import properties

_LOGGER: logging.Logger = logging.getLogger(__name__)


# TODO(b/564946370): Remove temporary wrapper once 'adb root' operations are
# stable on USB.
class _SuRootContextManager(
    contextlib.AbstractContextManager[None],
    contextlib.AbstractAsyncContextManager[None],
):
    """Wrapper around use_su_root supporting both sync and async contexts."""

    def __init__(self, cm: contextlib.AbstractContextManager[None]) -> None:
        self._cm = cm

    def __enter__(self) -> None:
        return self._cm.__enter__()

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> bool | None:
        return self._cm.__exit__(exc_type, exc_val, exc_tb)

    async def __aenter__(self) -> None:
        return self.__enter__()

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> bool | None:
        return self.__exit__(exc_type, exc_val, exc_tb)


def _parse_screen_size(screen_size: Any) -> ui_types.Size | None:
    """Parses a "widthxheight" screen size string (e.g. "480x800").

    Args:
        screen_size: Screen size string, or None if not provided.

    Returns:
        Parsed screen size, or None if `screen_size` is None.

    Raises:
        ValueError: If `screen_size` is not in "widthxheight" format.
    """
    if screen_size is None:
        return None
    try:
        width_str, height_str = screen_size.split("x")
        return ui_types.Size(int(width_str), int(height_str))
    except (ValueError, AttributeError) as e:
        raise ValueError(
            f"Invalid screen_size parameter: {screen_size!r}. "
            "Expected format 'widthxheight' (e.g., '480x800')."
        ) from e


class AndroidDevice(fuchsia_device.FuchsiaDevice):
    """Base Android-in-Starnix device inheriting from FuchsiaDevice."""

    # Product profile class attributes - overridden by product specializations.
    sysui_package: str = ""
    launcher_package: str = ""
    setup_wizard_package: str = ""
    adb_auth_activity: str = ""
    idle_screen_resource_id: str = ""
    apps_list_resource_id: str = ""
    apps_list_activity: str | None = None
    standardize_screenshots: bool = False

    _default_screen_size: ui_types.Size | None = None

    def __init__(
        self,
        *,
        device_info: custom_types.DeviceInfo,
        ffx_config_data: FfxConfigData,
        config: dict[str, Any] | None = None,
    ) -> None:
        self._screen_size_override: ui_types.Size | None = _parse_screen_size(
            (config or {}).get("screen_size")
        )
        self._screen_size_cache: ui_types.Size | None = None
        super().__init__(
            device_info=device_info,
            ffx_config_data=ffx_config_data,
            config=config,
        )
        self.initialize()

    def _is_pontis_target(self) -> bool:
        """Returns True if the target device is connected via Pontis."""
        target_addr: custom_types.TargetAddr | None = self._device_info.ip_port
        if target_addr is None:
            try:
                target_addr = self.ffx.get_target_address()
            except Exception as err:
                _LOGGER.debug(
                    "Failed to get target address from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                return False

        if isinstance(target_addr, custom_types.IpPort) and isinstance(
            target_addr.ip, ipaddress.IPv6Address
        ):
            scope_id: str | None = target_addr.ip.scope_id
            if scope_id is not None and "pontis" in scope_id.lower():
                return True

        return False

    @properties.Transport
    def adb(self) -> adb_transport.Adb:
        """Returns the ADB transport object configured to use su_root methods.

        Note: Returns a modified Adb instance where root/unroot methods (root,
        unroot, and use_adb_root) are redirected to their 'su' counterparts
        (su_root, su_unroot, and use_su_root) as a temporary workaround for
        Starnix (b/564946370).

        Returns:
            ADB transport interface implementation.

        Raises:
            errors.NotEnabledError: If ADB transport is not enabled.
            errors.NotSupportedError: If ADB transport is not supported by Fuchsia device.
        """
        run_isolated_server = True
        vendor_keys_path = None
        enabled = True
        output_dir: Path | str | None = None
        attempts = adb_transport._DEFAULT_RUN_ATTEMPTS

        if self._config:
            adb_config = self._config.get("transports", {}).get("adb", {})
            run_isolated_server = adb_config.get("run_isolated_server", True)
            vendor_keys_path = adb_config.get("vendor_keys_path")
            enabled = adb_config.get("enabled", True)
            output_dir = adb_config.get("output_dir")
            attempts = adb_config.get(
                "attempts", adb_transport._DEFAULT_RUN_ATTEMPTS
            )

        if not output_dir and self._ffx_config_data.logs_dir:
            ffx_logs_path = Path(self._ffx_config_data.logs_dir)
            if ffx_logs_path.name == "ffx":
                output_dir = ffx_logs_path.parent / "adb"
            else:
                output_dir = ffx_logs_path / "adb"

        # Note - An existing ADB implementation in //vendor/google is used by
        # some Lacewing tests. Running two ADB server implementations against
        # the same device causes conflicts. Tests that manage ADB externally
        # can set `enabled` to False to disable Honeydew's ADB transport.
        # TODO(b/559563915): Delete this `enabled` config once legacy ADB
        # server implementations are removed and all Lacewing tests use
        # Honeydew's ADB transport.
        if not enabled:
            _LOGGER.info(
                "ADB transport is not enabled for '%s'",
                self.device_name,
            )
            raise errors.NotEnabledError(
                f"ADB transport is not enabled for '{self.device_name}'"
            )

        # TODO(b/572903114): Enable ADB on Pontis targets once ADB-over-TCP is
        # supported.
        if self._is_pontis_target():
            _LOGGER.info(
                "ADB transport is disabled for '%s' as it is connected via Pontis",
                self.device_name,
            )
            raise errors.NotEnabledError(
                f"ADB transport is disabled for '{self.device_name}' as it is connected via Pontis"
            )

        serial_number: str | None = self._device_info.serial_number
        if serial_number is None:
            try:
                serial_number = self.ffx.serial_number
            except errors.HoneydewError as err:
                _LOGGER.debug(
                    "Failed to get serial number from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                serial_number = None

        if serial_number is None:
            _LOGGER.warning(
                "ADB transport is not supported on %s as 'serial_number' was not provided and could not be retrieved via FFX",
                self.device_name,
            )
            raise errors.NotSupportedError(
                f"ADB transport is not supported on {self.device_name} "
                f"as 'serial_number' was not provided and could not be retrieved via FFX"
            )

        adb_obj: adb_transport.Adb = adb_transport.Adb(
            device_name=self.device_name,
            serial_number=serial_number,
            run_isolated_server=run_isolated_server,
            output_dir=output_dir,
            vendor_keys_path=vendor_keys_path,
            ffx_transport=self.ffx,
            attempts=attempts,
        )
        # TODO(b/564946370): Remove temporary workaround once 'adb root' is
        # stable on USB connections.
        adb_obj.root = adb_obj.su_root  # type: ignore[method-assign]
        adb_obj.unroot = adb_obj.su_unroot  # type: ignore[method-assign]

        def _use_su_root(*args: Any, **kwargs: Any) -> _SuRootContextManager:
            return _SuRootContextManager(adb_obj.use_su_root(*args, **kwargs))

        adb_obj.use_adb_root = _use_su_root  # type: ignore[method-assign,assignment]
        self.register_for_on_device_close(adb_obj.close)
        return adb_obj

    def _check_connections(self) -> None:
        """Checks health of all device transports including ADB."""
        super()._check_connections()

        try:
            self.adb.check_connection()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping the ADB connection check.",
                self.device_name,
            )

    async def on_device_boot(self) -> None:
        """Take actions after the device is rebooted.

        Raises:
            FuchsiaControllerError: On communications failure.
        """
        try:
            self.adb.on_device_boot()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping ADB on_device_boot.",
                self.device_name,
            )

        await super().on_device_boot()

    def initialize(self) -> None:
        """Initializes the Android device connection and product state."""
        if not self.sysui_package:
            return
        try:
            self.screen_size()
            self._product_setup()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping Android initialization.",
                self.device_name,
            )

    def _product_setup(self) -> None:
        """Product-specific setup hook, no-op by default."""

    def _on_screen_size_changed(self) -> None:
        """Hook called when cached screen size changes."""

    def screen_size(self) -> ui_types.Size:
        """Returns the screen size for this device.

        If 'screen_size' was provided in config, it must be a string in the
        form "widthxheight" (e.g., "480x800") and takes precedence.
        """
        if self._screen_size_override is not None:
            return self._screen_size_override

        if self._screen_size_cache is not None:
            return self._screen_size_cache

        try:
            output = self.adb.run(["shell", "wm", "size"])
            override_match = re.search(r"Override size:\s*(\d+)x(\d+)", output)
            physical_match = re.search(r"Physical size:\s*(\d+)x(\d+)", output)
            match = override_match or physical_match
            if match:
                new_size = ui_types.Size(
                    int(match.group(1)), int(match.group(2))
                )
                self._screen_size_cache = new_size
                self._on_screen_size_changed()
                return new_size
        except errors.HoneydewError as e:
            _LOGGER.warning(f"Failed to query screen size via wm size: {e}")

        if self._default_screen_size is not None:
            return self._default_screen_size

        raise ValueError("Screen size not configured for this device")

    # --- Affordances ---

    @properties.Affordance
    def android_session(self) -> android_session_affordance.AndroidSession:
        """Returns an Android session affordance object.

        Returns:
            android_session.AndroidSession object
        """
        return android_session_affordance.AndroidSession(
            device_name=self.device_name, adb=self.adb
        )

    @properties.Affordance
    def diagnostics(self) -> android_diagnostics_affordance.AndroidDiagnostics:
        """Returns an Android diagnostics affordance object.

        Returns:
            android_diagnostics.AndroidDiagnostics object
        """
        return android_diagnostics_affordance.AndroidDiagnostics(
            device_name=self.device_name, adb=self.adb
        )

    @properties.Affordance
    def android_package(self) -> android_package_affordance.AndroidPackage:
        """Returns an Android package affordance object.

        Returns:
            android_package.AndroidPackage object
        """
        return android_package_affordance.AndroidPackage(
            device_name=self.device_name, adb=self.adb
        )
