// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_
#define SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_

#include <fidl/fuchsia.hardware.pci/cpp/wire_types.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/pci/constants.h>
#include <lib/pci/hw.h>
#include <lib/zx/result.h>
#include <stdio.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <memory>
#include <type_traits>

#include <hwreg/bitfields.h>

namespace pci {

class Config;

namespace config {

// Fields correspond to the name in the PCI Local Bus Spec section 6.2.
struct CommandReg : public hwreg::RegisterBase<CommandReg, uint16_t> {
  // 15-11 are reserved preserve.
  DEF_BIT(10, interrupt_disable);
  DEF_BIT(9, fast_back_to_back_enable);
  DEF_BIT(8, serr_enable);
  // 7 is reserved preserve.
  DEF_BIT(6, parity_error_response);
  DEF_BIT(5, vga_palette_snoop);
  DEF_BIT(4, memory_write_and_invalidate_enable);
  DEF_BIT(3, special_cycles);
  DEF_BIT(2, bus_master);
  DEF_BIT(1, memory_space);
  DEF_BIT(0, io_space);
};
inline const hwreg::RegisterAddr<CommandReg> Command(pci::kConfigCommand);

// Status register (PCI Local Bus Spec section 6.2.3, PCIe Base Spec section 7.5.1.1.4).
// Bits 8 and 11-15 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct StatusReg : public hwreg::RegisterBase<StatusReg, uint16_t> {
  DEF_BIT(15, detected_parity_error);
  DEF_BIT(14, signaled_system_error);
  DEF_BIT(13, received_master_abort);
  DEF_BIT(12, received_target_abort);
  DEF_BIT(11, signaled_target_abort);
  DEF_FIELD(10, 9, devsel_timing);
  DEF_BIT(8, master_data_parity_error);
  DEF_BIT(7, fast_b2b_capable);
  // 6 is reserved preserve.
  DEF_BIT(5, capable_66mhz);
  DEF_BIT(4, has_capabilities_list);
  DEF_BIT(3, interrupt_status);
  // 2-1 are reserved preserve.
  DEF_BIT(0, immediate_readiness);
};
inline const hwreg::RegisterAddr<StatusReg> Status(pci::kConfigStatus);

// The layout of a Base Address Register changes based on its type.
// PCI Local Bus Spec section 6.2.5.1.
struct IoBaseAddressReg : public hwreg::RegisterBase<IoBaseAddressReg, uint32_t> {
  DEF_UNSHIFTED_FIELD(31, 2, base_address);
  DEF_BIT(0, is_io_space);
};

struct MmioBaseAddressReg : public hwreg::RegisterBase<MmioBaseAddressReg, uint32_t> {
  static constexpr uint32_t kType32Bit = 0b00;
  static constexpr uint32_t kType64Bit = 0b10;

  DEF_UNSHIFTED_FIELD(31, 4, base_address);
  DEF_BIT(3, is_prefetchable);
  DEF_FIELD(2, 1, type);
  DEF_BIT(0, is_io_space);
};

struct BaseAddressReg : public hwreg::RegisterBase<BaseAddressReg, uint32_t> {
  // Bit 1 is reserved in I/O BARs and part of the 2-bit type field in MMIO BARs.
  DEF_BIT(0, is_io_space);

  IoBaseAddressReg io() const {
    return hwreg::RegisterAddr<IoBaseAddressReg>(reg_addr()).FromValue(reg_value());
  }
  MmioBaseAddressReg mmio() const {
    return hwreg::RegisterAddr<MmioBaseAddressReg>(reg_addr()).FromValue(reg_value());
  }
};
inline hwreg::RegisterAddr<BaseAddressReg> BaseAddress(uint32_t bar_id) {
  ZX_DEBUG_ASSERT(bar_id < pci::kMaxBarCount);
  return hwreg::RegisterAddr<BaseAddressReg>(
      static_cast<uint32_t>(pci::kConfigBaseAddresses + (bar_id * sizeof(uint32_t))));
}

}  // namespace config

