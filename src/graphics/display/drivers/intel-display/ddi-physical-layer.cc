// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/intel-display/ddi-physical-layer.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <lib/mmio/mmio-buffer.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>

#include <cinttypes>
#include <cstdint>

#include <fbl/string_printf.h>

#include "src/graphics/display/drivers/intel-display/ddi-physical-layer-internal.h"
#include "src/graphics/display/drivers/intel-display/hardware-common.h"
#include "src/graphics/display/drivers/intel-display/intel-display.h"
#include "src/graphics/display/drivers/intel-display/power-controller.h"
#include "src/graphics/display/drivers/intel-display/registers-ddi-phy-tiger-lake.h"
#include "src/graphics/display/drivers/intel-display/registers-typec.h"
#include "src/graphics/display/lib/driver-utils/poll-until.h"

namespace intel_display {

namespace {

const char* DdiTypeToString(DdiPhysicalLayer::DdiType type) {
  switch (type) {
    case DdiPhysicalLayer::DdiType::kCombo:
      return "COMBO";
    case DdiPhysicalLayer::DdiType::kTypeC:
      return "Type-C";
  }
}

const char* PortTypeToString(DdiPhysicalLayer::ConnectionType type) {
  switch (type) {
    case DdiPhysicalLayer::ConnectionType::kNone:
      return "None";
    case DdiPhysicalLayer::ConnectionType::kBuiltIn:
      return "Built In";
    case DdiPhysicalLayer::ConnectionType::kTypeCDisplayPortAltMode:
      return "Type-C DisplayPort Alt Mode";
    case DdiPhysicalLayer::ConnectionType::kTypeCThunderbolt:
      return "Type-C Thunderbolt Mode";
      break;
  }
}

}  // namespace

void DdiPhysicalLayer::AddRef() {
  ZX_DEBUG_ASSERT(IsEnabled());
  ++ref_count_;
  fdf::trace("DdiPhysicalLayer: Reference count of DDI {} increased to {}", ddi_id(), ref_count_);
}

void DdiPhysicalLayer::Release() {
  ZX_DEBUG_ASSERT(ref_count_ > 0);
  if (--ref_count_ == 0) {
    fdf::trace("DdiPhysicalLayer: Reference count of DDI {} decreased to {}", ddi_id(), ref_count_);
    if (!Disable()) {
      fdf::error("DdiPhysicalLayer: Failed to disable unused DDI {}", ddi_id());
    }
  }
}

fbl::String DdiPhysicalLayer::PhysicalLayerInfo::DebugString() const {
  return fbl::StringPrintf("PhysicalLayerInfo { type: %s, port: %s, max_allowed_dp_lane: %u }",
                           DdiTypeToString(ddi_type), PortTypeToString(connection_type),
                           max_allowed_dp_lane_count);
}

bool DdiSkylake::Enable() {
  if (enabled_) {
    fdf::warn("DDI {}: Enable: PHY already enabled", ddi_id());
  }
  enabled_ = true;
  return true;
}

bool DdiSkylake::Disable() {
  if (!enabled_) {
    fdf::warn("DDI {}: Disable: PHY already disabled", ddi_id());
  }
  enabled_ = false;
  return true;
}

DdiPhysicalLayer::PhysicalLayerInfo DdiSkylake::GetPhysicalLayerInfo() const {
  return {
      .ddi_type = DdiPhysicalLayer::DdiType::kCombo,
      .connection_type = DdiPhysicalLayer::ConnectionType::kBuiltIn,
      .max_allowed_dp_lane_count = 4u,
  };
}

ComboDdiTigerLake::ComboDdiTigerLake(DdiId ddi_id, fdf::MmioBuffer* mmio_space)
    : DdiPhysicalLayer(ddi_id), mmio_space_(mmio_space) {
  ZX_DEBUG_ASSERT(mmio_space);
}

bool ComboDdiTigerLake::Enable() {
  if (enabled_) {
    fdf::warn("DDI {}: Enable: PHY already enabled", ddi_id());
  }
  enabled_ = true;
  return true;
}

bool ComboDdiTigerLake::Disable() {
  if (!enabled_) {
    fdf::warn("DDI {}: Enable: PHY already disabled", ddi_id());
  }
  enabled_ = false;
  return true;
}

namespace {

struct TigerLakeProcessCompensationConfig {
  struct VoltageReferences {
    struct Pair {
      uint16_t low = 0;
      uint16_t high = 0;

      // True for default-constructed values.
      bool IsEmpty() const { return low == 0 && high == 0; }
    };
    Pair negative, positive;

    // True for default-constructed values.
    bool IsEmpty() const { return negative.IsEmpty() && positive.IsEmpty(); }
  };
  VoltageReferences nominal, low;

