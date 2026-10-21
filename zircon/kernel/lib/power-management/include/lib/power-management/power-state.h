// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_POWER_STATE_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_POWER_STATE_H_

#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>

#include <atomic>
#include <cstdint>
#include <optional>

#include <fbl/intrusive_single_list.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

#ifdef _KERNEL
#include <kernel/ffi.h>
#endif

#include "energy-model.h"

namespace power_management {

class PowerState;

// Represents a requested power level update to be executed at a later stage.
struct PowerLevelUpdateRequest {
  // Domain ID where the request is originating from.
  uint32_t domain_id;

  // Target ID of the device or domain that should be transitioned.
  uint32_t target_id;

  // Interface this request should be routed to.
  ControlInterface control;

  // Argument used for transitioning through the control interface.
  uint64_t control_argument;

  // Options that determine what `target_id` is. This is determined by the
  // power level being transitioned to.
  uint32_t options;
};

}  // namespace power_management

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
__BEGIN_CDECLS

void rust_power_state_init(power_management::PowerState* state);
void rust_power_state_destroy(power_management::PowerState* state);
bool rust_power_state_is_enabled(const power_management::PowerState* state);
const fbl::RefPtr<power_management::PowerDomain>* rust_power_state_domain(
    const power_management::PowerState* state);
bool rust_power_state_domain_id(const power_management::PowerState* state, uint32_t* out);
bool rust_power_state_is_serving(const power_management::PowerState* state);
bool rust_power_state_scheduler_control_enabled(const power_management::PowerState* state);
bool rust_power_state_active_power_level(const power_management::PowerState* state, uint8_t* out);
bool rust_power_state_desired_active_power_level(const power_management::PowerState* state,
                                                 uint8_t* out);
bool rust_power_state_max_idle_power_level(const power_management::PowerState* state, uint8_t* out);
uint64_t rust_power_state_active_power_coefficient_nw(const power_management::PowerState* state);
uint64_t rust_power_state_max_idle_power_coefficient_nw(const power_management::PowerState* state);
bool rust_power_state_max_idle_power_level_interface(const power_management::PowerState* state,
                                                     power_management::ControlInterface* out);
bool rust_power_state_is_valid_active_power_level(const power_management::PowerState* state,
                                                  uint8_t power_level);
power_management::ProcessingRate rust_power_state_max_processing_rate(
    const power_management::PowerState* state);
power_management::ProcessingRate rust_power_state_active_processing_rate(
    const power_management::PowerState* state);
power_management::ProcessingRate rust_power_state_preceding_active_processing_rate(
    const power_management::PowerState* state);
bool rust_power_state_target_active_power_level(const power_management::PowerState* state,
                                                uint8_t* out);
power_management::ProcessingRate rust_power_state_target_processing_rate(
    const power_management::PowerState* state);
power_management::ProcessingRate rust_power_state_preceding_target_processing_rate(
    const power_management::PowerState* state);
bool rust_power_state_has_preceding_target_power_level(const power_management::PowerState* state);
power_management::Utilization rust_power_state_normalized_utilization(
    const power_management::PowerState* state);
power_management::Utilization rust_power_state_total_normalized_utilization(
    const power_management::PowerState* state);
void rust_power_state_update_power_domain_set(power_management::PowerState* state,
                                              power_management::PowerDomainSet* power_domain_set,
                                              uint32_t cpu_num);
const power_management::PowerDomainSet* rust_power_state_power_domain_set(
    const power_management::PowerState* state);
bool rust_power_state_request_transition(power_management::PowerState* state, uint32_t cpu_num,
                                         uint8_t active_power_level,
                                         power_management::PowerLevelUpdateRequest* out_request);
zx_status_t rust_power_state_update_active_power_level(power_management::PowerState* state,
                                                       uint8_t active_power_level);
power_management::Utilization rust_power_state_update_utilization(
    power_management::PowerState* state, power_management::Utilization utilization_delta);

__END_CDECLS
#pragma clang diagnostic pop

namespace power_management {

// PowerState encapsulates the current power level, power domain, and energy model for an individual
// processor.
//
// Instances of PowerState are not safe for concurrent use and must be protected by an external lock
// associated with the owning processor.
class PowerState {
 public:
  PowerState() { rust_power_state_init(this); }
  ~PowerState() { rust_power_state_destroy(this); }

  PowerState(const PowerState&) = delete;
  PowerState& operator=(const PowerState&) = delete;

  // Returns true if the power domain is set.
  bool is_enabled() const { return rust_power_state_is_enabled(this); }

  // Domain the PowerState is being modeled after. Returns nullptr is there is
  // no power domain configured for processor owning this power state.
  const fbl::RefPtr<PowerDomain>& domain() const { return *rust_power_state_domain(this); }

  // Returns the id of the domain this power state belongs to.
  std::optional<uint32_t> domain_id() const {
    uint32_t id = 0;
    if (rust_power_state_domain_id(this, &id)) {
      return id;
    }
    return std::nullopt;
  }

  // Returns whether the domain's controller is serving requests. Returns false if there is no
  // domain.
  bool is_serving() const { return rust_power_state_is_serving(this); }

  // Returns whether the kernel scheduler should send power level update requests through the
  // control interface to handle utilization changes.
  bool scheduler_control_enabled() const {
    return rust_power_state_scheduler_control_enabled(this);
  }