// Per spec (quoting Linux DT host-generic bindings):
// For CAM, this 24-bit offset is:
//         cfg_offset(bus, device, function, register) =
//                    bus << 16 | device << 11 | function << 8 | register
// While ECAM extends this by 4 bits to accommodate 4k of function space:
//         cfg_offset(bus, device, function, register) =
//                    bus << 20 | device << 15 | function << 12 | register
struct CamOffset {
  zx_vaddr_t offset = 0;
  DEF_SUBFIELD(offset, 23, 16, bus);
  DEF_SUBFIELD(offset, 15, 11, device);
  DEF_SUBFIELD(offset, 10, 8, function);
};

struct EcamOffset {
  zx_vaddr_t offset = 0;
  DEF_SUBFIELD(offset, 27, 20, bus);
  DEF_SUBFIELD(offset, 19, 15, device);
  DEF_SUBFIELD(offset, 14, 12, function);
};

template <class CamType>
constexpr zx_vaddr_t GetConfigOffsetInCam(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t start_bus) {
  zx_vaddr_t bus_offset = CamType().set_bus(start_bus).offset;
  zx_vaddr_t bdf_offset =
      CamType().set_bus(bdf.bus_id).set_device(bdf.device_id).set_function(bdf.function_id).offset;
  return bdf_offset - bus_offset;
}

// Find the offset into the cam region for the given bdf address. Every bus
// has 32 devices, every device has 8 functions, and each function has a
// configuration access region of 256 bytes base, 4096 if extended. The base
// address of the vmo provided to the bus driver corresponds to the
// start_bus_num, so offset the bdf address based on the bottom of our ecam.
constexpr zx_vaddr_t GetConfigOffsetInCam(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t start_bus,
                                          bool is_extended) {
  if (is_extended) {
    return GetConfigOffsetInCam<EcamOffset>(bdf, start_bus);
  }
  return GetConfigOffsetInCam<CamOffset>(bdf, start_bus);
}

class PciReg8 {
 public:
  constexpr PciReg8() = default;
  constexpr explicit PciReg8(uint16_t offset) : offset_(offset) {}
  constexpr uint16_t offset() const { return offset_; }
  bool operator==(const PciReg8& other) const { return (offset() == other.offset()); }

 private:
  uint16_t offset_ = 0;
};

class PciReg16 {
 public:
  constexpr PciReg16() = default;
  constexpr explicit PciReg16(uint16_t offset) : offset_(offset) {}
  constexpr uint16_t offset() const { return offset_; }
  bool operator==(const PciReg8& other) const { return (offset() == other.offset()); }

 private:
  uint16_t offset_ = 0;
};

class PciReg32 {
 public:
  constexpr PciReg32() = default;
  constexpr explicit PciReg32(uint16_t offset) : offset_(offset) {}
  constexpr uint16_t offset() const { return offset_; }
  bool operator==(const PciReg8& other) const { return (offset() == other.offset()); }

 private:
  uint16_t offset_ = 0;
};

// Config supplies the factory for creating the appropriate pci config
// object based on the address space of the pci device.
class Config {
 public:
  // Standard PCI configuration space values. Offsets from PCI Firmware Spec ch 6.
  static constexpr PciReg16 kVendorId = PciReg16(pci::kConfigVendorId);
  static constexpr PciReg16 kDeviceId = PciReg16(pci::kConfigDeviceId);
  static constexpr PciReg16 kCommand = PciReg16(pci::kConfigCommand);
  static constexpr PciReg16 kStatus = PciReg16(pci::kConfigStatus);
  static constexpr PciReg8 kRevisionId = PciReg8(pci::kConfigRevisionId);
  static constexpr PciReg8 kProgramInterface = PciReg8(pci::kConfigClassCodeIntr);
  static constexpr PciReg8 kSubClass = PciReg8(pci::kConfigClassCodeSub);
  static constexpr PciReg8 kBaseClass = PciReg8(pci::kConfigClassCodeBase);
  static constexpr PciReg8 kCacheLineSize = PciReg8(pci::kConfigCacheLineSize);
  static constexpr PciReg8 kLatencyTimer = PciReg8(pci::kConfigLatencyTimer);
  static constexpr PciReg8 kHeaderType = PciReg8(pci::kConfigHeaderType);
  static constexpr PciReg8 kBist = PciReg8(pci::kConfigBist);
  // 0x10 is the address of the first BAR in config space
  // BAR rather than BaseAddress for brevity
  static constexpr PciReg32 kBar(uint32_t bar) {
    ZX_ASSERT(bar < pci::kMaxBarCount);
    return PciReg32(static_cast<uint16_t>(pci::kConfigBaseAddresses + (bar * sizeof(uint32_t))));
  }
  static constexpr PciReg32 kCardbusCisPtr = PciReg32(pci::kConfigCardbusCisPtr);
  static constexpr PciReg16 kSubsystemVendorId = PciReg16(pci::kConfigSubsysVendorId);
  static constexpr PciReg16 kSubsystemId = PciReg16(pci::kConfigSubsysId);
  static constexpr PciReg32 kExpansionRomAddress = PciReg32(pci::kConfigExpRomAddress);
  static constexpr PciReg8 kCapabilitiesPtr = PciReg8(pci::kConfigCapabilities);
  // 0x35 through 0x3B is reserved
  static constexpr PciReg8 kInterruptLine = PciReg8(pci::kConfigInterruptLine);
  static constexpr PciReg8 kInterruptPin = PciReg8(pci::kConfigInterruptPin);
  static constexpr PciReg8 kMinGrant = PciReg8(pci::kConfigMinGrant);
  static constexpr PciReg8 kMaxLatency = PciReg8(pci::kConfigMaxLatency);
  static constexpr uint8_t kStdCfgEnd =
      static_cast<uint8_t>(kMaxLatency.offset() + sizeof(uint8_t));

