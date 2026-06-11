# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for starting a scheduled scan on an interface.
"""

import asyncio
import logging
import struct
from dataclasses import dataclass
from datetime import timedelta
from typing import Any, Iterator

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
import wlanix_testing.base_test as base_test
from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from common.utils.ies import read_ssid
from fuchsia_controller_py import Channel
from honeydew.utils.deadline import Deadline
from mobly import test_runner
from mobly.asserts import assert_equal, assert_true, fail
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)

logger = logging.getLogger(__name__)

NL80211_CMD_START_SCHED_SCAN = 0x4B
NL80211_CMD_SCHED_SCAN_RESULTS = 77
NL80211_ATTR_IFINDEX = 0x03
NL80211_ATTR_SCHED_SCAN_INTERVAL = 0x77
NL80211_ATTR_SCHED_SCAN_MATCH = 132
NL80211_CMD_GET_SCAN = 32
NL80211_ATTR_BSS = 47
NL80211_BSS_INFORMATION_ELEMENTS = 6
IE_TYPE_SSID = 0


def parse_netlink_attributes(
    payload: bytes, start_offset: int = 4
) -> dict[int, bytes]:
    """Parses Netlink attributes from a payload.

    Args:
        payload: The bytes to parse.
        start_offset: The offset to start parsing from (default 4 to skip GenNetlink header).

    Returns:
        A dictionary mapping attribute type to attribute value.
    """
    attrs = {}
    offset = start_offset
    while offset + 4 <= len(payload):
        nla_len, nla_type = struct.unpack_from("<HH", payload, offset)
        if nla_len < 4:
            break
        # The most significant bits are reserved for NLA_F_NESTED and NLA_F_NET_BYTEORDER.
        nla_type &= 0x3FFF
        value = payload[offset + 4 : offset + nla_len]
        attrs[nla_type] = value
        # Move offset forward to the next 4-byte boundary.
        offset += (nla_len + 3) & ~3
    return attrs


@dataclass
class Nl80211MulticastServerContext:
    message_queue: asyncio.Queue[fidl_wlanix.Nl80211Message]
    callback_channel: Channel


class Nl80211MulticastServer(fidl_wlanix.Nl80211MulticastServer):
    def __init__(self, client: Channel, server: Channel) -> None:
        self.client = client
        self.server = server

    async def message(
        self, request: fidl_wlanix.Nl80211MulticastMessageRequest
    ) -> None:
        if request.message is not None:
            self.message_queue.put_nowait(request.message)

    async def __aenter__(self) -> Nl80211MulticastServerContext:
        super().__init__(channel=self.server)
        self.message_queue: asyncio.Queue[
            fidl_wlanix.Nl80211Message
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return Nl80211MulticastServerContext(
            message_queue=self.message_queue,
            callback_channel=self.client,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task:
            self.server_task.cancel()


class SchedScanTest(base_test.ConnectionBaseTestClass):
    async def _query_iface_index(self) -> int:
        get_interface_message = fidl_wlanix.Nl80211Message(
            message=fidl_wlanix.Message(
                payload=[
                    0x05,  # Command: GetInterface
                    0x01,  # Version
                    0x00,
                    0x00,  # Reserved
                ],
            )
        )
        response_list = (
            (await self.nl80211_proxy.message(message=get_interface_message))
            .unwrap()
            .responses
        )
        assert response_list is not None
        attrs = base_test.verify_new_interface_response(response_list)
        return struct.unpack("<I", attrs[base_test.NL80211_ATTR_IFINDEX])[0]

    async def test_start_sched_scan(self) -> None:
        # Setup AP
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_2G)
        security = DeprecatedSecurity(security_mode=SecurityMode.OPEN)

        ap = self.access_point()
        if isinstance(ap, OpenWrtAP):
            ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    security=SecurityOpen(),
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(ap, AccessPoint):
            setup_ap(
                access_point=ap,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=security,
            )
        logger.info("Setup AP with SSID: %s", ssid)
        ssid_bytes = ssid.encode("ascii")

        iface_index = await self._query_iface_index()
        logger.info(
            "Using iface index %d for start sched scan test", iface_index
        )

        # Helper to build NLA
        def build_nla(attr_type: int, value: bytes) -> bytes:
            true_len = 4 + len(value)
            header = struct.pack("<HH", true_len, attr_type)
            attr = header + value
            pad_len = (4 - (len(attr) % 4)) % 4
            return attr + b"\x00" * pad_len

        # Construct match attributes
        ssid_attr = build_nla(1, ssid_bytes)
        match_set = build_nla(1, ssid_attr)
        match_attr = build_nla(NL80211_ATTR_SCHED_SCAN_MATCH, match_set)
        payload = [
            NL80211_CMD_START_SCHED_SCAN,  # Command
            0x01,  # Version
            0x00,
            0x00,  # Reserved
            *list(
                build_nla(NL80211_ATTR_IFINDEX, struct.pack("<I", iface_index))
            ),
            *list(
                build_nla(
                    NL80211_ATTR_SCHED_SCAN_INTERVAL, struct.pack("<I", 5000)
                )
            ),
            *list(match_attr),
        ]

        # Send command
        logger.info("Sending start scheduled scan netlink message")
        start_sched_scan_message = fidl_wlanix.Nl80211Message(
            message=fidl_wlanix.Message(payload=payload)
        )

        # Listen for multicast events
        client, server = self.dut.fuchsia_controller.channel_create()
        async with Nl80211MulticastServer(client, server) as ctx:
            scan_queue = ctx.message_queue
            scan_callback_channel = ctx.callback_channel

            self.nl80211_proxy.get_multicast(
                group="scan", multicast=scan_callback_channel.take()
            )

            # Send command
            response_list = (
                (
                    await self.nl80211_proxy.message(
                        message=start_sched_scan_message
                    )
                )
                .unwrap()
                .responses
            )

            # Verify acknowledgement
            assert response_list is not None
            assert_equal(
                len(response_list),
                1,
                "Response from start scheduled scan message should contain a single ack.",
            )
            assert_true(
                response_list[0].ack is not None,
                "Response from start scheduled scan message should have been an ack.",
            )
            logger.info("Received ack for start scheduled scan message")

            # Poll for scan results
            deadline = Deadline.from_timeout(timedelta(seconds=10))
            delay = 0.5
            while not deadline.is_due():
                try:
                    msg = scan_queue.get_nowait()
                    if msg.message and msg.message.payload:
                        cmd = msg.message.payload[0]
                        if cmd == NL80211_CMD_SCHED_SCAN_RESULTS:
                            logger.info(
                                "Received scheduled scan results message."
                            )
                            # Send GET_SCAN to retrieve scan results
                            logger.info(
                                "Sending get scan netlink message to verify results"
                            )
                            get_scan_payload = [
                                NL80211_CMD_GET_SCAN,  # Command: GetScan
                                0x01,  # Version
                                0x00,
                                0x00,  # Reserved
                                *list(
                                    build_nla(
                                        NL80211_ATTR_IFINDEX,
                                        struct.pack("<I", iface_index),
                                    )
                                ),
                            ]
                            get_scan_message = fidl_wlanix.Nl80211Message(
                                message=fidl_wlanix.Message(
                                    payload=get_scan_payload
                                )
                            )
                            get_scan_response = (
                                (
                                    await self.nl80211_proxy.message(
                                        message=get_scan_message
                                    )
                                )
                                .unwrap()
                                .responses
                            )
                            assert get_scan_response is not None

                            def get_ssids_from_responses(
                                responses: Any,
                            ) -> Iterator[str]:
                                for resp in responses:
                                    if (
                                        resp.done
                                        or resp.message is None
                                        or resp.message.payload is None
                                    ):
                                        continue
                                    if resp.error is not None:
                                        logger.warning(
                                            f"Error in get scan response: {resp.error}"
                                        )
                                        continue

                                    resp_attrs = parse_netlink_attributes(
                                        bytes(resp.message.payload)
                                    )
                                    if NL80211_ATTR_BSS not in resp_attrs:
                                        continue

                                    bss_attrs = parse_netlink_attributes(
                                        resp_attrs[NL80211_ATTR_BSS],
                                        start_offset=0,
                                    )
                                    if (
                                        NL80211_BSS_INFORMATION_ELEMENTS
                                        not in bss_attrs
                                    ):
                                        continue

                                    ssid_str = read_ssid(
                                        bss_attrs[
                                            NL80211_BSS_INFORMATION_ELEMENTS
                                        ]
                                    )
                                    if ssid_str is not None:
                                        yield ssid_str

                            if any(
                                parsed_ssid == ssid
                                for parsed_ssid in get_ssids_from_responses(
                                    get_scan_response
                                )
                            ):
                                logger.info("Found SSID in scan results.")
                            else:
                                fail(
                                    "SSID not found in get scan response after scheduled scan results message."
                                )
                            break
                except asyncio.QueueEmpty:
                    await asyncio.sleep(delay)
                    delay = min(delay * 2, 2.0)
            else:
                fail("Timed out waiting for scheduled scan results message")


if __name__ == "__main__":
    test_runner.main()
