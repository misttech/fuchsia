// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_fuchsia_hardware_usb_connector::Orientation;
use std::fmt;

const CC_STATUS_LOOKING4CONN_BIT: u8 = 1 << 5;
const CC_STATUS_CONNECT_RESULT_BIT: u8 = 1 << 4;
const ROLE_CONTROL_DRP_BIT: u8 = 1 << 6;

/// TCPCI `COMMAND` (`0x23`) value for `Look4Connection` (`0x99`, TCPCI Rev 2.0 Ver 1.3 Section
/// 4.4.8 Table 4-27).
///
/// Dual-role (`ROLE_CONTROL.DRP = 1`) TCPCs require writing `COMMAND_LOOK4CONNECTION` to `COMMAND`
/// (`0x23`) when entering `Unattached` so the hardware autonomous DRP toggle machine starts
/// searching for a connection.
pub const COMMAND_LOOK4CONNECTION: u8 = 0x99;

/// TCPCI `ROLE_CONTROL` (`0x1A`) `RP_VALUE` pull-up current advertisement setting (bits `5..=4`).
///
/// Variants are ordered by increasing pull-up current strength (`Default < Rp1a5 < Rp3a0`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RpValue {
    /// Default USB Power (`00b`).
    #[default]
    Default = 0,
    /// 1.5 A @ 5 V (`01b`).
    Rp1a5 = 1,
    /// 3.0 A @ 5 V (`10b`).
    Rp3a0 = 2,
}

impl RpValue {
    const fn decode(value: u8) -> Self {
        match value & 0b11 {
            1 => Self::Rp1a5,
            2 => Self::Rp3a0,
            _ => Self::Default,
        }
    }
}

/// TCPCI `ROLE_CONTROL` (`0x1A`) CC pin termination setting (`Ra`, `Rp`, `Rd`, or `Open`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CcTermination {
    /// `Ra` pull-down termination (`00b`).
    Ra = 0,
    /// `Rp` pull-up termination (`01b`).
    Rp = 1,
    /// `Rd` pull-down termination (`10b`).
    Rd = 2,
    /// Open / disconnected (`11b` when `DRP = 0`; when `DRP = 1`, TCPCI requires both CC pins set
    /// to `Rp` or both set to `Rd`).
    #[default]
    Open = 3,
}

impl CcTermination {
    const fn decode(value: u8) -> Self {
        match value & 0b11 {
            1 => Self::Rp,
            2 => Self::Rd,
            3 => Self::Open,
            _ => Self::Ra,
        }
    }
}

/// Decoded TCPCI `ROLE_CONTROL` (`0x1A`) register value.
///
/// [`RoleControl::default()`] returns [`Self::STATIC_RD`] (`0x0A`), the power-on default for
/// Sink-only and dead-battery DRP ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RoleControl {
    /// Whether autonomous DRP toggling (`ROLE_CONTROL.DRP`, bit 6) is enabled.
    pub drp: bool,
    /// Configured Source `Rp` current advertisement (`ROLE_CONTROL.RP_VALUE`, bits `5..=4`).
    pub rp_value: RpValue,
    /// Configured termination for `CC1` (`ROLE_CONTROL.CC1`, bits `1..=0`).
    pub cc1: CcTermination,
    /// Configured termination for `CC2` (`ROLE_CONTROL.CC2`, bits `3..=2`).
    pub cc2: CcTermination,
}

impl Default for RoleControl {
    fn default() -> Self {
        Self::STATIC_RD
    }
}

impl RoleControl {
    /// Forces both CC pins open with DRP disabled (`0x0F`: `DRP = 0`, `CC1 = Open`, `CC2 = Open`).
    pub const FORCE_OPEN: Self = Self {
        drp: false,
        rp_value: RpValue::Default,
        cc1: CcTermination::Open,
        cc2: CcTermination::Open,
    };

