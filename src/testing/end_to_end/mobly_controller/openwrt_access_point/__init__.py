# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Library for interacting with OpenWRT Access Points."""

import ipaddress
import json
import logging
import os
import re
import time
from dataclasses import dataclass
from enum import StrEnum
from typing import Any, Dict, List

from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.iperf_server import IPerfServerOverSsh
from antlion.controllers.utils_lib.commands.tcpdump import LinuxTcpdumpCommand
from honeydew.typing.custom_types import MacAddress
from libs.ssh import connection, settings
from libs.types import ControllerConfig, Json
from libs.validation import MapValidator
from mobly import logger, utils
from openwrt_access_point.lib import capabilities
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssSettings,
    SecurityOpen,
)
from openwrt_access_point.lib.dhcp_config import DhcpConfig as DhcpConfig
from openwrt_access_point.lib.dhcp_config import Dnsmasq as Dnsmasq
from openwrt_access_point.lib.dhcp_config import Lan as Lan
from openwrt_access_point.lib.dhcp_controller import DhcpController
from openwrt_access_point.lib.extended_capabilities import ExtendedCapabilities

_LOGGER: logging.Logger = logging.getLogger(__name__)


MOBLY_CONTROLLER_CONFIG_NAME: str = "OpenWrtAP"

PHY_2G: str = "phy0"
PHY_5G: str = "phy1"


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
        ap.dhcp.reset_dhcp_config()
        ap.iperf_server.stop()
        ap.ssh.close()


def get_info(objects: List["OpenWrtAP"]) -> List[Json]:
    """Gets information from OpenWRT controller objects.

    Args:
      objects: A list of OpenWRT objects.

    Returns:
      A list of hostnames for each OpenWRT device.
    """
    return [ap.ssh_settings.hostname for ap in objects]


class Radio(StrEnum):
    RADIO_2G = "radio0"
    RADIO_5G = "radio1"


class AddrType(StrEnum):
    ipv4_private = "ipv4_private"
    """Any 192.168, 172.16, 10, or 169.254 addresses"""
    ipv4_public = "ipv4_public"
    """Any IPv4 public addresses"""
    ipv6_link_local = "ipv6_link_local"
    """Any fe80:: addresses"""
    ipv6_private_local = "ipv6_private_local"
    """Any fd00:: addresses"""
    ipv6_public = "ipv6_public"
    """Any publicly routable addresses"""


class InterfaceName(StrEnum):
    lan = "br-lan"
    """The default LAN interface."""


