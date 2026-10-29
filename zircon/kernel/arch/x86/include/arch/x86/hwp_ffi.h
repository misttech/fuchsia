// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_FFI_H_

#include <lib/boot-options/x86.h>  // IntelHwpPolicy
#include <zircon/compiler.h>

__BEGIN_CDECLS

void rust_x86_intel_hwp_init(IntelHwpPolicy policy);
bool rust_x86_intel_hwp_supported();

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_FFI_H_