  // True for default-constructed values.
  bool IsEmpty() const { return nominal.IsEmpty() && low.IsEmpty(); }
};

TigerLakeProcessCompensationConfig ReadTigerLakeProcessCompensationConfig(
    DdiId ddi_id, fdf::MmioBuffer* mmio_space) {
  auto compensation1 = registers::PortCompensation1::GetForDdi(ddi_id).ReadFrom(mmio_space);
  auto compensation_nominal =
      registers::PortCompensationNominalVoltageReferences::GetForDdi(ddi_id).ReadFrom(mmio_space);
  auto compensation_low =
      registers::PortCompensationLowVoltageReferences::GetForDdi(ddi_id).ReadFrom(mmio_space);

  fdf::trace("DDI {} PORT_COMP_DW1: {:08x} PORT_COMP_DW_9: {:08x} PORT_COMP_DW10: {:08x}", ddi_id,
             compensation1.reg_value(), compensation_nominal.reg_value(),
             compensation_low.reg_value());

  return TigerLakeProcessCompensationConfig{
      .nominal =
          {
              .negative =
                  {
                      .low = static_cast<uint16_t>(
                          compensation_nominal
                              .negative_nominal_voltage_reference_low_value_bits70() |
                          (compensation1.negative_nominal_voltage_reference_low_value_bits98()
                           << 8)),
                      .high = static_cast<uint16_t>(
                          compensation_nominal
                              .negative_nominal_voltage_reference_high_value_bits70() |
                          (compensation1.negative_nominal_voltage_reference_high_value_bits98()
                           << 8)),
                  },
              .positive =
                  {
                      .low = static_cast<uint16_t>(
                          compensation_nominal
                              .positive_nominal_voltage_reference_low_value_bits70() |
                          (compensation1.positive_nominal_voltage_reference_low_value_bits98()
                           << 8)),
                      .high = static_cast<uint16_t>(
                          compensation_nominal
                              .positive_nominal_voltage_reference_high_value_bits70() |
                          (compensation1.positive_nominal_voltage_reference_high_value_bits98()
                           << 8)),
                  },
          },
      .low =
          {
              .negative{
                  .low = static_cast<uint16_t>(
                      compensation_low.negative_low_voltage_reference_low_value_bits70() |
                      (compensation1.negative_low_voltage_reference_low_value_bits98() << 8)),
                  .high = static_cast<uint16_t>(
                      compensation_low.negative_low_voltage_reference_high_value_bits70() |
                      (compensation1.negative_low_voltage_reference_high_value_bits98() << 8)),
              },
              .positive{
                  .low = static_cast<uint16_t>(
                      compensation_low.positive_low_voltage_reference_low_value_bits70() |
                      (compensation1.positive_low_voltage_reference_low_value_bits98() << 8)),
                  .high = static_cast<uint16_t>(
                      compensation_low.positive_low_voltage_reference_high_value_bits70() |
                      (compensation1.positive_low_voltage_reference_high_value_bits98() << 8)),
              },
          },
  };
}

void WriteTigerLakeProcessCompensationConfig(const TigerLakeProcessCompensationConfig& config,
                                             DdiId ddi_id, fdf::MmioBuffer* mmio_space) {
  auto compensation1 = registers::PortCompensation1::GetForDdi(ddi_id).ReadFrom(mmio_space);
  compensation1.set_negative_low_voltage_reference_low_value_bits98(config.low.negative.low >> 8)
      .set_negative_low_voltage_reference_high_value_bits98(config.low.negative.high >> 8)
      .set_positive_low_voltage_reference_low_value_bits98(config.low.positive.low >> 8)
      .set_positive_low_voltage_reference_high_value_bits98(config.low.positive.high >> 8)
      .set_negative_nominal_voltage_reference_low_value_bits98(config.nominal.negative.low >> 8)
      .set_negative_nominal_voltage_reference_high_value_bits98(config.nominal.negative.high >> 8)
      .set_positive_nominal_voltage_reference_low_value_bits98(config.nominal.positive.low >> 8)
      .set_positive_nominal_voltage_reference_high_value_bits98(config.nominal.positive.high >> 8)
      .WriteTo(mmio_space);

  auto compensation_nominal =
      registers::PortCompensationNominalVoltageReferences::GetForDdi(ddi_id).FromValue(0);
  compensation_nominal
      .set_negative_nominal_voltage_reference_low_value_bits70(config.nominal.negative.low & 0xff)
      .set_negative_nominal_voltage_reference_high_value_bits70(config.nominal.negative.high & 0xff)
      .set_positive_nominal_voltage_reference_low_value_bits70(config.nominal.positive.low & 0xff)
      .set_positive_nominal_voltage_reference_high_value_bits70(config.nominal.positive.high & 0xff)
      .WriteTo(mmio_space);

  auto compensation_low =
      registers::PortCompensationLowVoltageReferences::GetForDdi(ddi_id).FromValue(0);
  compensation_low
      .set_negative_low_voltage_reference_low_value_bits70(config.low.negative.low & 0xff)
      .set_negative_low_voltage_reference_high_value_bits70(config.low.negative.high & 0xff)
      .set_positive_low_voltage_reference_low_value_bits70(config.low.positive.low & 0xff)
      .set_positive_low_voltage_reference_high_value_bits70(config.low.positive.high & 0xff)
      .WriteTo(mmio_space);
}

// Returns an empty configuration for unsupported process monitor values.
TigerLakeProcessCompensationConfig ProcessCompensationConfigFor(
    registers::PortCompensationStatus::ProcessSelect process,
    registers::PortCompensationStatus::VoltageSelect voltage) {
  switch (voltage) {
    case registers::PortCompensationStatus::VoltageSelect::k850mv:
      switch (process) {
        case registers::PortCompensationStatus::ProcessSelect::kDot0:
          return TigerLakeProcessCompensationConfig{
              .nominal = {.negative = {.low = 0x62, .high = 0xab},
                          .positive = {.low = 0x67, .high = 0xbb}},
              .low = {.negative = {.low = 0x51, .high = 0x91},
                      .positive = {.low = 0x4f, .high = 0x96}}};
        case registers::PortCompensationStatus::ProcessSelect::kDot1:
        case registers::PortCompensationStatus::ProcessSelect::kDot4:
          break;
      };
      break;
    case registers::PortCompensationStatus::VoltageSelect::k950mv:
      switch (process) {
        case registers::PortCompensationStatus::ProcessSelect::kDot0:
          return TigerLakeProcessCompensationConfig{
              .nominal = {.negative = {.low = 0x86, .high = 0xe1},
                          .positive = {.low = 0x72, .high = 0xc7}},
              .low = {.negative = {.low = 0x77, .high = 0xca},
                      .positive = {.low = 0x5e, .high = 0xab}}};
        case registers::PortCompensationStatus::ProcessSelect::kDot1:
          return TigerLakeProcessCompensationConfig{
              .nominal = {.negative = {.low = 0x93, .high = 0xf8},
                          .positive = {.low = 0x7e, .high = 0xf1}},
              .low = {.negative = {.low = 0x8a, .high = 0xe8},
                      .positive = {.low = 0x71, .high = 0xc5}}};
        case registers::PortCompensationStatus::ProcessSelect::kDot4:
          break;
      };
      break;
    case registers::PortCompensationStatus::VoltageSelect::k1050mv:
      switch (process) {
        case registers::PortCompensationStatus::ProcessSelect::kDot0:
          return TigerLakeProcessCompensationConfig{
              .nominal = {.negative = {.low = 0x98, .high = 0xfa},
                          .positive = {.low = 0x82, .high = 0xdd}},
              .low = {.negative = {.low = 0x89, .high = 0xe4},
                      .positive = {.low = 0x6d, .high = 0xc1}}};
        case registers::PortCompensationStatus::ProcessSelect::kDot1:
          return TigerLakeProcessCompensationConfig{
              .nominal = {.negative = {.low = 0x9a, .high = 0x100},
                          .positive = {.low = 0xab, .high = 0x125}},
              .low = {.negative = {.low = 0x8a, .high = 0xe3},
                      .positive = {.low = 0x8f, .high = 0xf1}}};
        case registers::PortCompensationStatus::ProcessSelect::kDot4:
          break;
      };
  };

  fdf::error("Undocumented process/voltage combination");
  return {};
}

}  // namespace

bool ComboDdiTigerLake::Initialize() {
  // This implements the section "Digital Display Interface" > "Combo PHY
  // Initialization Sequence" in display engine PRMs.
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev2.0 pages 391-392
  // DG1: IHD-OS-DG1-Vol 12-2.21 pages 337-338
  // Ice Lake: IHD-OS-ICLLP-Vol 12-1.22-Rev2.0 pages 334-335

  // TODO(https://fxbug.dev/42065111): Implement the compensation source dependency
  // between DDI A and DDIs B-C.

  auto procmon_status =
      registers::PortCompensationStatus::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  {
    const char* process_name;
    switch (procmon_status.process_select()) {
      case registers::PortCompensationStatus::ProcessSelect::kDot0:
        process_name = "dot-0";
        break;
      case registers::PortCompensationStatus::ProcessSelect::kDot1:
        process_name = "dot-1";
        break;
      case registers::PortCompensationStatus::ProcessSelect::kDot4:
        process_name = "dot-4";
        break;
      default:
        fdf::warn("DDI {} process monitor reports undocumented process variation {}", ddi_id(),
                  static_cast<unsigned int>(procmon_status.process_select()));
        process_name = "dot-undocumented";
    };

    const char* voltage_name;
    switch (procmon_status.voltage_select()) {
      case registers::PortCompensationStatus::VoltageSelect::k850mv:
        voltage_name = "0.85v";
        break;
      case registers::PortCompensationStatus::VoltageSelect::k950mv:
        voltage_name = "0.95v";
        break;
      case registers::PortCompensationStatus::VoltageSelect::k1050mv:
        voltage_name = "1.05v";
        break;
      default:
        fdf::warn("DDI {} process monitor reports undocumented voltage variation {}", ddi_id(),
                  static_cast<unsigned int>(procmon_status.voltage_select()));
        voltage_name = "undocumented-v";
    };

    fdf::trace("DDI {} Process variation: {} {}, Process monitor done: {} ", ddi_id(), process_name,
               voltage_name, procmon_status.process_monitor_done() ? "yes" : "no");
    fdf::trace("DDI {} Current comp: {}{}{}, MIPI LPDn code: {}{}{}, First compensation done: {}",
               ddi_id(), procmon_status.current_compensation_code(),
               procmon_status.current_compensation_code_maxout() ? " maxout" : "",
               procmon_status.current_compensation_code_minout() ? " minout" : "",
               procmon_status.mipi_low_power_data_negative_code(),
               procmon_status.mipi_low_power_data_negative_code_maxout() ? " maxout" : "",
               procmon_status.mipi_low_power_data_negative_code_minout() ? " minout" : "",
               procmon_status.first_compensation_done() ? "yes" : "no");
  }

  {
    TigerLakeProcessCompensationConfig process_compensation =
        ReadTigerLakeProcessCompensationConfig(ddi_id(), mmio_space_);
    fdf::trace(
        "DDI {} Process monitor nominal voltage references: -ve low {:x} high {:x}, +ve low {:x} high {:x}",
        ddi_id(), process_compensation.nominal.negative.low,
        process_compensation.nominal.negative.high, process_compensation.nominal.positive.low,
        process_compensation.nominal.positive.high);
    fdf::trace(
        "DDI {} Process monitor low voltage references: -ve low {:x} high {:x}, +ve low {:x} high {:x}",
        ddi_id(), process_compensation.low.negative.low, process_compensation.low.negative.high,
        process_compensation.low.positive.low, process_compensation.low.positive.high);
  }

  auto common_lane5 = registers::PortCommonLane5::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  fdf::trace(
      "DDI {} PORT_CL_DW5: {:08x}, common lane power down {}, suspend clock config {}, "
      "downlink broadcast {}, force {:02x}, CRI clock: count max {} select {}, "
      "IOSF PD: count {} divider select {}, PHY power ack override {}, "
      "staggering: port {} power gate {}, fuse flags: {} {} {}",
      ddi_id(), common_lane5.reg_value(),
      common_lane5.common_lane_power_down_enabled() ? "enabled" : "disabled",
      common_lane5.suspend_clock_config(),
      common_lane5.downlink_broadcast_enable() ? "enabled" : "disabled", common_lane5.force(),
      common_lane5.common_register_interface_clock_count_max(),
      common_lane5.common_register_interface_clock_select(),
      common_lane5.onchip_system_fabric_presence_detection_count(),
      common_lane5.onchip_system_fabric_clock_divider_select(),
      common_lane5.phy_power_ack_override() ? "enabled" : "disabled",
      common_lane5.port_staggering_enabled() ? "enabled" : "disabled",
      common_lane5.port_staggering_enabled() ? "enabled" : "disabled",
      common_lane5.fuse_valid_override() ? "valid override" : "-",
      common_lane5.fuse_valid_reset() ? "valid reset" : "-",
      common_lane5.fuse_repull() ? "repull" : "-");

  static constexpr registers::PortLane kAllLanes[] = {
      registers::PortLane::kAux, registers::PortLane::kMainLinkLane0,
      registers::PortLane::kMainLinkLane1, registers::PortLane::kMainLinkLane2,
      registers::PortLane::kMainLinkLane3};
  for (registers::PortLane lane : kAllLanes) {
    auto transmitter_dcc =
        registers::PortTransmitterDutyCycleCorrection::GetForDdiLane(ddi_id(), lane)
            .ReadFrom(mmio_space_);
    fdf::trace(
        "DDI {} Lane {} PORT_TX_DW8: {:08x}, output DCC clock: select {} divider select {}, "
        "output DCC code: override {} {} limits {} - {}, output DCC fuse {}, "
        "input DCC code: {} thermal {}",
        ddi_id(), static_cast<int>(lane), transmitter_dcc.reg_value(),
        transmitter_dcc.output_duty_cycle_correction_clock_select(),
        static_cast<int>(transmitter_dcc.output_duty_cycle_correction_clock_divider_select()),
        transmitter_dcc.output_duty_cycle_correction_code_override_valid() ? "valid" : "invalid",
        transmitter_dcc.output_duty_cycle_correction_code_override(),
        transmitter_dcc.output_duty_cycle_correction_lower_limit(),
        transmitter_dcc.output_duty_cycle_correction_upper_limit(),
        transmitter_dcc.output_duty_cycle_correction_fuse_enabled() ? "enabled" : "disabled",
        transmitter_dcc.input_duty_cycle_correction_code(),
        (transmitter_dcc.input_duty_cycle_correction_thermal_bits43() << 2) |
            transmitter_dcc.input_duty_cycle_correction_thermal_bits20());

    auto physical_coding1 =
        registers::PortPhysicalCoding1::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space_);
    fdf::trace(
        "DDI {} Lane {} PORT_PCS_DW1: {:08x}, power-gated {}, DCC schedule {}, "
        "DCC calibration: force {} bypass {} on wake {}, clock request {}, "
        "commmon keeper: {} / {} while power-gated / bias control {}, latency optimization {}, "
        "soft lane reset: {} {}, transmitter fifo reset override: {} {}, "
        "transmiter de-emphasis {}, TBC as symbol clock {}",
        ddi_id(), static_cast<int>(lane), physical_coding1.reg_value(),
        physical_coding1.power_gate_powered_down() ? "yes" : "no",
        static_cast<int>(physical_coding1.duty_cycle_correction_schedule_select()),
        physical_coding1.force_transmitter_duty_cycle_correction_calibration() ? "yes" : "no",
        physical_coding1.duty_cycle_correction_calibration_bypassed() ? "enabled" : "disabled",
        physical_coding1.duty_cycle_correction_calibration_on_wake() ? "yes" : "no",
        physical_coding1.clock_request(),
        physical_coding1.common_mode_keeper_enabled() ? "enabled" : "disabled",
        physical_coding1.common_mode_keeper_enabled_while_power_gated() ? "enabled" : "disabled",
        physical_coding1.common_mode_keeper_bias_control(),
        physical_coding1.latency_optimization_value(),
        physical_coding1.soft_lane_reset() ? "on" : "off",
        physical_coding1.soft_lane_reset_valid() ? "valid" : "invalid",
        physical_coding1.transmitter_fifo_reset_main_override() ? "on" : "off",
        physical_coding1.transmitter_fifo_reset_main_override_valid() ? "valid" : "invalid",
        physical_coding1.transmitter_deemphasis_value(),
        physical_coding1.use_transmitter_buffer_clock_as_symbol_clock() ? "yes" : "no");
  }

