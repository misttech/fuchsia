// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/machine.h>
#include <zircon/sanitizer.h>

#include <atomic>
#include <cassert>
#include <optional>

#include "../weak.h"
#include "thread-list.h"
#include "thread-storage.h"
#include "thread.h"
#include "threads_impl.h"
#include "zxr-thread.h"

// This file is compiled in the basic machine ABI (user.basic) environment.
// It switches off of, and then frees, the thread's main stacks.

extern "C" decltype(__sanitizer_thread_exit_hook) __sanitizer_thread_exit_hook [[gnu::weak]];

namespace LIBC_NAMESPACE_DECL {
namespace {

using SanitizerExitHook = Weak<__sanitizer_thread_exit_hook>;

constexpr uintptr_t kStackAlignment = elfldltl::AbiTraits<>::kStackAlignment<>;

[[maybe_unused]] constexpr size_t kMinimumStack = 256;

// This is the very last thing to run on an exiting thread.  Not only does it
// use only the basic machine ABI, but it runs on a tiny stub stack that's
// scavenged from the unused part of the thread block.
[[noreturn]] void FinalExit() {
  Thread& self = *__pthread_self();

  // Transfer ownership of the thread stacks, but not the thread block itself.
  // This will one day just be `old_storage = std::move(self.storage_);`.
  std::optional<ThreadStorage> old_storage = ThreadStorage::FromThread(self, false);

  // The TCB fields that __sanitizer_memory_snapshot reads for the stacks were
  // cleared by FromThread (and would be by the eventual ThreadStorage move
  // ctor used directly from a Thread member).  Make sure those stores have
  // definitely completed before the stacks are deallocated.  In case we get
  // suspended by __sanitizer_memory_snapshot, the TCB is always expected to
  // contain valid pointers.
  std::atomic_signal_fence(std::memory_order_seq_cst);

  // Now actually free the stacks.  But first, collect the copy of the
  // (unowned) root VMAR handle that was saved there.  This avoids a symbolic
  // dependency on the global stash of "the" root VMAR handle.
  zx::unowned_vmar unmap_vmar = old_storage->vmar().borrow();
  old_storage.reset();

  // This deallocates the TCB region too for the detached case.  If not
  // detached, thrd_join / pthread_join will deallocate it.  This always makes
  // the __thread_list_erase callback before deallocating the TCB.  Hence
  // __sanitizer_memory_snapshot should not consider the thread to be "alive"
  // any more, safely before the memory might be unmapped.
  zxr_thread_exit_unmap_if_detached(  //
      &self.zxr_thread, __thread_list_erase, &self, unmap_vmar->get(),
      reinterpret_cast<uintptr_t>(self.tcb_region.iov_base), self.tcb_region.iov_len);
}

// Switch to a new machine stack and call FinalExit(), which cannot return.
[[noreturn]] void FinalExitOnStack(uintptr_t sp) {
  __asm__ volatile(
#if defined(__aarch64__)
      R"""(
        mov sp, %[sp]
        bl %cc[FinalExit]
      )"""
#elif defined(__riscv)
      R"""(
        mv sp, %[sp]
        call %cc[FinalExit]
      )"""
#elif defined(__x86_64__)
      R"""(
        mov %[sp], %%rsp
        call %cc[FinalExit]
      )"""
#else
#error "unsupported machine"
#endif
      :
      : [sp] "r"(sp), [FinalExit] "X"(FinalExit));
  __builtin_trap();
}

// Compute a new machine stack pointer to start at for the call to FinalExit().
// This can use some part of the thread block that won't overlap with the
// actual Thread object that stays valid until a join is complete.
uintptr_t FinalExitSp(Thread& self) {
  // The thread block includes one-page guards before and after its usable pages.
  const size_t page_size = zx_system_get_page_size();
  uintptr_t tcb_base = reinterpret_cast<uintptr_t>(self.tcb_region.iov_base) + page_size;
  uintptr_t tcb_size = self.tcb_region.iov_len - (page_size * 2);

  if constexpr (elfldltl::TlsTraits<>::kTlsNegative) {
    // The thread descriptor is at the end of the region, so the space
    // before it (formerly TLS) is available as the temporary stack.
    uintptr_t sp = reinterpret_cast<uintptr_t>(&self) & -kStackAlignment;
    assert(sp > tcb_base);
    assert(sp - tcb_base >= kMinimumStack);
    return sp;
  } else {
    // The thread descriptor is at the start of the region, so the rest of
    // the space up to the guard page is available as the temporary stack.
    uintptr_t sp = tcb_base + tcb_size;
    assert(sp % kStackAlignment == 0);
    assert(tcb_size >= sizeof(Thread) + kMinimumStack);
    return sp;
  }
}

}  // namespace

[[noreturn]] void ThreadExitFinish(Thread& self) {
  // Notify the sanitizer runtime that the thread is about to exit.  As soon as
  // the exit returns, no more sanitizer runtime callbacks are safe to make.
  // Hence this is called in a function that won't itself ever be instrumented.
  SanitizerExitHook::Call(self.sanitizer_hook, ToC11Thread(self));

  // Switch to the temporary stack and free the real stacks on the way out.
  FinalExitOnStack(FinalExitSp(self));
}

}  // namespace LIBC_NAMESPACE_DECL