  // pci to pci bridge config
  // Unlike a normal PCI header, a bridge only has two BARs, but the BAR offset in config space
  // is the same.
  static constexpr PciReg8 kPrimaryBusId = PciReg8(0x18);
  static constexpr PciReg8 kSecondaryBusId = PciReg8(0x19);
  static constexpr PciReg8 kSubordinateBusId = PciReg8(0x1A);
  static constexpr PciReg8 kSecondaryLatencyTimer = PciReg8(0x1B);
  static constexpr PciReg8 kIoBase = PciReg8(0x1C);
  static constexpr PciReg8 kIoLimit = PciReg8(0x1D);
  static constexpr PciReg16 kSecondaryStatus = PciReg16(0x1E);
  static constexpr PciReg16 kMemoryBase = PciReg16(0x20);
  static constexpr PciReg16 kMemoryLimit = PciReg16(0x22);
  static constexpr PciReg16 kPrefetchableMemoryBase = PciReg16(0x24);
  static constexpr PciReg16 kPrefetchableMemoryLimit = PciReg16(0x26);
  static constexpr PciReg32 kPrefetchableMemoryBaseUpper = PciReg32(0x28);
  static constexpr PciReg32 kPrefetchableMemoryLimitUpper = PciReg32(0x2C);
  static constexpr PciReg16 kIoBaseUpper = PciReg16(0x30);
  static constexpr PciReg16 kIoLimitUpper = PciReg16(0x32);
  // Capabilities Pointer for a bridge matches the standard 0x34 offset
  // 0x35 through 0x38 is reserved
  static constexpr PciReg32 kBridgeExpansionRomAddress = PciReg32(0x38);
  // interrupt line for a bridge matches the standard 0x3C offset
  // interrupt pin for a bridge matches the standard 0x3D offset
  static constexpr PciReg16 kBridgeControl = PciReg16(0x3E);

  const fuchsia_hardware_pci::wire::Bdf& bdf() const { return bdf_; }
  const char* addr() const { return addr_; }
  virtual const char* type() const = 0;
  // Return a copy of the MmioView backing the Config's MMIO space, if supported.
  virtual zx::result<fdf::MmioView> get_view() const { return zx::error(ZX_ERR_NOT_SUPPORTED); }

  // Virtuals
  virtual uint8_t Read(PciReg8 addr) const = 0;
  virtual uint16_t Read(PciReg16 addr) const = 0;
  virtual uint32_t Read(PciReg32 addr) const = 0;
  virtual void Write(PciReg8 addr, uint8_t val) const = 0;
  virtual void Write(PciReg16 addr, uint16_t val) const = 0;
  virtual void Write(PciReg32 addr, uint32_t val) const = 0;

