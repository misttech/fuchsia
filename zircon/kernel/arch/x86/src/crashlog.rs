// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Architectural crashlog register formatting for x86_64.

use core::fmt::Write;
use libc::stdio::FILE;

use super::Iframe;

/// Hardware crashlog registers capturing CPU exception state on x86_64 (`crashlog_regs_t`).
pub(super) use x86_crashlog_bindings::crashlog_regs_t as CrashlogRegs;

// `regs.iframe` is read as an `Iframe`, whose field offsets mod.rs pins to `iframe_t`'s.
zr::static_assert!(
    core::mem::size_of::<x86_crashlog_bindings::iframe_t>() == core::mem::size_of::<Iframe>()
);
zr::static_assert!(
    core::mem::align_of::<x86_crashlog_bindings::iframe_t>() == core::mem::align_of::<Iframe>()
);

#[cold]
#[inline(never)]
fn write_hex_reg(w: &mut FILE, name: &str, val: u64) -> core::fmt::Result {
    if val == 0 {
        writeln!(w, "{:>13}:                  0", name)
    } else {
        writeln!(w, "{:>13}: {:#18x}", name, val)
    }
}

/// Render the architecture-specific details of an [`Iframe`] in a fashion suitable for a crashlog
/// into the specified `FILE` target.
///
/// # Safety
///
/// The caller must ensure that if `regs.iframe` is non-null, it is properly aligned for [`Iframe`],
/// points to a live (not freed) [`Iframe`] object, and remains valid for shared reads
/// (no concurrent mutation) for the duration of the call.
#[cold]
#[sanitize(address = "off")]
pub(super) unsafe fn arch_render_crashlog_registers(target: &mut FILE, regs: &CrashlogRegs) {
    if regs.iframe.is_null() {
        let _ = write!(target, "missing");
        return;
    }
    // SAFETY: Null was ruled out above, and the caller guarantees a non-null `regs.iframe` is
    // aligned, initialized, points to a live (not freed) object, and is valid for shared reads for
    // the duration of the call. We access fields directly through the raw pointer without
    // materializing a reference to avoid triggering faults if the stack frame is poisoned.
    let iframe = regs.iframe.cast::<Iframe>();
    let _ = (|| -> core::fmt::Result {
        // SAFETY: `iframe` is non-null and valid for reads as documented above.
        unsafe {
            write_hex_reg(target, "CS", (*iframe).cs)?;
            write_hex_reg(target, "RIP", (*iframe).ip)?;
            write_hex_reg(target, "EFL", (*iframe).flags)?;
            write_hex_reg(target, "CR2", regs.cr2)?;
            write_hex_reg(target, "RAX", (*iframe).rax)?;
            write_hex_reg(target, "RBX", (*iframe).rbx)?;
            write_hex_reg(target, "RCX", (*iframe).rcx)?;
            write_hex_reg(target, "RDX", (*iframe).rdx)?;
            write_hex_reg(target, "RSI", (*iframe).rsi)?;
            write_hex_reg(target, "RDI", (*iframe).rdi)?;
            write_hex_reg(target, "RBP", (*iframe).rbp)?;
            write_hex_reg(target, "RSP", (*iframe).user_sp)?;
            write_hex_reg(target, "R8", (*iframe).r8)?;
            write_hex_reg(target, "R9", (*iframe).r9)?;
            write_hex_reg(target, "R10", (*iframe).r10)?;
            write_hex_reg(target, "R11", (*iframe).r11)?;
            write_hex_reg(target, "R12", (*iframe).r12)?;
            write_hex_reg(target, "R13", (*iframe).r13)?;
            write_hex_reg(target, "R14", (*iframe).r14)?;
            write_hex_reg(target, "R15", (*iframe).r15)?;
            write_hex_reg(target, "vector", (*iframe).vector)?;
            write_hex_reg(target, "errc", (*iframe).err_code)?;
            write_hex_reg(target, "fsbase", regs.fsbase)?;
            write_hex_reg(target, "gsbase", regs.gsbase)?;
            write_hex_reg(target, "swapgs gsbase", regs.swapgs_gsbase)?;
            writeln!(target)
        }
    })();
}
