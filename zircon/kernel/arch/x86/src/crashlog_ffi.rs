// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use libc::stdio::FILE;

use super::crashlog::CrashlogRegs;

/// FFI entry point for [`arch_render_crashlog_registers`].
///
/// # Safety
///
/// The caller must ensure that if `regs.iframe` is non-null, it is properly aligned for
/// [`super::Iframe`], points to a live (not freed) [`super::Iframe`] object, and remains valid for
/// shared reads (no concurrent mutation) for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_render_crashlog_registers(target: &mut FILE, regs: &CrashlogRegs) {
    // SAFETY: Caller guarantees `regs.iframe` is null or aligned, points to a live (not freed)
    // object, and is valid for shared reads for the duration of the call.
    unsafe { super::crashlog::arch_render_crashlog_registers(target, regs) };
}
