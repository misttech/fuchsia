// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_

#include <lib/fit/function.h>
#include <lib/zx/result.h>
#include <stdint.h>
#include <stdlib.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/syscalls-next.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <limits>
#include <span>
#include <string_view>
#include <utility>

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <fbl/vector.h>
#include <ffl/fixed.h>

#include "power-level-controller.h"

namespace power_management {

// forward declaration.
class EnergyModel;

// Enum representing supported control interfaces.
enum class ControlInterface : uint64_t {
  kCpuDriver = ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER,
  kArmPsci = ZX_PROCESSOR_POWER_CONTROL_ARM_PSCI,
  kArmWfi = ZX_PROCESSOR_POWER_CONTROL_ARM_WFI,
  kRiscvSbi = ZX_PROCESSOR_POWER_CONTROL_RISCV_SBI,
  kRiscvWfi = ZX_PROCESSOR_POWER_CONTROL_RISCV_WFI,
};

constexpr const char* ToString(ControlInterface control_interface) {
  switch (control_interface) {
    case ControlInterface::kCpuDriver:
      return "CPU_DRIVER";
    case ControlInterface::kArmPsci:
      return "ARM_PSCI";
    case ControlInterface::kArmWfi:
      return "ARM_WFI";
    case ControlInterface::kRiscvSbi:
      return "RISCV_SBI";
    case ControlInterface::kRiscvWfi:
      return "RISCV_WFI";
    default:
      return "[unknown]";
  }
}

// List of support control interfaces.
static constexpr auto kSupportedControlInterfaces = std::to_array(
    {ControlInterface::kArmPsci, ControlInterface::kArmWfi, ControlInterface::kRiscvSbi,
     ControlInterface::kRiscvWfi, ControlInterface::kCpuDriver});

// Returns whether the interface is a supported or not.
constexpr bool IsSupportedControlInterface(zx_processor_power_control_t interface) {
  for (auto supported_interface : kSupportedControlInterfaces) {
    if (supported_interface == static_cast<ControlInterface>(interface)) {
      return true;
    }
  }
  return false;
}

// Returns whether the interface is handled by the kernel or not.
constexpr bool IsKernelControlInterface(ControlInterface interface) {
  return interface != ControlInterface::kCpuDriver;
}

// The normalized processing rate of a CPU, relative to the fastest CPU in the system.
using ProcessingRate = ffl::Fixed<int64_t, 31>;

// The normalized utilization of a CPU by a task or set of tasks.
using Utilization = ffl::Fixed<int64_t, 31>;

// Kernel representation of `zx_processor_power_level_t` with useful accessors and option support.
class PowerLevel {
 public:
  enum Type : bool {
    // Entity is not eligible for active work.
    kIdle,

    // Entity is eligible for work, but the rate at which work is completed is determined by the
    // active power level.
    kActive,
  };

  constexpr PowerLevel() = default;
  explicit PowerLevel(uint8_t level_index, const zx_processor_power_level_t& level)
      : options_(level.options),
        control_(static_cast<ControlInterface>(level.control_interface)),
        control_argument_(level.control_argument),
        processing_rate_(ffl::FromRatio<uint64_t>(
            level.processing_rate, 1000)),  // TODO(eieio): Normalize relative to the max processing
                                            // rate of all power levels.
        power_coefficient_nw_(level.power_coefficient_nw),
        power_cost_nw_per_rate_(level.processing_rate > 0
                                    ? level.power_coefficient_nw * 1000 / level.processing_rate
                                    : 0),
        level_(level_index) {
    memcpy(name_.data(), level.diagnostic_name, name_.size());
    size_t end = std::string_view(name_.data(), name_.size()).find('\0');
    name_len_ = end == std::string_view::npos ? ZX_MAX_NAME_LEN : end;
  }

  // Power level type. Idle and Active power levels are orthogonal, that is, an entity may be idle
  // while keepings its active power level unchanged. This means that the actual power level of
  // an entity should be determined by the tuple <Idle Power Level*, Active Power Level>, where if
  // `Idle Power Level`is absent then that means that the entity is active and the active power
  // level should be used.
  //
  // This situation happens for example, when a CPU transitions from an active power level A
  // (which may be interpreted as a known OPP or P-State) into an idle state, such as suspension,
  // idle thread or even powering it off.
  constexpr Type type() const { return processing_rate_ == 0 ? Type::kIdle : kActive; }

