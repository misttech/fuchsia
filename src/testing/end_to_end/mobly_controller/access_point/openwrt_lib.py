# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Library for interacting with OpenWRT Access Points."""
import json
import logging
import time

from libs.ssh import connection, settings
from libs.types import ControllerConfig
from libs.validation import MapValidator
from mobly_controller.access_point.access_point_config import (
    AccessPointConfig,
    Band,
    Security,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)


RADIO_2G = "radio0"
RADIO_5G = "radio1"

DEFAULT_2G_INTERFACE = "default_radio0"
DEFAULT_5G_INTERFACE = "default_radio1"


class OpenwrtAp:
    """A basic client to interact with an OpenWRT AP via SSH."""

    def __init__(self, config: ControllerConfig) -> None:
        """Initializes the OpenWrt controller.

        Args:
            config: The configuration for the OpenWRT device.
        """
        logging.info("Connecting to OpenWRT AP with config: %s", config)
        c = MapValidator(config)
        self.ssh_settings = settings.from_config(c.get(dict, "ssh_config"))
        self.ssh = connection.SshConnection(self.ssh_settings)
        self.reset_ap()

    def configure_wifi(self, config: AccessPointConfig) -> None:
        """Configures the Wi-Fi on the Access Point.

        Args:
            config: The Wi-Fi configuration containing SSID, password, band, etc.
        """
        match config.band:
            case Band.BAND_2G:
                radio = RADIO_2G
                iface = DEFAULT_2G_INTERFACE
            case Band.BAND_5G:
                radio = RADIO_5G
                iface = DEFAULT_5G_INTERFACE

        self.ssh.run(f"uci set wireless.{radio}.disabled='0'")
        self.ssh.run(f"uci set wireless.{iface}.mode='ap'")
        self.ssh.run(f"uci set wireless.{iface}.ssid='{config.ssid}'")
        self.ssh.run(
            f"uci set wireless.{iface}.encryption='{config.security.value}'"
        )
        if config.password:
            self.ssh.run(f"uci set wireless.{iface}.key='{config.password}'")
        # Explicitly clear the password when using 'none' encryption
        if config.security == Security.NONE:
            self.ssh.run(f"uci delete wireless.{iface}.key || true")
        self.ssh.run(f"uci set wireless.{radio}.channel='{config.channel}'")
        self.ssh.run("uci commit wireless")
        self.start_ap()

    def get_wifi_status(self, band: Band) -> bool:
        """Checks if the wireless interface is up and running.

        Returns:
            True if the radio interface is marked as 'up', False otherwise
            or if the status command fails.
        """
        try:
            radio = RADIO_2G if band == Band.BAND_2G else RADIO_5G
            result = self.ssh.run(f"wifi status {radio}").stdout.decode()
            radio_data = json.loads(result)
            return radio_data[radio]["up"]
        except Exception as e:
            logging.error("Failed to get WiFi status: %s", e)
            return False

    # TODO(https://fxbug.dev/487804746): Use async functions in this file.
    def verify_wifi_status(
        self,
        band: Band,
        timeout_sec: int = 20,
    ) -> bool:
        """Polls the AP until the Wi-Fi interfaces are ready.

        Args:
            timeout_sec: Maximum time in seconds to wait for the interface
                to report as 'up'.
            band: The band to verify the status for.

        Returns:
            True if the radios are confirmed up within the timeout, False otherwise.
        """
        start_time = time.time()
        end_time = start_time + timeout_sec
        while time.time() < end_time:
            if self.get_wifi_status(band=band):
                return True
            time.sleep(1)
        return False

    def start_ap(self) -> None:
        """Starts the access point."""
        self.ssh.run("wifi up")

    def stop_ap(self) -> None:
        """Stops the access point."""
        self.ssh.run("wifi down")

    def reset_ap(self) -> None:
        """Resets wireless configuration to system defaults."""
        self.ssh.run("rm -f /etc/config/wireless")
        self.ssh.run("wifi config")

    def close(self) -> None:
        """Cleans up the AP state and terminates the SSH connection."""
        self.stop_ap()
        self.reset_ap()
        self.ssh.close()
