// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "generic_riscv64_ffi.h"

#include <lib/system-topology.h>
#include <platform.h>
#include <zircon/types.h>

#include <dev/init.h>
#include <kernel/cpu_distance_map.h>
#include <kernel/topology.h>
#include <ktl/iterator.h>
#include <ktl/optional.h>
#include <phys/handoff/handoff.h>
#include <platform/timer.h>
#include <platform/uart.h>

#include <ktl/enforce.h>

// <platform/efi.h>, whose declaration needs the EFI library headers this
// platform does not otherwise use.

bool IsEfiExpected() { return rust_is_efi_expected(); }

// <platform.h>

void platform_early_init() { rust_platform_early_init(); }

void platform_prevm_init() { rust_platform_prevm_init(); }

void platform_init() { rust_platform_init(); }

void platform_panic_start(PanicStartHaltOtherCpus option) {
  rust_platform_panic_start(option == PanicStartHaltOtherCpus::Yes);
}

void platform_halt_cpu() { rust_platform_halt_cpu(); }

bool platform_supports_suspend_cpu() { return rust_platform_supports_suspend_cpu(); }

zx_status_t platform_suspend_cpu(PlatformAllowDomainPowerDown allow_domain) {
  return rust_platform_suspend_cpu(allow_domain == PlatformAllowDomainPowerDown::Yes);
}

bool platform_supports_suspend_system() { return rust_platform_supports_suspend_system(); }

zx_status_t platform_suspend_system() { return rust_platform_suspend_system(); }

zx::result<power_cpu_state> platform_get_cpu_state(cpu_num_t cpu_id) {
  power_cpu_state state{};
  if (zx_status_t status = rust_platform_get_cpu_state(cpu_id, &state); status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(state);
}

void platform_specific_halt(platform_halt_action suggested_action, zircon_crash_reason_t reason,
                            bool halt_on_panic) {
  rust_platform_specific_halt(static_cast<uint32_t>(suggested_action),
                              static_cast<uint32_t>(reason), halt_on_panic);
}

// <platform/timer.h>

zx_instant_mono_ticks_t platform_convert_early_ticks(arch::EarlyTicks sample) {
  return rust_platform_convert_early_ticks(sample.time);
}

zx_status_t platform_set_oneshot_timer(zx_ticks_t deadline) {
  return rust_platform_set_oneshot_timer(deadline);
}

void platform_stop_timer() { rust_platform_stop_timer(); }

void platform_shutdown_timer() { rust_platform_shutdown_timer(); }

zx_status_t platform_suspend_timer_curr_cpu() { return rust_platform_suspend_timer_curr_cpu(); }

zx_status_t platform_resume_timer_curr_cpu() { return rust_platform_resume_timer_curr_cpu(); }

bool platform_usermode_can_access_tick_registers() {
  return rust_platform_usermode_can_access_tick_registers();
}

namespace {

// The Rust entry point for each GetTicksSyncFlag combination, indexed by the
// flag bits.
constexpr zx_ticks_t (*const kRawTicksSynchronized[])() = {
    rust_platform_current_raw_ticks_synchronized_0,
    rust_platform_current_raw_ticks_synchronized_1,
    rust_platform_current_raw_ticks_synchronized_2,
    rust_platform_current_raw_ticks_synchronized_3,
    rust_platform_current_raw_ticks_synchronized_4,
    rust_platform_current_raw_ticks_synchronized_5,
    rust_platform_current_raw_ticks_synchronized_6,
    rust_platform_current_raw_ticks_synchronized_7,
    rust_platform_current_raw_ticks_synchronized_8,
    rust_platform_current_raw_ticks_synchronized_9,
    rust_platform_current_raw_ticks_synchronized_10,
    rust_platform_current_raw_ticks_synchronized_11,
    rust_platform_current_raw_ticks_synchronized_12,
    rust_platform_current_raw_ticks_synchronized_13,
    rust_platform_current_raw_ticks_synchronized_14,
    rust_platform_current_raw_ticks_synchronized_15,
};

}  // anonymous namespace

template <GetTicksSyncFlag Flags>
zx_ticks_t platform_current_raw_ticks_synchronized() {
  static_assert(static_cast<size_t>(Flags) < ktl::size(kRawTicksSynchronized));
  return kRawTicksSynchronized[static_cast<size_t>(Flags)]();
}

// Explicit instantiation of all of the forms of synchronized tick access.
#define EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(flags) \
  template zx_ticks_t                                         \
  platform_current_raw_ticks_synchronized<static_cast<GetTicksSyncFlag>(flags)>()
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(0);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(1);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(2);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(3);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(4);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(5);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(6);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(7);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(8);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(9);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(10);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(11);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(12);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(13);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(14);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(15);
#undef EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED

// <dev/init.h>

void PlatformDriverHandoffEarly(const ArchPhysHandoff& arch_handoff) {
  ktl::optional plic = arch_handoff.plic_driver.to_std();
  ktl::optional timer = arch_handoff.generic_timer_driver.to_std();
  rust_platform_driver_handoff_early(plic ? &*plic : nullptr, timer ? &*timer : nullptr);
}

void PlatformDriverHandoffPostVm(const ArchPhysHandoff& arch_handoff) {
  ktl::optional plic = arch_handoff.plic_driver.to_std();
  rust_platform_driver_handoff_post_vm(plic ? &*plic : nullptr);
}

void PlatformDriverHandoffLate(const ArchPhysHandoff& arch_handoff) {
  ktl::optional plic = arch_handoff.plic_driver.to_std();
  rust_platform_driver_handoff_late(plic ? &*plic : nullptr);
}

// Entry points that stay in C++: their signatures or callees have no Rust
// counterpart yet.

void topology_init() {
  // Setup the CPU distance map with the already initialized topology.
  const auto processor_count =
      static_cast<uint>(system_topology::GetSystemTopology().processor_count());
  CpuDistanceMap::Initialize(processor_count, [](cpu_num_t from_id, cpu_num_t to_id) { return 0; });

  const CpuDistanceMap::Distance kDistanceThreshold = 2u;
  CpuDistanceMap::Get().set_distance_threshold(kDistanceThreshold);

  CpuDistanceMap::Get().Dump();
}

ktl::optional<uint32_t> PlatformUartGetIrqNumber(uint32_t irq_num) { return irq_num; }
