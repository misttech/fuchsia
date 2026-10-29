// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::idt::Idt;

unsafe extern "C" {
    #[link_name = "_isr_table"]
    pub(super) static ISR_TABLE: [usize; 256];
}

/// FFI entry point for [`Idt::set_ist_index`].
#[unsafe(no_mangle)]
pub extern "C" fn idt_set_ist_index(idt: &mut Idt, vec: u8, ist_idx: u8) {
    idt.set_ist_index(vec, ist_idx);
}

/// FFI entry point for [`Idt::setup`].
#[unsafe(no_mangle)]
pub extern "C" fn idt_setup(idt: &mut Idt) {
    idt.setup();
}

/// FFI entry point for [`super::idt::idt_setup_readonly`].
///
/// # Safety
///
/// The caller must ensure that this function is called once at CPL 0 on CPU 0 while only CPU 0 is
/// online, after the kernel address space and `_idt` (via [`idt_setup`]) are initialized, and while
/// no concurrent mutation of `_idt` can occur.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idt_setup_readonly() {
    // SAFETY: Caller guarantees CPU 0 single-CPU arch init context after `_idt` initialization.
    unsafe { super::idt::idt_setup_readonly() };
}

/// FFI entry point for [`super::idt::idt_load`].
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and that `idt` is non-null,
/// properly aligned, and points to an initialized [`Idt`] that remains mapped for as long as it is
/// loaded in `IDTR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idt_load(idt: *const Idt) {
    // SAFETY: Caller guarantees CPL 0 and that `idt` points to a valid, mapped `Idt`.
    unsafe { super::idt::idt_load(idt) };
}

/// FFI entry point for [`super::idt::idt_get_readonly`].
#[unsafe(no_mangle)]
pub extern "C" fn idt_get_readonly() -> *mut Idt {
    super::idt::idt_get_readonly()
}
