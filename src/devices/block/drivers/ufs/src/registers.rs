// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFSHCI memory-mapped register definitions and bitfields (UFSHCI 3.0/4.0 section 5).
//!
//! `IS` and `UTRLCNR` are write-1-to-clear, so never read-modify-write them: build the value from
//! 0 instead. `UTRLCLR` clears a slot when a 0 is written and is declared write-only so that a
//! read-modify-write cannot compile; clear slot `n` with `!(1 << n)`.

use crate::constants::MCQ_QUEUE_CONFIG_PTR_UNIT;
use mmio::register;

register! {
    #[register(offset = 0x00, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.2.1: CAP - Controller Capabilities.
    pub struct CapabilityReg(u32) {
        // UFSHCI 4.0 only: MCQS (30), LSDBS (29), and EHSLUTRDS (22).
        pub mcq_support, _: 30;
        pub legacy_single_doorbell_removed, _: 29;
        pub crypto_support, _: 28;
        pub uic_dme_test_mode_command_supported, _: 26;
        pub out_of_order_data_delivery_supported, _: 25;
        pub addressing_64_bit_supported, _: 24;
        pub auto_hibernation_support, _: 23;
        pub ehs_in_lu_transfer_request_supported, _: 22;
        pub number_of_utp_task_management_request_slots, _: 18, 16;
        pub number_of_outstanding_rtt_requests_supported, _: 15, 8;
        // UFSHCI 4.0 MCQ mode: 8-bit, 0-based maximum active commands. Single-doorbell (SDB)
        // mode uses only bits 4:0 (NUTRS).
        pub number_of_mcq_transfer_request_slots, _: 7, 0;
        pub number_of_utp_transfer_request_slots, _: 4, 0;
    }

    #[register(offset = 0x04, mode = RO)]
    /// UFSHCI 4.0 section 5.2.2: MCQCAP - MCQ Capabilities.
    pub struct McqCapabilityReg(u32) {
        pub max_interrupt_aggregation_groups, _: 31, 24;
        // Queue configuration base in units of `MCQ_QUEUE_CONFIG_PTR_UNIT` (0x200).
        pub queue_config_pointer, _: 23, 16;
        pub extended_iid_supported, _: 10;
        pub round_robin_priority_supported, _: 9;
        pub max_queues_minus_one, _: 7, 0;
    }

    #[register(offset = 0x08, mode = RO)]
    /// UFSHCI 3.0 section 5.2.2 / 4.0 section 5.2.3: VER - UFS Version.
    pub struct VersionReg(u32) {
        pub major_version_number, _: 15, 8;
        pub minor_version_number, _: 7, 4;
        pub version_suffix, _: 3, 0;
    }

    #[register(offset = 0x18, mode = RW)]
    /// UFSHCI 3.0 section 5.2.5 / 4.0 section 5.2.7: AHIT - Auto-Hibernate Idle Timer.
    pub struct AutoHibernateIdleTimerReg(u32) {
        pub timer_scale, set_timer_scale: 12, 10;
        pub timer_value, set_timer_value: 9, 0;
    }

    #[register(offset = 0x20, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.3.1: IS - Interrupt Status. Write 1 to clear a bit; never
    /// read-modify-write.
    pub struct InterruptStatusReg(u32) {
        // UFSHCI 4.0 only: IAGES (21) and CQES (20).
        pub iag_event_status, set_iag_event_status: 21;
        pub cq_event_status, set_cq_event_status: 20;
        pub crypto_engine_fatal_error_status, set_crypto_engine_fatal_error_status: 18;
        pub system_bus_fatal_error_status, set_system_bus_fatal_error_status: 17;
        pub host_controller_fatal_error_status, set_host_controller_fatal_error_status: 16;
        pub utp_error_status, set_utp_error_status: 12;
        pub device_fatal_error_status, set_device_fatal_error_status: 11;
        pub uic_command_completion_status, set_uic_command_completion_status: 10;
        pub utp_task_management_request_completion_status,
            set_utp_task_management_request_completion_status: 9;
        pub uic_link_startup_status, set_uic_link_startup_status: 8;
        pub uic_link_lost_status, set_uic_link_lost_status: 7;
        pub uic_hibernate_enter_status, set_uic_hibernate_enter_status: 6;
        pub uic_hibernate_exit_status, set_uic_hibernate_exit_status: 5;
        pub uic_power_mode_status, set_uic_power_mode_status: 4;
        pub uic_test_mode_status, set_uic_test_mode_status: 3;
        pub uic_error, set_uic_error: 2;
        pub uic_dme_endpointreset_indication, set_uic_dme_endpointreset_indication: 1;
        pub utp_transfer_request_completion_status, set_utp_transfer_request_completion_status: 0;
    }

    #[register(offset = 0x24, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.3.2: IE - Interrupt Enable.
    pub struct InterruptEnableReg(u32) {
        // UFSHCI 4.0 only: IAGEE (21) and CQEE (20).
        pub iag_event_enable, set_iag_event_enable: 21;
        pub cq_event_enable, set_cq_event_enable: 20;
        pub crypto_engine_fatal_error_enable, set_crypto_engine_fatal_error_enable: 18;
        pub system_bus_fatal_error_enable, set_system_bus_fatal_error_enable: 17;
        pub host_controller_fatal_error_enable, set_host_controller_fatal_error_enable: 16;
        pub utp_error_enable, set_utp_error_enable: 12;
        pub device_fatal_error_enable, set_device_fatal_error_enable: 11;
        pub uic_command_completion_enable, set_uic_command_completion_enable: 10;
        pub utp_task_management_request_completion_enable,
            set_utp_task_management_request_completion_enable: 9;
        pub uic_link_startup_status_enable, set_uic_link_startup_status_enable: 8;
        pub uic_link_lost_status_enable, set_uic_link_lost_status_enable: 7;
        pub uic_hibernate_enter_status_enable, set_uic_hibernate_enter_status_enable: 6;
        pub uic_hibernate_exit_status_enable, set_uic_hibernate_exit_status_enable: 5;
        pub uic_power_mode_status_enable, set_uic_power_mode_status_enable: 4;
        pub uic_test_mode_status_enable, set_uic_test_mode_status_enable: 3;
        pub uic_error_enable, set_uic_error_enable: 2;
        pub uic_dme_endpointreset_enable, set_uic_dme_endpointreset_enable: 1;
        pub utp_transfer_request_completion_enable, set_utp_transfer_request_completion_enable: 0;
    }

    #[register(offset = 0x30, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.3: HCS - Host Controller Status.
    pub struct HostControllerStatusReg(u32) {
        pub target_lun_of_utp_error, _: 31, 24;
        pub task_tag_of_utp_error, _: 23, 16;
        pub utp_error_code, _: 15, 12;
        pub uic_power_mode_change_request_status, _: 10, 8;
        pub uic_command_ready, _: 3;
        pub utp_task_management_request_list_ready, _: 2;
        pub utp_transfer_request_list_ready, _: 1;
        pub device_present, _: 0;
    }

    #[register(offset = 0x34, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.3.4: HCE - Host Controller Enable.
    pub struct HostControllerEnableReg(u32) {
        pub crypto_general_enable, set_crypto_general_enable: 1;
        pub host_controller_enable, set_host_controller_enable: 0;
    }

    #[register(offset = 0x38, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.5: UECPA - Host UIC Error Code PHY Adapter Layer.
    pub struct HostUicErrorCodePhyAdapterLayerReg(u32) {
        pub uic_phy_adapter_layer_error, _: 31;
        pub uic_phy_adapter_layer_error_code, _: 4, 0;
    }

    #[register(offset = 0x3C, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.6: UECDL - Host UIC Error Code Data Link Layer.
    pub struct HostUicErrorCodeDataLinkLayerReg(u32) {
        pub uic_data_link_layer_error, _: 31;
        pub uic_data_link_layer_error_code, _: 15, 0;
    }

    #[register(offset = 0x40, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.7: UECN - Host UIC Error Code Network Layer.
    pub struct HostUicErrorCodeNetworkLayerReg(u32) {
        pub uic_network_layer_error, _: 31;
        pub uic_network_layer_error_code, _: 2, 0;
    }

    #[register(offset = 0x44, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.8: UECT - Host UIC Error Code Transport Layer.
    pub struct HostUicErrorCodeTransportLayerReg(u32) {
        pub uic_transport_layer_error, _: 31;
        pub uic_transport_layer_error_code, _: 6, 0;
    }

    #[register(offset = 0x48, mode = RO)]
    /// UFSHCI 3.0/4.0 section 5.3.9: UECDME - Host UIC Error Code DME.
    pub struct HostUicErrorCodeDmeReg(u32) {
        pub uic_dme_error, _: 31;
        pub uic_dme_error_code, _: 3, 0;
    }

    #[register(offset = 0x50, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.4.1: UTRLBA - UTP Transfer Request List Base Address.
    pub struct UtrListBaseAddressReg(u32) {
        pub address, set_address: 31, 0;
    }

    #[register(offset = 0x54, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.4.2: UTRLBAU - UTP Transfer Request List Base Address Upper.
    pub struct UtrListBaseAddressUpperReg(u32) {
        pub address_upper, set_address_upper: 31, 0;
    }

    #[register(offset = 0x58, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.4.3: UTRLDBR - UTP Transfer Request List Door Bell.
    pub struct UtrListDoorBellReg(u32) {
        pub door_bell, set_door_bell: 31, 0;
    }

    #[register(offset = 0x5C, mode = WO)]
    /// UFSHCI 3.0/4.0 section 5.4.4: UTRLCLR - UTP Transfer Request List Clear. Writing 0 to a
    /// bit clears that slot and writing 1 has no effect, so clear slot `n` with `!(1 << n)`.
    /// Write-only: the driver never needs its value, and this rules out read-modify-write.
    pub struct UtrListClearReg(u32) {
        pub clear, set_clear: 31, 0;
    }

    #[register(offset = 0x60, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.4.5: UTRLRSR - UTP Transfer Request List Run Stop.
    pub struct UtrListRunStopReg(u32) {
        pub value, set_value: 0;
    }

    #[register(offset = 0x64, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.4.6: UTRLCNR - UTP Transfer Request List Completion
    /// Notification. Write 1 to clear a bit; never read-modify-write.
    pub struct UtrListCompletionNotificationReg(u32) {
        pub notification, set_notification: 31, 0;
    }

    #[register(offset = 0x90, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.6.1: UICCMD - UIC Command.
    pub struct UicCommandReg(u32) {
        pub command_opcode, set_command_opcode: 7, 0;
    }

    #[register(offset = 0x94, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.6.2: UICCMDARG1 - UIC Command Argument 1.
    pub struct UicCommandArgument1Reg(u32) {
        pub mib_attribute, set_mib_attribute: 31, 16;
        pub gen_selector_index, set_gen_selector_index: 15, 0;
    }

    #[register(offset = 0x98, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.6.3: UICCMDARG2 - UIC Command Argument 2.
    pub struct UicCommandArgument2Reg(u32) {
        pub attr_set_type, set_attr_set_type: 23, 16;
        pub result_code, set_result_code: 7, 0;
    }

    #[register(offset = 0x9C, mode = RW)]
    /// UFSHCI 3.0/4.0 section 5.6.4: UICCMDARG3 - UIC Command Argument 3.
    pub struct UicCommandArgument3Reg(u32) {
        pub value, set_value: 31, 0;
    }

    #[register(offset = 0x300, mode = RW)]
    /// UFSHCI 4.0 section 5.2.8: CONFIG - Global Configuration.
    pub struct GlobalConfigReg(u32) {
        pub event_specific_interrupt_enable, set_event_specific_interrupt_enable: 1;
        // Queue Type: 0 = legacy single doorbell, 1 = MCQ.
        pub queue_type, set_queue_type: 0;
    }

    #[register(offset = 0x380, mode = RW)]
    /// UFSHCI 4.0 section 5.2.9: MCQCONFIG - MCQ Configuration.
    pub struct McqConfigReg(u32) {
        // Maximum Active Commands, 0-based.
        pub max_active_commands, set_max_active_commands: 16, 8;
        pub arbitration_scheme, set_arbitration_scheme: 1, 0;
    }
}

