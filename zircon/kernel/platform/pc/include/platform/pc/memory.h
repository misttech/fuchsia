// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_MEMORY_H_
#define ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_MEMORY_H_

#include <stddef.h>

#include <ktl/span.h>

// Forward declaration; defined in <lib/memalloc/range.h>
namespace memalloc {
struct Range;
}

// TODO(https://fxbug.dev/576428709): Remove FFI wrapper once platform.cc is ported to Rust.
extern "C" void rust_pc_mem_init(const memalloc::Range* ranges, size_t count);

inline void pc_mem_init(ktl::span<const memalloc::Range> ranges) {
  rust_pc_mem_init(ranges.data(), ranges.size());
}

#endif  // ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_MEMORY_H_
