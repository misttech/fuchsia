// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch.h>
#include <inttypes.h>
#include <stdlib.h>
#include <trace.h>

#include <arch/arm64/feature.h>
#include <arch/arm64/registers.h>
#include <arch/debugger.h>
#include <arch/regs.h>
#include <arch/vm.h>
#include <kernel/restricted_state.h>

#define LOCAL_TRACE 0

namespace {
// In Arm32 mode, only registers r0-r14 are used from the general purpose
// register set.
constexpr size_t kArm32BitModeRegisterCount = 15;
}  // namespace

void RestrictedState::ArchDump(const zx_restricted_state_t& state) {
  for (size_t i = 0; i < ktl::size(state.x); i++) {
    printf("R%zu: %#18" PRIx64 "\n", i, state.x[i]);
  }
  printf("CPSR: %#18" PRIx32 "\n", state.cpsr);
  printf("PC: %#18" PRIx64 "\n", state.pc);
  printf("SP: %#18" PRIx64 "\n", state.sp);
  printf("TPIDR_EL0: %#18" PRIx64 "\n", state.tpidr_el0);
}

zx_status_t RestrictedState::ArchValidateStatePreRestrictedEntry(
    const zx_restricted_state_t& state) {
  // Validate that PC is within userspace.
  if (unlikely(!is_user_accessible(state.pc))) {
    LTRACEF("fail due to bad PC %#" PRIx64 "\n", state.pc);
    return ZX_ERR_BAD_STATE;
  }
  // If kArm32BitMode, perform additional checks.
  if (state.cpsr & kArm32BitMode) {
    if (unlikely(!arm64_feature_test(ZX_ARM64_FEATURE_ISA_ARM32))) {
      LTRACEF("fail due to lack of 32-bit ISA support\n");
      return ZX_ERR_BAD_STATE;
    }

    // Make sure PC is <4Gb is kArm32BitMode
    if (unlikely(state.pc >= (1ULL << 32))) {
      LTRACEF("fail due to out of range 32-bit PC %#" PRIx64 "\n", state.pc);
      return ZX_ERR_BAD_STATE;
    }

    // If CPSR's T bit is not set, then PC[1:0] must be 0 (32-bit aligned).
    if (unlikely((state.cpsr & kArm32BitThumbMode) == 0 && (state.pc & 0x3))) {
      LTRACEF("fail due to unaligned A32 32-bit PC %#" PRIx64 "\n", state.pc);
      return ZX_ERR_BAD_STATE;
    }

    // If CPSR's T bit is set, then PC[0] must be 0 (16-bit aligned).
    if (unlikely((state.cpsr & kArm32BitThumbMode) == 1 && (state.pc & 0x1))) {
      LTRACEF("fail due to unaligned T32 32-bit PC %#" PRIx64 "\n", state.pc);
      return ZX_ERR_BAD_STATE;
    }

    // Validate that only the NCZV flags and 32-bit relevant flags of the CPSR are set.
    if (unlikely((state.cpsr & ~(kArmUserRestrictedVisibleFlags)) != 0)) {
      LTRACEF("fail due to flags outside of kArmUserRestrictedVisibleFlags set (%#" PRIx32 ")\n",
              state.cpsr);
      return ZX_ERR_BAD_STATE;
    }
  } else {
    // Validate that only the NCZV flags of the CPSR are set.
    // For aarch64 restricted threads, the flags are the same as normal user threads.
    if (unlikely((state.cpsr & ~(kArmUserVisibleFlags)) != 0)) {
      LTRACEF("fail due to flags outside of kArmUserVisibleFlags set (%#" PRIx32 ")\n", state.cpsr);
      return ZX_ERR_BAD_STATE;
    }
  }
  return ZX_OK;
}

void RestrictedState::ArchSaveStatePreRestrictedEntry(ArchSavedNormalState& arch_state) {
  // Save the thread local storage register(s) from normal mode.
  arch_state.tpidr_el0 = __arm_rsr64("tpidr_el0");
  arch_state.tpidrro_el0 = __arm_rsr64("tpidrro_el0");
}

[[noreturn]] void RestrictedState::ArchEnterRestricted(const zx_restricted_state_t& state) {
  DEBUG_ASSERT(arch_ints_disabled());

  // Copy restricted state to an interrupt frame.
  iframe_t iframe{};
  if (state.cpsr & kArm32BitMode) {
    // If the thread is in a 32-bit execution mode, then ignore the upper bits of
    // the registers and only copy the registers that map to to ARM32 state r0-r14.
    for (size_t i = 0; i < kArm32BitModeRegisterCount; i++) {
      iframe.r[i] = state.x[i] & 0x00000000ffffffff;
    }
  } else {
    static_assert(sizeof(iframe.r) <= sizeof(state.x));
    memcpy(iframe.r, state.x, sizeof(iframe.r));
    iframe.lr = state.x[30];
  }
  iframe.usp = state.sp;
  iframe.elr = state.pc;
  iframe.spsr = static_cast<uint64_t>(state.cpsr);

  // Restore TPIDR_EL0 from restricted mode.
  // TODO(https://fxbug.dev/42076040): Eventually the TPIDR register should be
  // inside the iframe.
  __arm_wsr64("tpidr_el0", state.tpidr_el0);
  // Mirror to tpidrro_el0 when supporting aarch32.
  // This allows aarch32 userland to read TPIDRURO which is needed
  // by some libc implementations.
  if (state.cpsr & kArm32BitMode) {
    __arm_wsr64("tpidrro_el0", state.tpidr_el0);
  }

  // Load the new state and enter restricted mode.
  arch_enter_uspace(&iframe);

  __UNREACHABLE;
}

