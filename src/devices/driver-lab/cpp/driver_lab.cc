// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver_lab/driver_lab.h>
#include <lib/zx/vmar.h>
#include <zircon/errors.h>

#include <atomic>
#include <utility>
#include <vector>

namespace driver_lab {

// -----------------------------------------------------------------------------
// StateVmoBank
// -----------------------------------------------------------------------------

zx::result<StateVmoBank> StateVmoBank::Create(size_t size, bool register_global) {
  if (size == 0 || (size % 4) != 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(size, 0, &vmo);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  zx_vaddr_t mapped_addr = 0;
  status =
      zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, size, &mapped_addr);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  if (register_global) {
    driver_lab_global_register_state_bank(reinterpret_cast<uint8_t*>(mapped_addr), size);
  }
  return zx::ok(StateVmoBank(std::move(vmo), mapped_addr, size, register_global));
}

StateVmoBank::StateVmoBank(zx::vmo vmo, zx_vaddr_t mapped_addr, size_t size, bool registered_global)
    : vmo_(std::move(vmo)),
      mapped_addr_(mapped_addr),
      size_(size),
      registered_global_(registered_global) {}

StateVmoBank::~StateVmoBank() { Reset(); }

StateVmoBank::StateVmoBank(StateVmoBank&& other) noexcept
    : vmo_(std::move(other.vmo_)),
      mapped_addr_(std::exchange(other.mapped_addr_, 0)),
      size_(std::exchange(other.size_, 0)),
      registered_global_(std::exchange(other.registered_global_, false)) {}

StateVmoBank& StateVmoBank::operator=(StateVmoBank&& other) noexcept {
  if (this != &other) {
    Reset();
    vmo_ = std::move(other.vmo_);
    mapped_addr_ = std::exchange(other.mapped_addr_, 0);
    size_ = std::exchange(other.size_, 0);
    registered_global_ = std::exchange(other.registered_global_, false);
  }
  return *this;
}

void StateVmoBank::Reset() {
  if (mapped_addr_ != 0) {
    if (registered_global_) {
      driver_lab_global_unregister_state_bank(reinterpret_cast<const uint8_t*>(mapped_addr_));
      registered_global_ = false;
    }
    (void)zx::vmar::root_self()->unmap(mapped_addr_, size_);
    mapped_addr_ = 0;
    size_ = 0;
  }
  vmo_.reset();
}

void StateVmoBank::RegisterGlobal() {
  if (mapped_addr_ != 0 && size_ > 0) {
    driver_lab_global_register_state_bank(reinterpret_cast<uint8_t*>(mapped_addr_), size_);
    registered_global_ = true;
  }
}

void StateVmoBank::SetState32(uint32_t offset, uint32_t value) const {
  if (mapped_addr_ == 0 || (offset % 4) != 0 || static_cast<size_t>(offset) + 4 > size_) {
    return;
  }
  auto* ptr = reinterpret_cast<uint32_t*>(mapped_addr_ + offset);
  std::atomic_ref<uint32_t>(*ptr).store(value, std::memory_order_seq_cst);
}

uint32_t StateVmoBank::GetState32(uint32_t offset, uint32_t default_value) const {
  if (mapped_addr_ == 0 || (offset % 4) != 0 || static_cast<size_t>(offset) + 4 > size_) {
    return default_value;
  }
  auto* ptr = reinterpret_cast<uint32_t*>(mapped_addr_ + offset);
  return std::atomic_ref<uint32_t>(*ptr).load(std::memory_order_seq_cst);
}

void StateVmoBank::SetKnob32(uint32_t offset, uint32_t value) const { SetState32(offset, value); }

uint32_t StateVmoBank::GetKnob32(uint32_t offset, uint32_t default_value) const {
  return GetState32(offset, default_value);
}

// -----------------------------------------------------------------------------
// EmbeddedServer
// -----------------------------------------------------------------------------

EmbeddedServer::EmbeddedServer(DriverLabServerHandle* handle,
                               std::unique_ptr<fit::function<void(bool)>> quiesce_hook)
    : handle_(handle), quiesce_hook_(std::move(quiesce_hook)) {}

EmbeddedServer::~EmbeddedServer() { Reset(); }

EmbeddedServer::EmbeddedServer(EmbeddedServer&& other) noexcept
    : handle_(std::exchange(other.handle_, nullptr)),
      quiesce_hook_(std::move(other.quiesce_hook_)) {}

EmbeddedServer& EmbeddedServer::operator=(EmbeddedServer&& other) noexcept {
  if (this != &other) {
    Reset();
    handle_ = std::exchange(other.handle_, nullptr);
    quiesce_hook_ = std::move(other.quiesce_hook_);
  }
  return *this;
}

void EmbeddedServer::Reset() {
  if (handle_ != nullptr) {
    driver_lab_server_destroy(handle_);
    handle_ = nullptr;
  }
  quiesce_hook_.reset();
}

bool EmbeddedServer::is_enabled() const { return driver_lab_server_is_enabled(handle_); }

bool EmbeddedServer::is_quiesced() const { return driver_lab_server_is_quiesced(handle_); }

zx::result<> EmbeddedServer::Publish(component::OutgoingDirectory& outgoing,
                                     std::string_view instance) const {
  if (!is_enabled()) {
    return zx::ok();
  }
  DriverLabServerHandle* raw_handle = handle_;
  return outgoing.AddService<fuchsia_driver_lab::Service>(
      fuchsia_driver_lab::Service::InstanceHandler({
          .proxy =
              [raw_handle](fidl::ServerEnd<fuchsia_driver_lab::Proxy> request) {
                if (request.is_valid()) {
                  (void)driver_lab_server_serve_proxy(raw_handle, request.TakeChannel().release());
                }
              },
      }),
      instance);
}

zx::result<> EmbeddedServer::ServeProxy(fidl::ServerEnd<fuchsia_driver_lab::Proxy> request) const {
  if (!request.is_valid()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  zx_status_t status = driver_lab_server_serve_proxy(handle_, request.TakeChannel().release());
  return zx::make_result(status);
}

void EmbeddedServer::NotifyInterrupt(uint32_t resource_id) const {
  driver_lab_server_notify_interrupt(handle_, resource_id);
}

void EmbeddedServer::Stop() const { driver_lab_server_stop(handle_); }

// -----------------------------------------------------------------------------
// Builder
// -----------------------------------------------------------------------------

Builder::Builder(std::string_view node_identity)
    : handle_(driver_lab_builder_new(node_identity.data(), node_identity.size())) {}

Builder::~Builder() {
  if (handle_ != nullptr) {
    driver_lab_builder_destroy(handle_);
    handle_ = nullptr;
  }
}

Builder::Builder(Builder&& other) noexcept
    : handle_(std::exchange(other.handle_, nullptr)),
      quiesce_hook_(std::move(other.quiesce_hook_)) {}

Builder& Builder::operator=(Builder&& other) noexcept {
  if (this != &other) {
    if (handle_ != nullptr) {
      driver_lab_builder_destroy(handle_);
    }
    handle_ = std::exchange(other.handle_, nullptr);
    quiesce_hook_ = std::move(other.quiesce_hook_);
  }
  return *this;
}

Builder& Builder::SetEnabled(bool enabled) {
  driver_lab_builder_with_enabled(handle_, enabled);
  return *this;
}

Builder& Builder::SetAllowMutatingSessions(bool allow) {
  driver_lab_builder_with_allow_mutating_sessions(handle_, allow);
  return *this;
}

zx::result<uint32_t> Builder::AddMmioVmo(std::string_view name, zx::unowned_vmo vmo, size_t offset,
                                         size_t size) {
  if (handle_ == nullptr || !vmo->is_valid() || size == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  uint32_t id = 0;
  zx_status_t status = driver_lab_builder_with_mmio_vmo(handle_, name.data(), name.size(),
                                                        vmo->get(), offset, size, &id);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(id);
}

zx::result<uint32_t> Builder::AddStateVmoBank(std::string_view name, const StateVmoBank& bank,
                                              cpp20::span<const uint64_t> writable_knob_offsets) {
  auto id_res = AddMmioVmo(name, bank.vmo(), 0, bank.size());
  if (id_res.is_error()) {
    return id_res.take_error();
  }
  if (!writable_knob_offsets.empty()) {
    SetWritableRegisters(*id_res, writable_knob_offsets);
  }
  return id_res;
}

Builder& Builder::SetWritableRegisters(uint32_t resource_id, cpp20::span<const uint64_t> offsets) {
  driver_lab_builder_with_writable_registers(handle_, resource_id, offsets.data(), offsets.size());
  return *this;
}

Builder& Builder::SetMaskedWritableRegisters(uint32_t resource_id,
                                             cpp20::span<const MaskedRegister> registers) {
  std::vector<uint64_t> offsets;
  std::vector<uint32_t> masks;
  offsets.reserve(registers.size());
  masks.reserve(registers.size());
  for (const auto& reg : registers) {
    offsets.push_back(reg.offset);
    masks.push_back(reg.allow_mask);
  }
  driver_lab_builder_with_masked_writable_registers(handle_, resource_id, offsets.data(),
                                                    masks.data(), offsets.size());
  return *this;
}

Builder& Builder::SetHardDeniedRanges(uint32_t resource_id, cpp20::span<const ByteRange> ranges) {
  std::vector<uint64_t> starts;
  std::vector<uint64_t> ends;
  starts.reserve(ranges.size());
  ends.reserve(ranges.size());
  for (const auto& range : ranges) {
    starts.push_back(range.start);
    ends.push_back(range.end);
  }
  driver_lab_builder_with_hard_denied_ranges(handle_, resource_id, starts.data(), ends.data(),
                                             ranges.size());
  return *this;
}

uint32_t Builder::AddInterrupt(std::string_view name) {
  return driver_lab_builder_with_interrupt(handle_, name.data(), name.size());
}

void Builder::QuiesceTrampoline(void* context, bool paused) {
  auto* fn = static_cast<fit::function<void(bool)>*>(context);
  if (fn != nullptr && *fn) {
    (*fn)(paused);
  }
}

Builder& Builder::SetQuiesceHook(fit::function<void(bool paused)> hook) {
  quiesce_hook_ = std::make_unique<fit::function<void(bool)>>(std::move(hook));
  driver_lab_builder_with_quiesce_hook(handle_, &Builder::QuiesceTrampoline, quiesce_hook_.get());
  return *this;
}

zx::result<EmbeddedServer> Builder::Build() {
  if (handle_ == nullptr) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  DriverLabBuilderHandle* raw_builder = std::exchange(handle_, nullptr);
  DriverLabServerHandle* server_handle = nullptr;
  zx_status_t status = driver_lab_builder_build(raw_builder, &server_handle);
  if (status != ZX_OK) {
    quiesce_hook_.reset();
    return zx::error(status);
  }
  return zx::ok(EmbeddedServer(server_handle, std::move(quiesce_hook_)));
}

}  // namespace driver_lab