    /// Presents static Sink `Rd` on both CC pins with DRP disabled (`0x0A`: `DRP = 0`,
    /// `CC1 = Rd`, `CC2 = Rd`).
    pub const STATIC_RD: Self = Self {
        drp: false,
        rp_value: RpValue::Default,
        cc1: CcTermination::Rd,
        cc2: CcTermination::Rd,
    };

    /// Enables autonomous DRP toggling with default `Rd` seed terminations (`0x4A`: `DRP = 1`,
    /// `RP_VALUE = Default`, `CC1 = Rd`, `CC2 = Rd`).
    ///
    /// Note: Per TCPCI Rev 2.0 Ver 1.3 Section 4.4.5.2 (Table 4-16), setting
    /// `ROLE_CONTROL.DRP = 1` configures the autonomous toggle parameters, and issuing
    /// [`COMMAND_LOOK4CONNECTION`] (`0x99`) to `COMMAND` (`0x23`) starts autonomous DRP toggling.
    pub const DRP_RD: Self = Self {
        drp: true,
        rp_value: RpValue::Default,
        cc1: CcTermination::Rd,
        cc2: CcTermination::Rd,
    };
}

impl From<u8> for RoleControl {
    fn from(raw: u8) -> Self {
        Self {
            drp: (raw & ROLE_CONTROL_DRP_BIT) != 0,
            rp_value: RpValue::decode(raw >> 4),
            cc2: CcTermination::decode(raw >> 2),
            cc1: CcTermination::decode(raw),
        }
    }
}

impl From<RoleControl> for u8 {
    fn from(role: RoleControl) -> Self {
        let drp_bit = if role.drp { ROLE_CONTROL_DRP_BIT } else { 0 };
        drp_bit | ((role.rp_value as u8) << 4) | ((role.cc2 as u8) << 2) | (role.cc1 as u8)
    }
}

/// Type-C Configuration Channel (CC) line state sensed on a single CC pin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CcPinState {
    /// Line is open (`vOpen`), reserved, or invalid while `Looking4Connection` is active.
    #[default]
    Open = 0,
    /// TCPC is presenting `Rd` (Sink) and senses partner `Rp` at Default USB current.
    RpDefault = 1,
    /// TCPC is presenting `Rd` (Sink) and senses partner `Rp` at 1.5 A @ 5 V.
    Rp1a5 = 2,
    /// TCPC is presenting `Rd` (Sink) and senses partner `Rp` at 3.0 A @ 5 V.
    Rp3a0 = 3,
    /// TCPC is presenting `Rp` (Source) and senses cable/accessory `Ra` pull-down (`VCONN`).
    SrcRa = 4,
    /// TCPC is presenting `Rp` (Source) and senses partner `Rd` pull-down (Sink partner attached).
    SrcRd = 5,
}

impl CcPinState {
    /// Decodes a 2-bit TCPCI `CC1_STATE` or `CC2_STATE` comparator value given the active local
    /// [`CcTermination`] on that pin.
    const fn decode(termination: CcTermination, raw_cc_state: u8) -> Self {
        match (termination, raw_cc_state & 0b11) {
            (CcTermination::Rd, 1) => Self::RpDefault,
            (CcTermination::Rd, 2) => Self::Rp1a5,
            (CcTermination::Rd, 3) => Self::Rp3a0,
            (CcTermination::Rp, 1) => Self::SrcRa,
            (CcTermination::Rp, 2) => Self::SrcRd,
            _ => Self::Open,
        }
    }

    /// Returns `true` if the pin senses a Source `Rp` termination while presenting `Rd`.
    pub(crate) const fn is_sink_rp(self) -> bool {
        matches!(self, Self::RpDefault | Self::Rp1a5 | Self::Rp3a0)
    }

    /// Returns `true` if this pin carries an active partner CC termination (`Rp*` or `SrcRd`).
    ///
    /// [`CcPinState::SrcRa`] indicates an active cable `Ra` (`VCONN`) load rather than the active CC
    /// signal wire, so it returns `false`.
    const fn is_cc_connected(self) -> bool {
        self.is_sink_rp() || matches!(self, Self::SrcRd)
    }

