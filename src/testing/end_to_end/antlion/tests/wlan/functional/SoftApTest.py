#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import multiprocessing as mp
import random
import time
from dataclasses import dataclass
from enum import Enum, StrEnum, auto, unique
from typing import Any, Mapping, Type, TypeAlias, TypeVar

from antlion import utils
from antlion.controllers import iperf_client, iperf_server
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.android_device import AndroidDevice
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.test_utils.abstract_devices.wlan_device import (
    AndroidWlanDevice,
    AssociationMode,
    FuchsiaWlanDevice,
    SupportsWLAN,
    create_wlan_device,
)
from fuchsia_wlan_base_test.deprecated.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectivityMode,
    OperatingBand,
    SecurityType,
)
from libs.ssh import settings
from libs.ssh.connection import SshConnection
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig

DEFAULT_AP_PROFILE = "whirlwind"
DEFAULT_IPERF_PORT = 5201
DEFAULT_TIMEOUT = 30
DEFAULT_IPERF_TIMEOUT = 60
DEFAULT_NO_ADDR_EXPECTED_TIMEOUT = 5
STATE_UP = True
STATE_DOWN = False

ConfigValue: TypeAlias = str | int | bool | list["ConfigValue"] | "Config"
Config: TypeAlias = dict[str, ConfigValue]

T = TypeVar("T")


def get_typed(
    map: Mapping[str, Any], key: str, value_type: Type[T], default: T
) -> T:
    value = map.get(key, default)
    if not isinstance(value, value_type):
        raise TypeError(
            f'"{key}" must be a {value_type.__name__}, got {type(value)}'
        )
    return value


@unique
class DeviceRole(Enum):
    AP = auto()
    CLIENT = auto()


@unique
class TestType(StrEnum):
    ASSOCIATE_ONLY = auto()
    ASSOCIATE_AND_PING = auto()
    ASSOCIATE_AND_PASS_TRAFFIC = auto()


@dataclass
class TestParams:
    test_type: TestType
    security_type: SecurityMode
    connectivity_mode: ConnectivityMode
    operating_band: OperatingBand
    ssid: str
    password: str
    iterations: int


@dataclass
class APParams:
    profile: str
    ssid: str
    channel: int
    security: Security
    password: str

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "APParams":
        security_mode_str = get_typed(
            d, "security_mode", str, SecurityMode.OPEN.value
        )
        security_mode = SecurityMode[security_mode_str]
        password = get_typed(
            d,
            "password",
            str,
            generate_random_password(security_mode=security_mode),
        )

        return APParams(
            profile=get_typed(d, "profile", str, DEFAULT_AP_PROFILE),
            ssid=get_typed(
                d,
                "ssid",
                str,
                utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G),
            ),
            channel=get_typed(
                d, "channel", int, hostapd_constants.AP_DEFAULT_CHANNEL_2G
            ),
            security=Security(security_mode, password),
            password=password,
        )

    def setup_ap(
        self, access_point: AccessPoint, timeout_sec: int = DEFAULT_TIMEOUT
    ) -> str:
        """Setup access_point and return the IPv4 address of its test interface."""
        setup_ap(
            access_point=access_point,
            profile_name=self.profile,
            channel=self.channel,
            ssid=self.ssid,
            security=self.security,
        )

        interface = (
            access_point.wlan_2g if self.channel < 36 else access_point.wlan_5g
        )

        end_time = time.time() + timeout_sec
        while time.time() < end_time:
            ips = utils.get_interface_ip_addresses(access_point.ssh, interface)
            if len(ips["ipv4_private"]) > 0:
                return ips["ipv4_private"][0]
            time.sleep(1)
        raise ConnectionError(
            f"After {timeout_sec}s, device {access_point.identifier} still does not have "
            f"an ipv4 address on interface {interface}."
        )


@dataclass
class SoftAPParams:
    ssid: str
    security_type: SecurityMode
    password: str | None
    connectivity_mode: ConnectivityMode
    operating_band: OperatingBand

    def __str__(self) -> str:
        if self.operating_band == OperatingBand.ANY:
            band = "any"
        elif self.operating_band == OperatingBand.ONLY_2_4GHZ:
            band = "2g"
        elif self.operating_band == OperatingBand.ONLY_5GHZ:
            band = "5g"
        else:
            raise TypeError(f'Unknown OperatingBand "{self.operating_band}"')
        return f'{band}_{self.security_type.replace("/", "_")}_{self.connectivity_mode}'

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "SoftAPParams":
        security_type = get_typed(
            d, "security_type", str, SecurityMode.OPEN.value
        )
        security_mode = SecurityMode[security_type]

        password = d.get("password")
        if password is None and security_mode != SecurityMode.OPEN:
            password = generate_random_password(security_mode=security_mode)
        if password is not None and not isinstance(password, str):
            raise TypeError(
                f'"password" must be a str or None, got {type(password)}'
            )
        if password is not None and security_mode == SecurityMode.OPEN:
            raise TypeError(
                f'"password" must be None if "security_type" is "{SecurityMode.OPEN}"'
            )

        connectivity_mode = get_typed(
            d, "connectivity_mode", str, str(ConnectivityMode.LOCAL_ONLY)
        )
        operating_band = get_typed(
            d, "operating_band", str, str(OperatingBand.ONLY_2_4GHZ)
        )

        return SoftAPParams(
            ssid=get_typed(
                d,
                "ssid",
                str,
                utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G),
            ),
            security_type=security_mode,
            password=password,
            connectivity_mode=ConnectivityMode[connectivity_mode],
            operating_band=OperatingBand[operating_band],
        )


@dataclass
class AssociationStressTestParams:
    test_type: TestType
    soft_ap_params: SoftAPParams
    iterations: int

    def __str__(self) -> str:
        return f"{self.soft_ap_params}_{self.test_type}_{self.iterations}_iterations"

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "AssociationStressTestParams":
        test_type = get_typed(
            d, "test_type", str, TestType.ASSOCIATE_AND_PASS_TRAFFIC.value
        )
        return AssociationStressTestParams(
            test_type=TestType[test_type],
            soft_ap_params=SoftAPParams.from_dict(d.get("soft_ap_params", {})),
            iterations=get_typed(d, "iterations", int, 10),
        )


