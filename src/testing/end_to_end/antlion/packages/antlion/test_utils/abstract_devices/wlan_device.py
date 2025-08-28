#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import enum
from typing import Protocol, runtime_checkable

import fidl_fuchsia_wlan_policy as f_wlan_policy
from antlion.controllers import iperf_client
from antlion.controllers.android_device import AndroidDevice
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.controllers.fuchsia_lib.lib_controllers.wlan_policy_controller import (
    WlanPolicyControllerError,
)
from antlion.controllers.iperf_client import IPerfClientBase
from antlion.controllers.pdu import PduDevice
from antlion.test_utils.wifi import wifi_test_utils as awutils
from antlion.utils import PingResult, adb_shell_ping
from honeydew.affordances.connectivity.wlan.utils.types import (
    ClientStatusConnected,
    ClientStatusConnecting,
    ClientStatusIdle,
    ConnectionState,
)
from honeydew.affordances.connectivity.wlan.utils.types import (
    SecurityType as HdSecurityType,
)
from mobly.records import TestResultRecord

DEFAULT_ASSOCIATE_TIMEOUT_SEC = 30


@runtime_checkable
class SupportsWLAN(Protocol):
    """A generic WLAN device."""

    @property
    def identifier(self) -> str:
        """Unique identifier for this device."""
        ...

    @property
    def has_wep_support(self) -> bool:
        "Whether the wlan_device has support for WEP security"
        ...

    @property
    def has_wpa_support(self) -> bool:
        "Whether the wlan_device has support for WPA security"
        ...

    def take_bug_report(self, record: TestResultRecord) -> None:
        """Take a bug report on the device and stores it on the host.

        Will store the bug report in the output directory for the currently running
        test, as specified by `record`.

        Args:
            record: Information about the current running test.
        """
        ...

    def associate(
        self,
        target_ssid: str,
        target_security: SecurityMode,
        target_pwd: str | None = None,
        key_mgmt: str | None = None,
        check_connectivity: bool = True,
        hidden: bool = False,
    ) -> bool:
        """Associate to a target network.

        Args:
            target_ssid: SSID to associate to.
            target_pwd: Password for the SSID, if necessary.
            key_mgmt: The hostapd wpa_key_mgmt, if specified.
            check_connectivity: Whether to check for internet connectivity.
            hidden: Whether the network is hidden.
            target_security: Target security for network, used to
                save the network in policy connects (see wlan_policy_lib)
        Returns:
            True if successfully connected to WLAN, False if not.
        """
        ...

    def disconnect(self) -> None:
        """Disconnect from all WLAN networks."""
        ...

    def get_default_wlan_test_interface(self) -> str:
        """Name of default WLAN interface to use for testing."""
        ...

    def is_connected(self, ssid: str | None = None) -> bool:
        """Determines if wlan_device is connected to wlan network.

        Args:
            ssid: If specified, check if device is connected to a specific network.

        Returns:
            True if connected to requested network; or if ssid not specified, True if
            connected to any network; otherwise, False.
        """
        ...

    def create_iperf_client(
        self, test_interface: str | None = None
    ) -> IPerfClientBase:
        """Create an iPerf3 client on this device.

        Args:
            test_interface: Name of test interface. Defaults to first found wlan client
                interface.

        Returns:
            IPerfClient object
        """
        ...

    def get_wlan_interface_id_list(self) -> list[int]:
        """List available WLAN interfaces.

        Returns:
            A list of wlan interface IDs.
        """
        ...

    def destroy_wlan_interface(self, iface_id: int) -> None:
        """Destroy the specified WLAN interface.

        Args:
            iface_id: ID of the interface to destroy.
        """
        ...

    def ping(
        self,
        dest_ip: str,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> PingResult:
        """Pings from a device to an IP address or hostname

        Args:
            dest_ip: IP or hostname to ping
            count: How many icmp packets to send
            interval: Milliseconds to wait between pings
            timeout: Milliseconds to wait before having the icmp packet timeout
            size: Size of the icmp packet in bytes
            additional_ping_params: Command option flags to append to the command string

        Returns:
            A dictionary for the results of the ping. The dictionary contains
            the following items:
                status: Whether the ping was successful.
                rtt_min: The minimum round trip time of the ping.
                rtt_max: The minimum round trip time of the ping.
                rtt_avg: The avg round trip time of the ping.
                stdout: The standard out of the ping command.
                stderr: The standard error of the ping command.
        """
        ...

    def hard_power_cycle(self, pdus: list[PduDevice]) -> None:
        """Reboot a device abruptly without notification.

        Args:
            pdus: All testbed PDUs
        """
        ...

    def feature_is_present(self, feature: str) -> bool:
        """Check if a WLAN feature is present.

        Args:
            feature: WLAN feature to query

        Returns:
            True if `feature` is present; otherwise, False.
        """
        ...

    def wifi_toggle_state(self, state: bool | None) -> None:
        """Toggle the state of Wi-Fi.

        Args:
            state: Wi-Fi state to set to. If None, opposite of the current state.
        """
        ...

    def reset_wifi(self) -> None:
        """Clears all saved Wi-Fi networks on a device.

        This will turn Wi-Fi on.
        """
        ...

    def turn_location_off_and_scan_toggle_off(self) -> None:
        """Turn off Wi-Fi location scans."""
        ...


class AndroidWlanDevice(SupportsWLAN):
    """Android device that supports WLAN."""

    def __init__(self, android_device: AndroidDevice) -> None:
        self.device = android_device

    @property
    def identifier(self) -> str:
        return self.device.serial

    @property
    def has_wep_support(self) -> bool:
        "Whether the wlan_device has support for WEP security"
        return True

    @property
    def has_wpa_support(self) -> bool:
        "Whether the wlan_device has support for WPA security"
        return True

    def wifi_toggle_state(self, state: bool | None) -> None:
        awutils.wifi_toggle_state(self.device, state)

    def reset_wifi(self) -> None:
        awutils.reset_wifi(self.device)

    def take_bug_report(self, record: TestResultRecord) -> None:
        self.device.take_bug_report(record.test_name, record.begin_time)

    def turn_location_off_and_scan_toggle_off(self) -> None:
        awutils.turn_location_off_and_scan_toggle_off(self.device)

    def associate(
        self,
        target_ssid: str,
        target_security: SecurityMode,
        target_pwd: str | None = None,
        key_mgmt: str | None = None,
        check_connectivity: bool = True,
        hidden: bool = False,
    ) -> bool:
        network = {"SSID": target_ssid, "hiddenSSID": hidden}
        if target_pwd:
            network["password"] = target_pwd
        if key_mgmt:
            network["security"] = key_mgmt
        try:
            awutils.connect_to_wifi_network(
                self.device,
                network,
                check_connectivity=check_connectivity,
                hidden=hidden,
            )
            return True
        except Exception as e:
            self.device.log.info(f"Failed to associated ({e})")
            return False

    def disconnect(self) -> None:
        awutils.turn_location_off_and_scan_toggle_off(self.device)

    def get_wlan_interface_id_list(self) -> list[int]:
        raise NotImplementedError(
            "get_wlan_interface_id_list is not implemented"
        )

    def get_default_wlan_test_interface(self) -> str:
        return "wlan0"

    def destroy_wlan_interface(self, iface_id: int) -> None:
        raise NotImplementedError("destroy_wlan_interface is not implemented")

    def is_connected(self, ssid: str | None = None) -> bool:
        wifi_info = self.device.droid.wifiGetConnectionInfo()
        if ssid:
            return "BSSID" in wifi_info and wifi_info["SSID"] == ssid
        return "BSSID" in wifi_info

    def ping(
        self,
        dest_ip: str,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> PingResult:
        success = adb_shell_ping(
            self.device, dest_ip, count=count, timeout=timeout
        )
        return PingResult(
            exit_status=0 if success else 1,
            # TODO: Implement the rest if needed for any tests
            stdout="",
            stderr="",
            transmitted=None,
            received=None,
            time_ms=None,
            rtt_min_ms=None,
            rtt_avg_ms=None,
            rtt_max_ms=None,
            rtt_mdev_ms=None,
        )

    def hard_power_cycle(self, pdus: list[PduDevice]) -> None:
        raise NotImplementedError("hard_power_cycle is not implemented")

    def create_iperf_client(
        self, test_interface: str | None = None
    ) -> IPerfClientBase:
        if not test_interface:
            test_interface = self.get_default_wlan_test_interface()

        return iperf_client.IPerfClientOverAdb(
            android_device=self.device, test_interface=test_interface
        )

    def feature_is_present(self, feature: str) -> bool:
        raise NotImplementedError("feature_is_present is not implemented")


class AssociationMode(enum.Enum):
    """Defines which FIDLs to use for WLAN association and disconnect."""

    DRIVER = 1
    """Call WLAN core FIDLs to provide all association and disconnect."""
    POLICY = 2
    """Call WLAN policy FIDLs to provide all association and disconnect."""


class FuchsiaWlanDevice(SupportsWLAN):
    """Fuchsia device that supports WLAN."""

    def __init__(self, fuchsia_device: FuchsiaDevice, mode: AssociationMode):
        self.device = fuchsia_device
        self.device.configure_wlan()
        self.association_mode = mode

    @property
    def identifier(self) -> str:
        return self.device.ip

    @property
    def has_wep_support(self) -> bool:
        for line in self._get_wlandevicemonitor_config().splitlines():
            if "wep_supported" in line and "Bool(true)" in line:
                return True
        return False

    @property
    def has_wpa_support(self) -> bool:
        for line in self._get_wlandevicemonitor_config().splitlines():
            if "wpa1_supported" in line and "Bool(true)" in line:
                return True
        return False

    def _get_wlandevicemonitor_config(self) -> str:
        return self.device.ffx.run(
            ["component", "show", "core/wlandevicemonitor"]
        )

    def wifi_toggle_state(self, state: bool | None) -> None:
        pass

    def reset_wifi(self) -> None:
        pass

    def take_bug_report(self, _: TestResultRecord) -> None:
        self.device.take_bug_report()

    def turn_location_off_and_scan_toggle_off(self) -> None:
        pass

    def associate(
        self,
        target_ssid: str,
        target_security: SecurityMode,
        target_pwd: str | None = None,
        key_mgmt: str | None = None,
        check_connectivity: bool = True,
        hidden: bool = False,
        timeout_sec: int = DEFAULT_ASSOCIATE_TIMEOUT_SEC,
    ) -> bool:
        match self.association_mode:
            case AssociationMode.DRIVER:
                ssid_bss_desc_map = (
                    self.device.honeydew_fd.wlan_core.scan_for_bss_info()
                )

                bss_descs_for_ssid = ssid_bss_desc_map.get(target_ssid, None)
                if not bss_descs_for_ssid or len(bss_descs_for_ssid) < 1:
                    self.device.log.error(
                        "Scan failed to find a BSS description for target_ssid "
                        f"{target_ssid}"
                    )
                    return False

                return self.device.honeydew_fd.wlan_core.connect(
                    target_ssid, target_pwd, bss_descs_for_ssid[0]
                )
            case AssociationMode.POLICY:
                try:
                    self.device.honeydew_fd.wlan_policy.save_network(
                        target_ssid,
                        HdSecurityType(target_security.fuchsia_security_type()),
                        target_pwd=target_pwd,
                    )
                    status = self.device.honeydew_fd.wlan_policy.connect(
                        target_ssid,
                        HdSecurityType(target_security.fuchsia_security_type()),
                    )
                    if status is f_wlan_policy.RequestStatus.ACKNOWLEDGED:
                        self.device.wlan_policy_controller.wait_for_network_state(
                            target_ssid,
                            ConnectionState.CONNECTED,
                            timeout_sec=timeout_sec,
                        )
                    else:
                        self.device.log.warning(
                            f"Received request status: {status.name} while trying to "
                            f"connect to ssid: {target_ssid}."
                        )
                        return False

                    return True
                except WlanPolicyControllerError as e:
                    self.device.log.error(
                        f"Failed to save and connect to {target_ssid} with "
                        f"error: {e}"
                    )
                    return False

    def disconnect(self) -> None:
        """Function to disconnect from a Fuchsia WLAN device.
        Asserts if disconnect was not successful.
        """
        match self.association_mode:
            case AssociationMode.DRIVER:
                self.device.honeydew_fd.wlan_core.disconnect()
            case AssociationMode.POLICY:
                self.device.honeydew_fd.wlan_policy.remove_all_networks()
                self.device.wlan_policy_controller.wait_for_no_connections()

    def ping(
        self,
        dest_ip: str,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> PingResult:
        return self.device.ping(
            dest_ip,
            count=count,
            interval=interval,
            timeout=timeout,
            size=size,
            additional_ping_params=additional_ping_params,
        )

    def get_wlan_interface_id_list(self) -> list[int]:
        return self.device.honeydew_fd.wlan_core.get_iface_id_list()

    def get_default_wlan_test_interface(self) -> str:
        if self.device.wlan_client_test_interface_name is None:
            raise TypeError(
                "Expected wlan_client_test_interface_name to be str"
            )
        return self.device.wlan_client_test_interface_name

    def destroy_wlan_interface(self, iface_id: int) -> None:
        self.device.honeydew_fd.wlan_core.destroy_iface(iface_id)

    def is_connected(self, ssid: str | None = None) -> bool:
        result = self.device.honeydew_fd.wlan_core.status()
        match result:
            case ClientStatusIdle():
                self.device.log.info("Client status idle")
                return False
            case ClientStatusConnecting():
                ssid_bytes = bytearray(result.ssid).decode(
                    encoding="utf-8", errors="replace"
                )
                self.device.log.info(
                    f"Client status connecting to ssid: {ssid_bytes}"
                )
                return False
            case ClientStatusConnected():
                ssid_bytes = bytearray(result.ssid).decode(
                    encoding="utf-8", errors="replace"
                )
                self.device.log.info(f"Client connected to ssid: {ssid_bytes}")
                if ssid is None:
                    return True
                return ssid == ssid_bytes
            case _:
                raise ValueError(
                    "Status did not return a valid status response: "
                    f"{result}"
                )

    def hard_power_cycle(self, pdus: list[PduDevice]) -> None:
        self.device.reboot(reboot_type="hard", testbed_pdus=pdus)

    def create_iperf_client(
        self, test_interface: str | None = None
    ) -> IPerfClientBase:
        if not test_interface:
            test_interface = self.get_default_wlan_test_interface()

        # A package server is necessary to acquire the iperf3 client for
        # some builds.
        self.device.start_package_server()

        return iperf_client.IPerfClientOverSsh(
            ssh_provider=self.device.ssh,
            test_interface=test_interface,
            # Fuchsia's date tool does not support setting system date/time.
            sync_date=False,
        )

    def feature_is_present(self, feature: str) -> bool:
        return feature in self.device.wlan_features


def create_wlan_device(
    hardware_device: FuchsiaDevice | AndroidDevice,
    associate_mode: AssociationMode,
) -> SupportsWLAN:
    """Creates a generic WLAN device based on type of device that is sent to
    the functions.

    Args:
        hardware_device: A WLAN hardware device that is supported by ACTS.
    """
    device: SupportsWLAN
    if isinstance(hardware_device, FuchsiaDevice):
        device = FuchsiaWlanDevice(hardware_device, associate_mode)
    elif isinstance(hardware_device, AndroidDevice):
        device = AndroidWlanDevice(hardware_device)
    else:
        raise ValueError(
            f"Unable to create WLAN device for type {type(hardware_device)}"
        )

    assert isinstance(device, SupportsWLAN)
    return device
