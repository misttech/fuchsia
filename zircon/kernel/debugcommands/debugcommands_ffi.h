// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEBUGCOMMANDS_DEBUGCOMMANDS_FFI_H_
#define ZIRCON_KERNEL_DEBUGCOMMANDS_DEBUGCOMMANDS_FFI_H_

#include <stdint.h>

extern "C" {

void cpp_before_stack_overflow(const char* what);
int cpp_crash_stackstomp();
int cpp_crash_shadow_call_stack_stomp();
uint8_t cpp_read_byte(const uint8_t* p);
int cpp_crash_assert();
int cpp_crash_stack_guard();

#if __has_feature(undefined_behavior_sanitizer)
void cpp_ub_array_oob();
void cpp_ub_invalid_builtin_clz();
void cpp_ub_invalid_builtin_ctz();
void cpp_ub_misaligned_ptr();
void cpp_ub_nonnull_return();
void cpp_ub_nullability_return();
void cpp_ub_overflow_ptr();
void cpp_ub_overflow_signed_int_add();
void cpp_ub_overflow_signed_int_shift();
void cpp_ub_unaligned_assumption();
void cpp_ub_undefined_enum();
void cpp_ub_undefined_bool();
void cpp_ub_unreachable();
#endif  // __has_feature(undefined_behavior_sanitizer)

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEBUGCOMMANDS_DEBUGCOMMANDS_FFI_H_