  // Processing rate when this power level is active. This is key to determining the available
  // bandwidth of the entity.
  constexpr ProcessingRate processing_rate() const { return processing_rate_; }

  // Relative to the system power consumption, determines how much power is being consumed at this
  // level. This allows determining if this power level should be a candidate when operating under
  // a given energy budget.
  constexpr uint64_t power_coefficient_nw() const { return power_coefficient_nw_; }

  // Power cost of this power level, normalized by the rate of this power level.
  constexpr uint64_t power_cost_nw_per_rate() const { return power_cost_nw_per_rate_; }

  // ID of the interface handling transitions for TO this power level.
  constexpr ControlInterface control() const { return control_; }

  // Argument to be interpreted by the control interface in order to transition to this level.
  //
  // The control interface is only aware of this arguments, and power levels are identified by
  // this argument.
  constexpr uint64_t control_argument() const { return control_argument_; }

  // This level may be transitioned in a per cpu basis, without affecting other entities in the
  // same power domain.
  constexpr bool TargetsCpus() const {
    return (options_ & ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) != 0;
  }

  // This level may be transitioned in a per power domain basis, that is, all other entities in
  // the power domain will be transitioned together.
  //
  // This means that underlying hardware elements are share and it is not possible to transition a
  // single member of the power domain.
  constexpr bool TargetsPowerDomain() const {
    return (options_ & ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) == 0;
  }

  // Name used to identify this power level, for diagnostic purposes.
  constexpr std::string_view name() const { return {name_.data(), name_len_}; }

  // Power Level as understood from the original model perspective.
  constexpr uint8_t level() const { return level_; }

 private:
  // Options.
  zx_processor_power_level_options_t options_{};

  // Control interface used to transition to this level.
  ControlInterface control_{};

  // Argument to be provided to the control interface.
  uint64_t control_argument_{0};

  // Processing rate.
  ProcessingRate processing_rate_{0};

  // Power coefficient in nanowatts.
  uint64_t power_coefficient_nw_{0};

  // Power cost. Memoized value of power_coefficient_nw_ / processing_rate_;
  uint64_t power_cost_nw_per_rate_{0};

  std::array<char, ZX_MAX_NAME_LEN> name_{};
  size_t name_len_{0};

  // Level as described in the model shared with user.
  uint8_t level_{0};
  [[maybe_unused]] std::array<uint8_t, 7> reserved_{};
};

// Represents an entry in a transition matrix, where the position in the matrix denotes
// the source and target power level. This constructs just denotes the properties of that
// cell.
class PowerLevelTransition {
 public:
  // Returns an invalid transition.
  static constexpr PowerLevelTransition Invalid() { return {}; }
  static constexpr PowerLevelTransition Zero() {
    return PowerLevelTransition(
        zx_processor_power_level_transition_t{.latency = 0, .energy_nj = 0});
  }

  constexpr PowerLevelTransition() = default;
  explicit constexpr PowerLevelTransition(const zx_processor_power_level_transition_t& transition)
      : latency_(zx_duration_from_nsec(transition.latency)),
        energy_cost_nj_(transition.energy_nj) {}

  // Latency for transitioning from a given level to another.
  constexpr zx_duration_mono_t latency() const { return latency_; }

  // Energy cost in nano joules(nj) for transition from a given level to another.
  constexpr uint64_t energy_cost_nj() const { return energy_cost_nj_; }

  // Whether the transition is valid or not.
  explicit constexpr operator bool() {
    return latency_ != Invalid().latency() && energy_cost_nj_ != Invalid().energy_cost_nj_;
  }

 private:
  // Time required for the transition to take effect. In some cases it may mean for the actual
  // voltage to stabilize.
  zx_duration_mono_t latency_ = ZX_TIME_INFINITE;

  // Amount of energy consumed to perform the transition.
  uint64_t energy_cost_nj_ = std::numeric_limits<uint64_t>::max();
};

// Represents a view of the `zx_processor_power_level_transition_t` array as
// a matrix. As per a view's concept, this view is only valid so long the original
// object remains valid and it's tied to its lifecycle.
//
// Additionally transition matrix are required to be squared matrixes, since
// they describe transition from every existent level to every other level.
struct TransitionMatrix {
 public:
  constexpr TransitionMatrix(const TransitionMatrix& other) = default;

