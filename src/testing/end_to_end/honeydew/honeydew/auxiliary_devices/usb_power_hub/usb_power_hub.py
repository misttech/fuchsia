# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for controlling usb power hub hardware."""

from __future__ import annotations

import abc
import logging

from honeydew.transports.ffx import ffx as ffx_transport

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbPowerHubError(Exception):
    """Exception class for UsbPowerHub relates error."""

    def __init__(self, msg: str | Exception) -> None:
        """Inits UsbPowerHubError with 'msg' (an error message string).

        Args:
            msg: an error message string or an Exception instance.

        Note: Additionally, logs 'msg' to debug log level file.
        """
        super().__init__(msg)
        _LOGGER.debug(repr(self), exc_info=True)


class UsbPowerHub(abc.ABC):
    """Abstract base class for usb power hub hardware."""

    ffx: ffx_transport.FFX

    def __init__(self, ffx: ffx_transport.FFX) -> None:
        self.ffx: ffx_transport.FFX = ffx

    # List all the public methods
    @abc.abstractmethod
    def power_off(self, port: int | None = None) -> None:
        """Turns off the usb power hub.

        Args:
            port: port on which we need to perform this operation.

        Raises:
            UsbPowerError: In case of failure.
        """

    @abc.abstractmethod
    def power_on(self, port: int | None = None) -> None:
        """Turns on the usb power hub.

        Args:
            port: port on which we need to perform this operation.

        Raises:
            UsbPowerError: In case of failure.
        """
