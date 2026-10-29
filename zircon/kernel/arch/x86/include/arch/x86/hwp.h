// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_

#include <lib/boot-options/x86.h>  // IntelHwpPolicy

#include <arch/x86/hwp_ffi.h>

namespace x86 {

using IntelHwpPolicy = IntelHwpPolicy;

static_assert(sizeof(IntelHwpPolicy) == 1);
static_assert(alignof(IntelHwpPolicy) == 1);

// Initialise the Intel HWP on the current CPU.
//
// If HWP is not supported on the current CPU, no action will be taken.
inline void IntelHwpInit(IntelHwpPolicy policy) { rust_x86_intel_hwp_init(policy); }

// Determine if Intel HWP is supported on the current CPU.
inline bool IntelHwpSupported() { return rust_x86_intel_hwp_supported(); }

}  // namespace x86

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_
