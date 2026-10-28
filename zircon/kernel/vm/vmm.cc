// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <inttypes.h>
#include <lib/affine/ratio.h>
#include <lib/ktrace.h>
#include <lib/zircon-internal/macros.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/null_lock.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/restricted_state.h>
#include <kernel/task_runtime_timers.h>
#include <object/diagnostics.h>
#include <vm/fault.h>
#include <vm/pmm.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vmm_ffi.h>

#include "lib/fxt/serializer.h"
#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)
#define TRACE_PAGE_FAULT 0

// This file mostly contains C wrappers around the underlying C++ objects, conforming to
// the older api.

namespace {

// TODO(https://fxbug.dev/42084841): See https://fxbug.dev/42084841 for why we translate results.
//
// Helper function for translating PageFault status in the context of the vmm_page_fault_handler.
//
// Long-term, we should eliminate this helper function.
zx_status_t TranslatePageFaultStatus(zx_status_t orig_status, uint flags) {
  if (orig_status == ZX_ERR_INTERNAL_INTR_RETRY) {
    // We were asked to suspend.
    //
    // Regardless of whether the fault occurred while in usermode or kernel mode, we lie to the
    // caller.
    //
    // If the fault originated while the CPU was in usermode, the handler will unwind, call
    // ProcessPendingSignals and suspend.  All good.
    //
    // If the fault occurred during a user-copy, while the CPU was in kernel mode, the user-copy
    // will resume and the thread will re-fault if the fault has not already been resolved.  This is
    // actually a problem because if we end up attempting to wait on the re-fault path, deep down in
    // WaitQueue::BlockEtcPreamble, we'll see that the thread is still signaled and return without
    // blocking.  We can end up in an expensive busy-loop, re-faulting over and over.
    //
    // TODO(https://fxbug.dev/443281947): Fix the busy-loop issue described above.  Once that's been
    // fixed or we correctly support suspend during page faults we should DEBUG_ASSERT here that
    // flags has VMM_PF_FLAG_USER.
    return ZX_OK;
  }

  if (orig_status == ZX_ERR_INTERNAL_INTR_KILLED && (flags & VMM_PF_FLAG_USER)) {
    // We were asked to terminate and the fault originated in usermode.
    //
    // Because it was usermode, we know we aren't in a usercopy.  Similar to above, lie to the
    // caller.  The thread will unwind and terminate before returning back to usermode.
    return ZX_OK;
  }

  return orig_status;
}

}  // namespace

void vmm_context_switch(VmAspace* oldspace, VmAspace* newaspace) {
  DEBUG_ASSERT(arch_ints_disabled());

  ArchVmAspace::ContextSwitch(oldspace ? &oldspace->arch_aspace() : nullptr,
                              newaspace ? &newaspace->arch_aspace() : nullptr);
}

class FlagsString {
 public:
  explicit FlagsString(uint flags) { vmm_pf_flags_to_string(flags, string_); }

  operator fxt::StringRef<fxt::RefType::kInline>() const {
    return fxt::StringRef{string_, sizeof(string_)};
  }

 private:
  char string_[5];
};

zx_status_t vmm_page_fault_handler(vaddr_t addr, uint flags) {
  // hardware fault, mark it as such
  flags |= VMM_PF_FLAG_HW_FAULT;

  Thread* current_thread = Thread::Current::Get();
  zx_instant_mono_ticks_t start_time = current_mono_ticks();
  zx_instant_boot_ticks_t trace_start_time = KTrace::Timestamp();
  PageFaultTimer timer(current_thread, start_time);

  if (TRACE_PAGE_FAULT || LOCAL_TRACE) {
    char flagstr[5];
    vmm_pf_flags_to_string(flags, flagstr);
    TRACEF("thread %s va %#" PRIxPTR ", flags 0x%x (%s)\n", current_thread->name(), addr, flags,
           flagstr);
  }

  // Page faults never happen on kernel addresses. Double check this is a valid user address, then
  // continue with the user aspace.
  if (unlikely(!is_user_accessible(addr))) {
    LTRACEF("PageFault: Invalid virtual address 0x%lx\n", addr);
    return ZX_ERR_NOT_FOUND;
  }

  // page fault it
  const zx_status_t status =
      TranslatePageFaultStatus(Thread::Current::PageFault(addr, flags), flags);

  KTRACE_COMPLETE("kernel:vm", "page_fault", trace_start_time, ("vaddr", ktrace::Pointer{addr}),
                  ("flags", FlagsString{flags}));

  return status;
}

void vmm_set_active_aspace(VmAspace* aspace) {
  LTRACEF("aspace %p\n", aspace);

  Thread* t = Thread::Current::Get();
  t->AssertIsCurrentThread();
  DEBUG_ASSERT(t);

  if (aspace == t->active_aspace()) {
    return;
  }

  InterruptDisableGuard irqd;
  VmAspace* old = t->switch_aspace(aspace);
  vmm_context_switch(old, t->active_aspace());
}

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vmm_set_active_aspace(VmAspace* aspace) {
  vmm_set_active_aspace(aspace);
}

}  // extern "C"
