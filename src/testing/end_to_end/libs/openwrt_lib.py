# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Library for interacting with OpenWRT Access Points."""
import logging
from typing import Any, Mapping

_LOGGER: logging.Logger = logging.getLogger(__name__)


# TODO(b/481539515): Define an 'AccessPoint' abstract base class to provide a
# unified interface. Refactor 'OpenwrtAp' to inherit from 'AccessPoint'. This
# will allow support for other APs without modifying callers.
class OpenwrtAp:
    """A basic client to interact with an OpenWRT AP via SSH."""

    def __init__(self, config: Mapping[str, Any]) -> None:
        logging.info("Connecting to OpenWRT AP with config: %s", config)
        # TODO(b/461905545): use MayValidator to validate config
        self.host: str = "placeholder host name"

        # TODO(b/461905545): reuse the SSH lib to start the connection
        # from https://source.corp.google.com/h/fuchsia/fuchsia/+/main:src/testing/end_to_end/antlion/packages/antlion/controllers/utils_lib/ssh/connection.py;bpv=0;bpt=0

    def setup_ap(self, ssid: str) -> None:
        """
        Configures and enables an OpenWrt Access Point with the specified SSID.
        """
        # TODO(b/461905545): security, band, etc will be added later

        _LOGGER.info("Starting Access Point...")
        commands = [
            "uci set wireless.radio0.disabled='0'",
            "uci set wireless.@wifi-iface[0].mode='ap'",
            f"uci set wireless.@wifi-iface[0].ssid='{ssid}'",
            f"uci set wireless.@wifi-iface[0].encryption='none'",
            "uci commit wireless",
        ]
        # TODO(b/461905545): run the above commands to start AP
        self.start_ap()

    def start_ap(self) -> None:
        """Starts the Access Point."""
        # TODO(b/461905545): run wifi up command and wait until it's ready

    def stop_ap(self) -> None:
        """Stops the Access Point."""
        # Deleting the wireless config and recreating it from defaults
        commands = [
            "wifi down",
        ]
        # TODO(b/461905545): run the above commands to stop AP

    def reset_ap(self) -> None:
        """Resets wireless configuration to system defaults."""
        # Deleting the wireless config and recreating it from defaults
        commands = [
            "rm -f /etc/config/wireless",
            "wifi config",
        ]
        # TODO(b/461905545): run the above commands to reset AP

    def close(self) -> None:
        """Stops the AP, resets configuration, and closes the SSH connection."""
        self.stop_ap()
        self.reset_ap()
        # TODO(b/461905545): reuse the SSH lib to close the connection
