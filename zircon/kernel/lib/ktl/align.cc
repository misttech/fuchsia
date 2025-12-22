// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#include <ktl/align.h>
#include <ktl/byte.h>

// TODO(https://fxbug.dev/470118858): libc++ is moving to defining this as an
// inline.  This definition is required for the old version where it's not
// defined as inline.  But in the new version, `std::align` would be a
// redefinition since that inline will already be in scope.  So the definition
// uses manual mangling to define the C++ name without the compiler realizing
// that's what's being defined.

void* align_impl(size_t alignment, size_t size, void*& ptr, size_t& space) __asm__(
#ifdef _WIN32
    "?align@__ktl@std@@YAPEAX_K0AEAPEAXAEA_K@Z"
#elif defined(_LP64)
    "_ZNSt5__ktl5alignEmmRPvRm"
#else
    "_ZNSt5__ktl5alignEjjRPvRj"
#endif
);

[[gnu::weak]] void* align_impl(size_t alignment, size_t size, void*& ptr, size_t& space) {
  if (size > space) {
    return nullptr;
  }
  uintptr_t addr = reinterpret_cast<uintptr_t>(ptr);
  uintptr_t aligned_addr = (addr + alignment - 1) & -alignment;
  size_t skipped = aligned_addr - addr;
  if (skipped > space - size) {
    return nullptr;
  }
  ptr = reinterpret_cast<void*>(aligned_addr);
  space -= skipped;
  return ptr;
}
