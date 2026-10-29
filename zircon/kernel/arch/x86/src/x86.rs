// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_status::Status;
use zx_types::zx_status_t;

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inp(port: u16) -> u8 {
    let value: u8;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inpw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in ax, dx",
            in("dx") port,
            out("ax") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inpd(port: u16) -> u32 {
    let value: u32;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outp(port: u16, value: u8) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outpw(port: u16, value: u16) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outpd(port: u16, value: u32) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads a 64-bit Model Specific Register (MSR).
///
/// # Safety
///
/// Caller must ensure that `msr_id` is a valid MSR supported by the current CPU.
/// Reading an unsupported MSR will cause a General Protection Fault (#GP).
pub unsafe fn read_msr(msr_id: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: rdmsr reads the MSR specified in ecx into edx:eax. Caller guarantees `msr_id` is valid.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr_id,
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Reads the low 32 bits of a Model Specific Register (MSR).
///
/// # Safety
///
/// Caller must ensure that `msr_id` is a valid MSR supported by the current CPU.
/// Reading an unsupported MSR will cause a General Protection Fault (#GP).
pub unsafe fn read_msr32(msr_id: u32) -> u32 {
    let low: u32;
    // SAFETY: rdmsr reads the MSR specified in ecx into edx:eax. Caller guarantees `msr_id` is valid.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr_id,
            out("eax") low,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    low
}

/// Writes a 64-bit value to a Model Specific Register (MSR).
///
/// # Safety
///
/// Caller must ensure that `msr_id` is a valid MSR supported by the current CPU and that `val`
/// contains valid configuration bits for that MSR. Writing invalid values or writing to an
/// unsupported MSR will cause a General Protection Fault (#GP) or undefined behavior.
pub unsafe fn write_msr(msr_id: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    // SAFETY: wrmsr writes edx:eax to the MSR specified in ecx. Caller guarantees `msr_id` and `val` are valid.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr_id,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags),
        );
    }
}

mod ffi {
    use super::*;

    // Implemented in assembly.
    unsafe extern "C" {
        /// Reads MSR `msr_id` into `*val`. Returns `ZX_OK` if the read was successful or a non-OK
        /// status if the read failed because we received a `#GP` fault.
        pub(super) fn read_msr_safe(msr_id: u32, val: *mut u64) -> zx_status_t;
        /// Writes value `val` into MSR `msr_id`. Returns `ZX_OK` if the write was successful or a
        /// non-OK status if the write failed because we received a `#GP` fault.
        pub(super) fn write_msr_safe(msr_id: u32, val: u64) -> zx_status_t;
        /// Arms the CPU's `MWAIT` address monitor at `addr`.
        pub(super) fn x86_monitor(addr: *const core::ffi::c_void);
        /// Enables maskable interrupts and enters `MWAIT` with `hints` (`sti; mwait`).
        pub(super) fn x86_enable_ints_and_mwait(hints: u32);
        /// Fills the CPU's Return Address Stack / Return Stack Buffer (`RSB`).
        pub(super) fn x86_ras_fill();
    }
}

/// Arms the CPU's `MWAIT` address monitor at `addr`.
///
/// # Safety
///
/// - The current privilege level must be `CPL == 0` (`#UD` on Intel / `#GP(0)` on AMD otherwise).
/// - `MONITOR` must be supported and enabled on the current logical processor
///   (`CPUID.01H:ECX[3]`, with Intel `IA32_MISC_ENABLE[18]` set and AMD `HWCR[MonMwaitDis]`
///   clear).
/// - `addr` must point to canonical, readable memory of Write-Back (`WB`) caching type (`UC`,
///   `WC`, or MMIO addresses do not fault on `MONITOR` yet fail to arm the hardware monitor).
pub unsafe fn x86_monitor(addr: *const core::ffi::c_void) {
    // SAFETY: Caller guarantees `CPL == 0`, `MONITOR` is enabled on this logical processor, and
    // `addr` points to canonical, readable Write-Back memory.
    unsafe { ffi::x86_monitor(addr) };
}

/// Enables maskable interrupts and enters `MWAIT` with `hints` (`sti; mwait`), returning with
/// `RFLAGS.IF = 1`.
///
/// # Safety
///
/// - The caller must be at `CPL == 0` with `MONITOR`/`MWAIT` enabled on the current logical
///   processor.
/// - Maskable interrupts must be disabled on entry (`RFLAGS.IF == 0`), as `STI` only creates the
///   1-instruction interrupt shadow covering `mwait` when transitioning `IF` from `0` to `1`.
/// - Because this function returns with `RFLAGS.IF = 1`, the caller must be in a context where
///   enabling and servicing interrupts is sound (no spinlocks or `InterruptDisableGuard` held
///   across the call).
/// - On processors susceptible to cross-thread Return Address Stack (RAS) consumption during
///   `MWAIT` (such as SMT AMD Family 17h `AMD-SB-1045`, even in `C1`), the caller must fill the
///   RAS (`x86_ras_fill`) before entering `MWAIT`.
/// - When `hints` requests a C-state deeper than `C1`, the caller must account for architectural
///   side effects of deep C-states (potential TLB flush, non-ARAT Local APIC timer stop, RSB/RAS
///   emptying on Skylake, and DMA stores not waking `MWAIT`).
pub unsafe fn x86_enable_ints_and_mwait(hints: u32) {
    // SAFETY: Caller guarantees `CPL == 0`, `MONITOR`/`MWAIT` is enabled, `RFLAGS.IF == 0` on
    // entry, enabling interrupts on return is sound, and C-state side effects are handled.
    unsafe { ffi::x86_enable_ints_and_mwait(hints) };
}

/// Overwrites the hardware Return Address Stack (RAS/RSB) with 32 speculation-trap entries.
#[inline]
pub fn x86_ras_fill() {
    // SAFETY: `x86_ras_fill` (`ops.S`) uses 264 bytes of temporary kernel stack space and restores
    // `%rcx` and `%rsp` before returning.
    unsafe { ffi::x86_ras_fill() };
}

/// Safely reads a 64-bit Model Specific Register (MSR) with fault recovery.
///
/// Returns `Ok(value)` if the read succeeded, or `Err(Status)` if a `#GP(0)` fault occurred.
///
/// # Safety
///
/// - While this catches `#GP(0)` faults, the caller must ensure that reading `msr_id` does not
///   trigger unwanted hardware side effects.
/// - Because `#GP(0)` recovery uses per-CPU `%gs:PERCPU_GPF_RETURN_OFFSET` and the kernel IDT's
///   `x86_exception_handler` -> `x86_gpf_handler`, `%gs.base` and the kernel IDT must be
///   initialized, `!blocking_disallowed()` must hold if `msr_id` can fault (otherwise
///   `x86_exception_handler` panics on interrupt-handler recursion), and thread-context callers
///   must have preemption or interrupts disabled so the thread cannot migrate or be preempted
///   before recovery completes (see `read_msr_safe_on_cpu`).
pub unsafe fn read_msr_safe(msr_id: u32) -> Result<u64, Status> {
    let mut val: u64 = 0;
    // SAFETY: Calls assembly implementation with valid pointer to local variable.
    let status = unsafe { ffi::read_msr_safe(msr_id, &mut val) };
    Status::ok(status).map(|()| val)
}

/// Safely writes a 64-bit value to a Model Specific Register (MSR) with fault recovery.
///
/// Returns `Ok(())` if the write succeeded, or `Err(Status)` if a `#GP(0)` fault occurred.
///
/// # Safety
///
/// - While this catches `#GP(0)` faults, the caller must ensure that writing `val` to `msr_id`
///   does not violate system invariants or trigger unwanted hardware side effects.
/// - Because `#GP(0)` recovery uses per-CPU `%gs:PERCPU_GPF_RETURN_OFFSET` and the kernel IDT's
///   `x86_exception_handler` -> `x86_gpf_handler`, `%gs.base` and the kernel IDT must be
///   initialized, `!blocking_disallowed()` must hold if `msr_id` can fault (otherwise
///   `x86_exception_handler` panics on interrupt-handler recursion), and thread-context callers
///   must have preemption or interrupts disabled so the thread cannot migrate or be preempted
///   before recovery completes (see `write_msr_safe_on_cpu`).
pub unsafe fn write_msr_safe(msr_id: u32, val: u64) -> Result<(), Status> {
    // SAFETY: Calls assembly implementation which handles #GP faults safely.
    let status = unsafe { ffi::write_msr_safe(msr_id, val) };
    Status::ok(status)
}

/// Halts the calling CPU until the next interrupt.
#[inline]
pub fn x86_hlt() {
    // SAFETY: Executing `hlt` in ring 0 halts the processor until the next interrupt.
    unsafe {
        core::arch::asm!("hlt", options(nostack, preserves_flags));
    }
}
