// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <debug.h>
#include <sys/types.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/riscv64.h>
#include <arch/riscv64/feature.h>
#include <arch/riscv64/mp.h>
#include <arch/riscv64/vector.h>
#include <arch/thread.h>
#include <arch/vm.h>
#include <kernel/thread.h>

#define LOCAL_TRACE 0

// assert that the context switch frame is a multiple of 16 to maintain
// stack alignment requirements per ABI
static_assert(sizeof(riscv64_context_switch_frame) % 16 == 0);

// A scratch word of memory to store into during context switches to wipe out any existing
// memory reservation in a LR/SC sequence. It is explicitly aligned and not shared with any
// other variables in the system to avoid it being aliased with another atomic.
namespace {
uint32_t memory_reservation_scratch __CPU_ALIGN_EXCLUSIVE;
}  // anonymous namespace

void arch_thread_initialize(Thread* t, vaddr_t entry_point) {
  // zero out the entire arch state, including fpu state, which defaults to all zero
  t->arch() = {};

  // create a default stack frame on the stack
  vaddr_t stack_top = t->stack().top();

  // Always leave space at the very top for an iframe.  In user threads,
  // arch_uspace_entry will clobber this space while the kernel stack below it
  // is still in use.  In non-user threads, this space is wasted but those
  // never need their full stack range anyway.
  static_assert(sizeof(iframe_t) % 16 == 0);
  stack_top -= sizeof(iframe_t);
  DEBUG_ASSERT(IS_ROUNDED(stack_top, alignof(iframe_t)));

  // make sure the top of the stack is 16 byte aligned for ABI compliance
  DEBUG_ASSERT(IS_ROUNDED(stack_top, 16));

  riscv64_context_switch_frame* frame =
      reinterpret_cast<riscv64_context_switch_frame*>(stack_top) - 1;

  // fill in the entry point
  frame->ra = entry_point;

  // set the stack pointer
  t->arch().sp = (vaddr_t)frame;

#if __has_feature(shadow_call_stack)
  // the shadow call stack grows up
  frame->*riscv64_context_switch_frame::kShadowCallStackPointer = t->stack().shadow_call_base();
#endif

  // set the thread pointer that will be restored on the first context switch
  frame->tp = reinterpret_cast<uintptr_t>(&t->arch().thread_pointer_location);
}

__NO_SAFESTACK void arch_thread_construct_first(Thread* t) {
  // In the case of the boot CPU, `initial` doesn't actually point to real
  // memory yet; in the case of secondaries though, `initial` will already
  // be `t` and set at the thread pointer (during riscv64_secondary_start()).
  Thread* initial = arch_get_current_thread();
  if (initial == t) {
    return;
  }

  // In the case of the boot CPU, physboot handed off a temporary region of
  // memory covering the subset of `arch_thread` dealing in the thread ABI. So
  // `fake_thread` is indeed fake, but accessing its `stack_guard` and
  // `unsafe_sp` members is kosher.
  const arch_thread& fake_arch = initial->arch();

  // Copy over the thread ABI values from the temporary region into the first
  // thread.
  auto& arch = t->arch();
  arch.stack_guard = fake_arch.stack_guard;
  arch.unsafe_sp = fake_arch.unsafe_sp;

  arch_set_current_thread(t);
}

iframe_t arch_prepare_uspace(const UserEntryState& state) {
  return {
      // Saved interrupt enable (so that interrupts are enabled when returning
      // to user space).  Current interrupt enable state set to disabled, which
      // will matter when the arch_uspace_entry loads sstatus temporarily
      // before switching to user space.  Set user space bitness to 64bit.  Set
      // the FPU and vector registers to the initial state, with the implicit
      // assumption that the context switch routine would have defaulted the
      // FPU/vector state a the time this thread enters user space.  All other
      // bits set to zero, default options.
      .status = RISCV64_CSR_SSTATUS_SPIE |        // Interrupts disabled.
                RISCV64_CSR_SSTATUS_UXL_64BIT |   // 64-bit user mode.
                RISCV64_CSR_SSTATUS_FS_INITIAL |  // Initial FPU state.
                (gRiscvFeatures[arch::RiscvFeature::kVector]
                     ? RISCV64_CSR_SSTATUS_VS_INITIAL  // Initial vector state.
                     : 0),
      .regs{
          .pc = state.pc,
          .sp = state.sp,
          .gp = state.abi_reg,
          .tp = state.tp,
          .a0 = state.arg1,
          .a1 = state.arg2,
      },
  };
}

// Switch to user mode, set the user stack pointer to user_stack_top, save the
// top of the kernel stack pointer.
void arch_enter_uspace(const iframe_t* iframe) {
  Thread* ct = Thread::Current::Get();

  LTRACEF("pc %#" PRIx64 " sp %#" PRIx64 " a0 %#" PRIx64 " a1 %#" PRIx64 "\n", iframe->regs.pc,
          ct->stack().top(), iframe->regs.a0, iframe->regs.a1);

  ASSERT(arch_is_valid_user_pc(iframe->regs.pc));

  // arch_thread_initialize() left space so the base of the stack won't overlap
  // with anything currently in use.  This function won't return, but instead
  // will abandon all the kernel register and stack state to start fresh at the
  // top of the machine stack and the base of the shadow call stack.
  iframe_t* user_iframe = reinterpret_cast<iframe_t*>(ct->stack().top()) - 1;
  *user_iframe = *iframe;

#if __has_feature(shadow_call_stack)
  const uint64_t scsp = ct->stack().shadow_call_base();
#else
  const uint64_t scsp = 0;
#endif

  // Disable interrupts and then warp into the stvec.S code as if just
  // returning from Riscv64UserException after entering the kernel for a user
  // mode exception.  To that code, it looks just like this initial iframe was
  // the interrupted user state now being resumed.
  arch_disable_ints();
  __asm__ volatile(
      R"""(
      mv sp, %[sp]
      mv gp, %[gp]
      tail Riscv64ReturnToUser
      unimp
      )"""
      :
      : [sp] "r"(user_iframe), "m"(*user_iframe), [gp] "r"(scsp));
  __builtin_unreachable();
}

