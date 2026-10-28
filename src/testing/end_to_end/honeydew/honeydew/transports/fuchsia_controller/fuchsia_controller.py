# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Provides Host-(Fuchsia)Target interactions via Fuchsia-Controller."""

import logging

import fuchsia_controller_py as fuchsia_controller
from honeydew import errors
from honeydew.affordances_capable import FuchsiaDeviceAddressChange
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.typing import custom_types
from honeydew.utils import decorators

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Exceptions raised by the native Fuchsia-Controller bindings on failure. Most
# failures raise `FcTransportStatus`, but internal failures (e.g. target not
# ready in `Context.target_wait()`) raise `RuntimeError`. See:
# - `mod::set_python_exception()` in
#   //src/developer/fuchsia-controller/cpp/fuchsia_controller_internal/mod.cc,
#   which maps `FC_ERR_INTERNAL` to `RuntimeError` and most other statuses to
#   `FcTransportStatus`.
# - `LibraryCommand::TargetWait` in
#   //src/developer/fuchsia-controller/src/commands.rs, which returns
#   `FC_ERR_INTERNAL` when the target wait fails.
_CONTROLLER_ERRORS: tuple[type[Exception], ...] = errors.FIDL_ERRORS + (
    RuntimeError,
    errors.HoneydewError,
)


