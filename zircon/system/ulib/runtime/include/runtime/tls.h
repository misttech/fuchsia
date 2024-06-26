// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <zircon/compiler.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

__BEGIN_CDECLS

// Get and set the thread pointer.
static inline void* zxr_tp_get(void);
static inline void zxr_tp_set(zx_handle_t self, void* tp);

// These are used in very early and low-level places where most kinds
// of instrumentation are not safe.
#ifdef __clang__
#define ZXR_NO_SANITIZERS __attribute__((no_sanitize("all")))
#else
#define ZXR_NO_SANITIZERS
#endif

// These are tiny functions meant to be inlined, where a call would often
// actually take more instruction bytes than just inlining it.  However,
// Clang avoids inlining across functions with differing attributes and the
// necessary `no_sanitize` attribute trips that logic.  So tell the
// compiler to force inlining here.
#define ZXR_TLS_INLINE ZXR_NO_SANITIZERS __attribute__((always_inline))

#if defined(__aarch64__)

ZXR_TLS_INLINE static inline void* zxr_tp_get(void) {
  // This just emits "mrs %[reg], tpidr_el0", but the compiler
  // knows what exactly it's doing (unlike an asm).  So it can
  // e.g. CSE it with another implicit thread-pointer fetch it
  // generated for its own reasons.
  return __builtin_thread_pointer();
}

ZXR_TLS_INLINE static inline void zxr_tp_set(zx_handle_t self, void* tp) {
  __asm__ volatile("msr tpidr_el0, %0" : : "r"(tp));
}

#elif defined(__x86_64__)

ZXR_TLS_INLINE static inline void* zxr_tp_get(void) {
// This fetches %fs:0, but the compiler knows what it's doing.
// LLVM knows that in the Fuchsia ABI %fs:0 always stores the
// %fs.base address, and its optimizer will see through this
// to integrate *(zxr_tp_get() + N) as a direct "mov %fs:N, ...".
// Note that these special pointer types can be used to access
// memory, but they cannot be cast to a normal pointer type
// (which in the abstract should add in the base address,
// but the compiler doesn't know how to do that).
#ifdef __clang__
  // Clang does it via magic address_space numbers (256 is %gs).
  void* __attribute__((address_space(257)))* fs = 0;
  // TODO(mcgrathr): GCC 6 supports this syntax instead (and __seg_gs):
  //     void* __seg_fs* fs = 0;
  // Unfortunately, it allows it only in C and not in C++.
  // It also requires -fasm under -std=c11 (et al), see:
  //     https://gcc.gnu.org/bugzilla/show_bug.cgi?id=79609
  // It's also buggy for the special case of 0, see:
  //     https://gcc.gnu.org/bugzilla/show_bug.cgi?id=79619
  return *fs;
#else
  void* tp;
  __asm__ __volatile__("mov %%fs:0,%0" : "=r"(tp));
  return tp;
#endif
}

ZXR_TLS_INLINE static inline void zxr_tp_set(zx_handle_t self, void* tp) {
  zx_status_t status =
      _zx_object_set_property(self, ZX_PROP_REGISTER_FS, (uintptr_t*)&tp, sizeof(uintptr_t));
  if (status != ZX_OK)
    __builtin_trap();
}

#elif defined(__riscv)

ZXR_TLS_INLINE static inline void* zxr_tp_get(void) { return __builtin_thread_pointer(); }

ZXR_TLS_INLINE static inline void zxr_tp_set(zx_handle_t self, void* tp) {
  __asm__ volatile("mv tp, %0" : : "r"(tp));
}

#else

#error Unsupported architecture

#endif

__END_CDECLS
