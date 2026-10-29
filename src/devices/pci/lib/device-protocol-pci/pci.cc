// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/device-protocol/pci.h>
#include <zircon/syscalls.h>

#include <utility>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace fdf {

namespace fpci = fuchsia_hardware_pci;

Pci::Pci(fidl::ClientEnd<fpci::Device> client) : client_(std::move(client)) {}

zx::result<Pci> Pci::Connect(const std::shared_ptr<fdf::Namespace>& incoming,
                             std::string_view instance) {
  if (!incoming) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return Connect(*incoming, instance);
}

zx::result<Pci> Pci::Connect(const fdf::Namespace& incoming, std::string_view instance) {
  zx::result client = incoming.Connect<fpci::Service::Device>(instance);
  if (client.is_error()) {
    return client.take_error();
  }
  return zx::ok(Pci(std::move(*client)));
}

zx_status_t Pci::GetDeviceInfo(fpci::wire::DeviceInfo* out_info) const {
  if (!out_info) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetDeviceInfo();
  if (!result.ok()) {
    return result.status();
  }
  *out_info = result.value().info;
  return ZX_OK;
}

zx_status_t Pci::GetBar(fidl::AnyArena& arena, uint32_t bar_id, fpci::wire::Bar* out_result) const {
  if (!out_result) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_.buffer(arena)->GetBar(bar_id);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_result = std::move(result->value()->result);
  if (out_result->result.is_io()) {
    zx_status_t status = zx_ioports_request(out_result->result.io().resource.get(),
                                            static_cast<uint16_t>(out_result->result.io().address),
                                            static_cast<uint32_t>(out_result->size));
    return status;
  } else {
    return ZX_OK;
  }
}

