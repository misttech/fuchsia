// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_PCI_EXPRESS_H_
#define SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_PCI_EXPRESS_H_

#include <utility>

#include <hwreg/bitfields.h>

#include "src/devices/pci/lib/cpp/capabilities.h"
#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

// The PCI Express capability structure is detailed in the PCI Express Base
// Specification Revision 7.0, Section 7.5.3: PCI Express Capability Structure
// (Capability ID 0x10).

// Section 7.5.3.2: PCI Express Capabilities Register (Offset +0x02)
struct PciExpressCapabilitiesReg : public hwreg::RegisterBase<PciExpressCapabilitiesReg, uint16_t> {
  DEF_FIELD(3, 0, capability_version);
  DEF_FIELD(7, 4, device_port_type);
  DEF_BIT(8, slot_implemented);
  DEF_FIELD(13, 9, interrupt_message_number);
  DEF_BIT(15, flit_mode_supported);

  enum DevicePortType : uint8_t {
    kEndpoint = 0b0000,
    kLegacyEndpoint = 0b0001,
    kRootPort = 0b0100,
    kSwitchUpstreamPort = 0b0101,
    kSwitchDownstreamPort = 0b0110,
    kPciExpressToPciBridge = 0b0111,
    kPciToPciExpressBridge = 0b1000,
    kRootComplexIntegratedEndpoint = 0b1001,
    kRootComplexEventCollector = 0b1010,
  };
};

// Section 7.5.3.3: Device Capabilities Register (Offset +0x04)
struct DeviceCapabilitiesReg : public hwreg::RegisterBase<DeviceCapabilitiesReg, uint32_t> {
  DEF_FIELD(2, 0, max_payload_size_supported);
  DEF_FIELD(4, 3, phantom_functions_supported);
  DEF_BIT(5, extended_tag_field_supported);
  DEF_FIELD(8, 6, endpoint_l0s_acceptable_latency);
  DEF_FIELD(11, 9, endpoint_l1_acceptable_latency);
  DEF_BIT(15, role_based_error_reporting);
  DEF_BIT(16, err_cor_subclass_capable);
  DEF_BIT(17, rx_mps_fixed);
  DEF_FIELD(25, 18, captured_slot_power_limit_value);
  DEF_FIELD(27, 26, captured_slot_power_limit_scale);
  DEF_BIT(28, function_level_reset_capability);
  DEF_BIT(29, mixed_mps_supported);
  DEF_BIT(30, tee_io_supported);
};

// Section 7.5.3.4: Device Control Register (Offset +0x08)
struct DeviceControlReg : public hwreg::RegisterBase<DeviceControlReg, uint16_t> {
  DEF_BIT(0, correctable_error_reporting_enable);
  DEF_BIT(1, non_fatal_error_reporting_enable);
  DEF_BIT(2, fatal_error_reporting_enable);
  DEF_BIT(3, unsupported_request_reporting_enable);
  DEF_BIT(4, enable_relaxed_ordering);
  DEF_FIELD(7, 5, max_payload_size);
  DEF_BIT(8, extended_tag_field_enable);
  DEF_BIT(9, phantom_functions_enable);
  DEF_BIT(10, aux_power_pm_enable);
  DEF_BIT(11, enable_no_snoop);
  DEF_FIELD(14, 12, max_read_request_size);
  DEF_BIT(15, bridge_cfg_retry_or_initiate_flr);
};

// Section 7.5.3.5: Device Status Register (Offset +0x0A)
// Bits 0-3 and 6 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct DeviceStatusReg : public hwreg::RegisterBase<DeviceStatusReg, uint16_t> {
  DEF_BIT(0, correctable_error_detected);
  DEF_BIT(1, non_fatal_error_detected);
  DEF_BIT(2, fatal_error_detected);
  DEF_BIT(3, unsupported_request_detected);
  DEF_BIT(4, aux_power_detected);
  DEF_BIT(5, transactions_pending);
  DEF_BIT(6, emergency_power_reduction_detected);
};

