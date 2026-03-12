# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging
from dataclasses import dataclass

from antlion.controllers.access_point import AccessPoint

logger = logging.getLogger(__name__)

from datetime import timedelta

import fidl_fuchsia_wlan_common as fw_common
import fidl_fuchsia_wlan_device_service as fw_device_service
import fidl_fuchsia_wlan_sme as fw_sme
from antlion import controllers
from core_testing.handlers import DeviceWatcherEventHandler
from fuchsia_controller_py import ZxStatus
from fuchsia_wlan_base_test import FuchsiaWlanBaseTest
from honeydew.typing.custom_types import FidlEndpoint
from mobly import signals
from mobly.asserts import abort_class_if, assert_equal
from mobly.records import TestResultRecord


@dataclass(frozen=True)
class CoreTestKit:
    device_monitor: fw_device_service.DeviceMonitorClient
    phy_id: int


class CoreBaseTestClass(FuchsiaWlanBaseTest):
    _core_test_kit: CoreTestKit

    _PAUSE_FOR_ADDITIONAL_PHY_DEVICES = timedelta(seconds=1)

    @property
    def test_kit(self) -> CoreTestKit:
        return self._core_test_kit

    async def setup_class(self) -> None:
        await super().setup_class()

        abort_class_if(
            len(self.fuchsia_devices) != 1,
            "Requires exactly one Fuchsia device",
        )
        self.fuchsia_device = self.fuchsia_devices[0]

        device_monitor = fw_device_service.DeviceMonitorClient(
            self.fuchsia_device.fuchsia_controller.connect_device_proxy(
                FidlEndpoint(
                    "core/wlandevicemonitor",
                    "fuchsia.wlan.device.service.DeviceMonitor",
                )
            )
        )

        (
            proxy,
            server,
        ) = self.fuchsia_device.fuchsia_controller.channel_create()

        # Wait for first phy device to appear, and assert no additional
        # phy devices are added after a brief pause.
        phy_id = None
        device_monitor.watch_devices(watcher=server.take())
        async with DeviceWatcherEventHandler(
            client=fw_device_service.DeviceWatcherClient(proxy.take()),
            verbose=True,
        ) as ctx:
            try:
                while next_txn := await asyncio.wait_for(
                    ctx.txn_queue.get(),
                    timeout=self._PAUSE_FOR_ADDITIONAL_PHY_DEVICES.total_seconds(),
                ):
                    if isinstance(
                        next_txn,
                        fw_device_service.DeviceWatcherOnPhyAddedRequest,
                    ):
                        if phy_id is not None:
                            raise signals.TestAbortClass(
                                "Detected second phy device."
                            )
                        phy_id = next_txn.phy_id
                    elif isinstance(
                        next_txn,
                        fw_device_service.DeviceWatcherOnIfaceAddedRequest,
                    ):
                        logger.info(
                            f"Ignoring notification of existing iface {next_txn.iface_id}"
                        )
                    else:
                        raise signals.TestFailure(
                            f"Expected OnPhyAdded, but received: {next_txn}"
                        )
            except asyncio.TimeoutError:
                logger.info(
                    f"Assuming all DeviceWatcher events observed. No new events "
                    f"after waiting "
                    f"{self._PAUSE_FOR_ADDITIONAL_PHY_DEVICES.total_seconds()} second(s)."
                )

        assert phy_id is not None, "DeviceWatcher failed to report a phy."

        self._core_test_kit = CoreTestKit(
            device_monitor=device_monitor, phy_id=phy_id
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self._destroy_all_ifaces()

    async def teardown_class(self) -> None:
        await self._destroy_all_ifaces()
        await super().teardown_class()

    async def _destroy_all_ifaces(self) -> None:
        list_ifaces_response = (
            await self._core_test_kit.device_monitor.list_ifaces()
        )
        for iface_id in list_ifaces_response.iface_list:
            logger.info(f"Destroying iface {iface_id} before next test...")
            await self._core_test_kit.device_monitor.destroy_iface(
                req=fw_device_service.DestroyIfaceRequest(iface_id=iface_id)
            )


@dataclass(frozen=True)
class ClassTestKit:
    access_point: AccessPoint


@dataclass(frozen=True)
class ConnectionTestKit(CoreTestKit):
    access_point: AccessPoint
    iface_id: int
    client_sme: fw_sme.ClientSmeClient


class ConnectionBaseTestClass(CoreBaseTestClass):
    _connection_test_kit: ConnectionTestKit

    @property
    def test_kit(self) -> ConnectionTestKit:
        return self._connection_test_kit

    async def setup_class(self) -> None:
        await super().setup_class()

        # Set the country code to US, to allow for 2.4 and 5 GHz connections.
        set_country_request = fw_device_service.SetCountryRequest(
            phy_id=self._core_test_kit.phy_id,
            alpha2=[ord("U"), ord("S")],
        )
        set_country_response = (
            await self._core_test_kit.device_monitor.set_country(
                req=set_country_request
            )
        )
        assert_equal(
            set_country_response.status,
            ZxStatus.ZX_OK,
            "DeviceMonitor.SetCountry() failed",
        )

        access_points = await self.register_controller(
            controllers.access_point, min_number=1
        )

        if access_points is None or len(access_points) == 0:
            raise signals.TestAbortClass("Requires at least one access point")
        self.class_test_kit = ClassTestKit(access_point=access_points[0])
        self.class_test_kit.access_point.stop_all_aps()

    async def setup_test(self) -> None:
        await super().setup_test()

        self.class_test_kit.access_point.stop_all_aps()

        create_iface_response = (
            (
                await self._core_test_kit.device_monitor.create_iface(
                    phy_id=self._core_test_kit.phy_id,
                    role=fw_common.WlanMacRole.CLIENT,
                    sta_address=[0, 0, 0, 0, 0, 0],
                )
            )
        ).unwrap()
        assert (
            create_iface_response.iface_id is not None
        ), "DeviceMonitor.CreateIface() response is missing a iface_id"
        iface_id = create_iface_response.iface_id

        (
            proxy,
            server,
        ) = self.fuchsia_device.fuchsia_controller.channel_create()
        (
            (
                await self._core_test_kit.device_monitor.get_client_sme(
                    iface_id=iface_id,
                    sme_server=server.take(),
                )
            )
        ).unwrap()
        self._connection_test_kit = ConnectionTestKit(
            device_monitor=self._core_test_kit.device_monitor,
            phy_id=self._core_test_kit.phy_id,
            access_point=self.class_test_kit.access_point,
            iface_id=iface_id,
            client_sme=fw_sme.ClientSmeClient(proxy),
        )

    async def teardown_test(self) -> None:
        # Maintain the invariant that every test starts with no access points.
        self._connection_test_kit.access_point.download_ap_logs(self.log_path)
        self._connection_test_kit.access_point.stop_all_aps()
        (
            await self._connection_test_kit.client_sme.disconnect(
                reason=fw_sme.UserDisconnectReason.UNKNOWN
            )
        )
        await super().teardown_test()

    async def on_fail(self, record: TestResultRecord) -> None:
        await super().on_fail(record)

        # Maintain the invariant that every test starts with no access points.
        self.class_test_kit.access_point.stop_all_aps()

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
