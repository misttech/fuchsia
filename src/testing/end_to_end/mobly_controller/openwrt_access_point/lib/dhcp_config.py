# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""DHCP configuration dataclasses for OpenWrt Access Point."""

import dataclasses


@dataclasses.dataclass
class Lan:
    """UCI dhcp.lan configuration.

    Attributes:
        dynamic_dhcp: Whether to enable dynamic DHCP.
        lease_time: The lease time for IP addresses (e.g., '12h', '30m').
    """

    dynamic_dhcp: bool = True
    lease_time: str = "12h"


@dataclasses.dataclass
class DhcpConfig:
    """DHCP configuration for OpenWrt.

    Attributes:
        lan: The LAN interface DHCP configuration.
    """

    lan: Lan = dataclasses.field(default_factory=Lan)