// Section 7.5.3.6: Link Capabilities Register (Offset +0x0C)
struct LinkCapabilitiesReg : public hwreg::RegisterBase<LinkCapabilitiesReg, uint32_t> {
  DEF_FIELD(3, 0, max_link_speed);
  DEF_FIELD(9, 4, max_link_width);
  DEF_FIELD(11, 10, aspm_support);
  DEF_FIELD(14, 12, l0s_exit_latency);
  DEF_FIELD(17, 15, l1_exit_latency);
  DEF_BIT(18, clock_power_management);
  DEF_BIT(19, surprise_down_error_reporting_capable);
  DEF_BIT(20, data_link_layer_link_active_reporting_capable);
  DEF_BIT(21, link_bandwidth_notification_capability);
  DEF_BIT(22, aspm_optionality_compliance);
  DEF_FIELD(31, 24, port_number);

  enum AspmSupport : uint8_t {
    kNoAspm = 0b00,
    kL0sSupported = 0b01,
    kL1Supported = 0b10,
    kL0sAndL1Supported = 0b11,
  };
};

// Section 7.5.3.7: Link Control Register (Offset +0x10)
struct LinkControlReg : public hwreg::RegisterBase<LinkControlReg, uint16_t> {
  DEF_FIELD(1, 0, aspm_control);
  DEF_BIT(2, ptm_propagation_delay_adaptation_b);
  DEF_BIT(3, read_completion_boundary);
  DEF_BIT(4, link_disable);
  DEF_BIT(5, retrain_link);
  DEF_BIT(6, common_clock_configuration);
  DEF_BIT(7, extended_synch);
  DEF_BIT(8, enable_clock_power_management);
  DEF_BIT(9, hw_autonomous_width_disable);
  DEF_BIT(10, link_bandwidth_mgmt_interrupt_enable);
  DEF_BIT(11, link_autonomous_bandwidth_interrupt_enable);
  DEF_BIT(12, sris_clocking);
  DEF_BIT(13, flit_mode_disable);
  DEF_FIELD(15, 14, drs_signaling_control);

  enum AspmControl : uint8_t {
    kDisabled = 0b00,
    kL0sEntryEnabled = 0b01,
    kL1EntryEnabled = 0b10,
    kL0sAndL1EntryEnabled = 0b11,
  };
};

// Section 7.5.3.8: Link Status Register (Offset +0x12)
// Bits 14-15 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct LinkStatusReg : public hwreg::RegisterBase<LinkStatusReg, uint16_t> {
  DEF_FIELD(3, 0, current_link_speed);
  DEF_FIELD(9, 4, negotiated_link_width);
  DEF_BIT(11, link_training);
  DEF_BIT(12, slot_clock_configuration);
  DEF_BIT(13, data_link_layer_link_active);
  DEF_BIT(14, link_bandwidth_management_status);
  DEF_BIT(15, link_autonomous_bandwidth_status);
};

// Section 7.5.3.9: Slot Capabilities Register (Offset +0x14)
struct SlotCapabilitiesReg : public hwreg::RegisterBase<SlotCapabilitiesReg, uint32_t> {
  DEF_BIT(0, attention_button_present);
  DEF_BIT(1, power_controller_present);
  DEF_BIT(2, mrl_sensor_present);
  DEF_BIT(3, attention_indicator_present);
  DEF_BIT(4, power_indicator_present);
  DEF_BIT(5, hot_plug_surprise);
  DEF_BIT(6, hot_plug_capable);
  DEF_FIELD(14, 7, slot_power_limit_value);
  DEF_FIELD(16, 15, slot_power_limit_scale);
  DEF_BIT(17, electromechanical_interlock_present);
  DEF_BIT(18, no_command_completed_support);
  DEF_FIELD(31, 19, physical_slot_number);
};

// Section 7.5.3.10: Slot Control Register (Offset +0x18)
struct SlotControlReg : public hwreg::RegisterBase<SlotControlReg, uint16_t> {
  DEF_BIT(0, attention_button_pressed_enable);
  DEF_BIT(1, power_fault_detected_enable);
  DEF_BIT(2, mrl_sensor_changed_enable);
  DEF_BIT(3, presence_detect_changed_enable);
  DEF_BIT(4, command_completed_interrupt_enable);
  DEF_BIT(5, hot_plug_interrupt_enable);
  DEF_FIELD(7, 6, attention_indicator_control);
  DEF_FIELD(9, 8, power_indicator_control);
  DEF_BIT(10, power_controller_control);
  DEF_BIT(11, electromechanical_interlock_control);
  DEF_BIT(12, data_link_layer_state_changed_enable);
  DEF_BIT(13, auto_slot_power_limit_disable);
  DEF_BIT(14, in_band_pd_disable);
};