  auto phy_misc = registers::PhyMisc::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  fdf::trace("DDI {} PHY_MISC {:08x}, DE to IO: {:x}, IO to DE: {:x}, Comp power down: {}",
             ddi_id(), phy_misc.reg_value(), phy_misc.display_engine_to_io(),
             phy_misc.io_to_display_engine(),
             phy_misc.compensation_resistors_powered_down() ? "enabled" : "disabled");

  auto compensation_source =
      registers::PortCompensationSource::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  fdf::trace(
      "DDI {} PORT_COMP_DW8 {:08x}, internal reference generation {}, periodic compensation {}",
      ddi_id(), compensation_source.reg_value(),
      compensation_source.generate_internal_references() ? "enabled" : "disabled",
      compensation_source.periodic_current_compensation_disabled() ? "disabled" : "enabled");

  auto compensation_initialized =
      registers::PortCompensation0::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  fdf::trace("DDI {} PORT_COMP_DW0: {:08x} PORT_COMP_DW3: {:08x} ", ddi_id(),
             compensation_initialized.reg_value(), procmon_status.reg_value());
  if (compensation_initialized.initialized()) {
    // The PRMs advise that we consider the PHY initialized if this bit is set,
    // and skip the entire initialize process. A more robust approach would be
    // to reset (de-initialize, initialize) the PHY if its current configuration
    // doesn't match what we expect.
    fdf::trace("DDI {} PHY already initialized. Assuming everything is correct.", ddi_id());
    return true;
  }

