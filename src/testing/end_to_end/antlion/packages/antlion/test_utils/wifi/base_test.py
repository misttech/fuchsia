#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Base Class for Defining Common WiFi Test Functionality
"""

import copy
import os
from typing import Any, TypedDict, TypeVar

from antlion import context, controllers, utils
from antlion.controllers.access_point import AccessPoint
from antlion.controllers.android_device import AndroidDevice
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import (
    OpenWRTEncryptionMode,
    SecurityMode,
)
from antlion.controllers.attenuator import Attenuator
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.controllers.iperf_client import IPerfClientBase
from antlion.controllers.iperf_server import IPerfServer, IPerfServerOverSsh
from antlion.controllers.openwrt_ap import PMF_ENABLED, BSSIDMap, OpenWrtAP
from antlion.controllers.openwrt_lib.wireless_config import WirelessConfig
from antlion.controllers.packet_capture import PacketCapture
from antlion.controllers.pdu import PduDevice
from antlion.keys import Config
from antlion.test_utils.abstract_devices.wlan_device import (
    AndroidWlanDevice,
    AssociationMode,
    FuchsiaWlanDevice,
    SupportsWLAN,
)
from antlion.test_utils.net import net_test_utils as nutils
from antlion.test_utils.wifi import wifi_test_utils as wutils
from antlion.types import Controller
from antlion.validation import MapValidator
from mobly import signals
from mobly.base_test import BaseTestClass
from mobly.config_parser import TestRunConfig
from mobly.records import TestResultRecord

WifiEnums = wutils.WifiEnums
MAX_AP_COUNT = 2


class Network(TypedDict):
    SSID: str
    security: SecurityMode
    password: str | None
    hiddenSSID: bool
    wepKeys: list[str] | None
    ieee80211w: str | None


class NetworkUpdate(TypedDict, total=False):
    SSID: str
    security: SecurityMode
    password: str | None
    hiddenSSID: bool
    wepKeys: list[str] | None
    ieee80211w: str | None


NetworkList = dict[str, Network]

_T = TypeVar("_T")


class WifiBaseTest(BaseTestClass):
    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.enable_packet_log = False
        self.packet_log_2g = hostapd_constants.AP_DEFAULT_CHANNEL_2G
        self.packet_log_5g = hostapd_constants.AP_DEFAULT_CHANNEL_5G
        self.tcpdump_proc: list[Any] = []
        self.packet_log_pid: dict[str, Any] = {}

        T = TypeVar("T")

        def register_controller(module: Controller[T]) -> list[T]:
            registered_controllers: list[T] | None = self.register_controller(
                module, required=False
            )
            if registered_controllers is None:
                return []
            return registered_controllers

        self.access_points: list[AccessPoint] = register_controller(
            controllers.access_point
        )
        self.openwrt_aps: list[OpenWrtAP] = register_controller(
            controllers.openwrt_ap
        )
        self.android_devices: list[AndroidDevice] = register_controller(
            controllers.android_device
        )
        self.attenuators: list[Attenuator] = register_controller(
            controllers.attenuator
        )
        self.fuchsia_devices: list[FuchsiaDevice] = register_controller(
            controllers.fuchsia_device
        )
        self.iperf_clients: list[IPerfClientBase] = register_controller(
            controllers.iperf_client
        )
        iperf_servers: list[
            IPerfServer | IPerfServerOverSsh
        ] = register_controller(controllers.iperf_server)
        self.iperf_servers = [
            iperf_server
            for iperf_server in iperf_servers
            if isinstance(iperf_server, IPerfServerOverSsh)
        ]
        self.pdu_devices: list[PduDevice] = register_controller(controllers.pdu)
        self.packet_capture: list[PacketCapture] = register_controller(
            controllers.packet_capture
        )

        for attenuator in self.attenuators:
            attenuator.set_atten(0)

        self.pixel_models: list[str] | None = self.user_params.get(
            "pixel_models"
        )
        self.cnss_diag_file: str | list[str] | None = self.user_params.get(
            "cnss_diag_file"
        )
        self.country_code_file: str | list[str] | None = self.user_params.get(
            "country_code_file"
        )

        if self.cnss_diag_file:
            if isinstance(self.cnss_diag_file, list):
                self.cnss_diag_file = self.cnss_diag_file[0]
            if not os.path.isfile(self.cnss_diag_file):
                self.cnss_diag_file = os.path.join(
                    self.user_params[Config.key_config_path.value],
                    self.cnss_diag_file,
                )

        self.packet_logger: PacketCapture | None = None
        if self.enable_packet_log and self.packet_capture:
            self.packet_logger = self.packet_capture[0]
            self.packet_logger.configure_monitor_mode("2G", self.packet_log_2g)
            self.packet_logger.configure_monitor_mode("5G", self.packet_log_5g)

        for ad in self.android_devices:
            wutils.wifi_test_device_init(ad)
            if self.country_code_file:
                if isinstance(self.country_code_file, list):
                    self.country_code_file = self.country_code_file[0]
                if not os.path.isfile(self.country_code_file):
                    self.country_code_file = os.path.join(
                        self.user_params[Config.key_config_path.value],
                        self.country_code_file,
                    )
                self.country_code = utils.load_config(self.country_code_file)[
                    "country"
                ]
            else:
                self.country_code = WifiEnums.CountryCode.US
            wutils.set_wifi_country_code(ad, self.country_code)

    def setup_test(self) -> None:
        if self.android_devices and self.cnss_diag_file and self.pixel_models:
            wutils.start_cnss_diags(
                self.android_devices, self.cnss_diag_file, self.pixel_models
            )
        self.tcpdump_proc = []
        for ad in self.android_devices:
            proc = nutils.start_tcpdump(ad, self.current_test_info.name)
            self.tcpdump_proc.append((ad, proc))
        if self.packet_logger:
            self.packet_log_pid = wutils.start_pcap(
                self.packet_logger, "dual", self.current_test_info.name
            )

    def teardown_test(self) -> None:
        if self.android_devices and self.cnss_diag_file and self.pixel_models:
            wutils.stop_cnss_diags(self.android_devices, self.pixel_models)
            for proc in self.tcpdump_proc:
                nutils.stop_tcpdump(
                    proc[0],
                    proc[1],
                    self.current_test_info.name,
                    pull_dump=False,
                )
            self.tcpdump_proc = []
        if self.packet_logger and self.packet_log_pid:
            wutils.stop_pcap(
                self.packet_logger, self.packet_log_pid, test_status=True
            )
            self.packet_log_pid = {}

    def teardown_class(self) -> None:
        super().teardown_class()
        if hasattr(self, "fuchsia_devices"):
            for device in self.fuchsia_devices:
                device.take_bug_report()
        self.download_logs()

    def on_fail(self, record: TestResultRecord) -> None:
        """A function that is executed upon a test failure.

        Args:
        record: A copy of the test record for this test, containing all information of
            the test execution including exception objects.
        """
        if self.android_devices:
            for ad in self.android_devices:
                ad.take_bug_report(record.test_name, record.begin_time)
                ad.cat_adb_log(record.test_name, record.begin_time)
                wutils.get_ssrdumps(ad)
            if self.cnss_diag_file and self.pixel_models:
                wutils.stop_cnss_diags(self.android_devices, self.pixel_models)
                for ad in self.android_devices:
                    wutils.get_cnss_diag_log(ad)
            for proc in self.tcpdump_proc:
                nutils.stop_tcpdump(proc[0], proc[1], record.test_name)
            self.tcpdump_proc = []
        if self.packet_logger and self.packet_log_pid:
            wutils.stop_pcap(
                self.packet_logger, self.packet_log_pid, test_status=False
            )
            self.packet_log_pid = {}

        # Gets a wlan_device log and calls the generic device fail on DUT.
        for fd in self.fuchsia_devices:
            self.on_device_fail(fd, record)

    def on_device_fail(
        self, device: FuchsiaDevice, _: TestResultRecord
    ) -> None:
        """Gets a generic device DUT bug report.

        This method takes a bug report if the device has the
        'take_bug_report_on_fail' config value, and if the flag is true. This
        method also power cycles if 'hard_reboot_on_fail' is True.

        Args:
            device: Generic device to gather logs from.
            record: More information about the test.
        """
        if (
            not hasattr(device, "take_bug_report_on_fail")
            or device.take_bug_report_on_fail
        ):
            device.take_bug_report()

        if (
            hasattr(device, "hard_reboot_on_fail")
            and device.hard_reboot_on_fail
        ):
            device.reboot(reboot_type="hard", testbed_pdus=self.pdu_devices)

    def get_dut(self, association_mode: AssociationMode) -> SupportsWLAN:
        """Get the DUT based on user_params, default to Fuchsia."""
        device_type = self.user_params.get("dut", "fuchsia_devices")
        if device_type == "fuchsia_devices":
            return self.get_dut_type(FuchsiaDevice, association_mode)[1]
        elif device_type == "android_devices":
            return self.get_dut_type(FuchsiaDevice, association_mode)[1]
        else:
            raise signals.TestAbortClass(
                f'Invalid "dut" type specified in config: "{device_type}". '
                'Expected "fuchsia_devices" or "android_devices".'
            )

    def get_dut_type(
        self, device_type: type[_T], association_mode: AssociationMode
    ) -> tuple[_T, SupportsWLAN]:
        if device_type is FuchsiaDevice:
            if len(self.fuchsia_devices) == 0:
                raise signals.TestAbortClass(
                    "Requires at least one Fuchsia device"
                )
            fd = self.fuchsia_devices[0]
            assert isinstance(fd, device_type)
            return fd, FuchsiaWlanDevice(fd, association_mode)

        if device_type is AndroidDevice:
            if len(self.android_devices) == 0:
                raise signals.TestAbortClass(
                    "Requires at least one Android device"
                )
            ad = self.android_devices[0]
            assert isinstance(ad, device_type)
            return ad, AndroidWlanDevice(ad)

        raise signals.TestAbortClass(
            f"Invalid device_type specified: {device_type.__name__}. "
            "Expected FuchsiaDevice or AndroidDevice."
        )

    def download_logs(self) -> None:
        """Downloads the DHCP and hostapad logs from the access_point.

        Using the current TestClassContext and TestCaseContext this method pulls
        the DHCP and hostapd logs and outputs them to the correct path.
        """
        current_path = context.get_current_context().get_full_output_path()
        if hasattr(self, "access_points"):
            for access_point in self.access_points:
                access_point.download_ap_logs(current_path)
        if hasattr(self, "iperf_servers"):
            for iperf_server in self.iperf_servers:
                iperf_server.download_logs(current_path)

    def get_psk_network(
        self,
        mirror_ap: bool,
        reference_networks: list[NetworkList],
        hidden: bool = False,
        same_ssid: bool = False,
        security_mode: SecurityMode = SecurityMode.WPA2,
        ssid_length_2g: int = hostapd_constants.AP_SSID_LENGTH_2G,
        ssid_length_5g: int = hostapd_constants.AP_SSID_LENGTH_5G,
        passphrase_length_2g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_2G,
        passphrase_length_5g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_5G,
    ) -> NetworkList:
        """Generates SSID and passphrase for a WPA2 network using random
        generator.

        Args:
            mirror_ap: Determines if both APs use the same hostapd config or
                different configs.
            reference_networks: PSK networks.
            same_ssid: Determines if both bands on AP use the same SSID.
            ssid_length_2g: Number of characters to use for 2G SSID.
            ssid_length_5g: Number of characters to use for 5G SSID.
            passphrase_length_2g: Length of password for 2G network.
            passphrase_length_5g: Length of password for 5G network.

        Returns: A dict of 2G and 5G network lists for hostapd configuration.
        """
        if same_ssid:
            ref_2g_ssid = f"xg_{utils.rand_ascii_str(ssid_length_2g)}"
            ref_5g_ssid = ref_2g_ssid

            ref_2g_passphrase = utils.rand_ascii_str(passphrase_length_2g)
            ref_5g_passphrase = ref_2g_passphrase

        else:
            ref_2g_ssid = f"2g_{utils.rand_ascii_str(ssid_length_2g)}"
            ref_2g_passphrase = utils.rand_ascii_str(passphrase_length_2g)

            ref_5g_ssid = f"5g_{utils.rand_ascii_str(ssid_length_5g)}"
            ref_5g_passphrase = utils.rand_ascii_str(passphrase_length_5g)

        network_dict_2g = Network(
            SSID=ref_2g_ssid,
            security=security_mode,
            password=ref_2g_passphrase,
            hiddenSSID=hidden,
            wepKeys=None,
            ieee80211w=None,
        )

        network_dict_5g = Network(
            SSID=ref_5g_ssid,
            security=security_mode,
            password=ref_5g_passphrase,
            hiddenSSID=hidden,
            wepKeys=None,
            ieee80211w=None,
        )

        for _ in range(MAX_AP_COUNT):
            reference_networks.append(
                {
                    "2g": copy.copy(network_dict_2g),
                    "5g": copy.copy(network_dict_5g),
                }
            )
            if not mirror_ap:
                break
        return {"2g": network_dict_2g, "5g": network_dict_5g}

    def get_open_network(
        self,
        mirror_ap: bool,
        open_network: list[NetworkList],
        hidden: bool = False,
        same_ssid: bool = False,
        ssid_length_2g: int = hostapd_constants.AP_SSID_LENGTH_2G,
        ssid_length_5g: int = hostapd_constants.AP_SSID_LENGTH_5G,
        security_mode: SecurityMode = SecurityMode.OPEN,
    ) -> NetworkList:
        """Generates SSIDs for a open network using a random generator.

        Args:
            mirror_ap: Boolean, determines if both APs use the same hostapd
                       config or different configs.
            open_network: List of open networks.
            same_ssid: Boolean, determines if both bands on AP use the same
                       SSID.
            ssid_length_2g: Int, number of characters to use for 2G SSID.
            ssid_length_5g: Int, number of characters to use for 5G SSID.
            security_mode: 'none' for open and 'OWE' for WPA3 OWE.

        Returns: A dict of 2G and 5G network lists for hostapd configuration.

        """
        if same_ssid:
            open_2g_ssid = f"xg_{utils.rand_ascii_str(ssid_length_2g)}"
            open_5g_ssid = open_2g_ssid
        else:
            open_2g_ssid = f"2g_{utils.rand_ascii_str(ssid_length_2g)}"
            open_5g_ssid = f"5g_{utils.rand_ascii_str(ssid_length_5g)}"

        network_dict_2g = Network(
            SSID=open_2g_ssid,
            security=security_mode,
            password=None,
            hiddenSSID=hidden,
            wepKeys=None,
            ieee80211w=None,
        )

        network_dict_5g = Network(
            SSID=open_5g_ssid,
            security=security_mode,
            password=None,
            hiddenSSID=hidden,
            wepKeys=None,
            ieee80211w=None,
        )

        for _ in range(MAX_AP_COUNT):
            open_network.append(
                {
                    "2g": copy.copy(network_dict_2g),
                    "5g": copy.copy(network_dict_5g),
                }
            )
            if not mirror_ap:
                break
        return {"2g": network_dict_2g, "5g": network_dict_5g}

    def get_wep_network(
        self,
        mirror_ap: bool,
        networks: list[NetworkList],
        hidden: bool = False,
        same_ssid: bool = False,
        ssid_length_2g: int = hostapd_constants.AP_SSID_LENGTH_2G,
        ssid_length_5g: int = hostapd_constants.AP_SSID_LENGTH_5G,
        passphrase_length_2g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_2G,
        passphrase_length_5g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_5G,
    ) -> NetworkList:
        """Generates SSID and passphrase for a WEP network using random
        generator.

        Args:
            mirror_ap: Determines if both APs use the same hostapd config or
                different configs.
            networks: List of WEP networks.
            same_ssid: Determines if both bands on AP use the same SSID.
            ssid_length_2g: Number of characters to use for 2G SSID.
            ssid_length_5g: Number of characters to use for 5G SSID.
            passphrase_length_2g: Length of password for 2G network.
            passphrase_length_5g: Length of password for 5G network.

        Returns: A dict of 2G and 5G network lists for hostapd configuration.

        """
        if same_ssid:
            ref_2g_ssid = f"xg_{utils.rand_ascii_str(ssid_length_2g)}"
            ref_5g_ssid = ref_2g_ssid

            ref_2g_passphrase = utils.rand_hex_str(passphrase_length_2g)
            ref_5g_passphrase = ref_2g_passphrase

        else:
            ref_2g_ssid = f"2g_{utils.rand_ascii_str(ssid_length_2g)}"
            ref_2g_passphrase = utils.rand_hex_str(passphrase_length_2g)

            ref_5g_ssid = f"5g_{utils.rand_ascii_str(ssid_length_5g)}"
            ref_5g_passphrase = utils.rand_hex_str(passphrase_length_5g)

        network_dict_2g = Network(
            SSID=ref_2g_ssid,
            security=SecurityMode.WEP,
            password=None,
            hiddenSSID=hidden,
            wepKeys=[ref_2g_passphrase] * 4,
            ieee80211w=None,
        )

        network_dict_5g = Network(
            SSID=ref_5g_ssid,
            security=SecurityMode.WEP,
            password=None,
            hiddenSSID=hidden,
            wepKeys=[ref_5g_passphrase] * 4,
            ieee80211w=None,
        )

        for _ in range(MAX_AP_COUNT):
            networks.append(
                {
                    "2g": copy.copy(network_dict_2g),
                    "5g": copy.copy(network_dict_5g),
                }
            )
            if not mirror_ap:
                break
        return {"2g": network_dict_2g, "5g": network_dict_5g}

    def configure_openwrt_ap_and_start(
        self,
        channel_5g: int = hostapd_constants.AP_DEFAULT_CHANNEL_5G,
        channel_2g: int = hostapd_constants.AP_DEFAULT_CHANNEL_2G,
        channel_5g_ap2: int | None = None,
        channel_2g_ap2: int | None = None,
        ssid_length_2g: int = hostapd_constants.AP_SSID_LENGTH_2G,
        passphrase_length_2g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_2G,
        ssid_length_5g: int = hostapd_constants.AP_SSID_LENGTH_5G,
        passphrase_length_5g: int = hostapd_constants.AP_PASSPHRASE_LENGTH_5G,
        mirror_ap: bool = False,
        hidden: bool = False,
        same_ssid: bool = False,
        open_network: bool = False,
        wpa1_network: bool = False,
        wpa_network: bool = False,
        wep_network: bool = False,
        ent_network: bool = False,
        ent_network_pwd: bool = False,
        owe_network: bool = False,
        sae_network: bool = False,
        saemixed_network: bool = False,
        radius_conf_2g: dict[str, Any] | None = None,
        radius_conf_5g: dict[str, Any] | None = None,
        radius_conf_pwd: dict[str, Any] | None = None,
        ap_count: int = 1,
        ieee80211w: int | None = None,
    ) -> None:
        """Create, configure and start OpenWrt AP.

        Args:
            channel_5g: 5G channel to configure.
            channel_2g: 2G channel to configure.
            channel_5g_ap2: 5G channel to configure on AP2.
            channel_2g_ap2: 2G channel to configure on AP2.
            ssid_length_2g: Int, number of characters to use for 2G SSID.
            passphrase_length_2g: Int, length of password for 2G network.
            ssid_length_5g: Int, number of characters to use for 5G SSID.
            passphrase_length_5g: Int, length of password for 5G network.
            same_ssid: Boolean, determines if both bands on AP use the same SSID.
            open_network: Boolean, to check if open network should be configured.
            wpa_network: Boolean, to check if wpa network should be configured.
            wep_network: Boolean, to check if wep network should be configured.
            ent_network: Boolean, to check if ent network should be configured.
            ent_network_pwd: Boolean, to check if ent pwd network should be configured.
            owe_network: Boolean, to check if owe network should be configured.
            sae_network: Boolean, to check if sae network should be configured.
            saemixed_network: Boolean, to check if saemixed network should be configured.
            radius_conf_2g: dictionary with enterprise radius server details.
            radius_conf_5g: dictionary with enterprise radius server details.
            radius_conf_pwd: dictionary with enterprise radiuse server details.
            ap_count: APs to configure.
            ieee80211w:PMF to configure
        """
        if mirror_ap and ap_count == 1:
            raise ValueError("ap_count cannot be 1 if mirror_ap is True.")
        if (channel_5g_ap2 or channel_2g_ap2) and ap_count == 1:
            raise ValueError(
                "ap_count cannot be 1 if channels of AP2 are provided."
            )
        # we are creating a channel list for 2G and 5G bands. The list is of
        # size 2 and this is based on the assumption that each testbed will have
        # at most 2 APs.
        if not channel_5g_ap2:
            channel_5g_ap2 = channel_5g
        if not channel_2g_ap2:
            channel_2g_ap2 = channel_2g
        channels_2g = [channel_2g, channel_2g_ap2]
        channels_5g = [channel_5g, channel_5g_ap2]

        if radius_conf_2g is None:
            radius_conf_2g = {}
        if radius_conf_5g is None:
            radius_conf_5g = {}
        if radius_conf_pwd is None:
            radius_conf_pwd = {}

        self.bssid_map: list[BSSIDMap] = []
        for i in range(ap_count):
            configs: list[WirelessConfig] = []

            num_2g: int = 1
            num_5g: int = 1

            if wpa1_network:
                networks = self.get_psk_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    SecurityMode.WPA,
                    ssid_length_2g,
                    ssid_length_5g,
                    passphrase_length_2g,
                    passphrase_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.PSK,
                            band=band,
                            password=networks[band]["password"],
                            hidden=networks[band]["hiddenSSID"],
                            ieee80211w=ieee80211w,
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if wpa_network:
                networks = self.get_psk_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    SecurityMode.WPA2,
                    ssid_length_2g,
                    ssid_length_5g,
                    passphrase_length_2g,
                    passphrase_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.PSK2,
                            band=band,
                            password=networks[band]["password"],
                            hidden=networks[band]["hiddenSSID"],
                            ieee80211w=ieee80211w,
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if wep_network:
                networks = self.get_wep_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g,
                    ssid_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.WEP,
                            band=band,
                            wep_key=networks[band]["wepKeys"],
                            hidden=networks[band]["hiddenSSID"],
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if ent_network:
                networks = self.get_open_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g,
                    ssid_length_5g,
                    SecurityMode.WPA2,
                )

                def add_config_with_radius(
                    name: str,
                    band: str,
                    radius_conf: dict[str, str | int | None],
                ) -> None:
                    conf = MapValidator(radius_conf)
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.WPA2,
                            band=band,
                            radius_server_ip=conf.get(
                                str, "radius_server_ip", None
                            ),
                            radius_server_port=conf.get(
                                int, "radius_server_port", None
                            ),
                            radius_server_secret=conf.get(
                                str, "radius_server_secret", None
                            ),
                            hidden=networks[band]["hiddenSSID"],
                        )
                    )

                add_config_with_radius(
                    f"wifi_2g_{num_2g}",
                    hostapd_constants.BAND_2G,
                    radius_conf_2g,
                )
                add_config_with_radius(
                    f"wifi_5g_{num_5g}",
                    hostapd_constants.BAND_5G,
                    radius_conf_5g,
                )
                num_2g += 1
                num_5g += 1
            if ent_network_pwd:
                networks = self.get_open_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g,
                    ssid_length_5g,
                    SecurityMode.WPA2,
                )

                radius_conf = {} if radius_conf_pwd is None else radius_conf_pwd

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.WPA2,
                            band=band,
                            radius_server_ip=radius_conf.get(
                                "radius_server_ip"
                            ),
                            radius_server_port=radius_conf.get(
                                "radius_server_port"
                            ),
                            radius_server_secret=radius_conf.get(
                                "radius_server_secret"
                            ),
                            hidden=networks[band]["hiddenSSID"],
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if open_network:
                networks = self.get_open_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g,
                    ssid_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.NONE,
                            band=band,
                            hidden=networks[band]["hiddenSSID"],
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if owe_network:
                networks = self.get_open_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g,
                    ssid_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.OWE,
                            band=band,
                            hidden=networks[band]["hiddenSSID"],
                            ieee80211w=PMF_ENABLED,
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if sae_network:
                networks = self.get_psk_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g=ssid_length_2g,
                    ssid_length_5g=ssid_length_5g,
                    passphrase_length_2g=passphrase_length_2g,
                    passphrase_length_5g=passphrase_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.SAE,
                            band=band,
                            password=networks[band]["password"],
                            hidden=networks[band]["hiddenSSID"],
                            ieee80211w=PMF_ENABLED,
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1
            if saemixed_network:
                networks = self.get_psk_network(
                    mirror_ap,
                    [],
                    hidden,
                    same_ssid,
                    ssid_length_2g=ssid_length_2g,
                    ssid_length_5g=ssid_length_5g,
                    passphrase_length_2g=passphrase_length_2g,
                    passphrase_length_5g=passphrase_length_5g,
                )

                def add_config(name: str, band: str) -> None:
                    configs.append(
                        WirelessConfig(
                            name=name,
                            ssid=networks[band]["SSID"],
                            security=OpenWRTEncryptionMode.SAE_MIXED,
                            band=band,
                            password=networks[band]["password"],
                            hidden=networks[band]["hiddenSSID"],
                            ieee80211w=ieee80211w,
                        )
                    )

                add_config(f"wifi_2g_{num_2g}", hostapd_constants.BAND_2G)
                add_config(f"wifi_5g_{num_5g}", hostapd_constants.BAND_5G)
                num_2g += 1
                num_5g += 1

            openwrt_ap = self.openwrt_aps[i]
            openwrt_ap.configure_ap(configs, channels_2g[i], channels_5g[i])
            openwrt_ap.start_ap()
            self.bssid_map.append(openwrt_ap.get_bssids_for_wifi_networks())

            if mirror_ap:
                openwrt_ap_mirror = self.openwrt_aps[i + 1]
                openwrt_ap_mirror.configure_ap(
                    configs, channels_2g[i + 1], channels_5g[i + 1]
                )
                openwrt_ap_mirror.start_ap()
                self.bssid_map.append(
                    openwrt_ap_mirror.get_bssids_for_wifi_networks()
                )
                break