impl McqCapabilityReg {
    /// Returns the number of queues (`MAXQ` + 1).
    pub fn max_queues(&self) -> usize {
        self.max_queues_minus_one() as usize + 1
    }

    /// Returns the byte offset of the queue configuration array, 0 if not implemented.
    pub fn queue_config_offset(&self) -> usize {
        self.queue_config_pointer() as usize * MCQ_QUEUE_CONFIG_PTR_UNIT
    }
}

impl CapabilityReg {
    /// Number of UTP Transfer Request slots. The `NUTRS` field is 0-based, so this is `NUTRS + 1`.
    pub fn transfer_request_slots(&self) -> u32 {
        self.number_of_utp_transfer_request_slots() + 1
    }

    /// Number of UTP Task Management Request slots. The `NUTMRS` field is 0-based, so this is
    /// `NUTMRS + 1`.
    pub fn task_management_request_slots(&self) -> u32 {
        self.number_of_utp_task_management_request_slots() + 1
    }

    /// Maximum active commands in MCQ mode. `CAP[7:0]` is 0-based, so this is `CAP[7:0] + 1`.
    pub fn mcq_transfer_request_slots(&self) -> u32 {
        self.number_of_mcq_transfer_request_slots() + 1
    }
}

/// Host Controller Status UTP Error Code (UFSHCI 3.0/4.0 section 5.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum UtpErrorCode {
    /// Reject UPIU received with an invalid task tag or LUN.
    RejectUpiuHasInvalidTaskTagOrLun = 0,
    /// Unrecognized or unsupported UPIU transaction type.
    InvalidUpiuType = 1,
    /// Transfer Request UPIU has an invalid task tag or LUN.
    TrUpiuHasInvalidTaskTagOrLun = 2,
    /// Task Management Request UPIU has an invalid task tag or LUN.
    TmrUpiuHasInvalidTaskTagOrLun = 3,
}