// Section 7.5.3.11: Slot Status Register (Offset +0x1A)
// Bits 0-4 and 8 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct SlotStatusReg : public hwreg::RegisterBase<SlotStatusReg, uint16_t> {
  DEF_BIT(0, attention_button_pressed);
  DEF_BIT(1, power_fault_detected);
  DEF_BIT(2, mrl_sensor_changed);
  DEF_BIT(3, presence_detect_changed);
  DEF_BIT(4, command_completed);
  DEF_BIT(5, mrl_sensor_state);
  DEF_BIT(6, presence_detect_state);
  DEF_BIT(7, electromechanical_interlock_status);
  DEF_BIT(8, data_link_layer_state_changed);
};

// Section 7.5.3.12: Root Control Register (Offset +0x1C)
struct RootControlReg : public hwreg::RegisterBase<RootControlReg, uint16_t> {
  DEF_BIT(0, sys_err_on_correctable_error_enable);
  DEF_BIT(1, sys_err_on_non_fatal_error_enable);
  DEF_BIT(2, sys_err_on_fatal_error_enable);
  DEF_BIT(3, pme_interrupt_enable);
  DEF_BIT(4, config_rrs_software_visibility_enable);
  DEF_BIT(5, no_nfm_subtree_below_this_root_port);
};

// Section 7.5.3.13: Root Capabilities Register (Offset +0x1E)
struct RootCapabilitiesReg : public hwreg::RegisterBase<RootCapabilitiesReg, uint16_t> {
  DEF_BIT(0, config_rrs_software_visibility);
};

// Section 7.5.3.14: Root Status Register (Offset +0x20)
// Bit 16 is RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct RootStatusReg : public hwreg::RegisterBase<RootStatusReg, uint32_t> {
  DEF_FIELD(15, 0, pme_requester_id);
  DEF_BIT(16, pme_status);
  DEF_BIT(17, pme_pending);
};

// Section 7.5.3.15: Device Capabilities 2 Register (Offset +0x24)
struct DeviceCapabilities2Reg : public hwreg::RegisterBase<DeviceCapabilities2Reg, uint32_t> {
  DEF_FIELD(3, 0, completion_timeout_ranges_supported);
  DEF_BIT(4, completion_timeout_disable_supported);
  DEF_BIT(5, ari_forwarding_supported);
  DEF_BIT(6, atomic_op_routing_supported);
  DEF_BIT(7, atomic_op_completer_32bit_supported);
  DEF_BIT(8, atomic_op_completer_64bit_supported);
  DEF_BIT(9, cas_completer_128bit_supported);
  DEF_BIT(10, no_ro_enabled_pr_pr_passing);
  DEF_BIT(11, ltr_mechanism_supported);
  DEF_FIELD(13, 12, tph_completer_supported);
  DEF_BIT(16, tag_completer_10bit_supported);
  DEF_BIT(17, tag_requester_10bit_supported);
  DEF_FIELD(19, 18, obff_supported);
  DEF_BIT(20, extended_fmt_field_supported);
  DEF_BIT(21, end_end_tlp_prefix_supported);
  DEF_FIELD(23, 22, max_end_end_tlp_prefixes);
  DEF_FIELD(25, 24, emergency_power_reduction_supported);
  DEF_BIT(26, emergency_power_reduction_init_required);
  DEF_BIT(27, dmwr_request_routing_supported);
  DEF_BIT(28, dmwr_completer_supported);
  DEF_FIELD(30, 29, dmwr_lengths_supported);
  DEF_BIT(31, frs_supported);
};

