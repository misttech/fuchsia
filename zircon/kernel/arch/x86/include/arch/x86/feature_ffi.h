// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_

#include <stdbool.h>
#include <stdint.h>
#include <zircon/compiler.h>

class X86IdleState;
struct x86_model_info;

// TODO(https://fxbug.dev/540178114): Remove once feature.cc is ported to Rust.
__BEGIN_CDECLS

void rust_x86_amd_init_percpu();
void rust_x86_intel_init_percpu();
bool rust_x86_intel_cpu_has_rsb_fallback();
uint32_t rust_x86_amd_get_patch_level();
uint32_t rust_x86_intel_get_patch_level();
bool rust_x86_amd_has_retbleed();
void rust_x86_amd_zen2_retbleed_mitigation(const x86_model_info* model);
bool rust_x86_intel_idle_state_may_empty_rsb(const X86IdleState* state);
const void* cpp_x86_get_microarch_config(bool is_intel, bool is_amd, uint16_t family,
                                         uint16_t model);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_