  constexpr std::span<const PowerLevelTransition> operator[](size_t index) const {
    return transitions_.subspan(index * num_rows_, num_rows_);
  }

 private:
  friend EnergyModel;
  TransitionMatrix(std::span<const PowerLevelTransition> transitions, size_t num_rows)
      : transitions_(transitions), num_rows_(num_rows) {
    ZX_DEBUG_ASSERT(transitions_.size() != 0);
    ZX_DEBUG_ASSERT(num_rows_ != 0);
    ZX_DEBUG_ASSERT(transitions_.size() % num_rows_ == 0);
    ZX_DEBUG_ASSERT(transitions.size() / num_rows_ == num_rows_);
  }

  const std::span<const PowerLevelTransition> transitions_;
  const size_t num_rows_;
};

// EnergyModel describes the power consumption rates of the available active and
// idle states a processor may enter, which interfaces to use to effect state
// transitions, and properties of the state transitions, such as energy cost and
// latency. This information is used to make efficient scheduling and load
// balancing decisions that meet the power vs. performance tradeoffs specified
// by a product.
//
// An energy model is constant once initialized. Updating the active energy
// model requires replacing it with a new instance and incrementing a generation
// count to detect stale values derived from the previous energy model.
class EnergyModel {
 public:
  static zx::result<EnergyModel> Create(
      std::span<const zx_processor_power_level_t> levels,
      std::span<const zx_processor_power_level_transition_t> transitions);

  EnergyModel() = default;
  EnergyModel(const EnergyModel&) = delete;
  EnergyModel(EnergyModel&&) = default;

  // All power levels described in the model, sorted by processing power and energy consumption.
  //
  // (1) The processing rate of power level i is less or equal than the processing rate of power
  // level j, where i <= j.
  //
  // (2) The energy cost of power level i is less or equal than the processing rate of power level
  // j, where i <= j.
  constexpr std::span<const PowerLevel> levels() const { return power_levels_; }

  // Following the same rules as `levels()` but returns only the set of power levels whose type is
  // `PowerLevel::Type::kIdle`. This set may be empty.
  constexpr std::span<const PowerLevel> idle_levels() const {
    return levels().subspan(0, idle_power_levels_);
  }

  // Returns the idle power level with the maximum power consumption.
  constexpr std::optional<uint8_t> max_idle_power_level() const {
    if (idle_power_levels_ > 0) {
      return idle_power_levels_ - 1;
    }
    return std::nullopt;
  }

  // Returns the power coefficient of the idle power level with the maximum power consumption. This
  // idle power level typically corresponds to clock gating, such that the power consumption is
  // almost entirely leakage power loss.
  constexpr std::optional<uint64_t> max_idle_power_coefficient_nw() const {
    if (idle_power_levels_ > 0) {
      return power_levels_[idle_power_levels_ - 1].power_coefficient_nw();
    }
    return std::nullopt;
  }

  // Returns the control interface of the idle power level with the maximum power consumption.
  constexpr std::optional<ControlInterface> max_idle_power_level_interface() const {
    if (idle_power_levels_ > 0) {
      return power_levels_[idle_power_levels_ - 1].control();
    }
    return std::nullopt;
  }

  // Returns the processing rate of the fastest power level.
  constexpr ProcessingRate max_processing_rate() const {
    return levels().size() > 0 ? levels().back().processing_rate() : ProcessingRate{0};
  }

  // Following the same rules as `levels()` but returns only the set of power levels whose type is
  // `PowerLevel::Type::kActive`. This set may be empty.
  constexpr std::span<const PowerLevel> active_levels() const {
    return levels().subspan(idle_power_levels_);
  }

  // Returns a transition matrix, where the entry <i,j> represents the transition costs for
  // transitioning from i to j.
  TransitionMatrix transitions() const {
    return TransitionMatrix(transitions_, power_levels_.size());
  }

  std::optional<uint8_t> FindPowerLevel(ControlInterface interface_id,
                                        uint64_t control_argument) const;

  // Returns the lowest active power level that is compatible with (i.e. has a
  // processing rate greater or equal to) the given processing rate. However, to
  // simplify power calculations, the maximum active power level is returned if
  // the given rate exceeds the maximum processing rate for the power domain.
  const PowerLevel* FindActivePowerLevelForRate(ProcessingRate processing_rate) const;