// Section 7.5.3.16: Device Control 2 Register (Offset +0x28)
struct DeviceControl2Reg : public hwreg::RegisterBase<DeviceControl2Reg, uint16_t> {
  DEF_FIELD(3, 0, completion_timeout_value);
  DEF_BIT(4, completion_timeout_disable);
  DEF_BIT(5, ari_forwarding_enable);
  DEF_BIT(6, atomic_op_requester_enable);
  DEF_BIT(7, atomic_op_egress_blocking);
  DEF_BIT(8, ido_request_enable);
  DEF_BIT(9, ido_completion_enable);
  DEF_BIT(10, ltr_mechanism_enable);
  DEF_BIT(11, emergency_power_reduction_request);
  DEF_BIT(12, tag_requester_10bit_enable);
  DEF_FIELD(14, 13, obff_enable);
  DEF_BIT(15, end_end_tlp_prefix_blocking);
};

// Section 7.5.3.17: Device Status 2 Register (Offset +0x2A)
struct DeviceStatus2Reg : public hwreg::RegisterBase<DeviceStatus2Reg, uint16_t> {
  DEF_RSVDZ_FIELD(15, 0);
};

// Section 7.5.3.18: Link Capabilities 2 Register (Offset +0x2C)
struct LinkCapabilities2Reg : public hwreg::RegisterBase<LinkCapabilities2Reg, uint32_t> {
  DEF_FIELD(7, 1, supported_link_speeds_vector);
  DEF_BIT(8, crosslink_supported);
  DEF_FIELD(15, 9, lower_skp_os_generation_supported_speeds_vector);
  DEF_FIELD(22, 16, lower_skp_os_reception_supported_speeds_vector);
  DEF_BIT(23, retimer_presence_detect_supported);
  DEF_BIT(24, two_retimers_presence_detect_supported);
  DEF_BIT(25, optical_retimer_presence_detect_supported);
  DEF_BIT(26, fra_retimers_detect_supported);
  DEF_BIT(31, drs_supported);
};

// Section 7.5.3.19: Link Control 2 Register (Offset +0x30)
struct LinkControl2Reg : public hwreg::RegisterBase<LinkControl2Reg, uint16_t> {
  DEF_FIELD(3, 0, target_link_speed);
  DEF_BIT(4, enter_compliance);
  DEF_BIT(5, hw_autonomous_speed_disable);
  DEF_BIT(6, selectable_de_emphasis);
  DEF_FIELD(9, 7, transmit_margin);
  DEF_BIT(10, enter_modified_compliance);
  DEF_BIT(11, compliance_sos);
  DEF_FIELD(15, 12, compliance_preset_or_de_emphasis);
};

// Section 7.5.3.20: Link Status 2 Register (Offset +0x32)
// Bits 5 and 15 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct LinkStatus2Reg : public hwreg::RegisterBase<LinkStatus2Reg, uint16_t> {
  DEF_BIT(0, current_de_emphasis_level);
  DEF_BIT(1, equalization_8gt_complete);
  DEF_BIT(2, equalization_8gt_phase1_successful);
  DEF_BIT(3, equalization_8gt_phase2_successful);
  DEF_BIT(4, equalization_8gt_phase3_successful);
  DEF_BIT(5, link_equalization_request_8gt);
  DEF_BIT(6, retimer_presence_detected);
  DEF_BIT(7, two_retimers_presence_detected);
  DEF_FIELD(9, 8, crosslink_resolution);
  DEF_BIT(10, flit_mode_status);
  DEF_BIT(11, optical_retimer_presence_detected);
  DEF_FIELD(14, 12, downstream_component_presence);
  DEF_BIT(15, drs_message_received);
};

// Section 7.5.3.21: Slot Capabilities 2 Register (Offset +0x34)
struct SlotCapabilities2Reg : public hwreg::RegisterBase<SlotCapabilities2Reg, uint32_t> {
  DEF_BIT(0, in_band_pd_disable_supported);
  DEF_FIELD(2, 1, scap2_oob_pd_supported);
};

// Section 7.5.3.22: Slot Control 2 Register (Offset +0x38)
// This register is RsvdP per PCIe Base Spec section 7.5.3.22. All bits are preserved.
struct SlotControl2Reg : public hwreg::RegisterBase<SlotControl2Reg, uint16_t> {};

// Section 7.5.3.23: Slot Status 2 Register (Offset +0x3A)
struct SlotStatus2Reg : public hwreg::RegisterBase<SlotStatus2Reg, uint16_t> {
  DEF_RSVDZ_FIELD(15, 0);
};

