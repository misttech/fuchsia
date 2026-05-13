# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


from mobly_controller.openwrt_access_point.lib.access_point_config import (
    HostapdOptions,
)


# WMM
class WmmParams:
    DEFAULT_11B: HostapdOptions = {
        "wmm_ac_bk_cwmin": 5,
        "wmm_ac_bk_cwmax": 10,
        "wmm_ac_bk_aifs": 7,
        "wmm_ac_bk_txop_limit": 0,
        "wmm_ac_be_aifs": 3,
        "wmm_ac_be_cwmin": 5,
        "wmm_ac_be_cwmax": 7,
        "wmm_ac_be_txop_limit": 0,
        "wmm_ac_vi_aifs": 2,
        "wmm_ac_vi_cwmin": 4,
        "wmm_ac_vi_cwmax": 5,
        "wmm_ac_vi_txop_limit": 188,
        "wmm_ac_vo_aifs": 2,
        "wmm_ac_vo_cwmin": 3,
        "wmm_ac_vo_cwmax": 4,
        "wmm_ac_vo_txop_limit": 102,
    }

    DEFAULT_PHYS_11A_11G_11N_11AC_DEFAULT_PARAMS: HostapdOptions = {
        "wmm_ac_bk_cwmin": 4,
        "wmm_ac_bk_cwmax": 10,
        "wmm_ac_bk_aifs": 7,
        "wmm_ac_bk_txop_limit": 0,
        "wmm_ac_be_aifs": 3,
        "wmm_ac_be_cwmin": 4,
        "wmm_ac_be_cwmax": 10,
        "wmm_ac_be_txop_limit": 0,
        "wmm_ac_vi_aifs": 2,
        "wmm_ac_vi_cwmin": 3,
        "wmm_ac_vi_cwmax": 4,
        "wmm_ac_vi_txop_limit": 94,
        "wmm_ac_vo_aifs": 2,
        "wmm_ac_vo_cwmin": 2,
        "wmm_ac_vo_cwmax": 3,
        "wmm_ac_vo_txop_limit": 47,
    }

    NON_DEFAULT: HostapdOptions = {
        "wmm_ac_bk_cwmin": 5,
        "wmm_ac_bk_cwmax": 9,
        "wmm_ac_bk_aifs": 3,
        "wmm_ac_bk_txop_limit": 94,
        "wmm_ac_be_aifs": 2,
        "wmm_ac_be_cwmin": 2,
        "wmm_ac_be_cwmax": 8,
        "wmm_ac_be_txop_limit": 0,
        "wmm_ac_vi_aifs": 1,
        "wmm_ac_vi_cwmin": 7,
        "wmm_ac_vi_cwmax": 10,
        "wmm_ac_vi_txop_limit": 47,
        "wmm_ac_vo_aifs": 1,
        "wmm_ac_vo_cwmin": 6,
        "wmm_ac_vo_cwmax": 10,
        "wmm_ac_vo_txop_limit": 94,
    }


class WmmAcm:
    BK: HostapdOptions = {"wmm_ac_bk_acm": 1}
    BE: HostapdOptions = {"wmm_ac_be_acm": 1}
    VI: HostapdOptions = {"wmm_ac_vi_acm": 1}
    VO: HostapdOptions = {"wmm_ac_vo_acm": 1}


class AssocRespIe:
    CORRECT_LENGTH: HostapdOptions = {"assocresp_elements": "dd0411223301"}
    ZERO_LENGTH_WITHOUT_DATA: HostapdOptions = {"assocresp_elements": "dd00"}


class Country3:
    ALL: HostapdOptions = {"country3": "0x20"}
    OUTDOOR: HostapdOptions = {"country3": "0x4f"}
    INDOOR: HostapdOptions = {"country3": "0x49"}
    NONCOUNTRY: HostapdOptions = {"country3": "0x58"}
    GLOBAL: HostapdOptions = {"country3": "0x04"}