 private:
  EnergyModel(fbl::Vector<PowerLevel> levels, fbl::Vector<PowerLevelTransition> transitions,
              fbl::Vector<size_t> control_lookup, size_t idle_levels)
      : power_levels_(std::move(levels)),
        transitions_(std::move(transitions)),
        control_lookup_(std::move(control_lookup)),
        idle_power_levels_(idle_levels) {}

  fbl::Vector<PowerLevel> power_levels_;
  fbl::Vector<PowerLevelTransition> transitions_;
  fbl::Vector<size_t> control_lookup_;
  size_t idle_power_levels_ = 0;
};

// PowerDomain establishes the relationship between a set of CPUs, the energy
// model that describes their characteristics, and the power level controller
// responsible for changing the active power levels for the set of CPUs.
//
// Instances of PowerDomain are safe for concurrent use.
class PowerDomain : public fbl::RefCounted<PowerDomain> {
 public:
  PowerDomain(uint32_t id, zx_cpu_set_t cpus, EnergyModel model)
      : PowerDomain(id, cpus, std::move(model), nullptr) {}
  PowerDomain(uint32_t id, zx_cpu_set_t cpus, EnergyModel model,
              fbl::RefPtr<PowerLevelController> controller)
      : cpus_(cpus), id_(id), energy_model_(std::move(model)), controller_(std::move(controller)) {}

  // ID representing the relationship between a set of CPUs and a power model.
  constexpr uint32_t id() const { return id_; }

  // Set of CPUs associated with `model()`.
  constexpr const zx_cpu_set_t& cpus() const { return cpus_; }

  // Model describing the behavior of the power domain.
  constexpr const EnergyModel& model() const { return energy_model_; }

  // The total normalized utilization of the set of processors associated with this power domain.
  //
  // Uses relaxed semantics, since the value does not need to synchronize with other memory accesses
  // and innaccuracy is acceptable.
  Utilization total_normalized_utilization() const {
    return Utilization::FromRaw(total_normalized_utilization_.load(std::memory_order_relaxed));
  }

  // Handler for transitions where the target level's control interface is not kernel handled.
  const fbl::RefPtr<PowerLevelController>& controller() const { return controller_; }

  // Returns whether the kernel scheduler should send power level update requests to the controller.
  // This is used by tests to prevent the scheduler from sending power level change requests through
  // the fake control interface that could confuse the test. It does not prevent the kernel from
  // exercising the control interface, however, only whether the scheduler will interact with the
  // control interface to handle utilization changes.
  //
  // Uses relaxed semantics, since this variable will generally be synchronized by the lock
  // protecting each PowerState as it is associated with this PowerDomain. However, an atomic is
  // used to prevent formal data races if the value is read outside of external synchronization,
  // which can't be statically checked internally.
  bool scheduler_control_enabled() const {
    return scheduler_control_enabled_.load(std::memory_order_relaxed);
  }

  // Sets whether the kernel scheduler should send power level update requests through the control
  // interface. When set to false, the scheduler must not send requests to avoid confusing tests.
  void SetSchedulerControlEnabled(bool enabled) {
    scheduler_control_enabled_.store(enabled, std::memory_order_relaxed);
  }

 private:
  friend class PowerState;

  const zx_cpu_set_t cpus_;
  const uint32_t id_;
  const EnergyModel energy_model_;

  // Although this value is exposed publicly with the Utilization type, it needs to be atomically
  // updated via fetch_add, which only is only supported for fundamental types.
  std::atomic<int64_t> total_normalized_utilization_{0};

  const fbl::RefPtr<PowerLevelController> controller_ = nullptr;

  std::atomic<bool> scheduler_control_enabled_ = false;
};

// Maintains an array of ref pointers to the power domains in the system. This is used for both the
// power domain registry storage and processor-local caches of the power domain set. Processor-local
// caches avoid cross-processor and central lock contention when consulting a snapshot of the system
// power domains during sensitive scheduling operations.
class PowerDomainSet {
 public:
  static constexpr size_t kMaxPowerDomains = 4;

  using ArrayType = std::array<fbl::RefPtr<PowerDomain>, kMaxPowerDomains>;

  // Empty by default.
  PowerDomainSet() = default;
  ~PowerDomainSet() = default;

