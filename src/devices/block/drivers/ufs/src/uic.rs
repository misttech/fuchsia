// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFS InterConnect (UIC) layer DME command primitives and MIPI UniPro MIB attribute definitions
//! (UFSHCI 3.0/4.0 section 5.6, MIPI UniPro v1.8/v2.0).
//!
//! A UIC command is issued by waiting for `HCS.UCRDY`, writing `UICCMDARG1..3`, then `UICCMD`,
//! and waiting for `IS.UCCS`. `UCCS` is always cleared afterwards. Power mode changes
//! (`DME_SET(PA_PWRMode)`) and hibernate enter/exit additionally complete through `IS.UPMS`,
//! `IS.UHES`, or `IS.UHXS` plus `HCS.UPMCRS`. [`crate::controller::UfsController`] builds
//! complete commands out of these primitives.

use crate::UfsError;
use crate::registers::{
    HostControllerStatusReg, InterruptStatusReg, PowerModeStatus, UicCommandArgument1Reg,
    UicCommandArgument2Reg, UicCommandArgument3Reg, UicCommandReg,
};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};

/// UIC command opcodes (UFSHCI 3.0/4.0 section 5.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UicCommandOpcode {
    /// Read a local UniPro MIB attribute (`DME_GET`).
    DmeGet = 0x01,
    /// Write a local UniPro MIB attribute (`DME_SET`).
    DmeSet = 0x02,
    /// Read a peer (device) UniPro MIB attribute (`DME_PEER_GET`).
    DmePeerGet = 0x03,
    /// Write a peer (device) UniPro MIB attribute (`DME_PEER_SET`).
    DmePeerSet = 0x04,
    /// Power on the UniPro stack (`DME_POWERON`).
    DmePowerOn = 0x10,
    /// Power off the UniPro stack (`DME_POWEROFF`).
    DmePowerOff = 0x11,
    /// Enable the UniPro stack (`DME_ENABLE`).
    DmeEnable = 0x12,
    /// Reset the local UniPro stack (`DME_RESET`).
    DmeReset = 0x14,
    /// Reset the remote UniPro endpoint (`DME_ENDPOINTRESET`).
    DmeEndpointReset = 0x15,
    /// Initiate UniPro link startup (`DME_LINKSTARTUP`).
    DmeLinkStartup = 0x16,
    /// Enter UniPro Hibernate state (`DME_HIBERNATE_ENTER`).
    DmeHibernateEnter = 0x17,
    /// Exit UniPro Hibernate state (`DME_HIBERNATE_EXIT`).
    DmeHibernateExit = 0x18,
    /// Enter UniPro test mode (`DME_TEST_MODE`).
    DmeTestMode = 0x1A,
}

impl UicCommandOpcode {
    /// Returns whether the command completes through `IS.UCCS` alone. Hibernate enter/exit also
    /// need `IS.UHES`/`IS.UHXS`.
    pub fn completes_with_uccs(self) -> bool {
        !matches!(self, Self::DmeHibernateEnter | Self::DmeHibernateExit)
    }
}