  for (registers::PortLane lane : kAllLanes) {
    auto transmitter_dcc =
        registers::PortTransmitterDutyCycleCorrection::GetForDdiLane(ddi_id(), lane)
            .ReadFrom(mmio_space_);
    transmitter_dcc.set_output_duty_cycle_correction_clock_select(1)
        .set_output_duty_cycle_correction_clock_divider_select(
            registers::PortTransmitterDutyCycleCorrection::ClockDividerSelect::k2)
        .WriteTo(mmio_space_);

    auto physical_coding1 =
        registers::PortPhysicalCoding1::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space_);
    physical_coding1
        .set_duty_cycle_correction_schedule_select(
            registers::PortPhysicalCoding1::DutyCycleCorrectionScheduleSelect::kContinuously)
        .WriteTo(mmio_space_);
  }

  phy_misc.set_compensation_resistors_powered_down(false).WriteTo(mmio_space_);

  TigerLakeProcessCompensationConfig process_compensation = ProcessCompensationConfigFor(
      procmon_status.process_select(), procmon_status.voltage_select());
  if (process_compensation.IsEmpty()) {
    return false;
  }
  WriteTigerLakeProcessCompensationConfig(process_compensation, ddi_id(), mmio_space_);

  bool is_compensation_source = (ddi_id() == DdiId::DDI_A);
  compensation_source.set_generate_internal_references(is_compensation_source).WriteTo(mmio_space_);

  compensation_initialized.set_initialized(true).WriteTo(mmio_space_);

  common_lane5.set_common_lane_power_down_enabled(true).WriteTo(mmio_space_);
  return true;
}

