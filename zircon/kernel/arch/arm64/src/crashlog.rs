// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Architectural crashlog register rendering for arm64.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use libarch::arm64::{ExceptionClass, ExceptionSyndromeRegister};
use libc::stdio::FILE;
use zx_status::Status;

use super::Iframe;
use super::vm::is_kernel_address;
use crate::kernel::types::{PAddr, VAddr};

/// Register state captured for the crashlog (`crashlog_regs_t`).
type CrashlogRegs = arm64_crashlog_bindings::crashlog_regs_t;

// `regs.iframe` is read as an `Iframe`, whose field offsets mod.rs pins to `iframe_t`'s.
zr::static_assert!(size_of::<arm64_crashlog_bindings::iframe_t>() == size_of::<Iframe>());
zr::static_assert!(align_of::<arm64_crashlog_bindings::iframe_t>() == align_of::<Iframe>());

unsafe extern "C" {
    // Bounds of the kernel's code, from kernel.ld. Only their addresses are used.
    static __code_start: [u8; 0];
    static __code_end: [u8; 0];

    fn arm64_mmu_translate(
        va: VAddr,
        pa: &mut PAddr,
        user: bool,
        write: bool,
    ) -> Result<(), Status>;
}

/// Labels of `Iframe::r`, x0-x29.
const GPR_LABELS: [&str; 30] = [
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14",
    "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27",
    "x28", "x29",
];

static SUPPRESS_ELR_DUMP: AtomicBool = AtomicBool::new(false);

/// Formats a register as the kernel printf's `%#18x` does: zero prints as a bare `0`, without
/// the `0x` prefix.
struct AltHex18(u64);

impl fmt::Display for AltHex18 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 { write!(f, "{:18}", 0) } else { write!(f, "{:#18x}", self.0) }
    }
}

/// Renders the registers in `regs` to `target` for a crashlog and, for some exception classes,
/// the instruction words around ELR.
///
/// # Safety
///
/// `regs.iframe` must be null or point at an `iframe_t` that stays live, and is not written,
/// for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_render_crashlog_registers(
    target: &mut FILE,
    regs: &CrashlogRegs,
) {
    if regs.iframe.is_null() {
        let _ = writeln!(target, "missing");
        return;
    }

    // SAFETY: `regs.iframe` is non-null, so the caller guarantees it points at a live `iframe_t`
    // that nothing writes during this call. `Iframe` has `iframe_t`'s layout: mod.rs pins its size
    // and every field offset to the values regs.h asserts for `iframe_t`, and the asserts above
    // tie it to the binding's size and alignment.
    let iframe = unsafe { &*regs.iframe.cast::<Iframe>() };

    // Labels are right-aligned to the width of the longest, `tpidrro_el0`.
    for (label, &value) in GPR_LABELS.iter().zip(&iframe.r) {
        let _ = writeln!(target, "{label:>11}: {}", AltHex18(value));
    }
    let _ = writeln!(target, "{:>11}: {}", "lr", AltHex18(iframe.lr));
    let _ = writeln!(target, "{:>11}: {}", "usp", AltHex18(iframe.usp));
    let _ = writeln!(target, "{:>11}: {}", "elr", AltHex18(iframe.elr));
    let _ = writeln!(target, "{:>11}: {}", "spsr", AltHex18(iframe.spsr));
    let _ = writeln!(target, "{:>11}: {}", "esr", AltHex18(u64::from(regs.esr)));
    let _ = writeln!(target, "{:>11}: {}", "far", AltHex18(regs.far));
    let _ = writeln!(target, "{:>11}: {}", "tpidr_el0", AltHex18(regs.tpidr_el0));
    let _ = writeln!(target, "{:>11}: {}", "tpidr_el1", AltHex18(regs.tpidr_el1));
    let _ = writeln!(target, "{:>11}: {}", "tpidrro_el0", AltHex18(regs.tpidrro_el0));
    let _ = writeln!(target);

    // Depending on the exception class, conditionally make an attempt to print
    // out some of the memory immediately surrounding the ELR.  Particularly for
    // an UnknownException, we would like to know whether or not the exception
    // was taken because we happened to fetch an instruction which had been
    // corrupted (somehow).
    //
    // Note, do not attempt to do this if we have even attempted to read from
    // this memory before.  We absolutely do not want to be in a situation where
    // we attempt to read the memory that the system faulted on, and we just
    // fault again for some unknown reason, putting us into an infinite fault
    // recursion situation.
    if SUPPRESS_ELR_DUMP.load(Ordering::SeqCst) {
        let _ = writeln!(target, "ELR region dumping suppressed");
        return;
    }

    let esr_reg = ExceptionSyndromeRegister::from(u64::from(regs.esr));

    if let Ok(ExceptionClass::Unknown | ExceptionClass::DataAbortSameEl) = esr_reg.try_ec() {
        // Set our suppression bit before we attempt any accesses.
        SUPPRESS_ELR_DUMP.store(true, Ordering::SeqCst);

        if iframe.elr & 0x3 != 0 {
            let _ = write!(target, "no ELR memory dump, misaligned ELR\n\n");
        } else {
            for offset in [-4i32, 0, 4] {
                let mut paddr = PAddr(0);

                let Some(vaddr) = iframe.elr.checked_add_signed(i64::from(offset)) else {
                    let _ = writeln!(target, "ELR[{offset:+}] = over/underflow");
                    continue;
                };
                let vaddr = VAddr::from(vaddr);

                if !is_kernel_address(vaddr.0) {
                    let _ = writeln!(target, "ELR[{offset:+}] = non-kernel");
                } else if
                // SAFETY: `arm64_mmu_translate` accepts any `va`.
                unsafe {
                    arm64_mmu_translate(
                        vaddr, &mut paddr, /* user */ false, /* write */ false,
                    )
                }
                .is_err()
                {
                    let _ = writeln!(target, "ELR[{offset:+}] = bad addr");
                } else {
                    let ptr = core::ptr::with_exposed_provenance::<u32>(vaddr.0);
                    // SAFETY: `ptr` is 4-byte aligned: ELR is (the misaligned case is handled
                    // above) and `offset` is a multiple of 4. It is a kernel address that
                    // `arm64_mmu_translate` just reported mapped for EL1 reads. A volatile read
                    // forms no reference, so the mapping may be read-only text or device memory.
                    let word = unsafe { core::ptr::read_volatile(ptr) };
                    let in_text = vaddr.0 >= (&raw const __code_start).addr()
                        && vaddr.0 < (&raw const __code_end).addr();
                    let _ = writeln!(
                        target,
                        "ELR[{offset:+}] = 0x{word:08x}{}",
                        if in_text { "" } else { " (not in .text)" }
                    );
                }
            }
            let _ = writeln!(target);
        }
    }
}
