// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cc::*;
use fidl_next_fuchsia_hardware_usb_connector::Orientation;

/// Verifies that `RpValue`, `CcTermination`, `RoleControl`, and `COMMAND_LOOK4CONNECTION` encode
/// and decode per TCPCI.
#[fuchsia::test]
fn test_register_bitfields() {
    assert_eq!(COMMAND_LOOK4CONNECTION, 0x99);
    assert_eq!(RpValue::default(), RpValue::Default);

    assert_eq!(CcTermination::default(), CcTermination::Open);
    assert_eq!(RoleControl::default(), RoleControl::STATIC_RD);
    assert_eq!(u8::from(RoleControl::FORCE_OPEN), 0x0f);
    assert_eq!(u8::from(RoleControl::STATIC_RD), 0x0a);
    assert_eq!(u8::from(RoleControl::DRP_RD), 0x4a);
    assert_eq!(RoleControl::from(0x0a), RoleControl::STATIC_RD);
    assert!(!RoleControl::from(0x0a).drp);

    // Reserved B7 (`0x80`) and reserved `RpValue = 0b11` (`0x30`) normalize to `B7 = 0` and
    // `RpValue::Default` (`0b00`) on `RoleControl::from(u8)` -> `u8::from(RoleControl)`.
    let reserved_rc = RoleControl::from(0xba);
    assert_eq!(reserved_rc, RoleControl::STATIC_RD);
    assert_eq!(u8::from(reserved_rc), 0x0a);

    let custom_rc = RoleControl {
        drp: true,
        rp_value: RpValue::Rp3a0,
        cc1: CcTermination::Rp,
        cc2: CcTermination::Rd,
    };
    let raw_rc = u8::from(custom_rc);
    assert_eq!(raw_rc, 0b01_10_10_01);
    let decoded_rc = RoleControl::from(raw_rc);
    assert_eq!(decoded_rc, custom_rc);
    assert!(decoded_rc.drp);
    assert_eq!(decoded_rc.rp_value, RpValue::Rp3a0);
    assert_eq!(decoded_rc.cc1, CcTermination::Rp);
    assert_eq!(decoded_rc.cc2, CcTermination::Rd);
}

/// Verifies that `CcPinState` defaults to `Open` and formats matching its static string labels.
#[fuchsia::test]
fn test_cc_state_formatting() {
    assert_eq!(CcPinState::default(), CcPinState::Open);
    let states = [
        (CcPinState::Open, "Open"),
        (CcPinState::RpDefault, "RpDefault"),
        (CcPinState::Rp1a5, "Rp1a5"),
        (CcPinState::Rp3a0, "Rp3a0"),
        (CcPinState::SrcRa, "SrcRa"),
        (CcPinState::SrcRd, "SrcRd"),
    ];
    for (state, label) in states {
        assert_eq!(state.as_str(), label);
        assert_eq!(format!("{state}"), label);
    }
}