bool ComboDdiTigerLake::Deinitialize() {
  // This implements the section "Digital Display Interface" > "Combo PHY
  // Un-Initialization Sequence" in display engine PRMs.
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev2.0 page 392
  // DG1: IHD-OS-DG1-Vol 12-2.21 page 338
  // Ice Lake: IHD-OS-ICLLP-Vol 12-1.22-Rev2.0 page 335

  // TODO(https://fxbug.dev/42065111): Implement the compensation source dependency
  // between DDI A and DDIs B-C.

  auto phy_misc = registers::PhyMisc::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  phy_misc.set_compensation_resistors_powered_down(true).WriteTo(mmio_space_);

  auto port_compensation0 = registers::PortCompensation0::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  port_compensation0.set_initialized(false).WriteTo(mmio_space_);

  return true;
}

DdiPhysicalLayer::PhysicalLayerInfo ComboDdiTigerLake::GetPhysicalLayerInfo() const {
  return {
      .ddi_type = DdiPhysicalLayer::DdiType::kCombo,
      .connection_type = DdiPhysicalLayer::ConnectionType::kBuiltIn,
      .max_allowed_dp_lane_count = 4u,
  };
}

TypeCDdiTigerLake::TypeCDdiTigerLake(DdiId ddi_id, Power* power, fdf::MmioBuffer* mmio_space,
                                     bool is_static_port)
    : DdiPhysicalLayer(ddi_id),
      power_(power),
      mmio_space_(mmio_space),
      initialization_phase_(InitializationPhase::kUninitialized),
      is_static_port_(is_static_port),
      physical_layer_info_(DefaultPhysicalLayerInfo()) {
  ZX_ASSERT(power_);
  ZX_ASSERT(mmio_space_);
  ZX_ASSERT(ddi_id >= DdiId::DDI_TC_1);
  ZX_ASSERT(ddi_id <= DdiId::DDI_TC_6);
}