@dataclass
class StationStatus:
    """Represents the connection status of a station on OpenWrt."""

    auth: bool
    assoc: bool
    authorized: bool


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
        self.dhcp = DhcpController(self.ssh)
        self.dhcp.reset_dhcp_config()
        self.reset_wifi_config()

        # Check for tcpdump
        try:
            self.ssh.run("which tcpdump")
        except Exception:
            _LOGGER.error("tcpdump command not found on OpenWrt AP")

        self.tcpdump = LinuxTcpdumpCommand(self.ssh)

        self.iperf_server = IPerfServerOverSsh(
            ssh_settings=self.ssh_settings,
            port=5201,
            test_interface=InterfaceName.lan,
        )

    @property
    def default_subnet(self) -> str:
        """Returns the default subnet for the AP."""
        return "192.168.1.0/24"

    @property
    def wlan_2g_interface(self) -> str:
        """Returns the default 2G wireless interface."""
        return f"{PHY_2G}-ap0"

    @property
    def wlan_5g_interface(self) -> str:
        """Returns the default 5G wireless interface."""
        return f"{PHY_5G}-ap0"

    def _configure_bss(self, bss: BssSettings, radio: Radio) -> None:
        """Configures a BSS on the Access Point.

        Args:
            bss: The BSS configuration containing SSID, password, band, etc.
        """
        section_name = f"{bss.name}_{radio}"
        self.ssh.run(f"uci set wireless.{section_name}='wifi-iface'")
        self.ssh.run(f"uci set wireless.{section_name}.device='{radio}'")
        self.ssh.run(f"uci set wireless.{section_name}.network='lan'")
        self.ssh.run(f"uci set wireless.{section_name}.mode='ap'")
        self.ssh.run(f"uci set wireless.{section_name}.ssid='{bss.ssid}'")
        encryption = bss.security.uci_encryption

        self.ssh.run(
            f"uci set wireless.{section_name}.encryption='{encryption}'"
        )

        if bss.password and not isinstance(bss.security, SecurityOpen):
            self.ssh.run(
                f"uci set wireless.{section_name}.key='{bss.password}'"
            )

        hidden = "1" if bss.hidden else "0"
        self.ssh.run(f"uci set wireless.{section_name}.hidden='{hidden}'")

        for option, value in bss.custom_uci_options.items():
            if isinstance(value, list):
                for item in value:
                    self.ssh.run(
                        f"uci add_list wireless.{section_name}.{option}='{item}'"
                    )
            elif isinstance(value, bool):
                v = str(int(value))
                self.ssh.run(f"uci set wireless.{section_name}.{option}='{v}'")
            else:
                self.ssh.run(
                    f"uci set wireless.{section_name}.{option}='{value}'"
                )

    def configure_wifi(self, config: AccessPointConfig) -> None:
        """Configures the Wi-Fi on the Access Point.

        Args:
            config: The Wi-Fi configuration containing multiple radios.
        """
        self.reset_wifi_config()

        for radio_config in config.radios:
            match radio_config.channel.band:
                case Band.BAND_2G:
                    radio = Radio.RADIO_2G
                case Band.BAND_5G:
                    radio = Radio.RADIO_5G
            self.ssh.run(f"uci set wireless.{radio}.disabled='0'")
            self.ssh.run(
                f"uci set wireless.{radio}.channel='{radio_config.channel.number}'"
            )
            self.ssh.run(
                f"uci set wireless.{radio}.htmode='{radio_config.channel.phy_mode.uci_htmode}'"
            )
            n_sel = radio_config.n_capabilities
            ac_sel = radio_config.ac_capabilities

            provided_caps: list[str] | None = None

            match n_sel.mode:
                case "DEFAULT":
                    pass
                case "DISABLED":
                    provided_caps = []
                case "CUSTOM":
                    provided_caps = [c for c in n_sel.capabilities if c]

            match ac_sel.mode:
                case "DEFAULT":
                    pass
                case "DISABLED":
                    if provided_caps is None:
                        provided_caps = []
                case "CUSTOM":
                    if provided_caps is None:
                        provided_caps = []
                    provided_caps.extend(c for c in ac_sel.capabilities if c)

            if provided_caps is not None:
                self._set_capabilities(radio, provided_caps)

            country = radio_config.country
            if str(radio_config.channel.number) in ["12", "13", "14"]:
                country = "AU"
            self.ssh.run(f"uci set wireless.{radio}.country='{country}'")

            for option, value in radio_config.custom_uci_options.items():
                if isinstance(value, list):
                    for item in value:
                        self.ssh.run(
                            f"uci add_list wireless.{radio}.{option}='{item}'"
                        )
                elif isinstance(value, bool):
                    v = str(int(value))
                    self.ssh.run(f"uci set wireless.{radio}.{option}='{v}'")
                else:
                    self.ssh.run(f"uci set wireless.{radio}.{option}='{value}'")

            # Apply hostapd options
            if hasattr(radio_config, "custom_hostapd_options"):
                for (
                    option,
                    value,
                ) in radio_config.custom_hostapd_options.items():
                    self.ssh.run(
                        f"uci add_list wireless.{radio}.hostapd_options='{option}={value}'"
                    )

            if radio_config.bss_settings:
                for bss in radio_config.bss_settings:
                    self._configure_bss(bss, radio)

        self.ssh.run("uci commit wireless")
        self.start_wifi()

        for radio_config in config.radios:
            self._verify_wifi_status(radio_config.channel.band)

    def _set_capabilities(self, radio: str, provided_caps: list[str]) -> None:
        """Applies the Wi-Fi capabilities to the specified radio.

        For a list of available OpenWrt UCI capabilities, refer to:
        https://openwrt.org/docs/guide-user/network/wifi/basic#ht_high_throughput_capabilities
        """

        # Start from a clean, controlled state by setting all known capabilities
        # to their default values.
        uci_options = {
            k: v
            for k, v in capabilities.UCI_OPTION_DEFAULTS.items()
            if v is not None
        }

        for cap in provided_caps:
            cap_info = capabilities.to_openwrt_capability(cap)
            uci_options[cap_info.uci_option] = cap_info.value

        cmds = [
            f"uci set wireless.{radio}.{k}='{v}'"
            for k, v in uci_options.items()
        ]
        self.ssh.run("; ".join(cmds))

    def get_configured_ssids(self) -> list[str]:
        """Retrieves all currently configured SSIDs from the AP's wireless configuration."""
        try:
            res = self.ssh.run("uci -q show wireless")
            output = res.stdout.decode("utf-8")
            ssids = []
            for line in output.splitlines():
                if ".ssid=" in line:
                    ssid_val = line.split("=", 1)[-1].strip("'\"")
                    ssids.append(ssid_val)
            return ssids
        except Exception:
            return []

    def _get_hostapd_interfaces(self, band: Band) -> list[str]:
        """Gets all hostapd interface names for a specific band."""
        match band:
            case Band.BAND_2G:
                phy = PHY_2G
            case Band.BAND_5G:
                phy = PHY_5G
            case _:
                raise ValueError(f"Unsupported band: {band}")
        res = self.ssh.run(f"ubus list hostapd.{phy}*")
        return [
            iface.strip().removeprefix("hostapd.")
            for iface in res.stdout.decode("utf-8").splitlines()
        ]

    def _is_ap_enabled(
        self, band: Band, expected_ssids: list[str] | None = None
    ) -> bool:
        """Checks if the active hostapd instances for a specific band are reporting 'ENABLED' status."""
        try:
            interfaces = self._get_hostapd_interfaces(band)
            if not interfaces:
                return False
            for iface in interfaces:
                status_res = self.ssh.run(
                    f"ubus call hostapd.{iface} get_status"
                )
                status_data = json.loads(status_res.stdout.decode("utf-8"))
                if status_data.get("status") != "ENABLED":
                    return False
                ssid = status_data.get("ssid")
                if expected_ssids and ssid not in expected_ssids:
                    return False
            return True
        except Exception:
            return False

    def get_sta_status(self, mac: str, band: Band) -> dict[str, StationStatus]:
        """Get station status for a specific band on OpenWrt."""
        result: dict[str, StationStatus] = {}
        try:
            interfaces = self._get_hostapd_interfaces(band)
            for iface in interfaces:
                clients_res = self.ssh.run(
                    f"ubus call hostapd.{iface} get_clients"
                ).stdout.decode()
                clients_data = json.loads(clients_res)
                clients = clients_data.get("clients", {})
                for client_mac, status in clients.items():
                    if client_mac.lower() == mac.lower():
                        result[iface] = StationStatus(
                            auth=status.get("auth", False),
                            assoc=status.get("assoc", False),
                            authorized=status.get("authorized", False),
                        )
        except Exception as e:
            error_msg = (
                f"Failed to get status for station {mac} on band {band}: {e}"
            )
            raise RuntimeError(error_msg) from e
        return result

    def _is_ap_broadcasting(
        self, interface: str, expected_ssids: list[str]
    ) -> bool:
        """Verifies via iwinfo that the radio is actively broadcasting one of the expected SSIDs."""
        try:
            res = self.ssh.run(f"iwinfo {interface} info")
            output = res.stdout.decode("utf-8")
            for ssid in expected_ssids:
                target_string = f'ESSID: "{ssid}"'
                if target_string in output:
                    return True
            return False
        except Exception:
            return False

    def get_bssid_from_ssid(self, ssid: str, band: Band) -> str:
        """Gets the BSSID for a given SSID and band."""
        if band == Band.BAND_2G:
            ifname = self.wlan_2g_interface
        else:
            ifname = self.wlan_5g_interface

        iw = self.ssh.run(f"iw dev {ifname} info")
        iw_out = iw.stdout.decode("utf-8")
        iw_lines = iw_out.splitlines()

        for line in iw_lines:
            if "ssid" in line and ssid in line:
                for line in iw_lines:
                    if "addr" in line:
                        tokens = line.split()
                        bssid = tokens[1]
                        try:
                            MacAddress(bssid).bytes()
                        except ValueError as e:
                            raise AssertionError(
                                f"Invalid BSSID format: {bssid}"
                            ) from e
                        return bssid
                raise RuntimeError(
                    f"iw dev info contained ssid but not addr: \n{iw_out}"
                )
        raise RuntimeError(f'iw dev did not contain ssid "{ssid}"')

    def get_sta_extended_capabilities(
        self, mac: str, band: Band
    ) -> dict[str, ExtendedCapabilities]:
        """Gets the extended capabilities of a station for all interfaces."""
        result: dict[str, ExtendedCapabilities] = {}
        try:
            interfaces = self._get_hostapd_interfaces(band)
            for iface in interfaces:
                cmd = f"hostapd_cli -i {iface} sta {mac}"
                try:
                    res = self.ssh.run(cmd)
                    output = res.stdout.decode("utf-8")
                    m = re.search(
                        r"ext_capab=([0-9A-Faf]+)", output, re.MULTILINE
                    )
                    if m:
                        raw_ext_capab = m.group(1)
                        result[iface] = ExtendedCapabilities(
                            bytearray.fromhex(raw_ext_capab)
                        )
                except Exception:
                    # Station might not be associated with this specific interface
                    continue
        except Exception as e:
            raise RuntimeError(f"Failed to run hostapd_cli on OpenWrt: {e}")
        return result

    def send_bss_transition_management_req(
        self, mac: str, band: Band, btm_req: Any
    ) -> None:
        """Sends a BSS Transition Management request to a station.

        Note:
            This implementation is derived from Antlion's `hostapd.py`
            (`antlion.controllers.ap_lib.hostapd._bss_tm_req`) to construct
            the command for `hostapd_cli`.
        """

        phy = "phy0" if band == Band.BAND_2G else "phy1"
        try:
            res = self.ssh.run(f"ubus list hostapd.{phy}*")
            interfaces = res.stdout.decode("utf-8").splitlines()
            if not interfaces:
                raise RuntimeError(f"No hostapd interface found for {phy}")
            iface = interfaces[0].strip().replace("hostapd.", "")

            bss_tm_req_cmd = f"bss_tm_req {mac}"
            if btm_req.abridged:
                bss_tm_req_cmd += " abridged=1"
            if (
                btm_req.bss_termination_included
                and btm_req.bss_termination_duration
            ):
                bss_tm_req_cmd += (
                    f" bss_term={btm_req.bss_termination_duration.duration}"
                )
            if btm_req.disassociation_imminent:
                bss_tm_req_cmd += " disassoc_imminent=1"
            if btm_req.disassociation_timer is not None:
                bss_tm_req_cmd += (
                    f" disassoc_timer={btm_req.disassociation_timer}"
                )
            if btm_req.preferred_candidate_list_included:
                bss_tm_req_cmd += " pref=1"
            if btm_req.session_information_url:
                bss_tm_req_cmd += f" url={btm_req.session_information_url}"
            if btm_req.validity_interval:
                bss_tm_req_cmd += f" valid_int={btm_req.validity_interval}"
            if btm_req.candidate_list is not None:
                for neighbor in btm_req.candidate_list:
                    bssid = neighbor.bssid
                    bssid_info = hex(neighbor.bssid_information)
                    op_class = neighbor.operating_class
                    chan_num = neighbor.channel_number
                    phy_type = int(neighbor.phy_type)
                    bss_tm_req_cmd += f" neighbor={bssid},{bssid_info},{op_class},{chan_num},{phy_type}"

            cmd = f"hostapd_cli -i {iface} {bss_tm_req_cmd}"
            self.ssh.run(cmd)
        except Exception as e:
            raise RuntimeError(f"Failed to send BTM request on OpenWrt: {e}")

    # TODO(https://fxbug.dev/487804746): Use async functions in this file.
    def _verify_wifi_status(
        self,
        band: Band,
        timeout_sec: int = 70,  # TODO(b/504795188): Bypass DFS wait times (60s) via custom regdb
    ) -> None:
        """Polls the AP until the hostapd BSS is actively transmitting beacons.

        Args:
            band: The band to verify the status for.
            timeout_sec: Maximum time in seconds to wait for AP to be 'ENABLED'.

        Raises:
            RuntimeError: If the AP is not 'ENABLED' or broadcasting within the timeout.
        """
        match band:
            case Band.BAND_2G:
                interface = self.wlan_2g_interface
            case Band.BAND_5G:
                interface = self.wlan_5g_interface
            case _:
                raise ValueError(f"Unsupported band: {band}")
        configured_ssids = self.get_configured_ssids()
        start_time = time.time()
        end_time = start_time + timeout_sec
        while time.time() < end_time:
            if self._is_ap_enabled(
                band, configured_ssids
            ) and self._is_ap_broadcasting(interface, configured_ssids):
                return
            time.sleep(1)
        raise RuntimeError(
            f"Wi-Fi band {band} failed to start transmitting beacons within {timeout_sec}s."
        )

    def start_wifi(self) -> None:
        """Starts the access point."""
        self.ssh.run("wifi up")

    def stop_wifi(self) -> None:
        """Stops the access point."""
        self.ssh.run("wifi down")

    def set_txpower(self, interface: str, dbm: int) -> None:
        """Sets the transmit power for the radio device associated with the interface.

        Note: the txpower value is only persisted during the phy's lifetime. If
        the phy is disabled and re-enabled (e.g. via `wifi reload`, which
        happens during `configure_wifi()`), the txpower will be reset to the
        default.

        Args:
            interface: The interface name (e.g. wlan_2g_interface, wlan_5g_interface).
            dbm: The power level in dBm
        """
        if interface == self.wlan_2g_interface:
            phy = PHY_2G
        elif interface == self.wlan_5g_interface:
            phy = PHY_5G
        else:
            raise ValueError(f"Unknown interface: {interface}")

        mbm = dbm * 100
        self.ssh.run(f"iw phy {phy} set txpower limit {mbm}")

    def reset_txpower(self, interface: str) -> None:
        """Resets the transmit power to the regulatory default maximum (full power).

        Args:
            interface: The interface name (e.g. wlan_2g_interface, wlan_5g_interface).
        """
        if interface == self.wlan_2g_interface:
            phy = PHY_2G
        elif interface == self.wlan_5g_interface:
            phy = PHY_5G
        else:
            raise ValueError(f"Unknown interface: {interface}")

        self.ssh.run(f"iw phy {phy} set txpower auto")

    def _set_radio_enabled(self, radio: Radio, enabled: bool) -> None:
        """Enables or disables the given radio.

        Args:
            radio: The radio (e.g. Radio.RADIO_2G, Radio.RADIO_5G).
            enabled: True to enable, False to disable.
        """
        disabled_val = "0" if enabled else "1"
        self.ssh.run(f"uci set wireless.{radio}.disabled='{disabled_val}'")
        self.ssh.run("uci commit wireless")
        self.ssh.run(f"wifi reload {radio}")

    def enable_radio(self, radio: Radio) -> None:
        """Enables the given radio and verifies it starts transmitting.

        Args:
            radio: The radio (e.g. Radio.RADIO_2G, Radio.RADIO_5G).
        """
        self._set_radio_enabled(radio, True)
        if radio == Radio.RADIO_2G:
            band = Band.BAND_2G
        elif radio == Radio.RADIO_5G:
            band = Band.BAND_5G
        else:
            raise ValueError(f"Unsupported radio: {radio}")
        self._verify_wifi_status(band)

    def disable_radio(self, radio: Radio) -> None:
        """Disables the given radio.

        Args:
            radio: The radio (e.g. Radio.RADIO_2G, Radio.RADIO_5G).
        """
        self._set_radio_enabled(radio, False)

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
        # Delete all default BSSs generated by `wifi config` to ensure a clean slate.
        self.ssh.run("while uci -q delete wireless.@wifi-iface[0]; do :; done")

    def get_addr(
        self,
        interface: InterfaceName,
        addr_type: AddrType,
        timeout_sec: int = 30,
    ) -> str:
        """Get the requested type of IP address for an interface.

        Args:
            interface: The interface name on the device.
            addr_type: Type of address to get.
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
    ) -> dict[AddrType, list[str]]:
        """Gets all of the IP addresses associated with a particular interface name.

        Args:
            interface: The interface name on the device.

        Returns:
            A dictionary of the various IP addresses:
                ipv4_private: Any 192.168, 172.16, 10, or 169.254 addresses
                ipv4_public: Any IPv4 public addresses
                ipv6_link_local: Any fe80:: addresses
                ipv6_private_local: Any fd00:: addresses
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

        ipv4_private_addresses: list[str] = []
        ipv4_public_addresses: list[str] = []
        ipv6_link_local_addresses: list[str] = []
        ipv6_private_local_addresses: list[str] = []
        ipv6_public_addresses: list[str] = []

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
                    ipv6_private_local_addresses.append(str(on_device_ip))
                elif on_device_ip.is_global:
                    ipv6_public_addresses.append(str(on_device_ip))

        return {
            AddrType.ipv4_private: ipv4_private_addresses,
            AddrType.ipv4_public: ipv4_public_addresses,
            AddrType.ipv6_link_local: ipv6_link_local_addresses,
            AddrType.ipv6_private_local: ipv6_private_local_addresses,
            AddrType.ipv6_public: ipv6_public_addresses,
        }

    def log_to_syslog(self, message: str) -> None:
        """Log a message to syslog on the access point.

        Args:
            message: The message to log.
        """
        self.ssh.run(f'logger "{message}"')

    def download_logs(self, path: str, start_marker: str | None = None) -> None:
        """Download all available logs from the OpenWRT AP.

        Args:
            path: Path to write the logs to.
            start_marker: If provided, only logs after this marker will be saved.
        """
        timestamp = logger.normalize_log_line_timestamp(
            logger.epoch_to_log_line_timestamp(utils.get_current_epoch_time())
        )

        logread_out = self.ssh.run("logread").stdout.decode("utf-8")
        hostname = self.ssh_settings.hostname.replace(".", "_")
        logread_path = os.path.join(
            path, f"openwrt_{hostname}_logread_{timestamp}.log"
        )

        if start_marker:
            logs = logread_out.splitlines()
            try:
                marker_index = next(
                    i for i, line in enumerate(logs) if start_marker in line
                )
                filtered_logs = "\n".join(logs[marker_index:])
            except StopIteration:
                _LOGGER.warning(
                    "Test start marker '%s' not found in logread output.",
                    start_marker,
                )
                filtered_logs = logread_out
        else:
            filtered_logs = logread_out

        with open(logread_path, "w") as f:
            f.write(filtered_logs)
        _LOGGER.debug("Wrote OpenWRT logread to %s", logread_path)

    def channel_switch(
        self,
        interface: str,
        channel_num: int,
        csa_beacon_count: int = 10,
    ) -> None:
        """Switch to a different channel on the given AP interface.

        Args:
            interface: The interface name (e.g. "phy0-ap0")
            channel_num: Channel number to switch to
            csa_beacon_count: Number of CSA beacons to send
        """

        try:
            channel_freq = hostapd_constants.FREQUENCY_MAP[channel_num]
        except KeyError:
            raise ValueError(f"Invalid channel number {channel_num}")

        cmd = f"hostapd_cli -i {interface} chan_switch {csa_beacon_count} {channel_freq}"
        self.ssh.run(cmd)

    def get_current_channel(self, interface: str) -> int:
        """Find the current channel on the given AP interface.

        Args:
            interface: The interface name (e.g. "phy0-ap0")

        Returns:
            The current channel number as an integer.
        """
        cmd = f"hostapd_cli -i {interface} status"
        res = self.ssh.run(cmd)
        output = res.stdout.decode("utf-8")
        match = re.search(r"^channel=(\d+)$", output, re.MULTILINE)
        if not match:
            raise RuntimeError(
                f"Current channel could not be determined for interface {interface}"
            )
        return int(match.group(1))