zx_status_t Pci::SetBusMastering(bool enabled) const {
  auto result = client_->SetBusMastering(enabled);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::ResetDevice() const {
  auto result = client_->ResetDevice();
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::AckInterrupt() const {
  auto result = client_->AckInterrupt();
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::MapInterrupt(uint32_t which_irq, zx::interrupt* out_interrupt) const {
  if (!out_interrupt) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->MapInterrupt(which_irq);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_interrupt = std::move(result->value()->interrupt);
  return ZX_OK;
}

void Pci::GetInterruptModes(fpci::wire::InterruptModes* out_modes) const {
  if (out_modes) {
    *out_modes = {};
  }
  auto result = client_->GetInterruptModes();
  if (!result.ok()) {
    return;
  }
  if (out_modes) {
    *out_modes = result.value().modes;
  }
}

zx_status_t Pci::SetInterruptMode(fpci::InterruptMode mode, uint32_t requested_irq_count) const {
  auto result = client_->SetInterruptMode(mode, requested_irq_count);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::ReadConfig8(uint16_t offset, uint8_t* out_value) const {
  if (!out_value) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->ReadConfig8(offset);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_value = result->value()->value;
  return ZX_OK;
}

zx_status_t Pci::ReadConfig8(fpci::Config offset, uint8_t* out_value) const {
  return ReadConfig8(static_cast<uint16_t>(offset), out_value);
}

zx_status_t Pci::ReadConfig16(uint16_t offset, uint16_t* out_value) const {
  if (!out_value) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->ReadConfig16(offset);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_value = result->value()->value;
  return ZX_OK;
}

zx_status_t Pci::ReadConfig16(fpci::Config offset, uint16_t* out_value) const {
  return ReadConfig16(static_cast<uint16_t>(offset), out_value);
}

zx_status_t Pci::ReadConfig32(uint16_t offset, uint32_t* out_value) const {
  if (!out_value) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->ReadConfig32(offset);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_value = result->value()->value;
  return ZX_OK;
}

zx_status_t Pci::ReadConfig32(fpci::Config offset, uint32_t* out_value) const {
  return ReadConfig32(static_cast<uint16_t>(offset), out_value);
}

zx_status_t Pci::WriteConfig8(uint16_t offset, uint8_t value) const {
  auto result = client_->WriteConfig8(offset, value);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::WriteConfig16(uint16_t offset, uint16_t value) const {
  auto result = client_->WriteConfig16(offset, value);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::WriteConfig32(uint16_t offset, uint32_t value) const {
  auto result = client_->WriteConfig32(offset, value);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Pci::GetFirstCapability(fpci::CapabilityId id, uint8_t* out_offset) const {
  if (!out_offset) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetCapabilities(id);
  if (!result.ok()) {
    return result.status();
  }
  if (result.value().offsets.empty()) {
    return ZX_ERR_NOT_FOUND;
  }
  *out_offset = result.value().offsets[0];
  return ZX_OK;
}

zx_status_t Pci::GetNextCapability(fpci::CapabilityId id, uint8_t start_offset,
                                   uint8_t* out_offset) const {
  if (!out_offset) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetCapabilities(id);
  if (!result.ok()) {
    return result.status();
  }
  fidl::VectorView<uint8_t> offsets = result.value().offsets;
  for (size_t i = 0; i + 1 < offsets.size(); i++) {
    if (offsets[i] == start_offset) {
      *out_offset = offsets[i + 1];
      return ZX_OK;
    }
  }
  return ZX_ERR_NOT_FOUND;
}

zx_status_t Pci::GetFirstExtendedCapability(fpci::ExtendedCapabilityId id,
                                            uint16_t* out_offset) const {
  if (!out_offset) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetExtendedCapabilities(id);
  if (!result.ok()) {
    return result.status();
  }
  if (result.value().offsets.empty()) {
    return ZX_ERR_NOT_FOUND;
  }
  *out_offset = result.value().offsets[0];
  return ZX_OK;
}

zx_status_t Pci::GetNextExtendedCapability(fpci::ExtendedCapabilityId id, uint16_t start_offset,
                                           uint16_t* out_offset) const {
  if (!out_offset) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetExtendedCapabilities(id);
  if (!result.ok()) {
    return result.status();
  }
  fidl::VectorView<uint16_t> offsets = result.value().offsets;
  for (size_t i = 0; i + 1 < offsets.size(); i++) {
    if (offsets[i] == start_offset) {
      *out_offset = offsets[i + 1];
      return ZX_OK;
    }
  }
  return ZX_ERR_NOT_FOUND;
}

zx_status_t Pci::GetBti(uint32_t index, zx::bti* out_bti) const {
  if (!out_bti) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto result = client_->GetBti(index);
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  *out_bti = std::move(result->value()->bti);
  return ZX_OK;
}

zx::result<fdf::MmioBuffer> Pci::MapMmio(uint32_t bar_id, uint32_t cache_policy) const {
  fidl::Arena arena;
  fpci::wire::Bar bar;
  zx_status_t status = GetBar(arena, bar_id, &bar);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  if (!bar.result.is_vmo()) {
    return zx::error(ZX_ERR_WRONG_TYPE);
  }
  size_t vmo_size;
  status = bar.result.vmo().get_size(&vmo_size);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return fdf::MmioBuffer::Create(0, vmo_size, std::move(bar.result.vmo()), cache_policy);
}

zx_status_t Pci::MapMmio(uint32_t bar_id, uint32_t cache_policy,
                         std::optional<fdf::MmioBuffer>* mmio) const {
  if (!mmio) {
    return ZX_ERR_INVALID_ARGS;
  }
  zx::result<fdf::MmioBuffer> result = MapMmio(bar_id, cache_policy);
  if (result.is_ok()) {
    *mmio = std::move(result.value());
  }
  return result.status_value();
}

zx_status_t Pci::ConfigureInterruptMode(uint32_t requested_irq_count,
                                        fpci::InterruptMode* out_mode) const {
  if (requested_irq_count == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto result = client_->GetInterruptModes();
  if (!result.ok()) {
    return result.status();
  }
  const auto& modes = result.value().modes;
  std::pair<fpci::InterruptMode, uint32_t> pairs[] = {
      {fpci::InterruptMode::kMsiX, modes.msix_count},
      {fpci::InterruptMode::kMsi, modes.msi_count},
      {fpci::InterruptMode::kLegacy, modes.has_legacy ? 1u : 0u},
  };
  for (auto& [mode, irq_cnt] : pairs) {
    if (irq_cnt >= requested_irq_count) {
      zx_status_t status = SetInterruptMode(mode, requested_irq_count);
      if (status == ZX_OK) {
        if (out_mode) {
          *out_mode = mode;
        }
        return status;
      }
    }
  }
  return ZX_ERR_NOT_SUPPORTED;
}

}  // namespace fdf

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