    /// Returns the static string label for this state (used in Inspect and logs).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::RpDefault => "RpDefault",
            Self::Rp1a5 => "Rp1a5",
            Self::Rp3a0 => "Rp3a0",
            Self::SrcRa => "SrcRa",
            Self::SrcRd => "SrcRd",
        }
    }
}

impl fmt::Display for CcPinState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Type-C Accessory Mode detected when both `CC1` and `CC2` are terminated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Accessory {
    /// Sink Debug Accessory Mode (`DebugAccessory.SNK`; both `CC1` and `CC2` sense `Rp*`).
    SinkDebug,
    /// Source Debug Accessory Mode (both `CC1` and `CC2` sense `SrcRd`).
    SourceDebug,
    /// Audio Adapter Accessory Mode (both `CC1` and `CC2` sense `SrcRa`).
    Audio,
}

/// Type-C port power role resolved from CC line terminations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowerRole {
    /// Port is acting as a power Sink (presenting `Rd`, partner presenting `Rp`).
    Sink,
    /// Port is acting as a power Source (presenting `Rp`, partner presenting `Rd`).
    Source,
}

/// Decoded snapshot of both Type-C CC pins and hardware connection-search status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CcStatus {
    /// Decoded state of the `CC1` pin.
    pub cc1: CcPinState,
    /// Decoded state of the `CC2` pin.
    pub cc2: CcPinState,
    /// Whether the TCPC is actively searching for a connection (`Looking4Connection == 1`).
    pub looking_for_connection: bool,
}

impl CcStatus {
    /// Port is actively searching / DRP toggling (`Looking4Connection == 1`, `CC1 = Open`,
    /// `CC2 = Open`).
    pub const LOOKING_FOR_CONNECTION: Self =
        Self { cc1: CcPinState::Open, cc2: CcPinState::Open, looking_for_connection: true };

    /// Creates a settled (`looking_for_connection = false`) [`CcStatus`] snapshot with `cc1` and
    /// `cc2`.
    pub const fn new(cc1: CcPinState, cc2: CcPinState) -> Self {
        Self { cc1, cc2, looking_for_connection: false }
    }

    /// Decodes a [`CcStatus`] from a raw TCPCI `CC_STATUS` (`0x1D`) byte and [`RoleControl`]
    /// (`0x1A`), handling both autonomous DRP (`DRP == 1`, using `ConnectResult`) and static
    /// `ROLE_CONTROL` pin configurations (`DRP == 0`, using `CC1`/`CC2` directly).
    ///
    /// While `Looking4Connection` (bit 5) is `1`, returns [`Self::LOOKING_FOR_CONNECTION`].
    pub const fn from_tcpci(raw_cc_status: u8, role_control: RoleControl) -> Self {
        if (raw_cc_status & CC_STATUS_LOOKING4CONN_BIT) != 0 {
            return Self::LOOKING_FOR_CONNECTION;
        }
        let (cc1_term, cc2_term) = if role_control.drp {
            let term = if (raw_cc_status & CC_STATUS_CONNECT_RESULT_BIT) != 0 {
                CcTermination::Rd
            } else {
                CcTermination::Rp
            };
            (term, term)
        } else {
            (role_control.cc1, role_control.cc2)
        };
        Self::new(
            CcPinState::decode(cc1_term, raw_cc_status),
            CcPinState::decode(cc2_term, raw_cc_status >> 2),
        )
    }

