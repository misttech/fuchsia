// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use super::feature;
use super::interrupts::{X86_INT_BREAKPOINT, X86InterruptVector};
use super::x86::x86_lidt;
use crate::arch_rs::curr_cpu_num;
use crate::kernel::mp;
use crate::vm::arch_vm_aspace::ARCH_MMU_FLAG_PERM_READ;
use crate::vm::vm::vaddr_to_paddr;
use crate::vm::vm_aspace::VmAspace;

/// The size of the `clac` instruction.
const CLAC_SIZE: usize = 3;

/// Kernel 64-bit code segment selector (`CODE_64_SELECTOR` in `arch/x86/descriptor.h`).
const CODE_64_SELECTOR: u16 = 0x10;

/// Single 16-byte Interrupt Descriptor Table (IDT) gate descriptor on x86_64.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct IdtEntry {
    pub w0: u32,
    pub w1: u32,
    pub w2: u32,
    pub w3: u32,
}

zr::static_assert!(core::mem::size_of::<IdtEntry>() == 16);
zr::static_assert!(core::mem::align_of::<IdtEntry>() == 4);

/// 256-entry Interrupt Descriptor Table (IDT) (`struct idt`).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Idt {
    pub entries: [IdtEntry; 256],
}

impl Idt {
    const fn new() -> Self {
        Self { entries: [IdtEntry { w0: 0, w1: 0, w2: 0, w3: 0 }; 256] }
    }
}

zr::static_assert!(core::mem::size_of::<Idt>() == 16 * 256);
zr::static_assert!(core::mem::align_of::<Idt>() == 16);

/// Packed IDTR descriptor structure for the `lidt` instruction.
#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Idtr {
    pub limit: u16,
    pub address: usize,
}

zr::static_assert!(core::mem::size_of::<Idtr>() == 10);
zr::static_assert!(core::mem::align_of::<Idtr>() == 1);

/// Gate descriptor type for 64-bit IDT entries (`enum idt_entry_type`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdtEntryType {
    InterruptGate64 = 0xe,
    #[expect(dead_code)]
    TrapGate64 = 0xf,
}

/// Descriptor Privilege Level (DPL) for IDT entries (`enum idt_dpl`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdtDpl {
    Dpl0 = 0,
    #[expect(dead_code)]
    Dpl1 = 1,
    #[expect(dead_code)]
    Dpl2 = 2,
    Dpl3 = 3,
}

/// Page-aligned wrapper around an [`Idt`] stored in a global `static` and mutated only during
/// single-threaded early boot on CPU 0 before secondary CPUs are brought online.
#[repr(C, align(4096))]
pub struct SharedIdt(UnsafeCell<Idt>);

zr::static_assert!(core::mem::size_of::<SharedIdt>() == page::SIZE);
zr::static_assert!(core::mem::align_of::<SharedIdt>() == page::SIZE);

// SAFETY: `_idt_startup` and `_idt` are only mutated during single-threaded early boot on CPU 0
// before secondary CPUs are brought online (`idt_setup`, `idt_set_ist_index`), after which `_idt`
// is remapped read-only (`idt_setup_readonly`) and only read by hardware. Callers of
// `SharedIdt::get` must never form `&mut Idt` concurrently with hardware or other CPUs reading it.
unsafe impl Sync for SharedIdt {}

impl SharedIdt {
    const fn new() -> Self {
        Self(UnsafeCell::new(Idt::new()))
    }

    pub(super) const fn get(&self) -> *mut Idt {
        self.0.get()
    }
}

/// Early boot shared IDT structure.
// SAFETY: `_idt_startup` is the unique global definition for `extern struct idt _idt_startup` in
// `arch/x86/idt.h`, and `SharedIdt` (`#[repr(C, align(4096))]` wrapping `UnsafeCell<Idt>`) is
// layout-compatible with `struct idt`.
#[unsafe(no_mangle)]
pub static _idt_startup: SharedIdt = SharedIdt::new();

/// IDT after early boot.
// SAFETY: `_idt` is the unique global definition for `extern struct idt _idt` in
// `arch/x86/mp.cc`, and `SharedIdt` (`#[repr(C, align(4096))]` wrapping `UnsafeCell<Idt>`) is
// layout-compatible with `struct idt`.
#[unsafe(no_mangle)]
pub static _idt: SharedIdt = SharedIdt::new();

/// Read-only remapping of the IDT.
static IDT_RO: AtomicPtr<Idt> = AtomicPtr::new(core::ptr::null_mut());

impl IdtEntry {
    #[inline]
    fn set_segment_sel(&mut self, sel: u16) {
        self.w0 = (self.w0 & 0x0000_ffff) | ((sel as u32) << 16);
    }

    #[inline]
    fn set_offset(&mut self, offset: usize) {
        let low_16 = (offset & 0xffff) as u32;
        let mid_16 = ((offset >> 16) & 0xffff) as u32;
        self.w0 = (self.w0 & 0xffff_0000) | low_16;
        self.w1 = (self.w1 & 0x0000_ffff) | (mid_16 << 16);
        let high_32 = (offset >> 32) as u32;
        self.w2 = high_32;
    }

    #[inline]
    fn set_present(&mut self, present: bool) {
        self.w1 = (self.w1 & !(1 << 15)) | (u32::from(present) << 15);
    }

    #[inline]
    fn set_dpl(&mut self, dpl: IdtDpl) {
        assert!((dpl as u32) <= 3);
        self.w1 = (self.w1 & !(3 << 13)) | ((dpl as u32) << 13);
    }