TypeCDdiTigerLake::~TypeCDdiTigerLake() {
  if (initialization_phase_ != InitializationPhase::kUninitialized) {
    fdf::warn("DDI {}: not fully disabled on port teardown", ddi_id());
  }
}

bool TypeCDdiTigerLake::IsEnabled() const {
  return initialization_phase_ == InitializationPhase::kInitialized;
}

bool TypeCDdiTigerLake::IsHealthy() const {
  // All the other states indicate that the DDI PHY is not fully initialized
  // or not fully deinitialized and thus in a limbo state.
  return initialization_phase_ == InitializationPhase::kInitialized ||
         initialization_phase_ == InitializationPhase::kUninitialized;
}

DdiPhysicalLayer::PhysicalLayerInfo TypeCDdiTigerLake::ReadPhysicalLayerInfo() const {
  PhysicalLayerInfo physical_layer_info = {
      .ddi_type = DdiType::kTypeC,
  };

  auto dp_sp = registers::DynamicFlexIoScratchPad::GetForDdi(ddi_id()).ReadFrom(mmio_space_);
  auto type_c_live_state = dp_sp.type_c_live_state(ddi_id());
  switch (type_c_live_state) {
    using TypeCLiveState = registers::DynamicFlexIoScratchPad::TypeCLiveState;
    case TypeCLiveState::kNoHotplugDisplay:
      if (is_static_port_) {
        physical_layer_info.connection_type = ConnectionType::kBuiltIn;
        physical_layer_info.max_allowed_dp_lane_count = 4u;
      } else {
        physical_layer_info.connection_type = ConnectionType::kNone;
        physical_layer_info.max_allowed_dp_lane_count = 0u;
      }
      break;
    case TypeCLiveState::kTypeCHotplugDisplay:
      physical_layer_info.connection_type = ConnectionType::kTypeCDisplayPortAltMode;
      physical_layer_info.max_allowed_dp_lane_count = [&]() {
        size_t count = dp_sp.display_port_assigned_tx_lane_count(ddi_id());
        ZX_DEBUG_ASSERT_MSG(count <= std::numeric_limits<uint8_t>::max(), "%lu overflows uint8_t",
                            count);
        return static_cast<uint8_t>(count);
      }();
      break;
    case TypeCLiveState::kThunderboltHotplugDisplay:
      physical_layer_info.connection_type = ConnectionType::kTypeCThunderbolt;
      physical_layer_info.max_allowed_dp_lane_count = 4u;
      break;
    default:
      ZX_ASSERT_MSG(false, "DDI %d: unsupported type C live state (0x%x)", ddi_id(),
                    static_cast<unsigned int>(type_c_live_state));
  }

  return physical_layer_info;
}

