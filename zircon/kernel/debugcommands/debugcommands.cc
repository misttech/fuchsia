// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <debug.h>
#include <inttypes.h>
#include <lib/instrumentation/asan.h>
#include <lib/thread-stack/abi.h>
#include <stdio.h>
#include <string.h>
#include <zircon/time.h>
#include <zircon/tls.h>
#include <zircon/types.h>

#include <kernel/thread.h>

#include "debugcommands_ffi.h"

static void BeforeStackOverflow(const char* what) {
  const auto& stack = Thread::Current::Get()->stack();
  printf("%s from %p in [%#" PRIxPTR ", %#" PRIxPTR ") $tp %p stack_guard %#" PRIx64 "...\n", what,
         __builtin_frame_address(0), stack.base(), stack.top(),
         arch_get_current_compiler_thread_pointer(),
         *reinterpret_cast<const uint64_t*>(arch_get_current_compiler_thread_pointer() +
                                            ZX_TLS_STACK_GUARD_OFFSET));
}

__attribute__((noinline)) static void stomp_stack(size_t size) {
  // -Wvla prevents VLAs but not explicit alloca.
  // Neither is allowed anywhere in the kernel outside this test code.
  void* death = __builtin_alloca(size);  // OK in test-only code.
  memset(death, 0xdb, size);
  Thread::Current::SleepRelative(ZX_USEC(1));
}

static int crash_stackstomp() {
  BeforeStackOverflow("Stomping stack");

  for (size_t i = 0; i < internal::kMachineStackSize * 2; i++) {
    stomp_stack(i);
  }

  printf("survived.\n");

  return 0;
}

static int crash_shadow_call_stack_stomp() {
#if !__has_feature(shadow_call_stack)
  printf("Shadow call stack not in use.\n");
  return -1;
#else
  BeforeStackOverflow("Overflowing shadow call stack");

  // Just warp it straight to the edge so the next spill will overflow.
  const uint64_t top = Thread::Current::Get()->stack().shadow_call_top();
#if defined(__aarch64__)
  __asm__ volatile("mov x18, %0" : : "r"(top));
#elif defined(__riscv)
  __asm__ volatile("mv gp, %0" : : "r"(top));
#endif

  // Cause some calls that definitely use the shadow call stack.
  Thread::Current::SleepRelative(ZX_USEC(1));

  printf("survived.\n");

  return 0;
#endif
}

// Marked with NO_ASAN because this will be called with a pointer to user memory.
NO_ASAN static uint8_t read_byte(const uint8_t* p) { return *p; }

static int crash_assert() {
  constexpr int kValue = 42;
  ASSERT_MSG(kValue == 0, "value %d\n", kValue);
  return -1;
}

static int crash_stack_guard() {
  printf("attempting to crash\n");
  // Attempt to crash by overwriting the compiler-inserted stack guard.
  constexpr size_t kSize = 128;
  // alloca should never be used outside of test code.
  char* p = static_cast<char*>(__builtin_alloca(kSize));
  memset(p, 0x45, 2 * kSize);
  return -1;
}

extern "C" {

void cpp_before_stack_overflow(const char* what) { BeforeStackOverflow(what); }

int cpp_crash_stackstomp() { return crash_stackstomp(); }

int cpp_crash_shadow_call_stack_stomp() { return crash_shadow_call_stack_stomp(); }

// Marked with NO_ASAN (and not FFI_ALWAYS_INLINE) because this will be called with a pointer to
// user memory.
NO_ASAN uint8_t cpp_read_byte(const uint8_t* p) { return read_byte(p); }

int cpp_crash_assert() { return crash_assert(); }

int cpp_crash_stack_guard() { return crash_stack_guard(); }

}  // extern "C"
