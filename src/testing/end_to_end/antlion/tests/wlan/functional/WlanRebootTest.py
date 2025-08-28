#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
import logging
import os
import time
from dataclasses import dataclass
from enum import Enum, StrEnum, auto, unique

from antlion import utils
from antlion.controllers.ap_lib.hostapd_ap_preset import create_ap_preset
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_SSID_LENGTH_2G,
    BandType,
)
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.controllers.ap_lib.radvd_config import RadvdConfig
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import asserts, signals, test_runner
from mobly.records import TestResultRecord

DUT_NETWORK_CONNECTION_TIMEOUT = 60


@unique
class DeviceType(StrEnum):
    AP = auto()
    DUT = auto()


@unique
class RebootType(StrEnum):
    SOFT = auto()
    HARD = auto()


@unique
class IpVersionType(Enum):
    IPV4 = auto()
    IPV6 = auto()
    DUAL_IPV4_IPV6 = auto()

    def ipv4(self) -> bool:
        match self:
            case IpVersionType.IPV4:
                return True
            case IpVersionType.IPV6:
                return False
            case IpVersionType.DUAL_IPV4_IPV6:
                return True

    def ipv6(self) -> bool:
        match self:
            case IpVersionType.IPV4:
                return False
            case IpVersionType.IPV6:
                return True
            case IpVersionType.DUAL_IPV4_IPV6:
                return True

    @staticmethod
    def all() -> list["IpVersionType"]:
        return [
            IpVersionType.IPV4,
            IpVersionType.IPV6,
            IpVersionType.DUAL_IPV4_IPV6,
        ]


@dataclass
class TestParams:
    reboot_device: DeviceType
    reboot_type: RebootType
    band: BandType
    security_mode: SecurityMode
    ip_version: IpVersionType