  // Sized configuration space accessors used by hwreg for register I/O and direct raw-offset
  // access.
  template <typename IntType, typename = std::enable_if_t<std::is_unsigned_v<IntType> &&
                                                          !std::is_same_v<IntType, bool>>>
  IntType Read(uint32_t offset) const {
    static_assert(sizeof(IntType) == 1 || sizeof(IntType) == 2 || sizeof(IntType) == 4,
                  "Invalid PCI config access size");
    ZX_DEBUG_ASSERT(offset <= UINT16_MAX);
    if constexpr (sizeof(IntType) == 1) {
      return Read(PciReg8(static_cast<uint16_t>(offset)));
    } else if constexpr (sizeof(IntType) == 2) {
      return Read(PciReg16(static_cast<uint16_t>(offset)));
    } else {
      return Read(PciReg32(static_cast<uint16_t>(offset)));
    }
  }

  template <typename IntType, typename = std::enable_if_t<std::is_unsigned_v<IntType> &&
                                                          !std::is_same_v<IntType, bool>>>
  void Write(IntType val, uint32_t offset) const {
    static_assert(sizeof(IntType) == 1 || sizeof(IntType) == 2 || sizeof(IntType) == 4,
                  "Invalid PCI config access size");
    ZX_DEBUG_ASSERT(offset <= UINT16_MAX);
    if constexpr (sizeof(IntType) == 1) {
      Write(PciReg8(static_cast<uint16_t>(offset)), val);
    } else if constexpr (sizeof(IntType) == 2) {
      Write(PciReg16(static_cast<uint16_t>(offset)), val);
    } else {
      Write(PciReg32(static_cast<uint16_t>(offset)), val);
    }
  }
  virtual ~Config() = default;

 protected:
  explicit Config(fuchsia_hardware_pci::wire::Bdf bdf) : bdf_(bdf) {
    snprintf(addr_, sizeof(addr_), "%02x:%02x.%01x", bdf_.bus_id, bdf_.device_id, bdf_.function_id);
  }

 private:
  const fuchsia_hardware_pci::wire::Bdf bdf_;
  char addr_[8];
};

// MMIO config is the standard method for accessing modern pci configuration space.
// A device's configuration space is mapped to a specific place in a given pci root's
// ecam and can be directly accessed with standard IO operations.
class MmioConfig : public Config {
 public:
  static zx::result<std::unique_ptr<Config>> Create(fuchsia_hardware_pci::wire::Bdf bdf,
                                                    const fdf::MmioBuffer& ecam, uint8_t start_bus,
                                                    uint8_t end_bus, bool is_extended) {
    if (bdf.bus_id < start_bus || bdf.bus_id > end_bus || bdf.device_id >= pci::kMaxDevicesPerBus ||
        bdf.function_id >= pci::kMaxFunctionsPerDevice) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    zx_vaddr_t config_offset = GetConfigOffsetInCam(bdf, start_bus, /*is_extended=*/is_extended);
    zx_vaddr_t config_size = is_extended ? pci::kExtendedConfigSize : pci::kBaseConfigSize;

    return zx::ok(std::make_unique<MmioConfig>(bdf, ecam.View(config_offset, config_size)));
  }

  MmioConfig(fuchsia_hardware_pci::wire::Bdf bdf, const fdf::MmioView& view)
      : Config(bdf), view_(view) {}
  explicit MmioConfig(const fdf::MmioView& view, fuchsia_hardware_pci::wire::Bdf bdf = {})
      : Config(bdf), view_(view) {}

  uint8_t Read(const PciReg8 addr) const final { return view_.Read<uint8_t>(addr.offset()); }
  uint16_t Read(const PciReg16 addr) const final { return view_.Read<uint16_t>(addr.offset()); }
  uint32_t Read(const PciReg32 addr) const final { return view_.Read<uint32_t>(addr.offset()); }
  void Write(PciReg8 addr, uint8_t val) const final { view_.Write(val, addr.offset()); }
  void Write(PciReg16 addr, uint16_t val) const final { view_.Write(val, addr.offset()); }
  void Write(PciReg32 addr, uint32_t val) const override { view_.Write(val, addr.offset()); }
  const char* type() const override { return "mmio"; }
  zx::result<fdf::MmioView> get_view() const final { return zx::ok(fdf::MmioView(view_)); }

 private:
  const fdf::MmioView view_;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_