class PciExpressCapability : public Capability {
 public:
  static constexpr uint8_t kPcieCapabilitiesOffset = 0x2;
  static constexpr uint8_t kDeviceCapabilitiesOffset = 0x4;
  static constexpr uint8_t kDeviceControlOffset = 0x8;
  static constexpr uint8_t kDeviceStatusOffset = 0xA;
  static constexpr uint8_t kLinkCapabilitiesOffset = 0xC;
  static constexpr uint8_t kLinkControlOffset = 0x10;
  static constexpr uint8_t kLinkStatusOffset = 0x12;
  static constexpr uint8_t kSlotCapabilitiesOffset = 0x14;
  static constexpr uint8_t kSlotControlOffset = 0x18;
  static constexpr uint8_t kSlotStatusOffset = 0x1A;
  static constexpr uint8_t kRootControlOffset = 0x1C;
  static constexpr uint8_t kRootCapabilitiesOffset = 0x1E;
  static constexpr uint8_t kRootStatusOffset = 0x20;
  static constexpr uint8_t kDeviceCapabilities2Offset = 0x24;
  static constexpr uint8_t kDeviceControl2Offset = 0x28;
  static constexpr uint8_t kDeviceStatus2Offset = 0x2A;
  static constexpr uint8_t kLinkCapabilities2Offset = 0x2C;
  static constexpr uint8_t kLinkControl2Offset = 0x30;
  static constexpr uint8_t kLinkStatus2Offset = 0x32;
  static constexpr uint8_t kSlotCapabilities2Offset = 0x34;
  static constexpr uint8_t kSlotControl2Offset = 0x38;
  static constexpr uint8_t kSlotStatus2Offset = 0x3A;

  PciExpressCapability(const Config& cfg, uint8_t base)
      : Capability(std::to_underlying(Capability::Id::kPciExpress), base),
        pcie_capabilities_(static_cast<uint16_t>(base + kPcieCapabilitiesOffset)),
        device_capabilities_(static_cast<uint16_t>(base + kDeviceCapabilitiesOffset)),
        device_control_(static_cast<uint16_t>(base + kDeviceControlOffset)),
        device_status_(static_cast<uint16_t>(base + kDeviceStatusOffset)),
        link_capabilities_(static_cast<uint16_t>(base + kLinkCapabilitiesOffset)),
        link_control_(static_cast<uint16_t>(base + kLinkControlOffset)),
        link_status_(static_cast<uint16_t>(base + kLinkStatusOffset)),
        slot_capabilities_(static_cast<uint16_t>(base + kSlotCapabilitiesOffset)),
        slot_control_(static_cast<uint16_t>(base + kSlotControlOffset)),
        slot_status_(static_cast<uint16_t>(base + kSlotStatusOffset)),
        root_control_(static_cast<uint16_t>(base + kRootControlOffset)),
        root_capabilities_(static_cast<uint16_t>(base + kRootCapabilitiesOffset)),
        root_status_(static_cast<uint16_t>(base + kRootStatusOffset)),
        device_capabilities_2_(static_cast<uint16_t>(base + kDeviceCapabilities2Offset)),
        device_control_2_(static_cast<uint16_t>(base + kDeviceControl2Offset)),
        device_status_2_(static_cast<uint16_t>(base + kDeviceStatus2Offset)),
        link_capabilities_2_(static_cast<uint16_t>(base + kLinkCapabilities2Offset)),
        link_control_2_(static_cast<uint16_t>(base + kLinkControl2Offset)),
        link_status_2_(static_cast<uint16_t>(base + kLinkStatus2Offset)),
        slot_capabilities_2_(static_cast<uint16_t>(base + kSlotCapabilities2Offset)),
        slot_control_2_(static_cast<uint16_t>(base + kSlotControl2Offset)),
        slot_status_2_(static_cast<uint16_t>(base + kSlotStatus2Offset)) {}

