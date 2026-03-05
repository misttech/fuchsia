# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
from antlion.controllers.ap_lib.hostapd_constants import BandType
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as HostapdSecurityMode,
)
from mobly_controller.openwrt_access_point.lib.access_point_config import (
    Band,
    Security,
)

# TODO(b/489927930): This file should be removed after OpenWRT migration is complete.


class AccessPointConfigMapper:
    @staticmethod
    def to_hostapd_band(band: Band) -> BandType:
        """Maps Band to BandType"""
        band_map = {
            Band.BAND_2G: BandType.BAND_2G,
            Band.BAND_5G: BandType.BAND_5G,
        }
        return band_map[band]

    @staticmethod
    def to_hostapd_security(security: Security) -> HostapdSecurityMode:
        """Maps Security to HostapdSecurityMode."""
        security_map = {
            Security.NONE: HostapdSecurityMode.OPEN,
            Security.WPA: HostapdSecurityMode.WPA,
            Security.WPA2: HostapdSecurityMode.WPA2,
            Security.WPA3: HostapdSecurityMode.WPA3,
            Security.WPA_WPA2: HostapdSecurityMode.WPA_WPA2,
            Security.WPA2_WPA3: HostapdSecurityMode.WPA2_WPA3,
        }
        return security_map[security]