class FuchsiaController:
    """Provides Host-(Fuchsia)Target interactions via Fuchsia-Controller.

    Args:
        target_name: Fuchsia device name.
        ffx_config_data: Configuration associated with FuchsiaController, FFX.
        target_addr: Fuchsia device target address (IP or USB).
        device_address_change: Object that implements FuchsiaDeviceAddressChange to handle Fuchsia
            device address changes.

    Raises:
        ValueError: If target_addr is provided and device_address_change is not provided.
        FuchsiaControllerConnectionError: If target is not ready.
        FuchsiaControllerError: Failed to instantiate.
    """

    def __init__(
        self,
        target_name: str,
        ffx_config_data: ffx_config.FfxConfigData,
        target_addr: custom_types.TargetAddr | None = None,
        device_address_change: FuchsiaDeviceAddressChange | None = None,
    ) -> None:
        self._target_name: str = target_name

        self._ffx_config_data: ffx_config.FfxConfigData = ffx_config_data

        self._target_addr: custom_types.TargetAddr | None = target_addr
        if self._target_addr is not None and device_address_change is None:
            raise ValueError(
                "Pass 'device_address_change' argument also when 'target_addr' is passed"
            )
        self._device_address_change: FuchsiaDeviceAddressChange | None = (
            device_address_change
        )
        if self._device_address_change:
            self._device_address_change.register_for_on_device_address_change(
                fn=self._on_device_address_change
            )

        self._target: str
        if self._target_addr:
            self._target = str(self._target_addr)
        else:
            self._target = self._target_name

        self.ctx: fuchsia_controller.Context

        self.create_context()
        self.check_connection()

    def create_context(self) -> None:
        """Creates the fuchsia-controller context.

        Raises:
            FuchsiaControllerError: Failed to create FuchsiaController Context.
            FuchsiaControllerConnectionError: If target is not ready.
        """
        try:
            config: dict[str, str] = {}
            if self._ffx_config_data.logs_level:
                level = self._ffx_config_data.logs_level
                _LOGGER.debug("log.level set to %s", level)
                config["log.level"] = level
            else:
                _LOGGER.debug("log level not set.")
            if self._ffx_config_data.logs_dir:
                log_dir = self._ffx_config_data.logs_dir
                _LOGGER.debug("log.dir set to %s", log_dir)
                config["log.dir"] = log_dir
            else:
                _LOGGER.debug("log dir not set.")
            if self._ffx_config_data.shared_data:
                shared_data = self._ffx_config_data.shared_data
                _LOGGER.debug("shared_data set to %s", shared_data)
                config["shared_data"] = shared_data
            else:
                _LOGGER.debug("shared_data not set.")
            if self._ffx_config_data.enable_usb:
                enable_usb = "true"
            else:
                enable_usb = "false"
            _LOGGER.debug("connectivity.enable_usb set to %s", enable_usb)
            config["connectivity.enable_usb"] = enable_usb
            if self._ffx_config_data.usb_driver_autostart:
                usb_driver_autostart = "true"
            else:
                usb_driver_autostart = "false"
            _LOGGER.debug(
                "connectivity.usb_driver_autostart set to %s",
                usb_driver_autostart,
            )
            config["connectivity.usb_driver_autostart"] = usb_driver_autostart
            if self._ffx_config_data.usb_socket_path:
                usb_socket_path = self._ffx_config_data.usb_socket_path
                _LOGGER.debug(
                    "connectivity.usb_socket_path set to %s", usb_socket_path
                )
                config["connectivity.usb_socket_path"] = usb_socket_path
            else:
                _LOGGER.debug("connectivity.usb_socket_path not set.")
            if self._ffx_config_data.ssh_auth_sock:
                ssh_auth_sock = self._ffx_config_data.ssh_auth_sock
                _LOGGER.debug("ssh.auth-sock set to %s", ssh_auth_sock)
                config["ssh.auth-sock"] = ssh_auth_sock
            else:
                _LOGGER.debug("ssh auth sock not set.")
            if self._ffx_config_data.identities_only is not None:
                identities_only = (
                    "true" if self._ffx_config_data.identities_only else "false"
                )
                _LOGGER.debug("ssh.identities-only set to %s", identities_only)
                config["ssh.identities-only"] = identities_only
            else:
                _LOGGER.debug("ssh identities only not set.")
            msg: str = (
                f"Creating Fuchsia-Controller Context with "
                f"target='{self._target}', config='{config}'"
            )
            _LOGGER.debug(msg)
            self.ctx = fuchsia_controller.Context(
                config=config, target=self._target
            )
        except _CONTROLLER_ERRORS as err:
            raise fc_errors.FuchsiaControllerError(
                "Failed to create Fuchsia-Controller context"
            ) from err

    @decorators.liveness_check
    def check_connection(self) -> None:
        """Checks the Fuchsia-Controller connection from host to Fuchsia device.

        Raises:
            FuchsiaControllerConnectionError
        """
        try:
            _LOGGER.info(
                "Waiting for Fuchsia-Controller to check the "
                "connection from host to %s...",
                self._target_name,
            )
            self.ctx.target_wait(timeout=0)
            _LOGGER.info(
                "Fuchsia-Controller completed the connection check from host "
                "to %s...",
                self._target_name,
            )
        except _CONTROLLER_ERRORS as err:
            raise fc_errors.FuchsiaControllerConnectionError(
                f"Fuchsia-Controller connection check failed for "
                f"{self._target_name} with error: {err}"
            )

    def connect_device_proxy(
        self, fidl_end_point: custom_types.FidlEndpoint
    ) -> fuchsia_controller.Channel:
        """Opens a proxy to the specified FIDL end point.

        Args:
            fidl_end_point: FIDL end point tuple containing moniker and protocol
              name.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.

        Returns:
            FIDL channel to proxy.
        """
        try:
            return self.ctx.connect_device_proxy(
                fidl_end_point.moniker, fidl_end_point.protocol
            )
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    def _on_device_address_change(
        self, target_addr: custom_types.TargetAddr
    ) -> None:
        """Callback method that gets invoked when device address changes.

        Args:
            target_addr: New target address of the device.
        """
        self._target_addr = target_addr
        self._target = str(self._target_addr)

        self.create_context()
        self.check_connection()

    def before_usb_disconnect(self) -> None:
        """Callback method that gets invoked before USB disconnect."""

    def after_usb_reconnect(self) -> None:
        """Callback method that gets invoked after USB reconnect."""
        self.create_context()
        self.check_connection()

    def channel_create(
        self,
    ) -> tuple[fuchsia_controller.Channel, fuchsia_controller.Channel]:
        """Opens a pair of connected channels, usef for FIDL endpoints.

        Raises: FuchsiaControllerError: On failure to create the channels.
        """
        try:
            return self.ctx.channel_create()
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller Channel Create Error"
            ) from status

    def socket_create(
        self,
    ) -> tuple[fuchsia_controller.Socket, fuchsia_controller.Socket]:
        """Opens a pair of connected sockets, used for FIDL endpoints.

        Raises: FuchsiaControllerError: On failure to create the sockets.
        """
        try:
            return self.ctx.socket_create()
        except fuchsia_controller.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller Socket Create Error"
            ) from status