  hwreg::RegisterAddr<PciExpressCapabilitiesReg> pcie_capabilities() const {
    return pcie_capabilities_;
  }
  hwreg::RegisterAddr<DeviceCapabilitiesReg> device_capabilities() const {
    return device_capabilities_;
  }
  hwreg::RegisterAddr<DeviceControlReg> device_control() const { return device_control_; }
  hwreg::RegisterAddr<DeviceStatusReg> device_status() const { return device_status_; }
  hwreg::RegisterAddr<LinkCapabilitiesReg> link_capabilities() const { return link_capabilities_; }
  hwreg::RegisterAddr<LinkControlReg> link_control() const { return link_control_; }
  hwreg::RegisterAddr<LinkStatusReg> link_status() const { return link_status_; }
  // Note: Slot registers are only valid when slot_implemented is set in PCIe Capabilities.
  hwreg::RegisterAddr<SlotCapabilitiesReg> slot_capabilities() const { return slot_capabilities_; }
  hwreg::RegisterAddr<SlotControlReg> slot_control() const { return slot_control_; }
  hwreg::RegisterAddr<SlotStatusReg> slot_status() const { return slot_status_; }
  // Note: Root registers are only valid for Root Ports and RCECs.
  hwreg::RegisterAddr<RootControlReg> root_control() const { return root_control_; }
  hwreg::RegisterAddr<RootCapabilitiesReg> root_capabilities() const { return root_capabilities_; }
  hwreg::RegisterAddr<RootStatusReg> root_status() const { return root_status_; }
  // Note: *2 registers are only present when capability_version >= 2.
  hwreg::RegisterAddr<DeviceCapabilities2Reg> device_capabilities_2() const {
    return device_capabilities_2_;
  }
  hwreg::RegisterAddr<DeviceControl2Reg> device_control_2() const { return device_control_2_; }
  hwreg::RegisterAddr<DeviceStatus2Reg> device_status_2() const { return device_status_2_; }
  hwreg::RegisterAddr<LinkCapabilities2Reg> link_capabilities_2() const {
    return link_capabilities_2_;
  }
  hwreg::RegisterAddr<LinkControl2Reg> link_control_2() const { return link_control_2_; }
  hwreg::RegisterAddr<LinkStatus2Reg> link_status_2() const { return link_status_2_; }
  hwreg::RegisterAddr<SlotCapabilities2Reg> slot_capabilities_2() const {
    return slot_capabilities_2_;
  }
  hwreg::RegisterAddr<SlotControl2Reg> slot_control_2() const { return slot_control_2_; }
  hwreg::RegisterAddr<SlotStatus2Reg> slot_status_2() const { return slot_status_2_; }

 private:
  const hwreg::RegisterAddr<PciExpressCapabilitiesReg> pcie_capabilities_;
  const hwreg::RegisterAddr<DeviceCapabilitiesReg> device_capabilities_;
  const hwreg::RegisterAddr<DeviceControlReg> device_control_;
  const hwreg::RegisterAddr<DeviceStatusReg> device_status_;
  const hwreg::RegisterAddr<LinkCapabilitiesReg> link_capabilities_;
  const hwreg::RegisterAddr<LinkControlReg> link_control_;
  const hwreg::RegisterAddr<LinkStatusReg> link_status_;
  const hwreg::RegisterAddr<SlotCapabilitiesReg> slot_capabilities_;
  const hwreg::RegisterAddr<SlotControlReg> slot_control_;
  const hwreg::RegisterAddr<SlotStatusReg> slot_status_;
  const hwreg::RegisterAddr<RootControlReg> root_control_;
  const hwreg::RegisterAddr<RootCapabilitiesReg> root_capabilities_;
  const hwreg::RegisterAddr<RootStatusReg> root_status_;
  const hwreg::RegisterAddr<DeviceCapabilities2Reg> device_capabilities_2_;
  const hwreg::RegisterAddr<DeviceControl2Reg> device_control_2_;
  const hwreg::RegisterAddr<DeviceStatus2Reg> device_status_2_;
  const hwreg::RegisterAddr<LinkCapabilities2Reg> link_capabilities_2_;
  const hwreg::RegisterAddr<LinkControl2Reg> link_control_2_;
  const hwreg::RegisterAddr<LinkStatus2Reg> link_status_2_;
  const hwreg::RegisterAddr<SlotCapabilities2Reg> slot_capabilities_2_;
  const hwreg::RegisterAddr<SlotControl2Reg> slot_control_2_;
  const hwreg::RegisterAddr<SlotStatus2Reg> slot_status_2_;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_PCI_EXPRESS_H_
