// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_H_
#define SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_H_

#include <fidl/fuchsia.driver.lab/cpp/wire.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver_lab/driver_lab_c.h>
#include <lib/fit/function.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <cstddef>
#include <cstdint>
#include <initializer_list>
#include <memory>
#include <string_view>

namespace driver_lab {

// Half-open byte offset range `[start, end)` for hard-denied MMIO regions.
struct ByteRange {
  uint64_t start;
  uint64_t end;
};

// A 32-bit writable register whose host-writable bits are limited to
// `allow_mask`.
struct MaskedRegister {
  uint64_t offset;
  uint32_t allow_mask;
};

// Shared-memory VMO bank for exposing internal driver state/telemetry and
// host-tunable fault-injection knobs over `fuchsia.driver.lab` without custom
// FIDL changes (Spec Phase 2 Section 3.2).
//
// Also supports optional registration with the process-wide C singleton
// (`driver_lab_global_set_state_u32` / `driver_lab_global_get_knob_u32`) so
// legacy `.c` translation units can read knobs and write telemetry in one line
// of C without plumbing C++ pointers through vendor structs.
class StateVmoBank final {
 public:
  static constexpr size_t kDefaultSize = 4096;

  // Allocates a zero-initialized `zx::vmo` of `size` bytes, maps it read/write
  // into the current process root VMAR, and optionally registers it as the
  // process-wide global C state bank.
  static zx::result<StateVmoBank> Create(size_t size = kDefaultSize, bool register_global = false);

  StateVmoBank() = default;
  ~StateVmoBank();

  StateVmoBank(StateVmoBank&& other) noexcept;
  StateVmoBank& operator=(StateVmoBank&& other) noexcept;

  StateVmoBank(const StateVmoBank&) = delete;
  StateVmoBank& operator=(const StateVmoBank&) = delete;

  // Atomically writes a 32-bit telemetry/state word at `offset` (must be
  // 4-byte aligned and within `[0, size() - 4]`).
  void SetState32(uint32_t offset, uint32_t value) const;

  // Atomically reads a 32-bit telemetry/state word at `offset`.
  uint32_t GetState32(uint32_t offset, uint32_t default_value = 0) const;

  // Atomically writes a 32-bit knob word at `offset`.
  void SetKnob32(uint32_t offset, uint32_t value) const;

  // Atomically reads a 32-bit host-tunable knob word at `offset`.
  uint32_t GetKnob32(uint32_t offset, uint32_t default_value = 0) const;

  // Registers this bank's mapped buffer with `driver_lab_global_*` C helpers.
  void RegisterGlobal();

  zx::unowned_vmo vmo() const { return vmo_.borrow(); }
  size_t size() const { return size_; }
  uint8_t* data() const { return reinterpret_cast<uint8_t*>(mapped_addr_); }

 private:
  StateVmoBank(zx::vmo vmo, zx_vaddr_t mapped_addr, size_t size, bool registered_global);
  void Reset();

  zx::vmo vmo_;
  zx_vaddr_t mapped_addr_ = 0;
  size_t size_ = 0;
  bool registered_global_ = false;
};

// RAII handle to a running embedded `fuchsia.driver.lab` server and its
// background FIDL executor thread.
class EmbeddedServer final {
 public:
  EmbeddedServer() = default;
  ~EmbeddedServer();

  EmbeddedServer(EmbeddedServer&& other) noexcept;
  EmbeddedServer& operator=(EmbeddedServer&& other) noexcept;

  EmbeddedServer(const EmbeddedServer&) = delete;
  EmbeddedServer& operator=(const EmbeddedServer&) = delete;

  // Returns whether the embedded server is enabled.
  bool is_enabled() const;

  // Returns whether the driver is currently quiesced by an active mutating
  // host session.
  bool is_quiesced() const;

  // Publishes `fuchsia.driver.lab.Service` onto a `component::OutgoingDirectory`.
  // When disabled, this is a no-op and returns `zx::ok()`.
  zx::result<> Publish(
      component::OutgoingDirectory& outgoing,
      std::string_view instance = component::OutgoingDirectory::kDefaultServiceInstance) const;

  // Publishes `fuchsia.driver.lab.Service` onto a DFv2 `fdf::OutgoingDirectory`
  // (or any outgoing directory wrapper providing `AddService<Service>(...)`).
  // When disabled, this is a no-op and returns `zx::ok()`.
  template <typename OutgoingDir>
  zx::result<> Publish(
      OutgoingDir& outgoing,
      std::string_view instance = component::OutgoingDirectory::kDefaultServiceInstance) const {
    if (!is_enabled()) {
      return zx::ok();
    }
    DriverLabServerHandle* raw_handle = handle_;
    return outgoing.template AddService<fuchsia_driver_lab::Service>(
        fuchsia_driver_lab::Service::InstanceHandler({
            .proxy =
                [raw_handle](fidl::ServerEnd<fuchsia_driver_lab::Proxy> request) {
                  if (request.is_valid()) {
                    (void)driver_lab_server_serve_proxy(raw_handle,
                                                        request.TakeChannel().release());
                  }
                },
        }),
        instance);
  }

