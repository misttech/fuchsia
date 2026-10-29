// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_SMP_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_SMP_FFI_H_

#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// TODO(https://fxbug.dev/540178114): Remove once callers in arch/x86/mp.cc and
// platform/pc/platform.cc are ported to Rust.
void rust_x86_init_smp(uint32_t* apic_ids, uint32_t num_cpus);
zx_status_t rust_x86_bringup_aps(uint32_t* apic_ids, uint32_t count);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_SMP_FFI_H_
