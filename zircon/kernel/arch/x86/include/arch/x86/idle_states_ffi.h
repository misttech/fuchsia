// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

// TODO(https://fxbug.dev/540178114): Remove once remaining C++ callers (mp.cc, intel.cc,
// idle_states_test.cc) are ported to Rust.

struct x86_idle_states;
class X86IdleState;
class X86IdleStates;

__BEGIN_CDECLS

const struct x86_idle_states* rust_x86_get_idle_states();
const char* rust_x86_idle_state_name(const X86IdleState* idle_state);
uint32_t rust_x86_idle_state_mwait_hint(const X86IdleState* idle_state);
void rust_x86_idle_state_count_entry(const X86IdleState* idle_state);
void rust_x86_idle_state_record_duration(const X86IdleState* idle_state, zx_duration_t duration);
void rust_x86_idle_states_init(X86IdleStates* idle_states, const struct x86_idle_states* states);
const X86IdleState* rust_x86_idle_states_states(const X86IdleStates* idle_states);
size_t rust_x86_idle_states_num_states(const X86IdleStates* idle_states);
const X86IdleState* rust_x86_idle_states_pick_idle_state(const X86IdleStates* idle_states);
void rust_x86_idle_states_record_duration(const X86IdleStates* idle_states, zx_duration_t duration);
void rust_x86_idle_states_set_state_mask(const X86IdleStates* idle_states, uint32_t mask);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_FFI_H_