/// Verifies that `CcStatus::from_tcpci` clamps `Looking4Connection` to `Open` and resolves
/// `PowerRole` and cable `Orientation`.
#[fuchsia::test]
fn test_cc_status_clamping_and_role_resolution() {
    assert_eq!(CcStatus::default(), CcStatus::new(CcPinState::Open, CcPinState::Open));

    let searching = CcStatus::from_tcpci(0b0011_1011, RoleControl::DRP_RD);
    assert_eq!(searching, CcStatus::LOOKING_FOR_CONNECTION);
    assert!(searching.looking_for_connection);
    assert_eq!(searching.cc1, CcPinState::Open);
    assert_eq!(searching.cc2, CcPinState::Open);
    assert_eq!(searching.power_role(), None);
    assert_eq!(searching.accessory(), None);
    assert_eq!(searching.orientation(), None);

    for manual_searching in [
        CcStatus { cc1: CcPinState::Rp3a0, cc2: CcPinState::Open, looking_for_connection: true },
        CcStatus { cc1: CcPinState::Rp3a0, cc2: CcPinState::Rp1a5, looking_for_connection: true },
        CcStatus { cc1: CcPinState::SrcRd, cc2: CcPinState::SrcRa, looking_for_connection: true },
    ] {
        assert_eq!(manual_searching.power_role(), None);
        assert_eq!(manual_searching.accessory(), None);
        assert_eq!(manual_searching.orientation(), None);
    }

    // Static Rd (`DRP = 0, CC1 = Rd, CC2 = Rd`) with `ConnectResult = 0` still decodes as Sink.
    let static_rd_snk = CcStatus::from_tcpci(0x03, RoleControl::STATIC_RD);
    assert_eq!(static_rd_snk.cc1, CcPinState::Rp3a0);
    assert_eq!(static_rd_snk.cc2, CcPinState::Open);
    assert_eq!(static_rd_snk.power_role(), Some(PowerRole::Sink));
    assert_eq!(static_rd_snk.orientation(), Some(Orientation::Normal));
    let static_rd_cc2_3a0 = CcStatus::from_tcpci(0x1c, RoleControl::STATIC_RD);
    assert_eq!(static_rd_cc2_3a0.cc1, CcPinState::Open);
    assert_eq!(static_rd_cc2_3a0.cc2, CcPinState::Rp3a0);

    // Static Rp (`DRP = 0, CC1 = Rp, CC2 = Rp`) ignores a stale `ConnectResult = 1` bit and
    // decodes `CC1 = SrcRa, CC2 = SrcRd` as Source, while reserved `11b` comparator bits on `Rp`
    // decode as `CcPinState::Open`.
    let static_rp = RoleControl {
        drp: false,
        rp_value: RpValue::Default,
        cc1: CcTermination::Rp,
        cc2: CcTermination::Rp,
    };
    let static_rp_src = CcStatus::from_tcpci(0x19, static_rp);
    assert_eq!(static_rp_src.cc1, CcPinState::SrcRa);
    assert_eq!(static_rp_src.cc2, CcPinState::SrcRd);
    assert_eq!(static_rp_src.power_role(), Some(PowerRole::Source));
    assert_eq!(static_rp_src.orientation(), Some(Orientation::Reverse));
    assert_eq!(CcStatus::from_tcpci(0x0f, static_rp), CcStatus::default());

    // Force Open (`DRP = 0, CC1 = Open, CC2 = Open`) clamps comparator bits to `CcPinState::Open`.
    let forced_open = CcStatus::from_tcpci(0x1b, RoleControl::FORCE_OPEN);
    assert_eq!(forced_open.cc1, CcPinState::Open);
    assert_eq!(forced_open.cc2, CcPinState::Open);

    // DRP mode (`DRP = 1`) uses `ConnectResult`.
    let drp_src_cc2 = CcStatus::from_tcpci(0x09, RoleControl::DRP_RD);
    assert_eq!(drp_src_cc2.cc1, CcPinState::SrcRa);
    assert_eq!(drp_src_cc2.cc2, CcPinState::SrcRd);
    assert_eq!(drp_src_cc2.power_role(), Some(PowerRole::Source));
    assert_eq!(drp_src_cc2.orientation(), Some(Orientation::Reverse));

    let src_cc1_with_ra = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRa);
    assert_eq!(src_cc1_with_ra.power_role(), Some(PowerRole::Source));
    assert_eq!(src_cc1_with_ra.orientation(), Some(Orientation::Normal));
    assert!(drp_src_cc2.shares_connected_cc_pin(static_rp_src));
    assert!(!drp_src_cc2.shares_connected_cc_pin(src_cc1_with_ra));

    let snk_cc2 = CcStatus::from_tcpci(0x14, RoleControl::DRP_RD);
    assert_eq!(snk_cc2.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_cc2.orientation(), Some(Orientation::Reverse));
}

/// Verifies that `CcStatus::accessory` decodes `SinkDebug`, `SourceDebug`, and `Audio` modes,
/// while `power_role` and `orientation` return `None` for unsupported accessory and mixed states.
#[fuchsia::test]
fn test_accessory_and_unsupported_terminations() {
    let snk_cc1 = CcStatus::new(CcPinState::RpDefault, CcPinState::Open);
    assert_eq!(snk_cc1.accessory(), None);

    let snk_dbg_normal = CcStatus::new(CcPinState::Rp3a0, CcPinState::RpDefault);
    assert_eq!(snk_dbg_normal.accessory(), Some(Accessory::SinkDebug));
    assert_eq!(snk_dbg_normal.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_dbg_normal.orientation(), Some(Orientation::Normal));
    assert!(!snk_dbg_normal.shares_connected_cc_pin(snk_cc1));

    let snk_dbg_reverse = CcStatus::new(CcPinState::RpDefault, CcPinState::Rp1a5);
    assert_eq!(snk_dbg_reverse.accessory(), Some(Accessory::SinkDebug));
    assert_eq!(snk_dbg_reverse.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_dbg_reverse.orientation(), Some(Orientation::Reverse));
    assert!(snk_dbg_normal.shares_connected_cc_pin(snk_dbg_reverse));

    // Unoriented Sink Debug Accessory with CC1 == CC2 -> orientation is `None`.
    for state in [CcPinState::RpDefault, CcPinState::Rp1a5, CcPinState::Rp3a0] {
        let unoriented = CcStatus::new(state, state);
        assert_eq!(unoriented.accessory(), Some(Accessory::SinkDebug));
        assert_eq!(unoriented.power_role(), Some(PowerRole::Sink));
        assert_eq!(unoriented.orientation(), None);
    }

    // TODO(https://fxbug.dev/575661321): Support SourceDebug (dual SrcRd) and Audio (dual SrcRa)
    // accessory modes in power_role / orientation.
    let src_dbg = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRd);
    assert_eq!(src_dbg.accessory(), Some(Accessory::SourceDebug));
    assert_eq!(src_dbg.power_role(), None);
    assert_eq!(src_dbg.orientation(), None);

    let audio = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);
    assert_eq!(audio.accessory(), Some(Accessory::Audio));
    assert_eq!(audio.power_role(), None);
    assert_eq!(audio.orientation(), None);

    // Invalid mixed Sink/Source states return `None`.
    for invalid in [
        CcStatus::new(CcPinState::RpDefault, CcPinState::SrcRd),
        CcStatus::new(CcPinState::RpDefault, CcPinState::SrcRa),
    ] {
        assert_eq!(invalid.accessory(), None);
        assert_eq!(invalid.power_role(), None);
        assert_eq!(invalid.orientation(), None);
    }
}