bool TypeCDdiTigerLake::AdvanceEnableFsm() {
  switch (initialization_phase_) {
    case InitializationPhase::kUninitialized:
      initialization_phase_ = InitializationPhase::kTypeCColdBlocked;
      return BlockTypeCColdPowerState();
    case InitializationPhase::kTypeCColdBlocked:
      initialization_phase_ = InitializationPhase::kSafeModeSet;
      if (!SetPhySafeModeDisabled(/*target_disabled=*/true)) {
        return false;
      }
      physical_layer_info_ = ReadPhysicalLayerInfo();
      return physical_layer_info_.connection_type != ConnectionType::kNone;
    case InitializationPhase::kSafeModeSet:
      initialization_phase_ = InitializationPhase::kAuxPoweredOn;
      return SetAuxIoPower(/*target_enabled=*/true);
    case InitializationPhase::kAuxPoweredOn:
      initialization_phase_ = InitializationPhase::kInitialized;
      return true;
    case InitializationPhase::kInitialized:
      return false;
  }
}

bool TypeCDdiTigerLake::AdvanceDisableFsm() {
  switch (initialization_phase_) {
    case InitializationPhase::kUninitialized:
      return false;
    case InitializationPhase::kTypeCColdBlocked:
      if (UnblockTypeCColdPowerState()) {
        physical_layer_info_ = DefaultPhysicalLayerInfo();
        initialization_phase_ = InitializationPhase::kUninitialized;
        return true;
      }
      return false;
    case InitializationPhase::kSafeModeSet:
      if (SetPhySafeModeDisabled(/*target_disabled=*/false)) {
        initialization_phase_ = InitializationPhase::kTypeCColdBlocked;
        return true;
      }
      return false;
    case InitializationPhase::kAuxPoweredOn:
      if (SetAuxIoPower(/*target_enabled=*/false)) {
        initialization_phase_ = InitializationPhase::kSafeModeSet;
        return true;
      }
      return false;
    case InitializationPhase::kInitialized:
      initialization_phase_ = InitializationPhase::kAuxPoweredOn;
      return true;
  }
}

bool TypeCDdiTigerLake::Enable() {
  ZX_ASSERT(IsHealthy());

  // `IsHealthy()` returns true entails that the device is either in
  // `kInitialized` state where it needs to do nothing because of the function's
  // idempotency, or in `kUninitialized` state where it needs to start the
  // finite state machine.
  if (initialization_phase_ == InitializationPhase::kInitialized) {
    return true;
  }
  ZX_DEBUG_ASSERT(initialization_phase_ == InitializationPhase::kUninitialized);

  while (AdvanceEnableFsm()) {
  }
  if (initialization_phase_ == InitializationPhase::kInitialized) {
    fdf::trace("DDI {}: Enabled. New physical layer info: {}", ddi_id(),
               physical_layer_info_.DebugString().c_str());
    return true;
  }
  while (AdvanceDisableFsm()) {
  }
  return false;
}

bool TypeCDdiTigerLake::Disable() {
  switch (initialization_phase_) {
    case InitializationPhase::kUninitialized:
      // Do nothing because of the function's idempotency.
      return true;
    case InitializationPhase::kInitialized:
      // Start the finite state machine of disable process.
      while (AdvanceDisableFsm()) {
      }
      if (initialization_phase_ == InitializationPhase::kUninitialized) {
        fdf::trace("DDI {}: Disabled successfully.", ddi_id());
        return true;
      }
      [[fallthrough]];
    default:
      ZX_ASSERT(!IsHealthy());
      fdf::error("DDI {}: Failed to disable.", ddi_id());
      return false;
  }
}

