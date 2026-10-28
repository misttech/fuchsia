# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utility functions for building and parsing scheduled scan NL80211 payloads."""

import struct

from wlanix_testing.nl80211 import (
    NL80211_ATTR_IFINDEX,
    NL80211_ATTR_SCHED_SCAN_INTERVAL,
    NL80211_CMD_START_SCHED_SCAN,
    NL80211_SCHED_SCAN_MATCH_ATTR_RSSI,
    NL80211_SCHED_SCAN_MATCH_ATTR_SSID,
    build_nla,
)

DEFAULT_SCHED_SCAN_INTERVAL_MS = 5000


def build_match_set(
    ssid: str | bytes,
    rssi: int | None = None,
    index: int = 1,
) -> bytes:
    """Builds an NLA structure for a single match set entry.

    The entry contains the SSID and an optional RSSI threshold.
    """
    ssid_bytes = ssid.encode("ascii") if isinstance(ssid, str) else ssid
    ssid_attr = build_nla(NL80211_SCHED_SCAN_MATCH_ATTR_SSID, ssid_bytes)
    rssi_attr = (
        build_nla(NL80211_SCHED_SCAN_MATCH_ATTR_RSSI, struct.pack("<i", rssi))
        if rssi is not None
        else b""
    )
    return build_nla(index, ssid_attr + rssi_attr)


def build_start_sched_scan_payload(
    iface_index: int,
    match_attr: bytes | None = None,
    interval_ms: int | None = DEFAULT_SCHED_SCAN_INTERVAL_MS,
    scan_ssids_attr: bytes | None = None,
    freqs_attr: bytes | None = None,
    plans_attr: bytes | None = None,
) -> list[int]:
    """Constructs the NL80211_CMD_START_SCHED_SCAN payload list."""
    payload = [
        NL80211_CMD_START_SCHED_SCAN,  # Command
        0x01,  # Version
        0x00,
        0x00,  # Reserved
        *list(build_nla(NL80211_ATTR_IFINDEX, struct.pack("<I", iface_index))),
    ]
    if interval_ms is not None:
        payload.extend(
            build_nla(
                NL80211_ATTR_SCHED_SCAN_INTERVAL,
                struct.pack("<I", interval_ms),
            )
        )
    if match_attr is not None:
        payload.extend(match_attr)
    if scan_ssids_attr is not None:
        payload.extend(scan_ssids_attr)
    if freqs_attr is not None:
        payload.extend(freqs_attr)
    if plans_attr is not None:
        payload.extend(plans_attr)
    return payload