/// Available TX data lanes (`PA_AvailTxDataLanes`, MIPI UniPro v1.8/v2.0).
pub const PA_AVAILTXDATALANES: u16 = 0x1520;
/// Available RX data lanes (`PA_AvailRxDataLanes`).
pub const PA_AVAILRXDATALANES: u16 = 0x1540;
/// Active TX data lanes (`PA_ActiveTxDataLanes`).
pub const PA_ACTIVETXDATALANES: u16 = 0x1560;
/// Connected TX data lanes (`PA_ConnectedTxDataLanes`).
pub const PA_CONNECTEDTXDATALANES: u16 = 0x1561;
/// TX gear (`PA_TxGear`).
pub const PA_TXGEAR: u16 = 0x1568;
/// TX termination enable (`PA_TxTermination`).
pub const PA_TXTERMINATION: u16 = 0x1569;
/// High-Speed rate series (`PA_HSSeries`: 1 = Rate A, 2 = Rate B).
pub const PA_HSSERIES: u16 = 0x156A;
/// Link power mode (`PA_PWRMode`).
pub const PA_PWRMODE: u16 = 0x1571;
/// Active RX data lanes (`PA_ActiveRxDataLanes`).
pub const PA_ACTIVERXDATALANES: u16 = 0x1580;
/// Connected RX data lanes (`PA_ConnectedRxDataLanes`).
pub const PA_CONNECTEDRXDATALANES: u16 = 0x1581;
/// RX gear (`PA_RxGear`).
pub const PA_RXGEAR: u16 = 0x1583;
/// RX termination enable (`PA_RxTermination`).
pub const PA_RXTERMINATION: u16 = 0x1584;
/// Maximum supported RX High-Speed gear (`PA_MaxRxHSGear`).
pub const PA_MAXRXHSGEAR: u16 = 0x1587;
/// Remote (device) UniPro version (`PA_RemoteVerInfo`).
pub const PA_REMOTEVERINFO: u16 = 0x15A0;
/// M-PHY `T_ACTIVATE` wake-up time (`PA_TActivate`).
pub const PA_TACTIVATE: u16 = 0x15A8;
/// Local (host) UniPro version (`PA_LocalVerInfo`).
pub const PA_LOCALVERINFO: u16 = 0x15A9;
/// M-PHY time granularity (`PA_Granularity`).
pub const PA_GRANULARITY: u16 = 0x15AA;
/// Power mode user data 0 (`PA_PWRModeUserData0`: `DL_FC0ProtectionTimeOutVal`).
pub const PA_PWRMODEUSERDATA0: u16 = 0x15B0;
/// Power mode user data 1 (`PA_PWRModeUserData1`: `DL_TC0ReplayTimeOutVal`).
pub const PA_PWRMODEUSERDATA1: u16 = 0x15B1;
/// Power mode user data 2 (`PA_PWRModeUserData2`: `DL_AFC0ReqTimeOutVal`).
pub const PA_PWRMODEUSERDATA2: u16 = 0x15B2;
/// Power mode user data 3 (`PA_PWRModeUserData3`).
pub const PA_PWRMODEUSERDATA3: u16 = 0x15B3;
/// Power mode user data 4 (`PA_PWRModeUserData4`).
pub const PA_PWRMODEUSERDATA4: u16 = 0x15B4;
/// Power mode user data 5 (`PA_PWRModeUserData5`).
pub const PA_PWRMODEUSERDATA5: u16 = 0x15B5;
/// Local Data Link FC0 protection timeout (`DME_LocalFC0ProtectionTimeOutVal`).
pub const DME_LOCAL_FC0_PROTECTION_TIMEOUT_VAL: u16 = 0xD041;
/// Local Data Link TC0 replay timeout (`DME_LocalTC0ReplayTimeOutVal`).
pub const DME_LOCAL_TC0_REPLAY_TIMEOUT_VAL: u16 = 0xD042;
/// Local Data Link AFC0 request timeout (`DME_LocalAFC0ReqTimeOutVal`).
pub const DME_LOCAL_AFC0_REQ_TIMEOUT_VAL: u16 = 0xD043;
/// TX High-Speed equalization adaptation type (`PA_TxHsAdaptType`, required for HS-G4/G5).
pub const PA_TXHSADAPTTYPE: u16 = 0x15D4;
/// Initial High-Speed adaptation (`PA_INITIAL_ADAPT`).
pub const PA_INITIAL_ADAPT: u32 = 0x01;

/// Default UniPro Data Link layer FC0 protection timeout value.
pub const DL_FC0_PROTECTION_TIMEOUT_VAL: u32 = 8191;
/// Default UniPro Data Link layer TC0 replay timeout value.
pub const DL_TC0_REPLAY_TIMEOUT_VAL: u32 = 65535;
/// Default UniPro Data Link layer AFC0 request timeout value.
pub const DL_AFC0_REQ_TIMEOUT_VAL: u32 = 32767;

/// Interval between register polls while waiting for UIC or controller state changes.
pub const REGISTER_POLL_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);
/// Local DME commands complete within a few microseconds, so [`poll_until`] spins this long
/// before falling back to [`REGISTER_POLL_INTERVAL`] timer polling: a timer wakeup costs more
/// than the command itself, and the PHY calibration issues hundreds of them back to back.
pub const SPIN_POLL_BUDGET: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);

/// A UIC command and its three argument register values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UicCommand {
    /// Command opcode written to `UICCMD`.
    pub opcode: UicCommandOpcode,
    /// `UICCMDARG1`: MIB attribute (`[31:16]`) and GenSelector index (`[15:0]`).
    pub arg1: u32,
    /// `UICCMDARG2`: attribute set type (`[23:16]`).
    pub arg2: u32,
    /// `UICCMDARG3`: value to write.
    pub arg3: u32,
}

impl UicCommand {
    /// Creates a command with all-zero arguments.
    pub fn new(opcode: UicCommandOpcode) -> Self {
        Self { opcode, arg1: 0, arg2: 0, arg3: 0 }
    }