@dataclass
class ClientModeAlternatingTestParams:
    ap_params: APParams
    soft_ap_params: SoftAPParams
    iterations: int

    def __str__(self) -> str:
        return (
            f"ap_{self.ap_params.security.security_mode}_"
            f"soft_ap_{self.soft_ap_params.security_type}_"
            f"{self.iterations}_iterations"
        )

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "ClientModeAlternatingTestParams":
        return ClientModeAlternatingTestParams(
            ap_params=APParams.from_dict(d.get("ap_params", {})),
            soft_ap_params=SoftAPParams.from_dict(d.get("soft_ap_params", {})),
            iterations=get_typed(d, "iterations", int, 10),
        )


@dataclass
class ToggleTestParams:
    soft_ap_params: SoftAPParams
    iterations: int

    def __str__(self) -> str:
        return f"{self.soft_ap_params}_{self.iterations}_iterations"

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "ToggleTestParams":
        return ToggleTestParams(
            soft_ap_params=SoftAPParams.from_dict(d.get("soft_ap_params", {})),
            iterations=get_typed(d, "iterations", int, 10),
        )


@dataclass
class ClientModeToggleTestParams:
    ap_params: APParams
    iterations: int

    def __str__(self) -> str:
        return f"{self.ap_params}_{self.iterations}_iterations"

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "ClientModeToggleTestParams":
        return ClientModeToggleTestParams(
            ap_params=APParams.from_dict(d.get("ap_params", {})),
            iterations=get_typed(d, "iterations", int, 10),
        )


class StressTestIterationFailure(Exception):
    """Used to differentiate a subtest failure from an actual exception"""


