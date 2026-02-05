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

_LOGGER: logging.Logger = logging.getLogger(__name__)


# TODO(b/481539515): Define an 'AccessPoint' abstract base class to provide a
# unified interface. Refactor 'OpenwrtAp' to inherit from 'AccessPoint'. This
# will allow support for other APs without modifying callers.
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

    def setup_ap(self, ssid: str) -> None:
        """Configures and enables an OpenWrt Access Point with the specified SSID.

        Args:
            ssid: The Service Set Identifier for the Wi-Fi network.
        """
        # TODO(b/461905545): security, band, etc will be added later
        self.ssh.run("uci set wireless.radio0.disabled='0'")
        self.ssh.run("uci set wireless.@wifi-iface[0].mode='ap'")
        self.ssh.run(f"uci set wireless.@wifi-iface[0].ssid='{ssid}'")
        self.ssh.run("uci set wireless.@wifi-iface[0].encryption='none'")
        self.ssh.run("uci commit wireless")
        self.start_ap()

    def get_wifi_status(self) -> bool:
        """Checks if the wireless interface is up and running.

        Returns:
            True if the 'radio0' interface is marked as 'up', False otherwise
            or if the status command fails.
        """

        try:
            # TODO(b/461905545): support dual band check
            result = self.ssh.run("wifi status radio0").stdout.decode()
            radio_data = json.loads(result)
            return radio_data["radio0"]["up"]
        except Exception as e:
            logging.error("Failed to get WiFi status: %s", e)
            return False

    def verify_wifi_status(self, timeout_sec: int = 20) -> bool:
        """Polls the AP until the Wi-Fi interfaces are ready.

        Args:
            timeout_sec: Maximum time in seconds to wait for the interface
                to report as 'up'.

        Returns:
            True if the radios are confirmed up within the timeout, False otherwise.
        """
        start_time = time.time()
        end_time = start_time + timeout_sec
        while time.time() < end_time:
            if self.get_wifi_status():
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