bool TypeCDdiTigerLake::SetAuxIoPower(bool target_enabled) const {
  power_->SetAuxIoPowerState(ddi_id(), /* enable */ target_enabled);

  if (target_enabled) {
    if (!display::PollUntil([&] { return power_->GetAuxIoPowerState(ddi_id()); }, zx::usec(1),
                            1500)) {
      fdf::error("DDI {}: failed to enable AUX power for ddi", ddi_id());
      return false;
    }

    const bool is_thunderbolt =
        physical_layer_info_.connection_type == DdiPhysicalLayer::ConnectionType::kTypeCThunderbolt;
    if (!is_thunderbolt) {
      // For every Type-C port (static and DP Alternate but not thunderbolt),
      // the driver need to wait for the microcontroller health bit on
      // DKL_CMN_UC_DW27 register after enabling AUX power.
      //
      // TODO(https://fxbug.dev/42182480): Currently Thunderbolt is not supported, so we
      // always check health bit of the IO subsystem microcontroller.
      //
      // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev 2.0, Page 417, "Type-C PHY
      //             Microcontroller health"
      if (!display::PollUntil(
              [&] {
                return registers::DekelCommonConfigMicroControllerDword27::GetForDdi(ddi_id())
                    .ReadFrom(mmio_space_)
                    .microcontroller_firmware_is_ready();
              },
              zx::usec(1), 10)) {
        fdf::error("DDI {}: microcontroller health bit is not set", ddi_id());
        return false;
      }
    }

    auto ddi_aux_ctl = registers::DdiAuxControl::GetForTigerLakeDdi(ddi_id()).ReadFrom(mmio_space_);
    ddi_aux_ctl.set_use_thunderbolt(is_thunderbolt);
    ddi_aux_ctl.WriteTo(mmio_space_);

    fdf::trace("DDI {}: AUX IO power enabled", ddi_id());
  } else {
    zx::nanosleep(zx::deadline_after(zx::usec(10)));
    fdf::trace("DDI {}: AUX IO power {}disabled", ddi_id(),
               power_->GetAuxIoPowerState(ddi_id()) ? "not " : "");
  }

  return true;
}

bool TypeCDdiTigerLake::SetPhySafeModeDisabled(bool target_disabled) const {
  if (target_disabled && !registers::DynamicFlexIoDisplayPortPhyModeStatus::GetForDdi(ddi_id())
                              .ReadFrom(mmio_space_)
                              .phy_is_ready_for_ddi(ddi_id())) {
    fdf::error("DDI {}: lane not in DP mode", ddi_id());
    return false;
  }

  auto dp_csss =
      registers::DynamicFlexIoDisplayPortControllerSafeStateSettings::GetForDdi(ddi_id()).ReadFrom(
          mmio_space_);
  dp_csss.set_safe_mode_disabled_for_ddi(ddi_id(), /*disabled=*/target_disabled);
  dp_csss.WriteTo(mmio_space_);
  dp_csss.ReadFrom(mmio_space_);
  fdf::trace("DDI {}: {} DP safe mode", ddi_id(), target_disabled ? "disabled" : "enabled");
  return true;
}

bool TypeCDdiTigerLake::BlockTypeCColdPowerState() {
  // TODO(https://fxbug.dev/42062380): TCCOLD (Type C cold power state) blocking should
  // be decided at the display engine level. We may have already blocked TCCOLD
  // while bringing up another Type C DDI.
  fdf::trace("Asking PCU firmware to block Type C cold power state");
  PowerController power_controller(mmio_space_);
  const zx::result<> power_status = power_controller.SetDisplayTypeCColdBlockingTigerLake(
      /*blocked=*/true, PowerController::RetryBehavior::kRetryUntilStateChanges);
  switch (power_status.status_value()) {
    case ZX_OK:
      fdf::trace("PCU firmware blocked Type C cold power state");
      return true;
    default:
      fdf::error("Type C ports unusable. PCU firmware didn't block Type C cold power state: {}",
                 power_status);
      return false;
  }
}

bool TypeCDdiTigerLake::UnblockTypeCColdPowerState() {
  // TODO(https://fxbug.dev/42062380): TCCOLD (Type C cold power state) blocking should
  // be decided at the display engine level. We may have already blocked TCCOLD
  // while bringing up another Type C DDI.
  fdf::trace("Asking PCU firmware to unblock Type C cold power state");
  PowerController power_controller(mmio_space_);
  const zx::result<> power_status = power_controller.SetDisplayTypeCColdBlockingTigerLake(
      /*blocked=*/false, PowerController::RetryBehavior::kNoRetry);
  switch (power_status.status_value()) {
    case ZX_OK:
      fdf::trace("PCU firmware unblocked and entered Type C cold power state");
      return true;
    case ZX_ERR_IO_REFUSED:
      fdf::info(
          "PCU firmware did not enter Type C cold power state. "
          "Type C ports in use elsewhere.");
      return true;
    default:
      fdf::error(
          "PCU firmware failed to unblock Type C cold power state. "
          "Type C ports unusable.");
      return false;
  }
}

}  // namespace intel_display
