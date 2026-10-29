// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_MSR_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_MSR_FFI_H_

#include <stdint.h>
#include <zircon/compiler.h>

#include <kernel/cpu.h>

// TODO(https://fxbug.dev/540178114): Remove once platform_tests.cc is ported to Rust.

__BEGIN_CDECLS

uint64_t rust_x86_read_msr_on_cpu(cpu_num_t cpu, uint32_t msr_id);
void rust_x86_write_msr_on_cpu(cpu_num_t cpu, uint32_t msr_id, uint64_t val);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_MSR_FFI_H_