  // Creates a PowerDomainSet with the given PowerDomain as its only entry for testing.
  static PowerDomainSet CreateForTest(const fbl::RefPtr<PowerDomain>& domain) {
    return PowerDomainSet{{domain}};
  }

  // Returns a borrowed pointer to the power domain with the given id, or nullptr if there isn't
  // one. Returns a raw pointer to avoid unnecessary ref count changes in contexts where the set is
  // guaranteed not to change and maintain the lifetime of its power domain elements.
  PowerDomain* FindByDomainId(uint32_t domain_id) const {
    for (const auto& element : domains_) {
      if (element && element->id() == domain_id) {
        return element.get();
      }
    }
    return nullptr;
  }

  // Returns a borrowed pointer to the power domain for the given CPU id, or nullptr is there isn't
  // one. Returns a raw pointer to avoid unnecessary ref count changes in contexts where the set is
  // guaranteed not to change and maintain the lifetime of its power domain elements.
  PowerDomain* FindByCpuNum(uint32_t cpu_num) const {
    ZX_DEBUG_ASSERT(cpu_num < ZX_CPU_SET_MAX_CPUS);
    const uint32_t mask_index = cpu_num / ZX_CPU_SET_BITS_PER_WORD;
    const uint64_t mask_bit = uint64_t{1} << (cpu_num % ZX_CPU_SET_BITS_PER_WORD);
    for (const auto& element : domains_) {
      if (element && (element->cpus().mask[mask_index] & mask_bit)) {
        return element.get();
      }
    }
    return nullptr;
  }

  // Looks up the active power coefficient for the given CPU operating at the given processing rate.
  // Returns 0 if there is no power domain for the given CPU or if there is no active power level
  // that lower bounds the given processing rate.
  uint64_t LookupActivePowerCoefficient(uint32_t cpu_num, ProcessingRate processing_rate) const {
    if (const PowerDomain* power_domain = FindByCpuNum(cpu_num)) {
      if (const PowerLevel* power_level =
              power_domain->model().FindActivePowerLevelForRate(processing_rate)) {
        return power_level->power_coefficient_nw();
      }
    }
    return 0u;
  }

  uint64_t LookupPowerCost(uint32_t cpu_num, ProcessingRate processing_rate) const {
    if (const PowerDomain* power_domain = FindByCpuNum(cpu_num)) {
      if (const PowerLevel* power_level =
              power_domain->model().FindActivePowerLevelForRate(processing_rate)) {
        return power_level->power_cost_nw_per_rate();
      }
    }
    return 0u;
  }

  Utilization LookupTotalNormalizedUtilization(uint32_t cpu_num) const {
    if (const PowerDomain* power_domain = FindByCpuNum(cpu_num)) {
      return power_domain->total_normalized_utilization();
    }
    return Utilization{0};
  }

  // Visits each non-empty power domain element with the given callable.
  template <typename Visitor>
  void Visit(Visitor&& visitor) const {
    for (const auto& element : domains_) {
      if (element) {
        visitor(element);
      }
    }
  }

  // Swaps the elements of the given power domain sets.
  friend constexpr void swap(PowerDomainSet& a, PowerDomainSet& b) {
    std::swap(a.domains_, b.domains_);
  }

  // Returns a const reference to the underlying power domain array.
  constexpr const ArrayType& domains() const { return domains_; }

  // Returns the count of non-empty array elements.
  constexpr size_t count() const {
    return std::ranges::count_if(domains_.begin(), domains_.end(),
                                 [](const auto& element) { return bool{element}; });
  }

  // Returns true if all of the array elements are empty.
  constexpr bool is_empty() const { return count() == 0u; }

 private:
  // Allow PowerDomainRegistry to add and remove power domains from the set.
  friend class PowerDomainRegistry;

  // Private constructor used by the testing named constructors.
  explicit PowerDomainSet(ArrayType domains) : domains_{std::move(domains)} {}

