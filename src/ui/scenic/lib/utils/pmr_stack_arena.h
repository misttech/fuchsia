// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_UTILS_PMR_STACK_ARENA_H_
#define SRC_UI_SCENIC_LIB_UTILS_PMR_STACK_ARENA_H_

#include <lib/syslog/cpp/macros.h>

#include <bit>
#include <cstddef>
#include <memory_resource>

#if !__has_declspec_attribute(no_init_all)
#error "PmrStackArena requires -fdeclspec for __declspec(no_init_all)"
#endif

namespace utils {

// A stack-allocated PMR monotonic arena of `Size` bytes aligned to `Alignment`.
//
// Because Fuchsia builds with `-ftrivial-auto-var-init=pattern`, a raw stack `std::array` or byte
// array used to back a `std::pmr::monotonic_buffer_resource` would be `memset(0xAA)`-filled on
// every function entry. `PmrStackArena` is annotated with `__declspec(no_init_all)` (enabled by
// `-fdeclspec` in `//src/ui/scenic/lib/utils:pmr_stack_arena_config`), which instructs Clang to
// skip automatic pattern-initialization for local variables of this class while still running the
// constructor to initialize `resource_`.
//
// `PmrStackArena` must be declared directly as a local stack variable (not on the heap, and not as
// a member of an unannotated struct or class, because Clang checks `__declspec(no_init_all)` only
// on the declared variable's type). `upstream` must be non-null and outlive the arena, and the
// arena must be declared before (and outlive) all containers using `resource()`. `resource()` is
// not thread-safe.
//
// The raw backing storage `buffer_` is kept private so callers can only access it by allocating and
// constructing objects through the `std::pmr::monotonic_buffer_resource`.
template <size_t Size, size_t Alignment = alignof(std::max_align_t)>
class __declspec(no_init_all) PmrStackArena {
  // `std::pmr::monotonic_buffer_resource` bump-allocates downward from `buffer_ + Size`, so both
  // `buffer_` and `buffer_ + Size` must be aligned to `Alignment` (and at least
  // `alignof(std::max_align_t)`) to avoid wasting trailing bytes or spilling full-capacity
  // allocations to `upstream`.
  static_assert(Size > 0, "PmrStackArena Size must be greater than 0.");
  static_assert(std::has_single_bit(Alignment), "PmrStackArena Alignment must be a power of 2.");
  static_assert(Alignment >= alignof(std::max_align_t),
                "PmrStackArena Alignment must be at least alignof(std::max_align_t).");
  static_assert(Size % Alignment == 0, "PmrStackArena Size must be a multiple of Alignment.");

 public:
  explicit PmrStackArena(std::pmr::memory_resource* upstream = std::pmr::get_default_resource())
      : resource_(buffer_, Size, upstream) {
    FX_DCHECK(upstream);
  }

  PmrStackArena(const PmrStackArena&) = delete;
  PmrStackArena& operator=(const PmrStackArena&) = delete;
  PmrStackArena(PmrStackArena&&) = delete;
  PmrStackArena& operator=(PmrStackArena&&) = delete;

  std::pmr::monotonic_buffer_resource* resource() & noexcept [[clang::lifetimebound]] {
    return &resource_;
  }
  std::pmr::monotonic_buffer_resource* resource() && = delete;

 private:
  alignas(Alignment) std::byte buffer_[Size];
  std::pmr::monotonic_buffer_resource resource_;
};

}  // namespace utils

#endif  // SRC_UI_SCENIC_LIB_UTILS_PMR_STACK_ARENA_H_