class WlanRebootTest(base_test.WifiBaseTest):
    """Tests wlan reconnects in different reboot scenarios.

    Testbed Requirement:
    * One ACTS compatible device (dut)
    * One Whirlwind Access Point
    * One PduDevice
    """

    def pre_run(self) -> None:
        test_params: list[tuple[TestParams]] = []
        for (
            device_type,
            reboot_type,
            band,
            security_mode,
            ip_version,
        ) in itertools.product(
            # DeviceType,
            # RebootType,
            # BandType,
            # SecurityMode,
            # IpVersionType,
            #
            # TODO(https://github.com/python/mypy/issues/14688): Replace the code below
            # with the commented code above once the bug affecting StrEnum resolves.
            [e for e in DeviceType],
            [e for e in RebootType],
            [e for e in BandType],
            [SecurityMode.OPEN, SecurityMode.WPA2, SecurityMode.WPA3],
            [e for e in IpVersionType],
        ):
            test_params.append(
                (
                    TestParams(
                        device_type,
                        reboot_type,
                        band,
                        security_mode,
                        ip_version,
                    ),
                )
            )

        def generate_test_name(t: TestParams) -> str:
            test_name = (
                "test"
                f"_{t.reboot_type}_reboot"
                f"_{t.reboot_device}"
                f"_{t.band}"
                f"_{t.security_mode}"
            )
            if t.ip_version.ipv4():
                test_name += "_ipv4"
            if t.ip_version.ipv6():
                test_name += "_ipv6"
            return test_name

        self.generate_tests(
            test_logic=self.run_reboot_test,
            name_func=generate_test_name,
            arg_sets=test_params,
        )

    def setup_class(self) -> None:
        super().setup_class()
        self.log = logging.getLogger()

        if len(self.access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = self.access_points[0]

        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

    def setup_test(self) -> None:
        super().setup_test()
        self.access_point.stop_all_aps()
        self.dut.wifi_toggle_state(True)
        for ad in self.android_devices:
            ad.droid.wakeLockAcquireBright()
            ad.droid.wakeUpNow()
        self.dut.disconnect()
        if self.fuchsia_device:
            self.fuchsia_device.configure_wlan()

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)
        self.access_point.download_ap_logs(self.current_test_info.output_path)

    def teardown_test(self) -> None:
        # TODO(b/273923552): We take a snapshot here and before rebooting the
        # DUT for every test because the persistence component does not make the
        # inspect logs available for 120 seconds. This helps for debugging
        # issues where we need previous state.
        self.dut.take_bug_report(self.current_test_info.record)
        self.download_logs()
        self.access_point.stop_all_aps()
        self.dut.disconnect()
        for ad in self.android_devices:
            ad.droid.wakeLockRelease()
            ad.droid.goToSleepNow()
        self.dut.turn_location_off_and_scan_toggle_off()
        self.dut.reset_wifi()
        if self.fuchsia_device:
            self.fuchsia_device.deconfigure_wlan()
        super().teardown_test()

    def setup_ap(
        self,
        ssid: str,
        band: BandType,
        ip_version: IpVersionType,
        security_mode: SecurityMode,
        password: str | None = None,
    ) -> None:
        """Setup ap with basic config.

        Args:
            ssid: The ssid to setup on ap
            band: The type of band to set up the ap with ('2g' or '5g').
            ip_version: The type of ip to use (ipv4 or ipv6)
            security_mode: The type of security mode.
            password: The PSK or passphase.
        """
        # TODO(fxb/63719): Add varying AP parameters
        security_profile = Security(
            security_mode=security_mode, password=password
        )

        self.access_point.start_ap(
            hostapd_config=create_ap_preset(
                iface_wlan_2g=self.access_point.wlan_2g,
                iface_wlan_5g=self.access_point.wlan_5g,
                profile_name="whirlwind",
                channel=band.default_channel(),
                ssid=ssid,
                security=security_profile,
                # TODO(http://b/271628778): Remove ap_max_inactivity once
                # Fuchsia respects 802.11w (PMF) comeback-time.
                ap_max_inactivity=100 if band is BandType.BAND_5G else None,
            ),
            radvd_config=RadvdConfig() if ip_version.ipv6() else None,
        )

        if not ip_version.ipv4():
            self.access_point.stop_dhcp()

        self.log.info(f"Network (SSID: {ssid}) is up.")

    def ping_dut_to_ap(
        self,
        band: BandType,
        ip_version: IpVersionType,
    ) -> None:
        """Validate the DUT is pingable."""
        if band is BandType.BAND_2G:
            test_interface = self.access_point.wlan_2g
        elif band is BandType.BAND_5G:
            test_interface = self.access_point.wlan_5g

        if ip_version == IpVersionType.IPV4:
            ap_address = utils.get_addr(self.access_point.ssh, test_interface)
        elif ip_version == IpVersionType.IPV6:
            ap_address = utils.get_addr(
                self.access_point.ssh,
                test_interface,
                addr_type="ipv6_link_local",
            )
        else:
            raise TypeError(f"Invalid IP type: {ip_version}")

        if ap_address:
            if ip_version == IpVersionType.IPV4:
                ping_result = self.dut.ping(ap_address)
            else:
                ap_address = (
                    f"{ap_address}%{self.dut.get_default_wlan_test_interface()}"
                )
                ping_result = self.dut.ping(ap_address)
            if ping_result.success:
                self.log.info("Ping was successful.")
            else:
                raise signals.TestFailure(
                    f"Ping was unsuccessful: {ping_result}"
                )
        else:
            raise ConnectionError("Failed to retrieve APs ping address.")

    def prepare_dut_for_reconnection(self) -> None:
        """Perform any actions to ready DUT for reconnection.

        These actions will vary depending on the DUT. eg. android devices may
        need to be woken up, ambient devices should not require any interaction,
        etc.
        """
        self.dut.wifi_toggle_state(True)
        for ad in self.android_devices:
            ad.droid.wakeUpNow()

    def wait_for_dut_network_connection(self, ssid: str) -> None:
        """Checks if device is connected to given network. Sleeps 1 second
        between retries.

        Args:
            ssid: ssid to check connection to.
        Raises:
            ConnectionError, if DUT is not connected after all timeout.
        """
        self.log.info(
            f"Checking if DUT is connected to {ssid} network. Will retry for "
            f"{DUT_NETWORK_CONNECTION_TIMEOUT} seconds."
        )
        timeout = time.time() + DUT_NETWORK_CONNECTION_TIMEOUT
        while time.time() < timeout:
            try:
                is_connected = self.dut.is_connected(ssid=ssid)
            except Exception as err:
                self.log.debug(
                    f"SL4* call failed. Retrying in 1 second. Error: {err}"
                )
                is_connected = False
            finally:
                if is_connected:
                    self.log.info("Success: DUT has connected.")
                    break
                else:
                    self.log.debug(
                        f"DUT not connected to network {ssid}...retrying in 1 second."
                    )
                    time.sleep(1)
        else:
            raise ConnectionError("DUT failed to connect to the network.")

    def write_csv_time_to_reconnect(
        self,
        test_name: str,
        reconnect_success: bool,
        time_to_reconnect: float = 0.0,
    ) -> None:
        """Writes the time to reconnect to a csv file.
        Args:
            test_name: the name of the test case
            reconnect_success: whether the test successfully reconnected or not
            time_to_reconnect: the time from when the rebooted device came back
                up to when it reassociated (or 'FAIL'), if it failed to
                reconnect.
        """
        csv_file_name = os.path.join(self.log_path, "time_to_reconnect.csv")
        self.log.info(f"Writing to {csv_file_name}")
        with open(csv_file_name, "a") as csv_file:
            if reconnect_success:
                csv_file.write(f"{test_name},{time_to_reconnect}\n")
            else:
                csv_file.write(f"{test_name},'FAIL'\n")

    def log_and_continue(
        self,
        ssid: str,
        time_to_reconnect: float = 0.0,
        error: Exception | None = None,
    ) -> None:
        """Writes the time to reconnect to the csv file before continuing, used
        in stress tests runs.

        Args:
            time_to_reconnect: the time from when the rebooted device came back
                ip to when reassociation occurred.
            error: error message to log before continuing with the test
        """
        if error:
            self.log.info(
                f"Device failed to reconnect to network {ssid}. Error: {error}"
            )
            self.write_csv_time_to_reconnect(
                f"{self.current_test_info.name}", False
            )

        else:
            self.log.info(
                f"Device successfully reconnected to network {ssid} after "
                f"{time_to_reconnect} seconds."
            )
            self.write_csv_time_to_reconnect(
                f"{self.current_test_info.name}", True, time_to_reconnect
            )

    def run_reboot_test(self, settings: TestParams) -> None:
        """Runs a reboot test based on a given config.
            1. Setups up a network, associates the dut, and saves the network.
            2. Verifies the dut receives ip address(es).
            3. Verifies traffic between DUT and AP (ping)
            4. Reboots (hard or soft) the device (dut or ap).
                - If the ap was rebooted, setup the same network again.
            5. Wait for reassociation or timeout.
            6. If reassocation occurs:
                - Verifies the dut receives ip address(es).
                - Verifies traffic between DUT and AP (ping).
            7. Logs time to reconnect (or failure to reconnect)

        Args:
            settings: TestParams dataclass containing the following values:
                reboot_device: the device to reboot either DUT or AP.
                reboot_type: how to reboot the reboot_device either hard or soft.
                band: band to setup either 2g or 5g
                security_mode: security mode to set up either OPEN, WPA2, or WPA3.
                ip_version: the ip version (ipv4 or ipv6)
        """
        # TODO(b/286443517): Properly support WLAN on android devices.
        assert (
            self.fuchsia_device is not None
        ), "Fuchsia device not found, test currently does not support android devices."

        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_2G)
        reboot_device: DeviceType = settings.reboot_device
        reboot_type: RebootType = settings.reboot_type
        band: BandType = settings.band
        ip_version: IpVersionType = settings.ip_version
        security_mode: SecurityMode = settings.security_mode
        password: str | None = None
        if security_mode is not SecurityMode.OPEN:
            password = generate_random_password(security_mode=security_mode)

        # Skip hard reboots if no PDU present
        asserts.skip_if(
            reboot_type is RebootType.HARD and len(self.pdu_devices) == 0,
            "Hard reboots require a PDU device.",
        )

        self.setup_ap(
            ssid,
            band,
            ip_version,
            security_mode,
            password,
        )

        if not self.dut.associate(
            ssid,
            target_security=security_mode,
            target_pwd=password,
        ):
            raise EnvironmentError("Initial network connection failed.")

        test_interface = self.dut.get_default_wlan_test_interface()

        if ip_version.ipv4():
            self.fuchsia_device.wait_for_ipv4_addr(test_interface)
            self.ping_dut_to_ap(band, IpVersionType.IPV4)
        if ip_version.ipv6():
            self.fuchsia_device.wait_for_ipv6_addr(test_interface)
            self.ping_dut_to_ap(band, IpVersionType.IPV6)

        # TODO(b/273923552): We take a snapshot here and during test
        # teardown for every test because the persistence component does not
        # make the inspect logs available for 120 seconds. This helps for
        # debugging issues where we need previous state.
        self.dut.take_bug_report(self.current_test_info.record)

        # DUT reboots
        if reboot_device is DeviceType.DUT:
            if reboot_type is RebootType.SOFT:
                self.fuchsia_device.reboot()
            elif reboot_type is RebootType.HARD:
                self.dut.hard_power_cycle(self.pdu_devices)

        # AP reboots
        elif reboot_device is DeviceType.AP:
            if reboot_type is RebootType.SOFT:
                self.log.info("Cleanly stopping ap.")
                self.access_point.stop_all_aps()
            elif reboot_type is RebootType.HARD:
                self.access_point.hard_power_cycle(self.pdu_devices)
            self.setup_ap(ssid, band, ip_version, security_mode, password)

        self.prepare_dut_for_reconnection()
        uptime = time.time()
        try:
            try:
                self.wait_for_dut_network_connection(ssid)
            except ConnectionError as e:
                if (
                    reboot_device is DeviceType.DUT
                    and security_mode is SecurityMode.WPA3
                ):
                    # TODO(http://b/271628778): Remove this try/except statement
                    # once Fuchsia respects 802.11w (PMF) comeback-time.
                    raise signals.TestSkip(
                        f"Received expected ConnectionError due to http://b/271628778: {e}"
                    )
                raise e
            time_to_reconnect = time.time() - uptime

            if ip_version.ipv4():
                self.fuchsia_device.wait_for_ipv4_addr(test_interface)
                self.ping_dut_to_ap(band, IpVersionType.IPV4)
            if ip_version.ipv6():
                self.fuchsia_device.wait_for_ipv6_addr(test_interface)
                self.ping_dut_to_ap(band, IpVersionType.IPV6)

        except ConnectionError as err:
            self.log_and_continue(ssid, error=err)
            raise signals.TestFailure(
                f"Failed to reconnect to {ssid} after reboot."
            )
        else:
            self.log_and_continue(ssid, time_to_reconnect=time_to_reconnect)


if __name__ == "__main__":
    test_runner.main()