class SoftApTest(base_test.WifiBaseTest):
    """Tests for Fuchsia SoftAP

    Testbed requirement:
    * One Fuchsia device
    * At least one client (Android) device
        * For multi-client tests, at least two client (Android) devices are
          required. Test will be skipped if less than two client devices are
          present.
    * For any tests that exercise client-mode (e.g. toggle tests, simultaneous
        tests), a physical AP (whirlwind) is also required. Those tests will be
        skipped if physical AP is not present.
    """

    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()
        self.soft_ap_test_params = configs.user_params.get(
            "soft_ap_test_params", {}
        )

    def pre_run(self) -> None:
        self.generate_soft_ap_tests()
        self.generate_association_stress_tests()
        self.generate_soft_ap_and_client_mode_alternating_stress_tests()
        self.generate_soft_ap_toggle_stress_tests()
        self.generate_client_mode_toggle_stress_tests()
        self.generate_soft_ap_toggle_stress_with_client_mode_tests()
        self.generate_client_mode_toggle_stress_with_soft_ap_tests()
        self.generate_soft_ap_and_client_mode_random_toggle_stress_tests()

    def generate_soft_ap_tests(self) -> None:
        tests: list[SoftAPParams] = []

        for operating_band in OperatingBand:
            for security_mode in [
                SecurityMode.OPEN,
                SecurityMode.WEP,
                SecurityMode.WPA,
                SecurityMode.WPA2,
                SecurityMode.WPA3,
            ]:
                for connectivity_mode in ConnectivityMode:
                    if security_mode == SecurityMode.OPEN:
                        ssid_length = hostapd_constants.AP_SSID_LENGTH_2G
                        password = None
                    else:
                        ssid_length = hostapd_constants.AP_SSID_LENGTH_5G
                        password = generate_random_password()

                    tests.append(
                        SoftAPParams(
                            ssid=utils.rand_ascii_str(ssid_length),
                            security_type=security_mode,
                            password=password,
                            connectivity_mode=connectivity_mode,
                            operating_band=operating_band,
                        )
                    )

        def generate_name(test: SoftAPParams) -> str:
            return f"test_soft_ap_{test}"

        self.generate_tests(
            self.associate_with_soft_ap_test,
            generate_name,
            tests,
        )

    def associate_with_soft_ap_test(self, soft_ap_params: SoftAPParams) -> None:
        self.start_soft_ap(soft_ap_params)
        self.associate_with_soft_ap(self.primary_client, soft_ap_params)
        self.assert_connected_to_ap(
            self.primary_client, self.dut, check_traffic=True
        )

    def setup_class(self) -> None:
        super().setup_class()
        self.fuchsia_device, self.dut = self.get_dut_type(
            FuchsiaDevice, AssociationMode.POLICY
        )

        # TODO(fxb/51313): Add in device agnosticity for clients
        # Create a wlan device and iperf client for each Android client
        self.clients: list[SupportsWLAN] = []
        self.iperf_clients_map: dict[Any, Any] = {}
        for device in self.android_devices:
            client_wlan_device = create_wlan_device(
                device, AssociationMode.POLICY
            )
            self.clients.append(client_wlan_device)
            self.iperf_clients_map[
                client_wlan_device
            ] = client_wlan_device.create_iperf_client()
        self.primary_client = self.clients[0]

        # Create an iperf server on the DUT, which will be used for any streaming.
        self.iperf_server_settings = settings.from_config(
            {
                "user": self.fuchsia_device.ssh_username,
                "host": self.fuchsia_device.ip,
                "ssh_config": self.fuchsia_device.ssh_config,
            }
        )
        self.iperf_server = iperf_server.IPerfServerOverSsh(
            self.iperf_server_settings,
            DEFAULT_IPERF_PORT,
            test_interface=self.dut.get_default_wlan_test_interface(),
            use_killall=True,
        )
        self.iperf_server.start()

        # Attempt to create an ap iperf server. AP is only required for tests
        # that use client mode.
        self.access_point: AccessPoint | None = None
        self.ap_iperf_client: iperf_client.IPerfClientOverSsh | None = None

        try:
            self.access_point = self.access_points[0]
            self.ap_iperf_client = iperf_client.IPerfClientOverSsh(
                self.access_point.ssh_provider,
                # Date is already synced by the AccessPoint controller.
                sync_date=False,
            )
            self.iperf_clients_map[self.access_point] = self.ap_iperf_client
        except AttributeError:
            pass

    def teardown_class(self) -> None:
        # Because this is using killall, it will stop all iperf processes
        self.iperf_server.stop()
        super().teardown_class()

    def setup_test(self) -> None:
        super().setup_test()
        for ad in self.android_devices:
            ad.droid.wakeLockAcquireBright()
            ad.droid.wakeUpNow()
        for client in self.clients:
            client.disconnect()
            client.reset_wifi()
            client.wifi_toggle_state(True)
        self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.stop_all()
        if self.access_point:
            self.access_point.stop_all_aps()
        self.dut.disconnect()

    def teardown_test(self) -> None:
        for client in self.clients:
            client.disconnect()
        for ad in self.android_devices:
            ad.droid.wakeLockRelease()
            ad.droid.goToSleepNow()
        self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.stop_all()
        self.download_logs()
        if self.access_point:
            self.access_point.stop_all_aps()
        self.dut.disconnect()
        super().teardown_test()

    def start_soft_ap(self, params: SoftAPParams) -> None:
        """Starts a softAP on Fuchsia device.

        Args:
            settings: a dict containing softAP configuration params
                ssid: string, SSID of softAP network
                security_type: string, security type of softAP network
                    - 'none', 'wep', 'wpa', 'wpa2', 'wpa3'
                password: string, password if applicable
                connectivity_mode: string, connecitivity_mode for softAP
                    - 'local_only', 'unrestricted'
                operating_band: string, band for softAP network
                    - 'any', 'only_5_ghz', 'only_2_4_ghz'
        """
        self.log.info(f"Starting SoftAP on DUT with settings: {params}")
        self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.start(
            params.ssid,
            SecurityType(params.security_type.fuchsia_security_type()),
            params.password,
            params.connectivity_mode,
            params.operating_band,
        )
        self.log.info(f"SoftAp network ({params.ssid}) is up.")

    def associate_with_soft_ap(
        self, device: SupportsWLAN, params: SoftAPParams
    ) -> None:
        """Associates client device with softAP on Fuchsia device.

        Args:
            device: wlan_device to associate with the softAP
            params: soft AP configuration

        Raises:
            TestFailure if association fails
        """
        self.log.info(
            f'Associating {device.identifier} to SoftAP on {self.dut.identifier} called "{params.ssid}'
        )

        associated = device.associate(
            params.ssid,
            target_pwd=params.password,
            target_security=params.security_type,
            check_connectivity=params.connectivity_mode
            == ConnectivityMode.UNRESTRICTED,
        )

        asserts.assert_true(
            associated,
            f'Failed to associate "{device.identifier}" to SoftAP "{params.ssid}"',
        )

    def disconnect_from_soft_ap(self, device: SupportsWLAN) -> None:
        """Disconnects client device from SoftAP.

        Args:
            device: wlan_device to disconnect from SoftAP
        """
        self.log.info(f"Disconnecting device {device.identifier} from SoftAP.")
        device.disconnect()

    def get_ap_test_interface(self, ap: AccessPoint, channel: int) -> str:
        if channel < 36:
            return ap.wlan_2g
        else:
            return ap.wlan_5g

    def get_device_test_interface(
        self, device: SupportsWLAN | FuchsiaDevice, role: DeviceRole
    ) -> str:
        """Retrieves test interface from a provided device, which can be the
        FuchsiaDevice DUT, the AccessPoint, or an AndroidClient.

        Args:
            device: the device do get the test interface from. Either
                FuchsiaDevice (DUT), Android client, or AccessPoint.
            role: str, either "client" or "ap". Required for FuchsiaDevice (DUT)

        Returns:
            String, name of test interface on given device.
        """

        if isinstance(device, FuchsiaDevice):
            device.update_wlan_interfaces()
            if role == DeviceRole.CLIENT:
                if device.wlan_client_test_interface_name is None:
                    raise TypeError(
                        "Expected wlan_client_test_interface_name to be str"
                    )
                return device.wlan_client_test_interface_name
            if role == DeviceRole.AP:
                if device.wlan_ap_test_interface_name is None:
                    raise TypeError(
                        "Expected wlan_ap_test_interface_name to be str"
                    )
                return device.wlan_ap_test_interface_name
            raise ValueError(f"Unsupported interface role: {role}")
        else:
            return device.get_default_wlan_test_interface()

    def wait_for_ipv4_address(
        self,
        device: SupportsWLAN | AccessPoint,
        interface_name: str,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> str:
        """Waits for interface on a wlan_device to get an ipv4 address.

        Args:
            device: wlan_device or AccessPoint to check interface
            interface_name: name of the interface to check
            timeout: seconds to wait before raising an error

        Returns:
            The IP address of interface_name.

        Raises:
            ConnectionError, if interface does not have an ipv4 address after timeout
        """
        comm_channel: SshConnection | FuchsiaDevice | AndroidDevice
        if isinstance(device, AccessPoint):
            comm_channel = device.ssh
        elif isinstance(device, FuchsiaWlanDevice):
            comm_channel = device.device
        elif isinstance(device, AndroidWlanDevice):
            comm_channel = device.device
        else:
            raise TypeError(f"Invalid device type {type(device)}")

        end_time = time.time() + timeout
        while time.time() < end_time:
            ips = utils.get_interface_ip_addresses(comm_channel, interface_name)
            if len(ips["ipv4_private"]) > 0:
                self.log.info(
                    f"Device {device.identifier} interface {interface_name} has "
                    f"ipv4 address {ips['ipv4_private'][0]}"
                )
                return ips["ipv4_private"][0]
            else:
                time.sleep(1)
        raise ConnectionError(
            f"After {timeout} seconds, device {device.identifier} still does not have "
            f"an ipv4 address on interface {interface_name}."
        )

    def run_iperf_traffic(
        self,
        ip_client: iperf_client.IPerfClientOverAdb
        | iperf_client.IPerfClientOverSsh,
        server_address: str,
        server_port: int = 5201,
    ) -> None:
        """Runs traffic between client and ap an verifies throughput.

        Args:
            ip_client: iperf client to use
            server_address: ipv4 address of the iperf server to use
            server_port: port of the iperf server

        Raises:
            ConnectionError if no traffic passes in either direction
        """
        ip_client_identifier = self.get_iperf_client_identifier(ip_client)

        self.log.info(
            f"Running traffic from iperf client {ip_client_identifier} to "
            f"iperf server {server_address}."
        )
        client_to_ap_path = ip_client.start(
            server_address,
            f"-i 1 -t 10 -J -p {server_port}",
            "client_to_soft_ap",
        )

        client_to_ap_result = iperf_server.IPerfResult(client_to_ap_path)
        if not client_to_ap_result.avg_receive_rate:
            raise ConnectionError(
                f"Failed to pass traffic from iperf client {ip_client_identifier} to "
                f"iperf server {server_address}."
            )

        self.log.info(
            f"Passed traffic from iperf client {ip_client_identifier} to "
            f"iperf server {server_address} with avg rate of "
            f"{client_to_ap_result.avg_receive_rate} MB/s."
        )

        self.log.info(
            f"Running traffic from iperf server {server_address} to "
            f"iperf client {ip_client_identifier}."
        )
        ap_to_client_path = ip_client.start(
            server_address,
            f"-i 1 -t 10 -R -J -p {server_port}",
            "soft_ap_to_client",
        )

        ap_to_client_result = iperf_server.IPerfResult(ap_to_client_path)
        if not ap_to_client_result.avg_receive_rate:
            raise ConnectionError(
                f"Failed to pass traffic from iperf server {server_address} to "
                f"iperf client {ip_client_identifier}."
            )

        self.log.info(
            f"Passed traffic from iperf server {server_address} to "
            f"iperf client {ip_client_identifier} with avg rate of "
            f"{ap_to_client_result.avg_receive_rate} MB/s."
        )

    def run_iperf_traffic_parallel_process(
        self,
        ip_client: iperf_client.IPerfClientOverAdb
        | iperf_client.IPerfClientOverSsh,
        server_address: str,
        error_queue: "mp.Queue[str]",
        server_port: int = 5201,
    ) -> None:
        """Executes run_iperf_traffic using a queue to capture errors. Used
        when running iperf in a parallel process.

        Args:
            ip_client: iperf client to use
            server_address: ipv4 address of the iperf server to use
            error_queue: multiprocessing queue to capture errors
            server_port: port of the iperf server
        """
        try:
            self.run_iperf_traffic(
                ip_client, server_address, server_port=server_port
            )
        except ConnectionError as err:
            error_queue.put(
                f"In iperf process from {self.get_iperf_client_identifier(ip_client)} to {server_address}: {err}"
            )

    def get_iperf_client_identifier(
        self,
        ip_client: iperf_client.IPerfClientOverAdb
        | iperf_client.IPerfClientOverSsh,
    ) -> str:
        """Retrieves an identifier string from iperf client, for logging.

        Args:
            ip_client: iperf client to grab identifier from
        """
        if type(ip_client) == iperf_client.IPerfClientOverAdb:
            assert hasattr(ip_client._android_device, "serial")
            assert isinstance(ip_client._android_device.serial, str)
            return ip_client._android_device.serial
        if type(ip_client) == iperf_client.IPerfClientOverSsh:
            return ip_client._ssh_provider.config.host_name
        raise TypeError(f'Unknown "ip_client" type {type(ip_client)}')

    def assert_connected_to_ap(
        self,
        client: SupportsWLAN,
        ap: SupportsWLAN | AccessPoint,
        channel: int | None = None,
        check_traffic: bool = False,
        timeout_sec: int = DEFAULT_TIMEOUT,
    ) -> None:
        """Assert the client device has L3 connectivity to the AP."""
        device_interface = self.get_device_test_interface(
            client, DeviceRole.CLIENT
        )

        if isinstance(ap, AccessPoint):
            if channel is None:
                raise TypeError(
                    "channel must not be None when ap is an AccessPoint"
                )
            ap_interface = self.get_ap_test_interface(ap, channel)
        else:
            ap_interface = self.get_device_test_interface(ap, DeviceRole.AP)

        client_ipv4 = self.wait_for_ipv4_address(
            client, device_interface, timeout=timeout_sec
        )
        ap_ipv4 = self.wait_for_ipv4_address(
            ap, ap_interface, timeout=timeout_sec
        )

        client_ping = client.ping(ap_ipv4, timeout=DEFAULT_TIMEOUT * 1000)
        asserts.assert_true(
            client_ping.success,
            f"Failed to ping from client to ap: {client_ping}",
        )

        ap_ping = ap.ping(client_ipv4, timeout=DEFAULT_TIMEOUT * 1000)
        asserts.assert_true(
            ap_ping.success,
            f"Failed to ping from ap to client: {ap_ping}",
        )

        if not check_traffic:
            return

        if client is self.dut:
            self.run_iperf_traffic(self.iperf_clients_map[ap], client_ipv4)
        else:
            self.run_iperf_traffic(self.iperf_clients_map[client], ap_ipv4)

    def assert_disconnected_to_ap(
        self,
        client: SupportsWLAN,
        ap: SupportsWLAN | AccessPoint,
        channel: int | None = None,
        timeout_sec: int = DEFAULT_NO_ADDR_EXPECTED_TIMEOUT,
    ) -> None:
        """Assert the client device does not have ping connectivity to the AP."""
        device_interface = self.get_device_test_interface(
            client, DeviceRole.CLIENT
        )

        if isinstance(ap, AccessPoint):
            if channel is None:
                raise TypeError(
                    "channel must not be None when ap is an AccessPoint"
                )
            ap_interface = self.get_ap_test_interface(ap, channel)
        else:
            ap_interface = self.get_device_test_interface(ap, DeviceRole.AP)

        try:
            client_ipv4 = self.wait_for_ipv4_address(
                client, device_interface, timeout=timeout_sec
            )
            ap_ipv4 = self.wait_for_ipv4_address(
                ap, ap_interface, timeout=timeout_sec
            )
        except ConnectionError:
            # When disconnected, IP addresses aren't always available.
            return

        asserts.assert_false(
            client.ping(ap_ipv4, timeout=DEFAULT_TIMEOUT * 1000).success,
            "Unexpectedly succeeded to ping from client to ap",
        )
        asserts.assert_false(
            ap.ping(client_ipv4, timeout=DEFAULT_TIMEOUT * 1000).success,
            "Unexpectedly succeeded to ping from ap to client",
        )

    # Runners for Generated Test Cases

    def run_soft_ap_association_stress_test(
        self, test: AssociationStressTestParams
    ) -> None:
        """Sets up a SoftAP, and repeatedly associates and disassociates a client."""
        self.log.info(
            f"Running association stress test type {test.test_type} in "
            f"iteration {test.iterations} times"
        )

        self.start_soft_ap(test.soft_ap_params)

        passed_count = 0
        for run in range(test.iterations):
            try:
                self.log.info(f"Starting SoftAp association run {str(run + 1)}")

                if test.test_type == TestType.ASSOCIATE_ONLY:
                    self.associate_with_soft_ap(
                        self.primary_client, test.soft_ap_params
                    )

                elif test.test_type == TestType.ASSOCIATE_AND_PING:
                    self.associate_with_soft_ap(
                        self.primary_client, test.soft_ap_params
                    )
                    self.assert_connected_to_ap(self.primary_client, self.dut)

                elif test.test_type == TestType.ASSOCIATE_AND_PASS_TRAFFIC:
                    self.associate_with_soft_ap(
                        self.primary_client, test.soft_ap_params
                    )
                    self.assert_connected_to_ap(
                        self.primary_client, self.dut, check_traffic=True
                    )

                else:
                    raise AttributeError(f"Invalid test type: {test.test_type}")

            except signals.TestFailure as err:
                self.log.error(
                    f"SoftAp association stress run {str(run + 1)} failed. "
                    f"Err: {err.details}"
                )
            else:
                self.log.info(
                    f"SoftAp association stress run {str(run + 1)} successful."
                )
                passed_count += 1

        if passed_count < test.iterations:
            asserts.fail(
                "SoftAp association stress test failed after "
                f"{passed_count}/{test.iterations} runs."
            )

        asserts.explicit_pass(
            f"SoftAp association stress test passed after {passed_count}/{test.iterations} "
            "runs."
        )

    # Alternate SoftAP and Client mode test

    def run_soft_ap_and_client_mode_alternating_test(
        self, test: ClientModeAlternatingTestParams
    ) -> None:
        """Runs a single soft_ap and client alternating stress test.

        See test_soft_ap_and_client_mode_alternating_stress for details.
        """
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        test.ap_params.setup_ap(self.access_point)

        for _ in range(test.iterations):
            # Toggle SoftAP on then off.
            self.toggle_soft_ap(test.soft_ap_params, STATE_DOWN)
            self.toggle_soft_ap(test.soft_ap_params, STATE_UP)

            # Toggle client mode on then off.
            self.toggle_client_mode(
                self.access_point, test.ap_params, STATE_DOWN
            )
            self.toggle_client_mode(self.access_point, test.ap_params, STATE_UP)

    # Toggle Stress Test Helper Functions

    # Stress Test Toggle Functions

    def start_soft_ap_and_verify_connected(
        self, client: SupportsWLAN, soft_ap_params: SoftAPParams
    ) -> None:
        """Sets up SoftAP, associates a client, then verifies connection.

        Args:
            client: SoftApClient, client to use to verify SoftAP
            soft_ap_params: dict, containing parameters to setup softap

        Raises:
            StressTestIterationFailure, if toggle occurs, but connection
            is not functioning as expected
        """
        # Change SSID every time, to avoid client connection issues.
        soft_ap_params.ssid = utils.rand_ascii_str(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        self.start_soft_ap(soft_ap_params)
        self.associate_with_soft_ap(client, soft_ap_params)
        self.assert_connected_to_ap(client, self.dut)

    def stop_soft_ap_and_verify_disconnected(
        self, client: SupportsWLAN, soft_ap_params: SoftAPParams
    ) -> None:
        """Tears down SoftAP, and verifies connection is down.

        Args:
            client: SoftApClient, client to use to verify SoftAP
            soft_ap_params: dict, containing parameters of SoftAP to teardown

        Raise:
            EnvironmentError, if client and AP can still communicate
        """
        self.log.info("Stopping SoftAP on DUT.")
        self.fuchsia_device.honeydew_fd.wlan_policy_ap_deprecated_sync.stop(
            soft_ap_params.ssid,
            SecurityType(soft_ap_params.security_type.fuchsia_security_type()),
            soft_ap_params.password,
        )
        self.assert_disconnected_to_ap(client, self.dut)

    def start_client_mode_and_verify_connected(
        self, access_point: AccessPoint, ap_params: APParams
    ) -> None:
        """Connects DUT to AP in client mode and verifies connection

        Args:
            ap_params: dict, containing parameters of the AP network

        Raises:
            EnvironmentError, if DUT fails to associate altogether
            StressTestIterationFailure, if DUT associates but connection is not
                functioning as expected.
        """
        self.log.info(f"Associating DUT with AP network: {ap_params.ssid}")
        associated = self.dut.associate(
            ap_params.ssid,
            ap_params.security.security_mode,
            target_pwd=ap_params.password,
        )
        if not associated:
            raise EnvironmentError("Failed to associate DUT in client mode.")
        else:
            self.log.info("Association successful.")

        self.assert_connected_to_ap(
            self.dut, access_point, channel=ap_params.channel
        )

    def stop_client_mode_and_verify_disconnected(
        self, access_point: AccessPoint, ap_params: APParams
    ) -> None:
        """Disconnects DUT from AP and verifies connection is down.

        Args:
            ap_params: containing parameters of the AP network

        Raises:
            EnvironmentError, if DUT and AP can still communicate
        """
        self.log.info("Disconnecting DUT from AP.")
        self.dut.disconnect()
        self.assert_disconnected_to_ap(
            self.dut, access_point, channel=ap_params.channel
        )

    # Toggle Stress Test Iteration and Pre-Test Functions

    # SoftAP Toggle Stress Test Helper Functions

    def soft_ap_toggle_test(self, test: ToggleTestParams) -> None:
        current_state = STATE_DOWN
        for i in range(test.iterations):
            self.toggle_soft_ap(test.soft_ap_params, current_state)
            current_state = not current_state

    def toggle_soft_ap(
        self, soft_ap_params: SoftAPParams, current_state: bool
    ) -> None:
        """Runs a single iteration of SoftAP toggle stress test

        Args:
            settings: dict, containing test settings
            current_state: bool, current state of SoftAP (True if up,
                else False)

        Raises:
            StressTestIterationFailure, if toggle occurs but mode isn't
                functioning correctly.
            EnvironmentError, if toggle fails to occur at all
        """
        self.log.info(f"Toggling SoftAP {'down' if current_state else 'up'}.")
        if current_state == STATE_DOWN:
            self.start_soft_ap_and_verify_connected(
                self.primary_client, soft_ap_params
            )
        else:
            self.stop_soft_ap_and_verify_disconnected(
                self.primary_client, soft_ap_params
            )

    # Client Mode Toggle Stress Test Helper Functions

    def client_mode_toggle_test(self, test: ClientModeToggleTestParams) -> None:
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        test.ap_params.setup_ap(self.access_point)

        current_state = STATE_DOWN
        for i in range(test.iterations):
            self.log.info(
                f"Iteration {i}: toggling client mode {'off' if current_state else 'on'}."
            )
            self.toggle_client_mode(
                self.access_point, test.ap_params, current_state
            )
            current_state = not current_state

    def toggle_client_mode(
        self,
        access_point: AccessPoint,
        ap_params: APParams,
        current_state: bool,
    ) -> None:
        if current_state == STATE_DOWN:
            self.start_client_mode_and_verify_connected(access_point, ap_params)
        else:
            self.stop_client_mode_and_verify_disconnected(
                access_point, ap_params
            )

    # TODO: Remove
    def client_mode_toggle_test_iteration(
        self,
        test: ClientModeToggleTestParams,
        access_point: AccessPoint,
        current_state: bool,
    ) -> None:
        """Runs a single iteration of client mode toggle stress test

        Args:
            settings: dict, containing test settings
            current_state: bool, current state of client mode (True if up,
                else False)

        Raises:
            StressTestIterationFailure, if toggle occurs but mode isn't
                functioning correctly.
            EnvironmentError, if toggle fails to occur at all
        """
        self.log.info(
            f"Toggling client mode {'off' if current_state else 'on'}"
        )
        if current_state == STATE_DOWN:
            self.start_client_mode_and_verify_connected(
                access_point, test.ap_params
            )
        else:
            self.stop_client_mode_and_verify_disconnected(
                access_point, test.ap_params
            )

    # Toggle SoftAP with Client Mode Up Test Helper Functions

    def soft_ap_toggle_with_client_mode_test(
        self, test: ClientModeAlternatingTestParams
    ) -> None:
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        test.ap_params.setup_ap(self.access_point)
        self.start_client_mode_and_verify_connected(
            self.access_point, test.ap_params
        )

        current_state = STATE_DOWN
        for i in range(test.iterations):
            self.toggle_soft_ap(test.soft_ap_params, current_state)
            self.assert_connected_to_ap(
                self.dut, self.access_point, channel=test.ap_params.channel
            )
            current_state = not current_state

    # Toggle Client Mode with SoftAP Up Test Helper Functions

    def client_mode_toggle_with_soft_ap_test(
        self, test: ClientModeAlternatingTestParams
    ) -> None:
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        test.ap_params.setup_ap(self.access_point)
        self.start_soft_ap_and_verify_connected(
            self.primary_client, test.soft_ap_params
        )

        current_state = STATE_DOWN
        for i in range(test.iterations):
            self.toggle_client_mode(
                self.access_point, test.ap_params, current_state
            )
            self.assert_connected_to_ap(self.primary_client, self.dut)
            current_state = not current_state

    # Toggle SoftAP and Client Mode Randomly

    def soft_ap_and_client_mode_random_toggle_test(
        self, test: ClientModeAlternatingTestParams
    ) -> None:
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        test.ap_params.setup_ap(self.access_point)

        current_soft_ap_state = STATE_DOWN
        current_client_mode_state = STATE_DOWN
        for i in range(test.iterations):
            # Randomly determine if softap, client mode, or both should
            # be toggled.
            rand_toggle_choice = random.randrange(0, 3)
            if rand_toggle_choice <= 1:
                self.toggle_soft_ap(test.soft_ap_params, current_soft_ap_state)
                current_soft_ap_state = not current_soft_ap_state
            if rand_toggle_choice >= 1:
                self.toggle_client_mode(
                    self.access_point, test.ap_params, current_client_mode_state
                )
                current_client_mode_state = not current_client_mode_state

            if current_soft_ap_state == STATE_UP:
                self.assert_connected_to_ap(self.primary_client, self.dut)
            else:
                self.assert_disconnected_to_ap(self.primary_client, self.dut)

            if current_client_mode_state == STATE_UP:
                self.assert_connected_to_ap(
                    self.dut, self.access_point, channel=test.ap_params.channel
                )
            else:
                self.assert_disconnected_to_ap(
                    self.dut, self.access_point, channel=test.ap_params.channel
                )

    # Test Cases

    def test_multi_client(self) -> None:
        """Tests multi-client association with a single soft AP network.

        This tests associates a variable length list of clients, verfying it can
        can ping the SoftAP and pass traffic, and then verfies all previously
        associated clients can still ping and pass traffic.

        The same occurs in reverse for disassocations.

        SoftAP parameters can be changed from default via ACTS config:
        Example Config
        "soft_ap_test_params" : {
            "multi_client_test_params": {
                "ssid": "testssid",
                "security_type": "wpa2",
                "password": "password",
                "connectivity_mode": "local_only",
                "operating_band": "only_2_4_ghz"
            }
        }
        """
        asserts.skip_if(
            len(self.clients) < 2, "Test requires at least 2 SoftAPClients"
        )

        test_params = self.soft_ap_test_params.get(
            "multi_client_test_params", {}
        )
        soft_ap_params = SoftAPParams.from_dict(
            test_params.get("soft_ap_params", {})
        )

        self.start_soft_ap(soft_ap_params)

        associated: list[dict[str, Any]] = []

        for client in self.clients:
            # Associate new client
            self.associate_with_soft_ap(client, soft_ap_params)
            self.assert_connected_to_ap(client, self.dut)

            # Verify previously associated clients still behave as expected
            for associated_client in associated:
                id = associated_client["device"].identifier
                self.log.info(
                    f"Verifying previously associated client {id} still "
                    "functions correctly."
                )
                self.assert_connected_to_ap(
                    associated_client["device"], self.dut, check_traffic=True
                )

            client_interface = self.get_device_test_interface(
                client, DeviceRole.CLIENT
            )
            client_ipv4 = self.wait_for_ipv4_address(client, client_interface)
            associated.append({"device": client, "address": client_ipv4})

        self.log.info("All devices successfully associated.")

        self.log.info("Verifying all associated clients can ping eachother.")
        for transmitter in associated:
            for receiver in associated:
                if transmitter != receiver:
                    if (
                        not transmitter["device"]
                        .ping(receiver["address"])
                        .success
                    ):
                        asserts.fail(
                            "Could not ping from one associated client "
                            f"({transmitter['address']}) to another "
                            f"({receiver['address']})."
                        )
                    else:
                        self.log.info(
                            "Successfully pinged from associated client "
                            f"({transmitter['address']}) to another "
                            f"({receiver['address']})"
                        )

        self.log.info(
            "All associated clients can ping each other. Beginning disassociations."
        )

        while len(associated) > 0:
            # Disassociate client
            client = associated.pop()["device"]
            self.disconnect_from_soft_ap(client)

            # Verify still connected clients still behave as expected
            for associated_client in associated:
                id = associated_client["device"].identifier
                self.log.info(
                    f"Verifying still associated client {id} still functions correctly."
                )
                self.assert_connected_to_ap(
                    associated_client["device"], self.dut, check_traffic=True
                )

        self.log.info("All disassociations occurred smoothly.")

    def test_simultaneous_soft_ap_and_client(self) -> None:
        """Tests FuchsiaDevice DUT can act as a client and a SoftAP
        simultaneously.

        Raises:
            ConnectionError: if DUT fails to connect as client
            RuntimeError: if parallel processes fail to join
            TestFailure: if DUT fails to pass traffic as either a client or an
                AP
        """
        if self.access_point is None:
            raise signals.TestSkip("No access point provided")

        self.log.info("Setting up AP using hostapd.")
        test_params = self.soft_ap_test_params.get(
            "soft_ap_and_client_test_params", {}
        )

        # Configure AP
        ap_params = APParams.from_dict(test_params.get("ap_params", {}))

        # Setup AP and associate DUT
        ap_params.setup_ap(self.access_point)
        try:
            self.start_client_mode_and_verify_connected(
                self.access_point, ap_params
            )
        except Exception as err:
            asserts.fail(f"Failed to set up client mode. Err: {err}")

        # Setup SoftAP
        soft_ap_params = SoftAPParams.from_dict(
            test_params.get("soft_ap_params", {})
        )
        self.start_soft_ap_and_verify_connected(
            self.primary_client, soft_ap_params
        )

        # Get FuchsiaDevice test interfaces
        dut_ap_interface = self.get_device_test_interface(
            self.dut, role=DeviceRole.AP
        )
        dut_client_interface = self.get_device_test_interface(
            self.dut, role=DeviceRole.CLIENT
        )

        # Get FuchsiaDevice addresses
        dut_ap_ipv4 = self.wait_for_ipv4_address(self.dut, dut_ap_interface)
        dut_client_ipv4 = self.wait_for_ipv4_address(
            self.dut, dut_client_interface
        )

        # Set up secondary iperf server of FuchsiaDevice
        self.log.info("Setting up second iperf server on FuchsiaDevice DUT.")
        secondary_iperf_server = iperf_server.IPerfServerOverSsh(
            self.iperf_server_settings,
            DEFAULT_IPERF_PORT + 1,
            test_interface=self.dut.get_default_wlan_test_interface(),
            use_killall=True,
        )
        secondary_iperf_server.start()

        # Set up iperf client on AP
        self.log.info("Setting up iperf client on AP.")
        ap_iperf_client = iperf_client.IPerfClientOverSsh(
            self.access_point.ssh_provider,
            # Date is already synced by the AccessPoint controller.
            sync_date=False,
        )

        # Setup iperf processes:
        #     Primary client <-> SoftAP interface on FuchsiaDevice
        #     AP <-> Client interface on FuchsiaDevice
        process_errors: "mp.Queue[str]" = mp.Queue()
        iperf_soft_ap = mp.Process(
            target=self.run_iperf_traffic_parallel_process,
            args=[
                self.iperf_clients_map[self.primary_client],
                dut_ap_ipv4,
                process_errors,
            ],
        )

        iperf_fuchsia_client = mp.Process(
            target=self.run_iperf_traffic_parallel_process,
            args=[ap_iperf_client, dut_client_ipv4, process_errors],
            kwargs={"server_port": 5202},
        )

        # Run iperf processes simultaneously
        self.log.info(
            "Running simultaneous iperf traffic: between AP and DUT "
            "client interface, and DUT AP interface and client."
        )

        iperf_soft_ap.start()
        iperf_fuchsia_client.start()

        # Block until processes can join or timeout
        for proc in [iperf_soft_ap, iperf_fuchsia_client]:
            proc.join(timeout=DEFAULT_IPERF_TIMEOUT)
            if proc.is_alive():
                proc.terminate()
                proc.join()
                raise RuntimeError(f"Failed to join process {proc}")

        # Stop iperf server (also stopped in teardown class as failsafe)
        secondary_iperf_server.stop()

        # Check errors from parallel processes
        if process_errors.empty():
            asserts.explicit_pass(
                "FuchsiaDevice was successfully able to pass traffic as a "
                "client and an AP simultaneously."
            )
        else:
            while not process_errors.empty():
                self.log.error(
                    f"Error in iperf process: {process_errors.get()}"
                )
            asserts.fail(
                "FuchsiaDevice failed to pass traffic as a client and an AP "
                "simultaneously."
            )

    def generate_association_stress_tests(self) -> None:
        """Repeatedly associate and disassociate a client.

        Creates one SoftAP and uses one client.

        Example config:

        soft_ap_test_params:
          soft_ap_association_stress_tests:
          - soft_ap_params:
              ssid: "test_network"
              security_type: "wpa2"
              password: "password"
              connectivity_mode: "local_only"
              operating_band: "only_2_4_ghz"
            iterations: 10
        """
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_soft_ap_association_stress",
            [],
        )

        tests = [
            AssociationStressTestParams.from_dict(spec) for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(AssociationStressTestParams.from_dict({}))

        def generate_name(test: AssociationStressTestParams) -> str:
            return f"test_association_stress_{test}"

        self.generate_tests(
            self.run_soft_ap_association_stress_test,
            generate_name,
            tests,
        )

    def generate_soft_ap_and_client_mode_alternating_stress_tests(self) -> None:
        """Alternate between SoftAP and Client modes.

        Each tests sets up an AP. Then, for each iteration:
            - DUT starts up SoftAP, client associates with SoftAP,
                connection is verified, then disassociates
            - DUT associates to the AP, connection is verified, then
                disassociates

        Example Config:

        soft_ap_test_params:
          toggle_soft_ap_and_client_tests:
          - ap_params:
              ssid: "test-ap-network"
              security_mode: "wpa2"
              password: "password"
              channel: 6
            soft_ap_params:
              ssid: "test-soft-ap-network"
              security_type: "wpa2"
              password: "other-password"
              connectivity_mode: "local_only"
              operating_band: "only_2_4_ghz"
            iterations: 5
        """
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "toggle_soft_ap_and_client_tests",
            [],
        )

        tests = [
            ClientModeAlternatingTestParams.from_dict(spec)
            for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(ClientModeAlternatingTestParams.from_dict({}))

        def generate_name(test: ClientModeAlternatingTestParams) -> str:
            return f"test_soft_ap_and_client_mode_alternating_stress_{test}"

        self.generate_tests(
            self.run_soft_ap_and_client_mode_alternating_test,
            generate_name,
            tests,
        )

    def generate_soft_ap_toggle_stress_tests(self) -> None:
        """Toggle SoftAP up and down.

        If toggled up, a client is associated and connection is verified
        If toggled down, test verifies client is not connected

        Will run with default params, but custom tests can be provided in the
        Mobly config.

        Example Config

        soft_ap_test_params:
          test_soft_ap_toggle_stress:
            soft_ap_params:
              security_type: "wpa2"
              password: "password"
              connectivity_mode: "local_only"
              operating_band: "only_2_4_ghz"
            iterations: 5
        """
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_soft_ap_toggle_stress",
            [],
        )

        tests = [ToggleTestParams.from_dict(spec) for spec in test_specs]

        if len(tests) == 0:
            # Add default test
            tests.append(ToggleTestParams.from_dict({}))

        def generate_name(test: ToggleTestParams) -> str:
            return f"test_soft_ap_toggle_stress_{test}"

        self.generate_tests(
            self.soft_ap_toggle_test,
            generate_name,
            tests,
        )

    def generate_client_mode_toggle_stress_tests(self) -> None:
        """Toggles client mode up and down.

        If toggled up, DUT associates to AP, and connection is verified
        If toggled down, test verifies DUT is not connected to AP

        Will run with default params, but custom tests can be provided in the
        Mobly config.

        Example Config

        soft_ap_test_params:
          test_client_mode_toggle_stress:
            soft_ap_params:
              security_type: "wpa2"
              password: "password"
              connectivity_mode: "local_only"
              operating_band: "only_2_4_ghz"
            iterations: 10
        """
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_client_mode_toggle_stress",
            [],
        )

        tests = [
            ClientModeToggleTestParams.from_dict(spec) for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(ClientModeToggleTestParams.from_dict({}))

        def generate_name(test: ClientModeToggleTestParams) -> str:
            return f"test_client_mode_toggle_stress_{test}"

        self.generate_tests(
            self.client_mode_toggle_test,
            generate_name,
            tests,
        )

    def generate_soft_ap_toggle_stress_with_client_mode_tests(self) -> None:
        """Same as test_soft_ap_toggle_stress, but client mode is set up
        at test start and verified after every toggle."""

        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_soft_ap_toggle_stress_with_client_mode",
            [],
        )

        tests = [
            ClientModeAlternatingTestParams.from_dict(spec)
            for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(ClientModeAlternatingTestParams.from_dict({}))

        def generate_name(test: ClientModeAlternatingTestParams) -> str:
            return f"test_soft_ap_toggle_stress_with_client_mode_{test}"

        self.generate_tests(
            self.soft_ap_toggle_with_client_mode_test,
            generate_name,
            tests,
        )

    def generate_client_mode_toggle_stress_with_soft_ap_tests(self) -> None:
        """Same as test_client_mode_toggle_stress, but softap is set up at
        test start and verified after every toggle."""
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_client_mode_toggle_stress_with_soft_ap",
            [],
        )

        tests = [
            ClientModeAlternatingTestParams.from_dict(spec)
            for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(ClientModeAlternatingTestParams.from_dict({}))

        def generate_name(test: ClientModeAlternatingTestParams) -> str:
            return f"test_client_mode_toggle_stress_with_soft_ap_{test}"

        self.generate_tests(
            self.soft_ap_toggle_with_client_mode_test,
            generate_name,
            tests,
        )

    def generate_soft_ap_and_client_mode_random_toggle_stress_tests(
        self,
    ) -> None:
        """Same as above toggle stres tests, but each iteration, either softap,
        client mode, or both are toggled, then states are verified."""
        test_specs: list[dict[str, Any]] = self.soft_ap_test_params.get(
            "test_soft_ap_and_client_mode_random_toggle_stress",
            [],
        )

        tests = [
            ClientModeAlternatingTestParams.from_dict(spec)
            for spec in test_specs
        ]

        if len(tests) == 0:
            # Add default test
            tests.append(ClientModeAlternatingTestParams.from_dict({}))

        def generate_name(test: ClientModeAlternatingTestParams) -> str:
            return f"test_soft_ap_and_client_mode_random_toggle_stress_{test}"

        self.generate_tests(
            self.soft_ap_and_client_mode_random_toggle_test,
            generate_name,
            tests,
        )


if __name__ == "__main__":
    test_runner.main()
