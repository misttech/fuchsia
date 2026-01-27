// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/thread.h>
#include <zircon/sanitizer.h>

#include <cstdint>

#include "../weak.h"
#include "thread-storage.h"
#include "thread.h"
#include "threads_impl.h"

extern "C" decltype(__sanitizer_thread_create_hook) __sanitizer_thread_create_hook [[gnu::weak]];
extern "C" decltype(__sanitizer_thread_start_hook) __sanitizer_thread_start_hook [[gnu::weak]];

namespace LIBC_NAMESPACE_DECL {

// TODO(https://gcc.gnu.org/bugzilla/show_bug.cgi?id=123565): This has to
// appear to have external linkage to work around a GCC bug with the new
// extended asm outside of functions.  The definition in assembly will still
// actually only define a local ELF symbol, just with different name mangling.
// When the GCC bug is fixed, the main declaration below will work.
#if defined(__aarch64__) && !defined(__clang__)
[[noreturn]] void AsmTrampoline(uintptr_t arg1, uintptr_t arg2);
#endif

namespace {

using SanitizerCreateHook = Weak<__sanitizer_thread_create_hook>;
using SanitizerStartHook = Weak<__sanitizer_thread_start_hook>;

// TODO(https://fxbug.dev/478347581): Ideally &StartThread itself would be the
// entry PC value given to zx::thread::start.  It gets two arguments in
// registers, which are the user's function pointer and the void* to pass it.
//
// However, the normal ABI requires that both the thread pointer and the
// shadow-call-stack pointer be set; and zx::thread::start only sets the PC,
// SP, and the two argument registers.  So between those two registers and the
// stack, the other pointers must be communicated to the AsmTrampoline code,
// which must install them before tail-calling StartThread.  StartTrampoline
// manages all that.
//
// Moreover, there is a window, between calling zx::thread::start and the
// thread actually getting scheduled and getting through the AsmTrampoline
// code, where normal invariants don't hold.  In this window, the thread
// pointer register is zero.  The thread and its registers can be seen by
// __sanitizer_memory_snapshot.  But with the thread pointer not yet set to
// point to a Thread on the gAllThreads list, it will have only its registers
// and nothing else to take as pointer references owned by that thread.  In
// particular, the thread's stack won't be scanned, so anything stored only
// there and not in the registers will be overlooked by the snapshot.
//
// The user's function pointer and void* argument for it don't get stored
// anywhere but in the initial register values passed to zx::thread::start.
// Once the creating thread's zx::thread::start call returns from the kernel,
// those values may no longer be visible via the creating thread's own state.
// So it's crucial that they go directly into the new thread's registers where
// they will be seen.  The new Thread block doesn't yet have anything
// interesting in it, so it's fine if the snapshot doesn't consider _it_ yet.
// The same is true for the shadow call stack and the machine stack.  So
// StartTrampoline::Prepare() transfers _those_ pointers via the stack, but
// keeps the user's pointers in the two available registers.
//
// In future, the zx::thread::start API should allow setting the thread pointer
// and shadow-call-stack registers directly.  Then no trampoline would be
// required and the subtleties about pointers being visible to the snapshot
// logic would be much simpler.

// TODO(https://gcc.gnu.org/bugzilla/show_bug.cgi?id=123565):
// Should be unconditional; see above.
#if !(defined(__aarch64__) && !defined(__clang__))
// A new thread starts at the AsmTrampoline entry point defined below in
// assembly code.  That establishes normal ABI conditions by setting up the
// shadow call stack and thread pointers.  It then tail-calls this function.
// This is the visible outermost frame of the new thread and the direct caller
// of the user's ThreadFunction.
[[noreturn, clang::cfi_unchecked_callee]]
void AsmTrampoline(uintptr_t arg1, uintptr_t arg2);
#else
// This applies to StartThread and prevents the buggy GCC from deciding that
// there are no references to it and eliding the function (as well as warning
// about it, which already breaks the build before the undefined references at
// link time have a chance to).
[[gnu::used]]
#endif
[[noreturn]] void StartThread(ThreadFunction* func, void* arg) {
  Thread& self = *__pthread_self();

  // Note that the sanitizer_hook value is not stored anywhere else and is
  // never made visible to __sanitizer_memory_snapshot
  SanitizerStartHook::Call(self.sanitizer_hook, ToC11Thread(self));

  // The function and arg pointers are never live anywhere but in temporary
  // registers; __sanitizer_memory_snapshot() will find them in the registers
  // if this thread is suspended before now (including before it ever runs the
  // first instruction of AsmTrampoline).  But once the call to func begins, it
  // won't find a way to reach them unless the user code makes them reachable.
  ThreadExit(func(arg));
}

uint64_t* ThreadStackLimit(Thread& thread) {
  auto* base = reinterpret_cast<std::byte*>(thread.safe_stack.iov_base);
  std::byte* limit = base + thread.safe_stack.iov_len;
  return reinterpret_cast<uint64_t*>(limit);
}

class StartTrampoline {
 public:
  StartTrampoline() = delete;