    /// Returns the [`Accessory`] mode indicated when both `CC1` and `CC2` carry a dual-pin
    /// accessory termination (`Rp/Rp` => [`Accessory::SinkDebug`], `SrcRd/SrcRd` =>
    /// [`Accessory::SourceDebug`], `SrcRa/SrcRa` => [`Accessory::Audio`]), or `None` otherwise.
    pub const fn accessory(self) -> Option<Accessory> {
        if self.looking_for_connection {
            return None;
        }
        match (self.cc1, self.cc2) {
            (cc1, cc2) if cc1.is_sink_rp() && cc2.is_sink_rp() => Some(Accessory::SinkDebug),
            (CcPinState::SrcRd, CcPinState::SrcRd) => Some(Accessory::SourceDebug),
            (CcPinState::SrcRa, CcPinState::SrcRa) => Some(Accessory::Audio),
            _ => None,
        }
    }

    /// Resolves the port's [`PowerRole`] if a supported partner termination is present.
    ///
    /// - Returns `Some(PowerRole::Sink)` when one CC pin senses `Rp*` and the other is `Open`, or
    ///   when both CC pins sense `Rp*` ([`Accessory::SinkDebug`]).
    /// - Returns `Some(PowerRole::Source)` when one CC pin senses `SrcRd` and the other is `Open`
    ///   or `SrcRa`.
    /// - Returns `None` when disconnected, searching, sensing invalid mixed terminations, or
    ///   sensing [`Accessory::SourceDebug`] / [`Accessory::Audio`].
    pub const fn power_role(self) -> Option<PowerRole> {
        if self.looking_for_connection {
            return None;
        }
        match (self.cc1, self.cc2) {
            (cc, CcPinState::Open) | (CcPinState::Open, cc) if cc.is_sink_rp() => {
                Some(PowerRole::Sink)
            }
            (cc1, cc2) if cc1.is_sink_rp() && cc2.is_sink_rp() => Some(PowerRole::Sink),
            (CcPinState::SrcRd, CcPinState::Open | CcPinState::SrcRa)
            | (CcPinState::Open | CcPinState::SrcRa, CcPinState::SrcRd) => Some(PowerRole::Source),
            // TODO(https://fxbug.dev/575661321): Support SourceDebug (dual SrcRd) and Audio
            // (dual SrcRa) accessory modes.
            (CcPinState::SrcRd, CcPinState::SrcRd) | (CcPinState::SrcRa, CcPinState::SrcRa) => None,
            _ => None,
        }
    }

    /// Returns `true` if `self` and `other` resolve to the same [`PowerRole`] and the exact same
    /// set of CC signal wires (`CC1`, `CC2`, or both for [`Accessory::SinkDebug`]) remain
    /// connected across both snapshots.
    pub(crate) const fn shares_connected_cc_pin(self, other: Self) -> bool {
        matches!(
            (self.power_role(), other.power_role()),
            (Some(PowerRole::Sink), Some(PowerRole::Sink))
                | (Some(PowerRole::Source), Some(PowerRole::Source))
        ) && self.cc1.is_cc_connected() == other.cc1.is_cc_connected()
            && self.cc2.is_cc_connected() == other.cc2.is_cc_connected()
    }

    /// Resolves the Type-C cable plug [`Orientation`] from the CC pin states.
    ///
    /// - For a standard Sink or Source connection, the pin carrying `Rp*` or `SrcRd` determines
    ///   orientation (`CC1 => Normal`, `CC2 => Reverse`).
    /// - For an oriented Sink Debug Accessory (`Rp*` on both pins with `CC1 != CC2`), the pin with
    ///   the stronger `Rp` termination determines orientation (`CC1 > CC2 => Normal`,
    ///   `CC2 > CC1 => Reverse`); equal `Rp` values resolve to `None`.
    pub const fn orientation(self) -> Option<Orientation> {
        if self.power_role().is_none() {
            return None;
        }
        match (self.cc1.is_cc_connected(), self.cc2.is_cc_connected()) {
            (true, false) => Some(Orientation::Normal),
            (false, true) => Some(Orientation::Reverse),
            _ if (self.cc1 as u8) > (self.cc2 as u8) => Some(Orientation::Normal),
            _ if (self.cc2 as u8) > (self.cc1 as u8) => Some(Orientation::Reverse),
            _ => None,
        }
    }
}
