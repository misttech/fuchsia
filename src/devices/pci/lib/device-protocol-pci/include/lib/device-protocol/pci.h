// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_PCI_LIB_DEVICE_PROTOCOL_PCI_INCLUDE_LIB_DEVICE_PROTOCOL_PCI_H_
#define SRC_DEVICES_PCI_LIB_DEVICE_PROTOCOL_PCI_INCLUDE_LIB_DEVICE_PROTOCOL_PCI_H_

#include <zircon/availability.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

#ifdef __cplusplus

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/result.h>

#include <memory>
#include <optional>
#include <string_view>

namespace fdf {

// A helper class that wraps the `fuchsia.hardware.pci/Device` FIDL calls.
// This class provides convenience methods for DFv2 drivers to interact with PCI devices.
class Pci {
 public:
  // The fragment and service instance name used by composite PCI devices.
  static constexpr char kFragmentName[] = "pci";

  Pci() = default;
  explicit Pci(fidl::ClientEnd<fuchsia_hardware_pci::Device> client);

  Pci(Pci&&) noexcept = default;
  Pci& operator=(Pci&&) noexcept = default;
  Pci(const Pci&) = delete;
  Pci& operator=(const Pci&) = delete;
  ~Pci() = default;

  // Connect to the PCI device in the given incoming namespace.
  //
  // If the instance/fragment name is not provided, it defaults to `pci` for composite devices.
  // Drivers binding directly to non-composite PCI nodes should pass "default".
  static zx::result<Pci> Connect(const std::shared_ptr<fdf::Namespace>& incoming,
                                 std::string_view instance = kFragmentName);
  static zx::result<Pci> Connect(const fdf::Namespace& incoming,
                                 std::string_view instance = kFragmentName);

  fidl::UnownedClientEnd<fuchsia_hardware_pci::Device> borrow() const {
    return client_.is_valid()
               ? client_.client_end().borrow()
               : fidl::UnownedClientEnd<fuchsia_hardware_pci::Device>(ZX_HANDLE_INVALID);
  }

  zx_status_t GetDeviceInfo(fuchsia_hardware_pci::wire::DeviceInfo* out_info) const;
  // The arena backs the memory of the Bar result and must have the same
  // lifetime or longer.
  zx_status_t GetBar(fidl::AnyArena& arena, uint32_t bar_id,
                     fuchsia_hardware_pci::wire::Bar* out_result) const;
  zx_status_t SetBusMastering(bool enabled) const;
  zx_status_t ResetDevice() const;
  zx_status_t AckInterrupt() const;
  zx_status_t MapInterrupt(uint32_t which_irq, zx::interrupt* out_interrupt) const;
  void GetInterruptModes(fuchsia_hardware_pci::wire::InterruptModes* out_modes) const;
  zx_status_t SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                               uint32_t requested_irq_count) const;
  zx_status_t ReadConfig8(uint16_t offset, uint8_t* out_value) const;
  zx_status_t ReadConfig8(fuchsia_hardware_pci::Config offset, uint8_t* out_value) const;
  zx_status_t ReadConfig16(uint16_t offset, uint16_t* out_value) const;
  zx_status_t ReadConfig16(fuchsia_hardware_pci::Config offset, uint16_t* out_value) const;
  zx_status_t ReadConfig32(uint16_t offset, uint32_t* out_value) const;
  zx_status_t ReadConfig32(fuchsia_hardware_pci::Config offset, uint32_t* out_value) const;
  zx_status_t WriteConfig8(uint16_t offset, uint8_t value) const;
  zx_status_t WriteConfig16(uint16_t offset, uint16_t value) const;
  zx_status_t WriteConfig32(uint16_t offset, uint32_t value) const;
  zx_status_t GetFirstCapability(fuchsia_hardware_pci::CapabilityId id, uint8_t* out_offset) const;
  zx_status_t GetNextCapability(fuchsia_hardware_pci::CapabilityId id, uint8_t start_offset,
                                uint8_t* out_offset) const;
  zx_status_t GetFirstExtendedCapability(fuchsia_hardware_pci::ExtendedCapabilityId id,
                                         uint16_t* out_offset) const;
  zx_status_t GetNextExtendedCapability(fuchsia_hardware_pci::ExtendedCapabilityId id,
                                        uint16_t start_offset, uint16_t* out_offset) const;
  zx_status_t GetBti(uint32_t index, zx::bti* out_bti) const;

  zx_status_t ConfigureInterruptMode(uint32_t requested_irq_count,
                                     fuchsia_hardware_pci::InterruptMode* out_mode = nullptr) const;
  zx::result<fdf::MmioBuffer> MapMmio(
      uint32_t bar_id, uint32_t cache_policy = ZX_CACHE_POLICY_UNCACHED_DEVICE) const;
  zx_status_t MapMmio(uint32_t bar_id, uint32_t cache_policy,
                      std::optional<fdf::MmioBuffer>* mmio) const;
  bool is_valid() const { return client_.is_valid(); }

 private:
  fidl::WireSyncClient<fuchsia_hardware_pci::Device> client_;
};

}  // namespace fdf

#endif  // __cplusplus

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

#endif  // SRC_DEVICES_PCI_LIB_DEVICE_PROTOCOL_PCI_INCLUDE_LIB_DEVICE_PROTOCOL_PCI_H_