    #[inline]
    fn set_type(&mut self, typ: IdtEntryType) {
        self.w1 = (self.w1 & !(0xf << 8)) | ((typ as u32) << 8);
    }
}

impl Idt {
    /// Changes an IDT entry.
    ///
    /// Caution: Interrupts should probably be disabled when this is called.
    pub(super) fn set_vector(
        &mut self,
        vec: u8,
        code_segment_sel: u16,
        entry_point_offset: usize,
        dpl: IdtDpl,
        typ: IdtEntryType,
    ) {
        let entry = &mut self.entries[vec as usize];
        *entry = IdtEntry::default();
        entry.set_segment_sel(code_segment_sel);
        entry.set_offset(entry_point_offset);
        entry.set_type(typ);
        entry.set_dpl(dpl);
        entry.set_present(true);
    }

    /// Sets the Interrupt Stack Table (IST) index to use for `vec`.
    ///
    /// `ist_idx` must be in the range `[0, 8)`. If `ist_idx == 0`, the normal stack for the
    /// target privilege level is used.
    pub(super) fn set_ist_index(&mut self, vec: u8, ist_idx: u8) {
        assert!(ist_idx < 8);
        let entry = &mut self.entries[vec as usize];
        entry.w1 = (entry.w1 & !0x7) | (ist_idx as u32);
    }

    /// Initializes this IDT with our default values.
    pub(super) fn setup(&mut self) {
        // If SMAP is not available, we need to skip past the CLAC instruction
        // at the beginning of the ISR stubs.
        let mut clac_shift = 0;
        if !feature::x86_feature_has_smap() {
            clac_shift += CLAC_SIZE;
        }

        let sel = CODE_64_SELECTOR;
        let typ = IdtEntryType::InterruptGate64;
        for i in 0..self.entries.len() {
            // SAFETY: `_isr_table` is an assembly table of 256 ISR stub entry point addresses
            // defined in `exceptions.S`.
            let offset = unsafe { super::idt_ffi::ISR_TABLE[i] } + clac_shift;
            let dpl = match X86InterruptVector(i as u8) {
                X86_INT_BREAKPOINT => IdtDpl::Dpl3,
                _ => IdtDpl::Dpl0,
            };
            self.set_vector(i as u8, sel, offset, dpl, typ);
        }
    }
}

/// Switches to the given IDT.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and that `idt` is non-null,
/// properly aligned, and points to an initialized [`Idt`] that remains mapped for as long as it is
/// loaded in `IDTR`.
#[inline]
pub(super) unsafe fn idt_load(idt: *const Idt) {
    // After VM exit IDT limit is always set to 0xffff, so in order to avoid
    // calling LIDT in hypervisor to restore the proper IDT limit after every
    // VM exit in hypervisor we decided to use 0xffff all the time. There is
    // no harm in doing that because IDT limit is only relevant if it's smaller
    // than sizeof(struct idt) - 1 and doesn't affect anything otherwise.
    let idtr = Idtr { limit: 0xffff, address: idt.expose_provenance() };
    // SAFETY: Caller guarantees CPL 0, and `idtr` is a valid stack-allocated `Idtr` pointing to
    // the caller-supplied `idt`.
    unsafe { x86_lidt((&raw const idtr).cast()) };
}

/// Creates a read-only remapping of the global IDT.
///
/// This function is called on arch initialization before additional CPUs are started. It reloads
/// the main processor IDT to be read-only. Each additional CPU will pick up the read-only IDT by
/// default.
///
/// TODO(thgarnie): Move to C++ and non-compact VMAR for KASLR support.
///
/// # Safety
///
/// The caller must ensure that this function is called once at CPL 0 on CPU 0 while only CPU 0 is
/// online, after the kernel address space and `_idt` (via [`Idt::setup`]) are initialized, and
/// while no concurrent mutation of `_idt` can occur.
pub(super) unsafe fn idt_setup_readonly() {
    debug_assert!(curr_cpu_num() == 0);
    debug_assert!(mp::get_online_mask() == 1);
    let mut idt_ro_ptr: *mut c_void = core::ptr::null_mut();
    let paddr = vaddr_to_paddr(_idt.get().cast());
    // SAFETY: Allocates a read-only virtual mapping in the kernel address space backed by the
    // physical page of `_idt`.
    let status = unsafe {
        VmAspace::kernel_aspace().alloc_physical(
            b"idt_readonly",
            core::mem::size_of::<Idt>(),
            &mut idt_ro_ptr,
            page::SHIFT as u8,
            paddr,
            0,
            ARCH_MMU_FLAG_PERM_READ,
        )
    };
    assert!(status.is_ok());
    let idt_ro = idt_ro_ptr.cast::<Idt>();
    // Written once on CPU 0 while `mp::get_online_mask() == 1` before secondary CPUs are brought
    // online; AP bringup synchronizes memory visibility.
    IDT_RO.store(idt_ro, Ordering::Relaxed);
    // SAFETY: Caller guarantees CPL 0, and `idt_ro` points to a valid read-only mapping of the
    // initialized `_idt`.
    unsafe { idt_load(idt_ro) };
}

/// Returns the read-only IDT pointer.
pub(super) fn idt_get_readonly() -> *mut Idt {
    let ptr = IDT_RO.load(Ordering::Relaxed);
    assert!(!ptr.is_null());
    ptr
}
