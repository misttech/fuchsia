// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/power-management/pdev-power-level-controller.h>
#include <lib/power-management/power-state.h>
#include <stddef.h>

#include <ktl/utility.h>

#ifdef UNITTESTS_ENABLED
#include <kernel/percpu.h>
#include <kernel/scheduler.h>
#endif

namespace power_management {

static_assert(sizeof(ControlInterface) == 8);
static_assert(alignof(ControlInterface) == 8);
static_assert(static_cast<uint64_t>(ControlInterface::kCpuDriver) == 0);
static_assert(static_cast<uint64_t>(ControlInterface::kArmPsci) == 1);
static_assert(static_cast<uint64_t>(ControlInterface::kArmWfi) == 2);
static_assert(static_cast<uint64_t>(ControlInterface::kRiscvSbi) == 3);
static_assert(static_cast<uint64_t>(ControlInterface::kRiscvWfi) == 4);

static_assert(sizeof(PowerLevelUpdateRequest) == 32);
static_assert(alignof(PowerLevelUpdateRequest) == 8);
static_assert(offsetof(PowerLevelUpdateRequest, domain_id) == 0);
static_assert(offsetof(PowerLevelUpdateRequest, target_id) == 4);
static_assert(offsetof(PowerLevelUpdateRequest, control) == 8);
static_assert(offsetof(PowerLevelUpdateRequest, control_argument) == 16);
static_assert(offsetof(PowerLevelUpdateRequest, options) == 24);

#ifdef UNITTESTS_ENABLED
namespace {
PowerDomainSet g_saved_power_domains[SMP_MAX_CPUS];
}  // namespace

extern "C" {

void cpp_save_and_clear_power_domains_for_test() {
  percpu::ForEach([](cpu_num_t cpu_num, percpu* percpu) {
    g_saved_power_domains[cpu_num] = percpu->scheduler.ExchangePowerDomainSet(PowerDomainSet{});
  });
}

void cpp_restore_power_domains_for_test() {
  percpu::ForEach([](cpu_num_t cpu_num, percpu* percpu) {
    percpu->scheduler.ExchangePowerDomainSet(ktl::move(g_saved_power_domains[cpu_num]));
  });
}

}  // extern "C"
#endif

}  // namespace power_management
