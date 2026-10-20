# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Netlink (nl80211) constants and packet builders/parsers for wlanix tests."""

import struct
from collections.abc import Sequence

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
from mobly.asserts import assert_equal, fail

# Generic Netlink / nl80211 commands
NL80211_CMD_GET_INTERFACE = 5
NL80211_CMD_NEW_INTERFACE = 7
NL80211_CMD_GET_SCAN = 32
NL80211_CMD_TRIGGER_SCAN = 33
NL80211_CMD_NEW_SCAN_RESULTS = 34
NL80211_CMD_START_SCHED_SCAN = 75
NL80211_CMD_STOP_SCHED_SCAN = 76
NL80211_CMD_SCHED_SCAN_RESULTS = 77

# nl80211 attributes
NL80211_ATTR_WIPHY = 1
NL80211_ATTR_IFINDEX = 3
NL80211_ATTR_IFNAME = 4
NL80211_ATTR_MAC = 6
NL80211_ATTR_SCAN_FREQUENCIES = 44
NL80211_ATTR_SCAN_SSIDS = 45
NL80211_ATTR_BSS = 47
NL80211_ATTR_SCHED_SCAN_INTERVAL = 119
NL80211_ATTR_SCHED_SCAN_MATCH = 132
NL80211_ATTR_SCHED_SCAN_PLANS = 225

# Nested nl80211 attributes
NL80211_BSS_INFORMATION_ELEMENTS = 6
NL80211_SCHED_SCAN_MATCH_ATTR_SSID = 1
NL80211_SCHED_SCAN_MATCH_ATTR_RSSI = 2
NL80211_SCHED_SCAN_PLAN_INTERVAL = 1
NL80211_SCHED_SCAN_PLAN_ITERATIONS = 2


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
    attrs: dict[int, bytes] = {}
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


def build_nla(attr_type: int, value: bytes) -> bytes:
    """Builds a 4-byte aligned Netlink Attribute (NLA)."""
    true_len = 4 + len(value)
    header = struct.pack("<HH", true_len, attr_type)
    attr = header + value
    pad_len = (4 - (len(attr) % 4)) % 4
    return attr + b"\x00" * pad_len


def verify_new_interface_response(
    response_list: Sequence[fidl_wlanix.Nl80211Message],
) -> dict[int, bytes]:
    """Verifies an NL80211_CMD_NEW_INTERFACE response list and returns its attributes."""
    last_response_index = len(response_list) - 1
    for response_index, response in enumerate(response_list):
        if response.done:
            assert_equal(
                response_index,
                last_response_index,
                "Nl80211 DONE message before end of response",
            )
            break
        elif response.error:
            fail(f"Received an error Nl80211 message type: {response.error}")
        elif not response.message:
            fail(f"Received an unexpected Nl80211 message: {response}")

        assert response.message
        assert (
            response.message.payload is not None
        ), "MESSAGE must contain a payload"

        payload = bytes(response.message.payload)
        formatted_response_payload = [
            format(b, "#04x") for b in response.message.payload
        ]
        assert_equal(
            response.message.payload[0],
            NL80211_CMD_NEW_INTERFACE,
            f"Payload is not a NewInterface message: {formatted_response_payload}",
        )

        attrs = parse_netlink_attributes(payload, start_offset=4)

        assert (
            NL80211_ATTR_IFINDEX in attrs
        ), f"Response missing attribute NL80211_ATTR_IFINDEX: {formatted_response_payload}"
        assert (
            NL80211_ATTR_WIPHY in attrs
        ), f"Response missing attribute NL80211_ATTR_WIPHY: {formatted_response_payload}"
        assert (
            NL80211_ATTR_IFNAME in attrs
        ), f"Response missing attribute NL80211_ATTR_IFNAME: {formatted_response_payload}"
        assert (
            NL80211_ATTR_MAC in attrs
        ), f"Response missing attribute NL80211_ATTR_MAC: {formatted_response_payload}"

        return attrs

    raise RuntimeError(
        f"Did not find an iface index in the response list: {response_list}"
    )