  // Serves a single `fuchsia.driver.lab/Proxy` server-end channel on the
  // embedded server's background executor.
  zx::result<> ServeProxy(fidl::ServerEnd<fuchsia_driver_lab::Proxy> request) const;

  // Taps an interrupt event from the driver's interrupt handler.
  void NotifyInterrupt(uint32_t resource_id) const;

  // Stops accepting new sessions, releases any active mutation lease and
  // quiesce hook, and cancels pending interrupt waiters.
  void Stop() const;

 private:
  friend class Builder;

  EmbeddedServer(DriverLabServerHandle* handle,
                 std::unique_ptr<fit::function<void(bool)>> quiesce_hook);
  void Reset();

  DriverLabServerHandle* handle_ = nullptr;
  std::unique_ptr<fit::function<void(bool)>> quiesce_hook_;
};

// Builder for configuring and instantiating an `EmbeddedServer` inside a C++
// DFv2 driver.
class Builder final {
 public:
  explicit Builder(std::string_view node_identity);
  ~Builder();

  Builder(Builder&& other) noexcept;
  Builder& operator=(Builder&& other) noexcept;

  Builder(const Builder&) = delete;
  Builder& operator=(const Builder&) = delete;

  // Explicitly enables or disables the embedded server.
  Builder& SetEnabled(bool enabled);

  // Configures whether mutating sessions are permitted on this embedded server.
  Builder& SetAllowMutatingSessions(bool allow);

  // Shares a driver MMIO VMO with the embedded server. `vmo` is duplicated
  // internally with `ZX_RIGHT_SAME_RIGHTS`; the caller retains ownership.
  zx::result<uint32_t> AddMmioVmo(std::string_view name, zx::unowned_vmo vmo, size_t offset,
                                  size_t size);

  // Convenience overload for `fdf::MmioBuffer` / `fdf::MmioView`-shaped types
  // exposing `get_vmo()`, `get_offset()`, and `get_size()`.
  template <typename MmioLike>
  zx::result<uint32_t> AddMmioBuffer(std::string_view name, const MmioLike& mmio) {
    return AddMmioVmo(name, mmio.get_vmo(), mmio.get_offset(), mmio.get_size());
  }

  // Registers a `StateVmoBank` and optionally marks 32-bit knob offsets as
  // host-writable under mutating sessions.
  zx::result<uint32_t> AddStateVmoBank(std::string_view name, const StateVmoBank& bank,
                                       cpp20::span<const uint64_t> writable_knob_offsets = {});

  zx::result<uint32_t> AddStateVmoBank(std::string_view name, const StateVmoBank& bank,
                                       std::initializer_list<uint64_t> writable_knob_offsets) {
    return AddStateVmoBank(
        name, bank,
        cpp20::span<const uint64_t>(writable_knob_offsets.begin(), writable_knob_offsets.size()));
  }

  // Configures the permitted 32-bit writable register offsets on `resource_id`.
  Builder& SetWritableRegisters(uint32_t resource_id, cpp20::span<const uint64_t> offsets);

  Builder& SetWritableRegisters(uint32_t resource_id, std::initializer_list<uint64_t> offsets) {
    return SetWritableRegisters(resource_id,
                                cpp20::span<const uint64_t>(offsets.begin(), offsets.size()));
  }

  // Configures 32-bit writable registers on `resource_id` that each permit
  // host changes only to the bits in `allow_mask`. Use this for control
  // registers that also hold reset or run/stop bits.
  //
  // Like `SetWritableRegisters()`, this replaces any writable registers
  // previously configured on `resource_id`. To mix full-width and masked
  // registers on one resource, give the full-width ones a mask of
  // `0xffffffff`.
  Builder& SetMaskedWritableRegisters(uint32_t resource_id,
                                      cpp20::span<const MaskedRegister> registers);

  Builder& SetMaskedWritableRegisters(uint32_t resource_id,
                                      std::initializer_list<MaskedRegister> registers) {
    return SetMaskedWritableRegisters(
        resource_id, cpp20::span<const MaskedRegister>(registers.begin(), registers.size()));
  }

  // Configures the hard-denied byte ranges (`[start, end)`) on `resource_id`.
  Builder& SetHardDeniedRanges(uint32_t resource_id, cpp20::span<const ByteRange> ranges);

  Builder& SetHardDeniedRanges(uint32_t resource_id, std::initializer_list<ByteRange> ranges) {
    return SetHardDeniedRanges(resource_id,
                               cpp20::span<const ByteRange>(ranges.begin(), ranges.size()));
  }

  // Registers a named interrupt resource for event tapping and returns its
  // assigned resource ID.
  uint32_t AddInterrupt(std::string_view name);

  // Registers a synchronous quiesce hook invoked when a mutating host session
  // acquires (`paused = true`) or releases (`paused = false`) the mutation
  // lease.
  Builder& SetQuiesceHook(fit::function<void(bool paused)> hook);

  // Validates registered resources and builds the `EmbeddedServer`.
  zx::result<EmbeddedServer> Build();

 private:
  static void QuiesceTrampoline(void* context, bool paused);

  DriverLabBuilderHandle* handle_ = nullptr;
  std::unique_ptr<fit::function<void(bool)>> quiesce_hook_;
};

}  // namespace driver_lab

#endif  // SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_H_
