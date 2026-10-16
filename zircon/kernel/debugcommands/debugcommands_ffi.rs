// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int};

unsafe extern "C" {
    pub(super) fn cpp_before_stack_overflow(what: *const c_char);
    pub(super) fn cpp_crash_stackstomp() -> c_int;
    pub(super) fn cpp_crash_shadow_call_stack_stomp() -> c_int;
    #[cfg(not(target_arch = "aarch64"))]
    pub(super) fn cpp_read_byte(p: *const u8) -> u8;
    pub(super) fn cpp_crash_assert() -> c_int;
    pub(super) fn cpp_crash_stack_guard() -> c_int;
    pub(super) fn arch_sync_cache_range(start: usize, len: usize);
}

#[cfg(ubsan)]
unsafe extern "C" {
    pub(super) fn cpp_ub_array_oob();
    pub(super) fn cpp_ub_invalid_builtin_clz();
    pub(super) fn cpp_ub_invalid_builtin_ctz();
    pub(super) fn cpp_ub_misaligned_ptr();
    pub(super) fn cpp_ub_nonnull_return();
    pub(super) fn cpp_ub_nullability_return();
    pub(super) fn cpp_ub_overflow_ptr();
    pub(super) fn cpp_ub_overflow_signed_int_add();
    pub(super) fn cpp_ub_overflow_signed_int_shift();
    pub(super) fn cpp_ub_unaligned_assumption();
    pub(super) fn cpp_ub_undefined_enum();
    pub(super) fn cpp_ub_undefined_bool();
    pub(super) fn cpp_ub_unreachable();
}
