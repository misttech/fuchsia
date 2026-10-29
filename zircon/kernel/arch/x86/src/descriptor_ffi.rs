// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::descriptor::{clear_tss_busy, initialize_percpu_tss, set_tss_sp};

/// Temporary GDT defined in assembly is used during AP/BP setup process.
pub const TEMP_GDT_SIZE: usize = 0x38 + crate::kernel::mp::SMP_MAX_CPUS * 16;

unsafe extern "C" {
    #[link_name = "_temp_gdt"]
    pub(super) static mut TEMP_GDT: [u8; TEMP_GDT_SIZE];

    #[link_name = "_temp_gdt_end"]
    pub(super) static TEMP_GDT_END: u8;

    pub(super) fn x86_reset_tss_io_bitmap();
}

/// FFI entry point for [`initialize_percpu_tss`].
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_initialize_percpu_tss() {
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    unsafe { initialize_percpu_tss() };
}

/// FFI entry point for [`set_tss_sp`].
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts or preemption are disabled so the calling thread cannot migrate CPUs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_set_tss_sp(sp: usize) {
    // SAFETY: Caller guarantees `%gs.base` is initialized and preemption/interrupts are disabled.
    unsafe { set_tss_sp(sp) };
}

/// FFI entry point for [`clear_tss_busy`].
///
/// # Safety
///
/// The caller must ensure that `sel` is a valid 64-bit TSS selector index within the active GDT
/// and that the caller has exclusive access to that TSS descriptor entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_clear_tss_busy(sel: u16) {
    // SAFETY: Caller guarantees `sel` is a valid, exclusively accessed 64-bit TSS selector.
    unsafe { clear_tss_busy(sel) };
}

/// FFI entry point for [`super::descriptor::gdt_setup`].
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 while only CPU 0 is online
/// and after the kernel address space and VMM are initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gdt_setup() {
    // SAFETY: Caller guarantees single-CPU arch initialization context on CPU 0.
    unsafe { super::descriptor::gdt_setup() };
}

/// FFI entry point for [`super::descriptor::gdt_get`].
#[unsafe(no_mangle)]
pub extern "C" fn gdt_get() -> usize {
    super::descriptor::gdt_get()
}
