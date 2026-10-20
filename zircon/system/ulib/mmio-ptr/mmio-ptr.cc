// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/mmio-ptr/mmio-ptr.h>

// These wrappers exist so that Rust can reach the MMIO accessors over FFI, and
// they must inline into their Rust callers for that to cost nothing:
//
//  * The parameters are plain pointers, and `MMIO_PTR` is re-applied inside.
//    The Rust declarations cannot express `MMIO_PTR`'s address space, and LLVM
//    does not inline through a call whose function type does not match the
//    callee's.
//
//  * They are `always_inline`. The kernel builds C++ with `-ffreestanding`,
//    which marks every function `no-builtins`, and LLVM only inlines such a
//    function into a caller without that attribute (any Rust function) when
//    the callee is `always_inline`. GCC rejects the attribute on a non-inline
//    definition, so it is applied only under Clang.
//
// TODO(https://fxbug.dev/537458631): Drop the attribute once cross-language
// inlining no longer needs it.
#ifdef __clang__
#define MMIO_PTR_FFI_INLINE [[gnu::always_inline]]
#else
#define MMIO_PTR_FFI_INLINE
#endif

extern "C" {

uint8_t mmio_read8(const volatile uint8_t* buffer);
void mmio_write8(uint8_t data, volatile uint8_t* buffer);

uint16_t mmio_read16(const volatile uint16_t* buffer);
void mmio_write16(uint16_t data, volatile uint16_t* buffer);

uint32_t mmio_read32(const volatile uint32_t* buffer);
void mmio_write32(uint32_t data, volatile uint32_t* buffer);

#ifdef _LP64
uint64_t mmio_read64(const volatile uint64_t* buffer);
void mmio_write64(uint64_t data, volatile uint64_t* buffer);
#endif

MMIO_PTR_FFI_INLINE uint8_t mmio_read8(const volatile uint8_t* buffer) {
  return MmioRead8((MMIO_PTR const volatile uint8_t*)buffer);
}

MMIO_PTR_FFI_INLINE void mmio_write8(uint8_t data, volatile uint8_t* buffer) {
  MmioWrite8(data, (MMIO_PTR volatile uint8_t*)buffer);
}

MMIO_PTR_FFI_INLINE uint16_t mmio_read16(const volatile uint16_t* buffer) {
  return MmioRead16((MMIO_PTR const volatile uint16_t*)buffer);
}

MMIO_PTR_FFI_INLINE void mmio_write16(uint16_t data, volatile uint16_t* buffer) {
  MmioWrite16(data, (MMIO_PTR volatile uint16_t*)buffer);
}

MMIO_PTR_FFI_INLINE uint32_t mmio_read32(const volatile uint32_t* buffer) {
  return MmioRead32((MMIO_PTR const volatile uint32_t*)buffer);
}

MMIO_PTR_FFI_INLINE void mmio_write32(uint32_t data, volatile uint32_t* buffer) {
  MmioWrite32(data, (MMIO_PTR volatile uint32_t*)buffer);
}

#ifdef _LP64
MMIO_PTR_FFI_INLINE uint64_t mmio_read64(const volatile uint64_t* buffer) {
  return MmioRead64((MMIO_PTR const volatile uint64_t*)buffer);
}

MMIO_PTR_FFI_INLINE void mmio_write64(uint64_t data, volatile uint64_t* buffer) {
  MmioWrite64(data, (MMIO_PTR volatile uint64_t*)buffer);
}
#endif

}  // extern "C"