/// Host Controller Status UIC Power Mode Change Request Status (UFSHCI 3.0/4.0 section 5.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PowerModeStatus {
    /// Power mode change is not needed (`PWR_OK`).
    PowerOk = 0,
    /// Power mode change initiated locally succeeded (`PWR_LOCAL`).
    PowerLocal = 1,
    /// Power mode change initiated remotely succeeded (`PWR_REMOTE`).
    PowerRemote = 2,
    /// Power mode change rejected because the peer is busy (`PWR_BUSY`).
    PowerBusy = 3,
    /// Power mode change rejected due to invalid capability (`PWR_ERROR_CAP`).
    PowerErrorCap = 4,
    /// Fatal error during power mode change (`PWR_FATAL_ERROR`).
    PowerFatalError = 5,
}

impl HostControllerStatusReg {
    /// Decodes the `utp_error_code` field into a typed [`UtpErrorCode`].
    pub fn error_code(&self) -> Option<UtpErrorCode> {
        match self.utp_error_code() {
            0 => Some(UtpErrorCode::RejectUpiuHasInvalidTaskTagOrLun),
            1 => Some(UtpErrorCode::InvalidUpiuType),
            2 => Some(UtpErrorCode::TrUpiuHasInvalidTaskTagOrLun),
            3 => Some(UtpErrorCode::TmrUpiuHasInvalidTaskTagOrLun),
            _ => None,
        }
    }

    /// Decodes the `uic_power_mode_change_request_status` field into a [`PowerModeStatus`].
    pub fn power_mode_status(&self) -> Option<PowerModeStatus> {
        match self.uic_power_mode_change_request_status() {
            0 => Some(PowerModeStatus::PowerOk),
            1 => Some(PowerModeStatus::PowerLocal),
            2 => Some(PowerModeStatus::PowerRemote),
            3 => Some(PowerModeStatus::PowerBusy),
            4 => Some(PowerModeStatus::PowerErrorCap),
            5 => Some(PowerModeStatus::PowerFatalError),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::Register;

    #[fuchsia::test]
    fn test_capability_reg_decode() {
        let caps = CapabilityReg::from_raw(0x4101_071F);
        assert!(caps.mcq_support());
        assert!(caps.addressing_64_bit_supported());
        assert!(!caps.legacy_single_doorbell_removed());
        assert_eq!(caps.transfer_request_slots(), 32);
        assert_eq!(caps.task_management_request_slots(), 2);
        assert_eq!(caps.mcq_transfer_request_slots(), 32);
        assert_eq!(caps.number_of_outstanding_rtt_requests_supported(), 7);
    }
}
