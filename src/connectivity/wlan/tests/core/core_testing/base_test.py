# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Base test classes for antlion tests of WLAN core.
"""

import logging

logger = logging.getLogger(__name__)


import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_device_service as fidl_svc
import fidl_fuchsia_wlan_sme as fidl_sme
import honeydew.affordances.connectivity.wlan.utils.types as hd_util_types
from antlion import controllers
from antlion.controllers.access_point import AccessPoint
from fuchsia_controller_py import Channel, ZxStatus
from fuchsia_controller_py.wrappers import AsyncAdapter, asyncmethod
from fuchsia_wlan_base_test import FuchsiaWlanBaseTest
from honeydew.typing.custom_types import FidlEndpoint
from mobly import signals
from mobly.asserts import abort_class_if, assert_equal, fail
from mobly.records import TestResultRecord


class ConnectionBaseTestClassSync(FuchsiaWlanBaseTest):
    iface_id: int | None = None
    access_point: AccessPoint

    def setup_class(self) -> None:
        super().setup_class()

        abort_class_if(
            len(self.fuchsia_devices) != 1,
            "Requires exactly one Fuchsia device",
        )
        self.fuchsia_device = self.fuchsia_devices[0]

        access_points = self.register_controller(
            controllers.access_point, min_number=1
        )
        if access_points is None or len(access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.access_point = access_points[0]
        self.access_point.stop_all_aps()

        # Get the phy
        phy_id_list = self.fuchsia_device.wlan_core.get_phy_id_list()
        assert_equal(
            len(phy_id_list),
            1,
            "Expecting exactly one phy_id",
        )
        self.phy_id = phy_id_list[0]
        logger.info(f"Using phy_id {self.phy_id}")

        # Check we have no interfaces
        ifaces = self.fuchsia_device.wlan_core.get_iface_id_list()
        assert len(ifaces) == 0, "Every test suite should start with no ifaces."

        # Set the country code to US, to allow for 2.4 and 5 GHz connections.
        self.fuchsia_device.wlan_core.set_country(
            self.phy_id, hd_util_types.CountryCode("US")
        )

        # Create an iface
        self.iface_id = self.fuchsia_device.wlan_core.create_iface(
            phy_id=self.phy_id,
            role=fidl_common.WlanMacRole.CLIENT,
            sta_addr=None,
        )

    def teardown_test(self) -> None:
        # Maintain the invariant that every test starts with no access points.
        self.access_point.download_ap_logs(self.log_path)
        self.access_point.stop_all_aps()
        self.fuchsia_device.wlan_core.disconnect()
        super().teardown_test()

    def teardown_class(self) -> None:
        if self.iface_id is not None:
            logger.info(f"Destroying iface_id {self.iface_id}")
            self.fuchsia_device.wlan_core.destroy_iface(self.iface_id)
        self.iface_id = None
        super().teardown_class()

    def on_fail(self, record: TestResultRecord) -> None:
        """A function that is executed upon a test failure.

        Args:
        record: A copy of the test record for this test, containing all information of
            the test execution including exception objects.
        """
        super().on_fail(record)

        # Maintain the invariant that every test starts with no access points.
        self.access_point.stop_all_aps()


class CoreBaseTestClass(AsyncAdapter, FuchsiaWlanBaseTest):
    device_monitor_proxy: fidl_svc.DeviceMonitorClient

    def setup_class(self) -> None:
        super().setup_class()

        abort_class_if(
            len(self.fuchsia_devices) != 1,
            "Requires exactly one Fuchsia device",
        )
        self.fuchsia_device = self.fuchsia_devices[0]

        self.device_monitor_proxy = fidl_svc.DeviceMonitorClient(
            self.fuchsia_device.fuchsia_controller.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )


class ConnectionBaseTestClass(CoreBaseTestClass):
    iface_id: int | None

    @asyncmethod
    async def setup_class(self) -> None:
        super().setup_class()

        list_phys_response = await self.device_monitor_proxy.list_phys()
        assert (
            list_phys_response.phy_list is not None
        ), "DeviceMonitor.ListPhys() response is missing a phy_list value"
        assert_equal(
            len(list_phys_response.phy_list),
            1,
            "DeviceMonitor.ListPhys() should return exactly one phy_id.",
        )

        self.phy_id = list_phys_response.phy_list[0]
        logger.info(f"Using phy_id {self.phy_id}")

        list_ifaces_response = await self.device_monitor_proxy.list_ifaces()
        assert (
            list_ifaces_response.iface_list is not None
        ), "DeviceMonitor.ListIfaces() response is missing iface_list"
        if len(list_ifaces_response.iface_list) > 0:
            fail(
                f"Found existing ifaces: {list_ifaces_response.iface_list}. Every test suite should start with no ifaces."
            )

        # Set the country code to US, to allow for 2.4 and 5 GHz connections.
        set_country_request = fidl_svc.SetCountryRequest(
            phy_id=self.phy_id, alpha2=[ord("U"), ord("S")]
        )
        set_country_response = await self.device_monitor_proxy.set_country(
            req=set_country_request
        )
        assert_equal(
            set_country_response.status,
            ZxStatus.ZX_OK,
            "DeviceMonitor.SetCountry() failed",
        )

        create_iface_response = (
            await self.device_monitor_proxy.create_iface(
                phy_id=self.phy_id,
                role=fidl_common.WlanMacRole.CLIENT,
                sta_address=[0, 0, 0, 0, 0, 0],
            )
        ).unwrap()
        assert (
            create_iface_response.iface_id is not None
        ), "DeviceMonitor.CreateIface() response is missing a iface_id"
        self.iface_id = create_iface_response.iface_id

        proxy, server = Channel.create()
        (
            await self.device_monitor_proxy.get_client_sme(
                iface_id=self.iface_id,
                sme_server=server.take(),
            )
        ).unwrap()
        self.client_sme_proxy = fidl_sme.ClientSmeClient(proxy)

        access_points = self.register_controller(
            controllers.access_point, min_number=1
        )
        if access_points is None or len(access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.__access_point = access_points[0]

        self.access_point().stop_all_aps()

    def teardown_test(self) -> None:
        # Maintain the invariant that every test starts with no access points.
        self.access_point().download_ap_logs(self.log_path)
        self.access_point().stop_all_aps()
        self.loop().run_until_complete(
            self.client_sme_proxy.disconnect(
                reason=fidl_sme.UserDisconnectReason.UNKNOWN
            )
        )
        super().teardown_test()

    @asyncmethod
    async def teardown_class(self) -> None:
        if self.iface_id is not None:
            logger.info(f"Destroying iface_id {self.iface_id}")
            req = fidl_svc.DestroyIfaceRequest(iface_id=self.iface_id)
            response = await self.device_monitor_proxy.destroy_iface(req=req)
            assert_equal(
                response.status,
                ZxStatus.ZX_OK,
                "DeviceMonitor.DestroyIface() failed",
            )
        self.iface_id = None
        super().teardown_class()

    def access_point(self) -> AccessPoint:
        if self.__access_point is None:
            raise RuntimeError("Connection tests require an access point.")
        return self.__access_point

    def on_fail(self, record: TestResultRecord) -> None:
        super().on_fail(record)

        # Maintain the invariant that every test starts with no access points.
        self.access_point().stop_all_aps()

    def ping(
        self,
        dest_ip: str,
        count: int = 3,
        interval: int = 1000,
        timeout: int = 1000,
        size: int = 25,
        additional_ping_params: str | None = None,
    ) -> str:
        """Pings from a Fuchsia device to an IPv4 address or hostname

        Args:
            dest_ip: (str) The ip or hostname to ping.
            count: (int) How many icmp packets to send.
            interval: (int) How long to wait between pings (ms)
            timeout: (int) How long to wait before having the icmp packet
                timeout (ms).
            size: (int) Size of the icmp packet.
            additional_ping_params: (str) command option flags to
                append to the command string
        """
        logger.info(f"Pinging {dest_ip}...")
        if not additional_ping_params:
            additional_ping_params = ""

        return self.fuchsia_device.ffx.run_ssh_cmd(
            f"ping -c {count} -i {interval} -t {timeout} -s {size} "
            f"{additional_ping_params} {dest_ip}"
        )
