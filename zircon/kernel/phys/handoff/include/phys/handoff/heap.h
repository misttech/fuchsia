// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_HEAP_H_
#define ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_HEAP_H_

// Arch-specific constants governing the kernel heap that physboot sets up and
// hands off.

#if defined(__aarch64__)
#include <phys/handoff/arm64/heap.h>
#elif defined(__riscv)
#include <phys/handoff/riscv64/heap.h>
#elif defined(__x86_64__)
#include <phys/handoff/x86/heap.h>
#endif

#endif  // ZIRCON_KERNEL_PHYS_HANDOFF_INCLUDE_PHYS_HANDOFF_HEAP_H_