    /// Creates a `DME_*GET`/`DME_*SET` command for `mib_attribute` and `gen_selector_index`.
    pub fn dme(
        opcode: UicCommandOpcode,
        mib_attribute: u16,
        gen_selector_index: u16,
        value: u32,
    ) -> Self {
        let arg1 = ((mib_attribute as u32) << 16) | (gen_selector_index as u32);
        Self { opcode, arg1, arg2: 0, arg3: value }
    }
}

/// Polls `step` until it returns `Some`: spins for [`SPIN_POLL_BUDGET`] after the first miss (so
/// a local DME command that completes within microseconds never pays for a timer wakeup), then
/// sleeps [`REGISTER_POLL_INTERVAL`] between attempts. The same helper waits for `HCE`, link
/// startup, hibernate and power mode changes, where the short initial spin is harmless.
///
/// # Errors
///
/// Returns `Timeout` if `step` has not returned `Some` after `timeout`.
pub async fn poll_until<T>(
    timeout: zx::MonotonicDuration,
    mut step: impl FnMut() -> Option<T>,
) -> Result<T, UfsError> {
    let start = zx::MonotonicInstant::get();
    let deadline = start + timeout;
    let spin_until = start + SPIN_POLL_BUDGET;
    loop {
        if let Some(value) = step() {
            return Ok(value);
        }
        let now = zx::MonotonicInstant::get();
        if now > deadline {
            return Err(UfsError::Timeout);
        }
        if now < spin_until {
            std::hint::spin_loop();
            continue;
        }
        fuchsia_async::Timer::new(zx::MonotonicInstant::after(REGISTER_POLL_INTERVAL)).await;
    }
}

/// Returns whether the controller can accept a UIC command (`HCS.UCRDY`).
pub fn uic_command_ready<M: Mmio>(mmio: &M) -> bool {
    HostControllerStatusReg::read(mmio).uic_command_ready()
}

/// Writes the arguments and opcode of `command`. The caller must have observed `HCS.UCRDY`.
pub fn issue_uic_command<M: Mmio>(mmio: &mut M, command: &UicCommand) {
    clear_interrupt_status(mmio, |is| is.set_uic_command_completion_status(true));
    UicCommandArgument1Reg::from_raw(command.arg1).write(mmio);
    UicCommandArgument2Reg::from_raw(command.arg2).write(mmio);
    UicCommandArgument3Reg::from_raw(command.arg3).write(mmio);
    let mut cmd_reg = UicCommandReg::from_raw(0);
    cmd_reg.set_command_opcode(command.opcode as u32);
    cmd_reg.write(mmio);
}

/// Returns `Some(result)` once `IS.UCCS` is set, clearing `UCCS`. The result is `UICCMDARG3` on
/// success or `UicError` with the `UICCMDARG2` result code.
pub fn poll_uic_command_completion<M: Mmio>(
    mmio: &mut M,
    opcode: UicCommandOpcode,
) -> Option<Result<u32, UfsError>> {
    if !InterruptStatusReg::read(mmio).uic_command_completion_status() {
        return None;
    }
    let result_code = UicCommandArgument2Reg::read(mmio).result_code();
    let return_value = UicCommandArgument3Reg::read(mmio).value();
    clear_interrupt_status(mmio, |is| is.set_uic_command_completion_status(true));
    if result_code != 0 {
        return Some(Err(UfsError::UicError { opcode: opcode as u8, code: result_code }));
    }
    Some(Ok(return_value))
}

/// Writes 1 to the `IS` bits selected by `select` (IS is write-1-to-clear).
pub fn clear_interrupt_status<M: Mmio>(mmio: &mut M, select: impl FnOnce(&mut InterruptStatusReg)) {
    let mut clear = InterruptStatusReg::from_raw(0);
    select(&mut clear);
    clear.write(mmio);
}

/// Maps `HCS.UPMCRS` after a power mode or hibernate change to a result.
pub fn check_power_mode_status<M: Mmio>(
    mmio: &M,
    opcode: UicCommandOpcode,
) -> Result<(), UfsError> {
    let hcs = HostControllerStatusReg::read(mmio);
    match hcs.power_mode_status() {
        Some(PowerModeStatus::PowerLocal) => Ok(()),
        _ => Err(UfsError::UicError {
            opcode: opcode as u8,
            code: hcs.uic_power_mode_change_request_status(),
        }),
    }
}
