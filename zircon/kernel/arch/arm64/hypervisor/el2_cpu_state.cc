// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/arm64/mmu.h>
#include <arch/hypervisor.h>
#include <dev/interrupt.h>
#include <fbl/alloc_checker.h>
#include <hypervisor/cpu.h>
#include <kernel/cpu.h>
#include <kernel/mutex.h>
#include <ktl/utility.h>

#include "el2_cpu_state_priv.h"

#include <ktl/enforce.h>

namespace {

DECLARE_SINGLETON_MUTEX(GuestMutex);
size_t num_guests TA_GUARDED(GuestMutex::Get()) = 0;
ktl::unique_ptr<El2CpuState> el2_cpu_state TA_GUARDED(GuestMutex::Get());

}  // namespace

zx::result<> El2CpuState::OnTask(void* context, cpu_num_t cpu_num) {
  auto cpu_state = static_cast<El2CpuState*>(context);
  cpu_state->vtcr_.Write();
  __isb(ARM_MB_SY);
  unmask_interrupt(kTimerVector);
  unmask_interrupt(kMaintenanceVector);
  return zx::ok();
}

static void el2_off_task(void* arg) {
  mask_interrupt(kMaintenanceVector);
  mask_interrupt(kTimerVector);
}

// static
zx::result<ktl::unique_ptr<El2CpuState>> El2CpuState::Create() {
  fbl::AllocChecker ac;
  ktl::unique_ptr<El2CpuState> cpu_state(new (&ac) El2CpuState);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  // Setup VTCR_EL2.
  auto address_size = arch::ArmIdAa64Mmfr0El1::Read().pa_range();
  cpu_state->vtcr_.set_reg_value(MMU_VTCR_EL2_FLAGS);
  cpu_state->vtcr_.set_ps(address_size);
  if (arch::ArmIdAa64Mmfr1El1::Read().vmid_bits() == arch::ArmAsidSize::k16bits) {
    cpu_state->vtcr_.set_vs(true);
  } else if (auto result = cpu_state->vmid_allocator_.Reset(UINT8_MAX); result.is_error()) {
    return result.take_error();
  }

  // Setup EL2 for all online CPUs.
  cpu_state->cpu_mask_ = hypervisor::percpu_exec(OnTask, cpu_state.get());
  if (cpu_state->cpu_mask_ != mp_get_online_mask()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  return zx::ok(ktl::move(cpu_state));
}

El2CpuState::~El2CpuState() { mp_sync_exec(mp_ipi_target::MASK, cpu_mask_, el2_off_task, nullptr); }

zx::result<uint16_t> El2CpuState::AllocVmid() { return vmid_allocator_.TryAlloc(); }

zx::result<> El2CpuState::FreeVmid(uint16_t id) { return vmid_allocator_.Free(id); }

zx::result<uint16_t> alloc_vmid() {
  Guard<Mutex> guard(GuestMutex::Get());
  if (num_guests == 0) {
    auto cpu_state = El2CpuState::Create();
    if (cpu_state.is_error()) {
      return cpu_state.take_error();
    }
    el2_cpu_state = ktl::move(*cpu_state);
  }
  auto vmid = el2_cpu_state->AllocVmid();
  if (vmid.is_error()) {
    if (num_guests == 0) {
      el2_cpu_state.reset();
    }
    return vmid.take_error();
  }
  num_guests++;
  return vmid;
}

zx::result<> free_vmid(uint16_t id) {
  Guard<Mutex> guard(GuestMutex::Get());
  if (auto result = el2_cpu_state->FreeVmid(id); result.is_error()) {
    return result.take_error();
  }
  num_guests--;
  if (num_guests == 0) {
    el2_cpu_state.reset();
  }
  return zx::ok();
}
