# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Library for interacting with OpenWRT Access Points."""

import ipaddress
import json
import logging
import time
from typing import Any, Dict, List

from libs.ssh import connection, settings
from libs.types import ControllerConfig, Json
from libs.validation import MapValidator
from mobly_controller.openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    Security,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)


MOBLY_CONTROLLER_CONFIG_NAME: str = "OpenWrtAP"


def create(configs: List[ControllerConfig]) -> List["OpenWrtAP"]:
    """Creates OpenWRT controller objects from testbed configs.

    Args:
      configs: A list of dictionaries, each representing a configuration for
        one OpenWRT device.

    Returns:
      A list of instantiated OpenWRT objects.
    """
    logging.info("Creating OpenWRT controllers with configs: %s", configs)
    return [OpenWrtAP(config) for config in configs]


def destroy(objects: List["OpenWrtAP"]) -> None:
    """Destroys OpenWRT controller objects.

    Args:
      objects: A list of OpenWRT objects to be destroyed.
    """
    logging.info("Destroying OpenWRT controllers.")
    for ap in objects:
        ap.stop_wifi()
        ap.reset_wifi_config()
        ap.ssh.close()


def get_info(objects: List["OpenWrtAP"]) -> List[Json]:
    """Gets information from OpenWRT controller objects.

    Args:
      objects: A list of OpenWRT objects.

    Returns:
      A list of hostnames for each OpenWRT device.
    """
    return [ap.ssh_settings.hostname for ap in objects]


RADIO_2G = "radio0"
RADIO_5G = "radio1"

DEFAULT_2G_INTERFACE = "default_radio0"
DEFAULT_5G_INTERFACE = "default_radio1"


class OpenWrtAP:
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
        self.reset_wifi_config()

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
        hidden = "1" if config.hidden else "0"
        self.ssh.run(f"uci set wireless.{iface}.hidden='{hidden}'")
        self.ssh.run("uci commit wireless")
        self.start_wifi()

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

    def start_wifi(self) -> None:
        """Starts the access point."""
        self.ssh.run("wifi up")

    def stop_wifi(self) -> None:
        """Stops the access point."""
        self.ssh.run("wifi down")

    def reset_wifi_config(self) -> None:
        """Resets wireless configuration to system defaults.

        On the OpenWRT One version 24.10.4 r28959-29397011cc, the default config
        generated by `wifi config` (as seen at `cat /etc/config/wireless`) is:

        config wifi-device 'radio0'
                option type 'mac80211'
                option path 'platform/soc/18000000.wifi'
                option band '2g'
                option channel '1'
                option htmode 'HE20'
                option num_global_macaddr '7'
                option disabled '1'

        config wifi-iface 'default_radio0'
                option device 'radio0'
                option network 'lan'
                option mode 'ap'
                option ssid 'OpenWrt'
                option encryption 'none'

        config wifi-device 'radio1'
                option type 'mac80211'
                option path 'platform/soc/18000000.wifi+1'
                option band '5g'
                option channel '36'
                option htmode 'HE80'
                option num_global_macaddr '7'
                option disabled '1'

        config wifi-iface 'default_radio1'
                option device 'radio1'
                option network 'lan'
                option mode 'ap'
                option ssid 'OpenWrt'
                option encryption 'none'
        """
        # Regenerate default wifi config as documented at
        # https://openwrt.org/docs/guide-user/network/wifi/basic#regenerate_configuration
        self.ssh.run("rm -f /etc/config/wireless")
        self.ssh.run("wifi config")

    def get_addr(
        self,
        interface: str,
        addr_type: str = "ipv4_private",
        timeout_sec: int = 30,
    ) -> str:
        """Get the requested type of IP address for an interface.

        Args:
            interface: The interface name on the device (e.g., 'br-lan').
            addr_type: Type of address to get (e.g., 'ipv4_private', 'ipv6_link_local').
            timeout_sec: Seconds to wait to acquire an address.

        Returns:
            A string containing the requested address.

        Raises:
            TimeoutError: No address is available after timeout_sec.
            ValueError: Several addresses are available or unknown addr_type.
        """
        end_time = time.time() + timeout_sec
        while time.time() < end_time:
            addrs_dict = self.get_interface_ip_addresses(interface)
            if addr_type not in addrs_dict:
                raise ValueError(f"Unknown addr_type: {addr_type}")

            ip_addrs = addrs_dict[addr_type]
            if len(ip_addrs) > 1:
                raise ValueError(
                    f'Expected only one "{addr_type}" address, got {ip_addrs}'
                )
            elif len(ip_addrs) == 1:
                return ip_addrs[0]
            time.sleep(1)

        raise TimeoutError(
            f'No available "{addr_type}" address on {interface} after {timeout_sec}s'
        )

    def get_interface_ip_addresses(
        self, interface: str
    ) -> Dict[str, List[str]]:
        """Gets all of the IP addresses associated with a particular interface name.

        Args:
            interface: The interface name on the device (e.g., 'br-lan').

        Returns:
            A dictionary of the various IP addresses:
                ipv4_private: Any 192.168, 172.16, 10, or 169.254 addresses
                ipv4_public: Any IPv4 public addresses
                ipv6_link_local: Any fe80:: addresses
                ipv6_private: Any fd00:: addresses
                ipv6_public: Any publicly routable addresses
        """
        stdout = self.ssh.run(f"ip -o addr show {interface}").stdout.decode(
            "utf-8"
        )
        addrs = [
            line.replace("/", " ").split()[3]
            for line in stdout.splitlines()
            if len(line.split()) > 3
        ]

        ipv4_private_addresses = []
        ipv4_public_addresses = []
        ipv6_link_local_addresses = []
        ipv6_private_addresses = []
        ipv6_public_addresses = []

        for addr in addrs:
            on_device_ip = ipaddress.ip_address(addr)
            if on_device_ip.version == 4:
                if on_device_ip.is_private:
                    ipv4_private_addresses.append(str(on_device_ip))
                elif on_device_ip.is_global or (
                    # Carrier private doesn't have a property, so we check if
                    # all other values are left unset.
                    not on_device_ip.is_reserved
                    and not on_device_ip.is_unspecified
                    and not on_device_ip.is_link_local
                    and not on_device_ip.is_loopback
                    and not on_device_ip.is_multicast
                ):
                    ipv4_public_addresses.append(str(on_device_ip))
            elif on_device_ip.version == 6:
                if on_device_ip.is_link_local:
                    ipv6_link_local_addresses.append(str(on_device_ip))
                elif on_device_ip.is_private:
                    ipv6_private_addresses.append(str(on_device_ip))
                elif on_device_ip.is_global:
                    ipv6_public_addresses.append(str(on_device_ip))

        return {
            "ipv4_private": ipv4_private_addresses,
            "ipv4_public": ipv4_public_addresses,
            "ipv6_link_local": ipv6_link_local_addresses,
            "ipv6_private": ipv6_private_addresses,
            "ipv6_public": ipv6_public_addresses,
        }