void arch_context_switch(Thread* oldthread, Thread* newthread)
    TA_REQ(oldthread->get_lock(), newthread->get_lock()) {
  DEBUG_ASSERT(arch_ints_disabled());

  LTRACEF("old %p (%s), new %p (%s)\n", oldthread, oldthread->name(), newthread, newthread->name());

  // Wipe out any LR/SC reservations this cpu may have.
  __asm__ volatile("sc.w zero, zero, %0" ::"A"(memory_reservation_scratch) : "memory");

  // FPU and vector context switch
  // Based on a combination of the current hardware state and whether or not the
  // threads have the dirty flags set, conditionally save and/or restore
  // hardware state.
  if constexpr (LOCAL_TRACE) {
    uint64_t status = riscv64_csr_read(RISCV64_CSR_SSTATUS);
    uint64_t fpu_status = status & RISCV64_CSR_SSTATUS_FS_MASK;
    uint64_t vector_status = status & RISCV64_CSR_SSTATUS_VS_MASK;
    LTRACEF("fpu: sstatus.fp %#lx, sstatus.vs %#lx, sd %u, old.dirty %u, new.dirty %u\n",
            fpu_status >> RISCV64_CSR_SSTATUS_FS_SHIFT,
            vector_status >> RISCV64_CSR_SSTATUS_VS_SHIFT, !!(status & RISCV64_CSR_SSTATUS_SD),
            oldthread->arch().fpu_dirty, newthread->arch().fpu_dirty);
  }

  Riscv64FpuStatus current_fpu_status = riscv64_fpu_status();
  Riscv64VectorStatus current_vector_status = riscv64_vector_status();
  if (likely(!oldthread->IsUserStateSavedLocked())) {
    // Save the fpu and vector state for the old (current) thread, depending on
    // whether the fpu or vector hardware is currently in the initial state.
    DEBUG_ASSERT(oldthread == Thread::Current().Get());
    riscv64_thread_fpu_save(oldthread, current_fpu_status);

    if (gRiscvFeatures[arch::RiscvFeature::kVector]) {
      riscv64_thread_vector_save(oldthread, current_vector_status);
    }
  }
  // Always restore the new thread's fpu and vector state even if it is
  // probably going to be restored by a higher layer later with a call to
  // arch_restore_user_state. Though it may be extra work in this case, it
  // avoids potential issues with state getting out of sync if the kernel
  // panicked or the higher layer forgot to restore.
  riscv64_thread_fpu_restore(newthread, current_fpu_status);
  if (gRiscvFeatures[arch::RiscvFeature::kVector]) {
    riscv64_thread_vector_restore(newthread, current_vector_status);
  }

  // Set the percpu in_restricted_mode field.
  const bool in_restricted =
      newthread->restricted_state() != nullptr && newthread->restricted_state()->in_restricted();
  arch_set_restricted_flag(in_restricted);

  // Regular integer context switch.
  riscv64_context_switch(&oldthread->arch().sp, newthread->arch().sp);
}

void arch_dump_thread(const Thread* t) {
  if (t->state() != THREAD_RUNNING) {
    dprintf(INFO, "\tarch: ");
    dprintf(INFO, "sp 0x%lx\n", t->arch().sp);
  }
}

vaddr_t arch_thread_get_blocked_fp(Thread* t) {
  if (!WITH_FRAME_POINTERS) {
    return 0;
  }

  const struct riscv64_context_switch_frame* frame;
  frame = reinterpret_cast<const riscv64_context_switch_frame*>(t->arch().sp);
  DEBUG_ASSERT(frame);

  return frame->s0;
}

void arch_save_user_state(Thread* thread) {
  riscv64_thread_fpu_save(thread, riscv64_fpu_status());
  if (gRiscvFeatures[arch::RiscvFeature::kVector]) {
    riscv64_thread_vector_save(thread, riscv64_vector_status());
  }
  // Not saving debug state because there isn't any.
}

void arch_restore_user_state(Thread* thread) {
  riscv64_thread_fpu_restore(thread, riscv64_fpu_status());
  if (gRiscvFeatures[arch::RiscvFeature::kVector]) {
    riscv64_thread_vector_restore(thread, riscv64_vector_status());
  }
}

void arch_set_suspended_general_regs(struct Thread* thread, GeneralRegsSource source,
                                     void* iframe) {
  DEBUG_ASSERT(thread->arch().suspended_general_regs == nullptr);
  DEBUG_ASSERT(iframe != nullptr);
  DEBUG_ASSERT_MSG(source == GeneralRegsSource::Iframe, "invalid source %u\n",
                   static_cast<uint32_t>(source));
  thread->arch().suspended_general_regs = static_cast<iframe_t*>(iframe);
}

void arch_reset_suspended_general_regs(struct Thread* thread) {
  thread->arch().suspended_general_regs = nullptr;
}
