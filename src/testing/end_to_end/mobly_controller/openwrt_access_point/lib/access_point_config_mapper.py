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
            Security.WEP: HostapdSecurityMode.WEP,
        }
        return security_map[security]

    @staticmethod
    def to_hostapd_n_cap(cap: str) -> object:
        """Maps a generic capability string to its hostapd_constants equivalent."""
        from antlion.controllers.ap_lib import hostapd_constants
        from mobly_controller.openwrt_access_point.lib import capabilities

        mapping = {
            capabilities.N_CAPABILITY_LDPC: hostapd_constants.N_CAPABILITY_LDPC,
            capabilities.N_CAPABILITY_SHORT_GI_20: hostapd_constants.N_CAPABILITY_SGI20,
            capabilities.N_CAPABILITY_SHORT_GI_40: hostapd_constants.N_CAPABILITY_SGI40,
            capabilities.N_CAPABILITY_TX_STBC: hostapd_constants.N_CAPABILITY_TX_STBC,
            capabilities.N_CAPABILITY_RX_STBC1: hostapd_constants.N_CAPABILITY_RX_STBC1,
            capabilities.N_CAPABILITY_MAX_AMSDU_7935: hostapd_constants.N_CAPABILITY_MAX_AMSDU_7935,
            capabilities.N_CAPABILITY_HT40_PLUS: hostapd_constants.N_CAPABILITY_HT40_PLUS,
            capabilities.N_CAPABILITY_HT20: hostapd_constants.N_CAPABILITY_HT20,
        }
        return mapping.get(cap, cap)

    @staticmethod
    def to_hostapd_ac_cap(cap: str) -> object:
        """Maps a generic capability string to its hostapd_constants equivalent."""
        from antlion.controllers.ap_lib import hostapd_constants
        from mobly_controller.openwrt_access_point.lib import capabilities

        mapping = {
            capabilities.AC_CAPABILITY_MAX_MPDU_7991: hostapd_constants.AC_CAPABILITY_MAX_MPDU_7991,
            capabilities.AC_CAPABILITY_MAX_MPDU_11454: hostapd_constants.AC_CAPABILITY_MAX_MPDU_11454,
            capabilities.AC_CAPABILITY_RXLDPC: hostapd_constants.AC_CAPABILITY_RXLDPC,
            capabilities.AC_CAPABILITY_SHORT_GI_80: hostapd_constants.AC_CAPABILITY_SHORT_GI_80,
            capabilities.AC_CAPABILITY_SHORT_GI_160: hostapd_constants.AC_CAPABILITY_SHORT_GI_160,
            capabilities.AC_CAPABILITY_TX_STBC_2BY1: hostapd_constants.AC_CAPABILITY_TX_STBC_2BY1,
            capabilities.AC_CAPABILITY_RX_STBC_1: hostapd_constants.AC_CAPABILITY_RX_STBC_1,
            capabilities.AC_CAPABILITY_RX_STBC_12: hostapd_constants.AC_CAPABILITY_RX_STBC_12,
            capabilities.AC_CAPABILITY_RX_STBC_123: hostapd_constants.AC_CAPABILITY_RX_STBC_123,
            capabilities.AC_CAPABILITY_RX_STBC_1234: hostapd_constants.AC_CAPABILITY_RX_STBC_1234,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP0: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP0,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP1: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP1,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP2: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP2,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP3: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP3,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP4: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP4,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP5: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP5,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP6: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP6,
            capabilities.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP7: hostapd_constants.AC_CAPABILITY_MAX_A_MPDU_LEN_EXP7,
            capabilities.AC_CAPABILITY_VHT_LINK_ADAPT2: hostapd_constants.AC_CAPABILITY_VHT_LINK_ADAPT2,
            capabilities.AC_CAPABILITY_VHT_LINK_ADAPT3: hostapd_constants.AC_CAPABILITY_VHT_LINK_ADAPT3,
            capabilities.AC_CAPABILITY_VHT160: hostapd_constants.AC_CAPABILITY_VHT160,
            capabilities.AC_CAPABILITY_VHT160_80PLUS80: hostapd_constants.AC_CAPABILITY_VHT160_80PLUS80,
            capabilities.AC_CAPABILITY_RX_ANTENNA_PATTERN: hostapd_constants.AC_CAPABILITY_RX_ANTENNA_PATTERN,
            capabilities.AC_CAPABILITY_TX_ANTENNA_PATTERN: hostapd_constants.AC_CAPABILITY_TX_ANTENNA_PATTERN,
        }
        return mapping.get(cap, cap)