  explicit StartTrampoline(Thread& thread) : thread_{thread} {}

  void Prepare(ThreadFunction* func, void* arg) {
    arg1_ = reinterpret_cast<uintptr_t>(func);
    arg2_ = reinterpret_cast<uintptr_t>(arg);
    *--sp_ = second_stack_value();
    *--sp_ = thread_pointer();
  }

  zx::result<> Start() const {
    uintptr_t entry = reinterpret_cast<uintptr_t>(AsmTrampoline);
    uintptr_t stack = reinterpret_cast<uintptr_t>(sp_);
    return zx::make_result(thread_handle()->start(entry, stack, arg1_, arg2_));
  }

 private:
  uintptr_t thread_pointer() const {
    void* tp = pthread_to_tp(&thread_);
    return reinterpret_cast<uintptr_t>(tp);
  }

  uint64_t second_stack_value() const {
#ifdef __x86_64__
    // On x86, the thread handle is needed to make a system call to install the
    // thread pointer, so pass it on the stack to make it easy.
    return thread_handle()->get();
#else
    // On other machines, the initial shadow call stack pointer goes there.
    const uintptr_t shadow_call_stack_sp =
        reinterpret_cast<uintptr_t>(thread_.shadow_call_stack.iov_base);
    // The first shadow call stack slot is left as zero so that a backtrace
    // can simply read downwards from the current shadow-call-stack pointer
    // and stop at the zero slot, without needing to know the base address to
    // avoid reading off the bottom.
    return shadow_call_stack_sp + sizeof(uintptr_t);
#endif
  }

  zx::unowned_thread thread_handle() const { return zx::unowned_thread{thread_.zxr_thread.handle}; }

