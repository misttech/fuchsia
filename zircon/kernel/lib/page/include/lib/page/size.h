// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_PAGE_INCLUDE_LIB_PAGE_SIZE_H_
#define ZIRCON_KERNEL_LIB_PAGE_INCLUDE_LIB_PAGE_SIZE_H_

#include <bit>
#include <cstddef>

#ifndef LIBPAGE_PAGE_SIZE
#error "LIBPAGE_PAGE_SIZE not defined??"
#endif

constexpr size_t kPageSize = LIBPAGE_PAGE_SIZE;

#undef LIBPAGE_PAGE_SIZE

#if defined(__aarch64__)
static_assert(kPageSize == 0x1000 || kPageSize == 0x4000,
              "Valid arm64 page sizes: 4KiB and (experimentally) 16KiB");
#elif defined(__riscv)
static_assert(kPageSize == 0x1000, "Valid riscv64 page sizes: 4KiB");
#elif defined(__x86_64__)
static_assert(kPageSize == 0x1000, "Valid x86 page sizes: 4KiB");
#else
#error Unsupported architecture
#endif

constexpr size_t kPageShift = std::countr_zero(kPageSize);

constexpr uintptr_t kPageMask = uintptr_t{kPageSize} - 1;

constexpr bool IsPageRounded(uintptr_t x) { return (x & kPageMask) == 0; }

constexpr uintptr_t RoundDownPageSize(uintptr_t x) { return x & -uintptr_t{kPageSize}; }

constexpr uintptr_t RoundUpPageSize(uintptr_t x) {
  return (x + uintptr_t{kPageSize} - 1) & -uintptr_t{kPageSize};
}

#endif  // ZIRCON_KERNEL_LIB_PAGE_INCLUDE_LIB_PAGE_SIZE_H_