  zx::result<fbl::RefPtr<PowerDomain>> Update(const fbl::RefPtr<PowerDomain>& power_domain) {
    // A power domain consists of a domain id, a CPU mask, and a list of power
    // level descriptions. Each power domain in a power domain set must have
    // domain id that is unique among the power domains in the set and a CPU
    // mask that does not intersect with any other power domains in the set.
    //
    // The following updates to the set are permitted:
    // 1. A new power domain is added with a unique domain id and
    //    CPU set that is non-intersecting with other power domains.
    // 2. A power domain is replaced by a new power domain with the same id and
    //    a potentially different CPU set, provided the CPU set is
    //    non-intersecting with other power domains.
    // 3. A power domain is replaced by a new power domain with an identical CPU
    //    set and a potentially different domain id, provide the domain id is
    //    unique among power domains in the set.
    //
    fbl::RefPtr<PowerDomain>* element_to_update = nullptr;
    fbl::RefPtr<PowerDomain>* first_empty_elemnt = nullptr;
    for (auto& element : domains_) {
      if (element) {
        if (element->id() == power_domain->id() ||
            HasSameCpuSet(element->cpus().mask, power_domain->cpus().mask)) {
          // If an element with a matching domain id and/or CPU set was already
          // found, then this update is attempting to make the power domain set
          // inconsistent by replicating either the domain id or the CPU set of
          // a another power domain.
          if (element_to_update) {
            return zx::error(ZX_ERR_INVALID_ARGS);
          }

          // Make note of the element to update with the new power domain, but
          // continue to check the rest of the power domains for intersection
          // with the CPU set.
          element_to_update = &element;
        } else if (HasOverlappingCpuSet(element->cpus().mask, power_domain->cpus().mask)) {
          return zx::error(ZX_ERR_INVALID_ARGS);
        }
      } else if (first_empty_elemnt == nullptr) {
        first_empty_elemnt = &element;
      }
    }

    // Replace an existing domain if a suitable match is found in the array.
    if (element_to_update) {
      fbl::RefPtr<PowerDomain> previous_element = power_domain;
      element_to_update->swap(previous_element);
      return zx::ok(std::move(previous_element));
    }

    // If no existing domain was replaced and there is at least one empty
    // element, add the new domain first empty element.
    if (first_empty_elemnt) {
      *first_empty_elemnt = power_domain;
      return zx::ok(nullptr);
    }

    return zx::error(ZX_ERR_NO_SPACE);
  }

  fbl::RefPtr<PowerDomain> Remove(uint32_t domain_id) {
    for (auto& element : domains_) {
      if (element && element->id() == domain_id) {
        return std::move(element);
      }
    }
    return nullptr;
  }

  template <size_t N>
  static constexpr bool HasOverlappingCpuSet(const uint64_t (&a)[N], const uint64_t (&b)[N]) {
    for (size_t i = 0; i < N; ++i) {
      if ((a[i] & b[i]) != 0) {
        return true;
      }
    }
    return false;
  }

  template <size_t N>
  static constexpr bool HasSameCpuSet(const uint64_t (&a)[N], const uint64_t (&b)[N]) {
    for (size_t i = 0; i < N; ++i) {
      if (a[i] != b[i]) {
        return false;
      }
    }
    return true;
  }

  ArrayType domains_;
};

// Tracks the set of configured power domains and provides methods for
// updating, querying, and visiting the power domain set.
class PowerDomainRegistry {
 public:
  // Callback provided by the host environment to update per-CPU copies of the
  // power domain set when the main set changes.
  using UpdateCallback = fit::inline_function<void(const PowerDomainSet&)>;

  // Constructs a power domain registry with the given optional callback. The
  // given callback may do nothing, but it may not be nullptr.
  explicit PowerDomainRegistry(UpdateCallback update_callback = [](const auto&) {})
      : update_callback_{std::move(update_callback)} {
    ZX_DEBUG_ASSERT(update_callback_);
  }

  // Registers the given power domain, using the given callback to update each
  // CPU with a copy of the new power domain set.
  zx::result<> Register(const fbl::RefPtr<PowerDomain>& power_domain);

  // Unregisters the given power domain, using the given callback to update each
  // CPU with a copy of the new power domain set.
  zx::result<> Unregister(uint32_t domain_id);

  // Returns a reference to the power domain with the given domain id, or
  // nullptr if one does not exist.
  PowerDomain* Find(uint32_t domain_id) const {
    return power_domain_set_.FindByDomainId(domain_id);
  }

  // Visits each registered power domain.
  template <typename Visitor>
  void Visit(Visitor&& visitor) const {
    return power_domain_set_.Visit(std::forward<Visitor>(visitor));
  }

  const PowerDomainSet& power_domain_set() const { return power_domain_set_; }

 private:
  PowerDomainSet power_domain_set_;
  UpdateCallback update_callback_;
};

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_