  Thread& thread_;
  uint64_t* sp_ = ThreadStackLimit(thread_);
  uintptr_t arg1_ = 0;
  uintptr_t arg2_ = 0;
};

#if defined(__aarch64__)

// The thread pointer and shadow call stack register values are popped from the
// stack.  The thread pointer is put into place in TPIDR_EL0.
#ifdef __clang__
// GCC doesn't support [[gnu::naked]] functions for aarch64!  But it supports
// extended asm _outside functions_ that can provide C++ symbol definitions!
[[noreturn, clang::cfi_unchecked_callee,  //
  gnu::naked, gnu::no_profile_instrument_function]]
void AsmTrampoline(uintptr_t arg1, uintptr_t arg2) {
#endif
  __asm__(
#ifndef __clang__
      R"""(
      .pushsection .text.AsmTrampoline, "ax", %%progbits
      %cc[AsmTrampoline]:
      .cfi_startproc
      )"""
#endif
      R"""(
        .cfi_def_cfa_offset 16
        ldp x17, x18, [sp], #16
        .cfi_def_cfa_offset 0
        msr TPIDR_EL0, x17
        b %cc[StartThread]
      )"""
#ifndef __clang__
      R"""(
      .cfi_endproc
      .popsection
      )"""
#endif
      :
      :
#ifdef __clang__
      [StartThread] "X"(StartThread)
#else
    [StartThread] "-s"(StartThread), [AsmTrampoline] ":"(AsmTrampoline)
#endif
  );
#ifdef __clang__
}
#endif

#elif defined(__riscv)

// This closely matches the AArch64 version above.
[[noreturn, clang::cfi_unchecked_callee,  //
  gnu::naked, gnu::no_profile_instrument_function]]
void AsmTrampoline(uintptr_t arg1, uintptr_t arg2) {
  __asm__ volatile(
      R"""(
        .cfi_def_cfa_offset 16
        ld tp, 0(sp)
        ld gp, 8(sp)
        add sp, sp, 16
        .cfi_def_cfa_offset 0
        tail %cc[StartThread]
      )"""
      :
      : [StartThread] "s"(StartThread));
}

#elif defined(__x86_64__)

// This must call:
//   zx_object_set_property(%edi=handle, %esi=ZX_PROP_REGISTER_FS, %rdx=&value)

// The starting SP points to where thread_pointer() was stored by Prepare(), so
// that's &value.  The handle is stored above that at SP+8.  The incoming %rdi
// and %rdi arguments need to be preserved around the system call, so those go
// into call-saved registers.  Once they've been restored after the call, those
// two call-saved registers are rezeroed so that only the user's code might be
// keeping those pointers alive anywhere once we reach StartThread(), above.
// The calling convention expects SP to be -8 mod 16 with the return address
// from the call on the top of the stack.  So the incoming SP is adjusted back
// up only one word, and the TOS word zeroed before the jump to reflect an
// apparent zero return address as is the convention for the outermost frame.
[[noreturn, clang::cfi_unchecked_callee,  //
  gnu::naked, gnu::no_profile_instrument_function]]
void AsmTrampoline(uintptr_t arg1, uintptr_t arg2) {
  __asm volatile(
      R"""(
        .cfi_def_cfa_offset 16
        .cfi_undefined %%rip
        mov %%rdi, %%r12
        mov %%rsi, %%r13
        mov %%rsp, %%rdx
        mov %[sizeof_ptr], %%ecx
        mov 8(%%rsp), %%edi
        mov %[prop], %%esi
        call _zx_object_set_property@PLT
        test %%eax, %%eax
        jnz .Lfail.%=
        mov %%r12, %%rdi
        mov %%r13, %%rsi
        pop %%r12
        .cfi_adjust_cfa_offset -8
        xor %%r12, %%r12
        xor %%r13, %%r13
        mov %%r12, (%%rsp)
        .cfi_offset %%rip, -8
        jmp %cc[StartThread]
      .pushsection .text.cold, "ax?", %%progbits
      .Lfail.%=:
        ud2
      .popsection
      )"""
      :
      : [prop] "i"(ZX_PROP_REGISTER_FS), [sizeof_ptr] "i"(sizeof(uintptr_t)),
        [StartThread] "s"(StartThread));
}

#else

#error "unsupported machine"

#endif

// TODO(https://fxbug.dev/478347581): All of that should be replaced with:
//   thread.thread_handle()->start(&StartThread, sp, func, arg, tp, scsp)
zx::result<> StartKernelThread(Thread& thread, ThreadFunction* func, void* arg) {
  StartTrampoline trampoline{thread};
  trampoline.Prepare(func, arg);
  return trampoline.Start();
}

}  // namespace

zx::result<Thread*> ThreadStart(CreatedThread thread, ThreadFunction* func, void* arg) {
  // Extract these before the thread starts, since once it starts, it could
  // exit immediately; if detached, the pointer would become invalid then.
  void* const hook = thread->sanitizer_hook;
  const thrd_t thrd = ToC11Thread(*thread);

  // Include the new thread in the count of running threads before it starts,
  // so there is no window where it's running but not accounted for.
  __libc.thread_count.fetch_add(1);
  zx::result result = StartKernelThread(*thread, func, arg);

  // The sanitizer callback is made to pair with the before-create callback
  // even when the thread doesn't actually get started: the thrd_error argument
  // tells it to clean up for a thread creation that never actually happened.
  SanitizerCreateHook::Call(hook, thrd, C11ThreadError(result.status_value()));

  if (result.is_error()) {
    // If it didn't really start, don't count it as a live thread after all.
    [[maybe_unused]] int old_count = __libc.thread_count.fetch_sub(1);
    assert(old_count > 0);
    return result.take_error();
  }
  return zx::ok(thread.release());
}

// This gets called when a CreatedThread dies without successful ThreadStart().
// Just closing the thread handle destroys the kernel thread object, since it
// was never started.  The ThreadStorage is recovered and immediately destroyed
// to deallocate the stacks and thread block.
void CreatedThreadDeleter::operator()(Thread* thread) const {
  __thread_list_erase(thread);
  zx::thread{thread->zxr_thread.handle}.reset();
  auto storage = ThreadStorage::FromThread(*thread, true);
  thread->~Thread();
}

}  // namespace LIBC_NAMESPACE_DECL
