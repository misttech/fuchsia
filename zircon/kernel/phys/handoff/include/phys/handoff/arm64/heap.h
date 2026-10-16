// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_ARM64_HEAP_H_
#define ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_ARM64_HEAP_H_

#include <cstddef>

// Align the heap to 2MiB to optionally support large page mappings in it.
constexpr size_t kArchHeapAlignmentBits = 21;

#endif  // ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_ARM64_HEAP_H_