  // Returns the active power level when device is not idle.
  std::optional<uint8_t> active_power_level() const {
    uint8_t level = 0;
    if (rust_power_state_active_power_level(this, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the active power level the device is in the processo of transitioning to.
  std::optional<uint8_t> desired_active_power_level() const {
    uint8_t level = 0;
    if (rust_power_state_desired_active_power_level(this, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the maximum (i.e. highest power, lowest latency) idle power level.
  std::optional<uint8_t> max_idle_power_level() const {
    uint8_t level = 0;
    if (rust_power_state_max_idle_power_level(this, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the power coefficient of the current active power level. Returns zero if there is no
  // active power level or no domain is set.
  uint64_t active_power_coefficient_nw() const {
    return rust_power_state_active_power_coefficient_nw(this);
  }

  // Returns the power coefficient of the maximum idle power level. Returns zero if there is no
  // idle power level or no domain is set.
  uint64_t max_idle_power_coefficient_nw() const {
    return rust_power_state_max_idle_power_coefficient_nw(this);
  }

  // Returns the control interface of the maximum idle power level. Returns nullopt if there is no
  // idle power level or no domain is set.
  std::optional<ControlInterface> max_idle_power_level_interface() const {
    ControlInterface iface{};
    if (rust_power_state_max_idle_power_level_interface(this, &iface)) {
      return iface;
    }
    return std::nullopt;
  }

  // Returns true if the given power level is valid active power level.
  bool IsValidActivePowerLevel(uint8_t power_level) const {
    return rust_power_state_is_valid_active_power_level(this, power_level);
  }

  // Returns the processing rate of the fastest power level in this domain.
  ProcessingRate max_processing_rate() const { return rust_power_state_max_processing_rate(this); }

  // Returns the processing rate of the current active power level. Returns zero if there is no
  // active power level or no domain is set.
  ProcessingRate active_processing_rate() const {
    return rust_power_state_active_processing_rate(this);
  }

  // Returns the processing rate of the preceding active power level (i.e. the
  // lower bound of the current active power level). Returns zero if there is no
  // preceding active power level, no active power level is set, or no domain
  // is set.
  ProcessingRate preceding_active_processing_rate() const {
    return rust_power_state_preceding_active_processing_rate(this);
  }

  // Returns the targeted active power level (the in-flight desired level if set,
  // otherwise the current active level).
  std::optional<uint8_t> target_active_power_level() const {
    uint8_t level = 0;
    if (rust_power_state_target_active_power_level(this, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the processing rate of the targeted active power level (the in-flight
  // desired level if set, otherwise the current active level).
  ProcessingRate target_processing_rate() const {
    return rust_power_state_target_processing_rate(this);
  }

  // Returns the processing rate of the active power level immediately preceding
  // the targeted active power level (i.e. the lower bound of the targeted active
  // power level).
  ProcessingRate preceding_target_processing_rate() const {
    return rust_power_state_preceding_target_processing_rate(this);
  }

  // Returns true if the targeted active power level has an active power level
  // preceding it (i.e. is not the lowest active power level).
  bool has_preceding_target_power_level() const {
    return rust_power_state_has_preceding_target_power_level(this);
  }

  // Returns the current utilization of the processor.
  Utilization normalized_utilization() const {
    return rust_power_state_normalized_utilization(this);
  }

  // Returns the total normalized utilization of the domain this processor belongs to.
  Utilization total_normalized_utilization() const {
    return rust_power_state_total_normalized_utilization(this);
  }

  // Updates the power domain set and associates this power state with the domain in the set
  // matching the given CPU number. Returns the previous power domain set.
  PowerDomainSet UpdatePowerDomainSet(PowerDomainSet power_domain_set, uint32_t cpu_num) {
    rust_power_state_update_power_domain_set(this, &power_domain_set, cpu_num);
    return power_domain_set;
  }

  const PowerDomainSet& power_domain_set() const {
    return *rust_power_state_power_domain_set(this);
  }

  // Posts a request to transition the power domain associated with this power state to a given
  // active power level from the current energy model.
  //
  // Ignored when no domain is set. Asserts that the requested power level is an active power level.
  //
  // `cpu_num` is the CPU associated with this power state.
  // `level` is the desired power level.
  std::optional<PowerLevelUpdateRequest> RequestTransition(uint32_t cpu_num,
                                                           uint8_t active_power_level) {
    PowerLevelUpdateRequest request{};
    if (rust_power_state_request_transition(this, cpu_num, active_power_level, &request)) {
      return request;
    }
    return std::nullopt;
  }

  // Updates the active power level of this power state. Typically called in response to the power
  // level controller completing a power level transition request.
  zx::result<> UpdateActivePowerLevel(uint8_t active_power_level) {
    return zx::make_result(rust_power_state_update_active_power_level(this, active_power_level));
  }

  // Update the utilization of this processor and the total utilization of its domain.
  Utilization UpdateUtilization(Utilization utilization_delta) {
    return rust_power_state_update_utilization(this, utilization_delta);
  }

 private:
  static constexpr size_t kStorageSize = 56;
#ifdef _KERNEL
  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_;
#else
  [[maybe_unused]] alignas(8) uint8_t storage_[kStorageSize];
#endif
};

static_assert(sizeof(PowerState) == 56);
static_assert(alignof(PowerState) == 8);

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_POWER_STATE_H_