void RestrictedState::ArchRedirectRestrictedExceptionToNormal(
    const ArchSavedNormalState& arch_state, uintptr_t vector_table, uintptr_t context) {
  zx_thread_state_general_regs_t regs = {};
  regs.pc = vector_table;
  regs.r[0] = context;
  regs.r[1] = ZX_RESTRICTED_REASON_EXCEPTION;
  regs.tpidr = arch_state.tpidr_el0;
  [[maybe_unused]] zx_status_t status = arch_set_general_regs(Thread::Current().Get(), &regs);
  // This will only fail if register state has not been saved, but this will always
  // have happened by this stage of exception handling.
  DEBUG_ASSERT(status == ZX_OK);
}

namespace {

// Save the registers from either a zx_thread_state_general_regs_t or a syscall_regs_t
// to the zx_restricted_state_t. The input structures are subtly different so requires templating
// to deal with the difference.
template <typename T>
  requires(ktl::is_same_v<T, zx_thread_state_general_regs_t> || ktl::is_same_v<T, syscall_regs_t>)
inline void SaveRegs(const uint64_t cpsr, zx_restricted_state_t& state, const T& regs) {
  // If the thread is in a 32-bit execution mode, then ignore the upper bits of
  // the registers and only copy the registers that map to to ARM32 state r0-r14.
  if (cpsr & kArm32BitMode) {
    for (size_t i = 0; i < kArm32BitModeRegisterCount; i++) {
      state.x[i] = regs.r[i] & 0x00000000ffffffff;
    }
  } else {
    static_assert(sizeof(regs.r) <= sizeof(state.x));
    memcpy(state.x, regs.r, sizeof(regs.r));
    state.x[30] = regs.lr;
  }

  // This part of the input structures have slightly different field names.
  if constexpr (ktl::is_same_v<T, zx_thread_state_general_regs_t>) {
    state.sp = regs.sp;
    state.pc = regs.pc;
    // Save only the non-reserved portions of the SPSR.
    state.cpsr = static_cast<uint32_t>(regs.cpsr);
  }
  if constexpr (ktl::is_same_v<T, syscall_regs_t>) {
    state.sp = regs.usp;
    state.pc = regs.elr;
    // Save only the non-reserved portions of the SPSR.
    state.cpsr = static_cast<uint32_t>(regs.spsr);
  }

  // Save the thread local storage location in restricted mode.
  state.tpidr_el0 = __arm_rsr64("tpidr_el0");
}

}  // namespace

void RestrictedState::ArchSaveRestrictedExceptionState(zx_restricted_state_t& state) {
  zx_thread_state_general_regs_t regs = {};
  [[maybe_unused]] zx_status_t status = arch_get_general_regs(Thread::Current().Get(), &regs);
  // This will only fail if register state has not been saved, but this will always
  // have happened by this stage of exception handling.
  DEBUG_ASSERT(status == ZX_OK);

  // Save the registers from restricted mode.
  SaveRegs(regs.cpsr, state, regs);
}

void RestrictedState::ArchSaveRestrictedSyscallState(zx_restricted_state_t& state,
                                                     const syscall_regs_t& regs) {
  DEBUG_ASSERT(arch_ints_disabled());

  // Save the registers from restricted mode.
  SaveRegs(regs.spsr, state, regs);
}

void RestrictedState::ArchSaveRestrictedIframeState(zx_restricted_state_t& state,
                                                    const iframe_t& frame) {
  // On arm64, iframe_t and syscalls_regs_t are the same type.
  static_assert(ktl::is_same_v<syscall_regs_t, iframe_t>);
  RestrictedState::ArchSaveRestrictedSyscallState(state, frame);
}

[[noreturn]] void RestrictedState::ArchEnterFull(const ArchSavedNormalState& arch_state,
                                                 uintptr_t vector_table, uintptr_t context,
                                                 uint64_t code) {
  DEBUG_ASSERT(arch_ints_disabled());

  // Restore TPIDR_EL0 from saved normal state.
  // TODO(https://fxbug.dev/42076040): Eventually the TPIDR register should be
  // inside the iframe.
  __arm_wsr64("tpidr_el0", arch_state.tpidr_el0);
  __arm_wsr64("tpidrro_el0", arch_state.tpidrro_el0);

  // Set up a mostly empty iframe and return back to normal mode.
  iframe_t iframe{};

  // Pass through the context and return code as arguments.
  iframe.r[0] = context;
  iframe.r[1] = code;

  // Set the ELR such that we return to the vector_table after entering normal
  // mode.
  iframe.elr = vector_table;

  // Load the new state and exit.
  arch_enter_uspace(&iframe);

  __UNREACHABLE;
}
