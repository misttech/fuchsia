// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Zero-cost environment seam (`SmmuEnv`) abstracting PMM page management, physmap conversions,
//! CPU data-cache cleaning, PTE write observation, and `PinnedVmObject` physical range lookups
//! across production kernel builds (`cfg(is_kernel)`) and host tests (`cfg(not(is_kernel))`).

use core::ffi::c_void;
#[cfg(not(is_kernel))]
use core::marker::PhantomData;
#[cfg(not(is_kernel))]
use core::ptr::NonNull;
#[cfg(is_kernel)]
use fbl::DoublyLinkedListContainable;
#[cfg(not(is_kernel))]
use fbl::DoublyLinkedListNode;
use zx_status::Status;
#[cfg(not(is_kernel))]
use zx_status::sys::zx_status_t;

use super::constants::{ArmSmmuMode, BtiPageLeakReason, InterruptPolarity, InterruptTriggerMode};
use super::smmu_mode::get_smmu_mode_bytes;

/// Raw spinlock type alias: uses `ksync::RawSpinlock` in the kernel and `ksync::RawMutex` in host
/// tests (since `RawSpinlock` is only available when `cfg(is_kernel)` is enabled).
#[cfg(is_kernel)]
pub type SmmuRawSpinLock = ksync::RawSpinlock;

/// Raw spinlock type alias for host tests (`ksync::RawMutex`).
#[cfg(not(is_kernel))]
pub type SmmuRawSpinLock = ksync::RawMutex;

/// Lock policy for acquiring an [`SmmuRawSpinLock`] from an interrupt handler where interrupts are
/// already disabled (`NoIrqSavePolicy` in kernel, default mutex policy on host).
#[cfg(is_kernel)]
pub type SmmuNoIrqSavePolicy = ksync::NoIrqSavePolicy;

/// Lock policy for acquiring an [`SmmuRawSpinLock`] in host tests.
#[cfg(not(is_kernel))]
pub type SmmuNoIrqSavePolicy = <ksync::RawMutex as ksync::RawLock>::DefaultPolicy;

/// Banner which must start every `DRIVER OOPS` report (the equivalent of the C++ `DRIVER_OOPS`
/// macro's `"\nDRIVER OOPS\n"` prefix, which `tefmocheck` looks for in logs).
pub const DRIVER_OOPS_BANNER: &str = "\nDRIVER OOPS\n";

// Host test environment bindings (`cpp_arm_smmu_*`).
#[cfg(not(is_kernel))]
unsafe extern "C" {
    /// Sleeps the calling thread for `delay_ns` nanoseconds (relative).
    fn cpp_arm_smmu_sleep_relative(delay_ns: i64) -> zx_status_t;

    /// Converts peripheral physical address `paddr` to its mapped virtual address (`0` if
    /// unmapped).
    fn cpp_arm_smmu_periph_paddr_to_vaddr(paddr: u64) -> usize;

    /// Converts peripheral virtual address `vaddr` back to its physical address (`0` if unmapped).
    fn cpp_arm_smmu_periph_vaddr_to_paddr(vaddr: usize) -> u64;

    /// Cleans CPU data cache lines covering `[start, start + len)` in host test builds.
    fn cpp_arm_smmu_clean_dcache_range(start: usize, len: usize);

    /// Records a pending write of `new_val` at `level` into the 64-bit PTE at `slot`.
    /// `slot` is borrowed only for the duration of the call and must point to a valid PTE slot.
    fn cpp_arm_smmu_on_pte_write(slot: *const u64, new_val: u64, level: u32);

    /// Records a pending zero-fill of `len` bytes of translation table memory at `table_vaddr`.
    fn cpp_arm_smmu_on_table_zero(table_vaddr: usize, len: usize);

    /// Allocates a single physical page from the host `PmmMock` and writes the non-null
    /// `vm_page_t*` to `*out_page` on `ZX_OK`. Ownership of the allocated page is transferred to
    /// the caller until returned via [`cpp_arm_smmu_pmm_free_page`].
    fn cpp_arm_smmu_pmm_alloc_page(alloc_flags: u32, out_page: *mut *mut c_void) -> zx_status_t;

    /// Transfers ownership of an unlinked `vm_page_t*` (`page`) back to the host `PmmMock`.
    fn cpp_arm_smmu_pmm_free_page(page: *mut c_void);

    /// Returns a borrowed `vm_page_t*` for physical address `pa` from the host `PmmMock` (or null
    /// if `pa` is not managed). Ownership remains with the PMM arena.
    fn cpp_arm_smmu_paddr_to_vm_page(pa: u64) -> *mut c_void;

    /// Returns the physical address of `page`. `page` is borrowed for the duration of the call and
    /// must point to a valid `vm_page_t`.
    fn cpp_arm_smmu_vm_page_paddr(page: *const c_void) -> u64;

    /// Converts physical address `pa` to its host mock physmap virtual address.
    fn cpp_arm_smmu_paddr_to_physmap(pa: u64) -> usize;

    /// Converts host mock physmap virtual address `vaddr` back to its physical address.
    fn cpp_arm_smmu_physmap_to_paddr(vaddr: usize) -> u64;

    /// Returns the byte offset of `pinned_vmo`. `pinned_vmo` is borrowed for the duration of the
    /// call and must point to a valid, initialized `PinnedVmObject`.
    fn cpp_arm_smmu_pinned_vmo_offset(pinned_vmo: *const c_void) -> u64;

    /// Returns the byte size of `pinned_vmo`. `pinned_vmo` is borrowed for the duration of the
    /// call and must point to a valid, initialized `PinnedVmObject`.
    fn cpp_arm_smmu_pinned_vmo_size(pinned_vmo: *const c_void) -> u64;

    /// Looks up a contiguous physical range `[offset, offset + size)` in `pinned_vmo` and writes
    /// the base physical address to `*out_paddr`. Both `pinned_vmo` and `out_paddr` are borrowed
    /// for the duration of the call and must be valid non-null pointers.
    fn cpp_arm_smmu_pinned_vmo_lookup_contiguous(
        pinned_vmo: *const c_void,
        offset: u64,
        size: usize,
        out_paddr: *mut u64,
    ) -> zx_status_t;

    /// Resets `pinned_vmo` in place, unpinning its pages and dropping its `VmObject` reference.
    /// `pinned_vmo` must point to a valid `PinnedVmObject` owned by the caller.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_pinned_vmo_reset(pinned_vmo: *mut c_void);

    /// Move-constructs a `PinnedVmObject` at `dst` from `src`, transferring ownership of the
    /// pinned range to `dst` and leaving `*src` empty. `dst` must point to 24-byte, 8-byte aligned
    /// uninitialized storage and `src` must point to a valid initialized `PinnedVmObject`.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_pinned_vmo_move_construct(dst: *mut c_void, src: *mut c_void);

    /// Copies `BootOptions::Get()->arm_smmu_mode` into `out_buf[..out_cap]` and returns the number
    /// of bytes written. `out_buf` is borrowed for the duration of the call and must be valid for
    /// writes of `out_cap` bytes.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_get_boot_option_mode(out_buf: *mut u8, out_cap: usize) -> usize;

    /// Adds `[base, base + size)` of resource `kind` to the root resource filter deny list.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_root_resource_filter_add_deny_region(base: u64, size: usize, kind: u32);

    /// Configures interrupt `vector` with `trigger_mode` and `polarity`.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_configure_interrupt(
        vector: u32,
        trigger_mode: u32,
        polarity: u32,
    ) -> zx_status_t;

    /// Masks interrupt `vector`.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_mask_interrupt(vector: u32) -> zx_status_t;

    /// Unmasks interrupt `vector`.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_unmask_interrupt(vector: u32) -> zx_status_t;

    /// Registers `handler(cookie)` for interrupt `vector`. `cookie` is not dereferenced by the
    /// interrupt manager directly but is passed back to `handler` on each interrupt; the caller
    /// must ensure `cookie` remains valid until [`cpp_arm_smmu_unregister_int_handler`] completes.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_register_int_handler(
        vector: u32,
        cookie: *mut c_void,
        handler: Option<extern "C" fn(*mut c_void)>,
    ) -> zx_status_t;

    /// Unregisters the interrupt handler for `vector`.
    #[cfg(not(is_kernel))]
    fn cpp_arm_smmu_unregister_int_handler(vector: u32) -> zx_status_t;

    /// Reads a 32-bit MMIO register at `base + offset` via the host `FakeSmmuMmioRouter` or raw
    /// memory fallback. `base + offset` must either belong to a registered router region or point
    /// to a valid 32-bit aligned host memory location.
    fn cpp_arm_smmu_mmio_read32(base: usize, offset: u32) -> u32;
    /// Writes `val` to a 32-bit MMIO register at `base + offset` via the host `FakeSmmuMmioRouter`
    /// or raw memory fallback. `base + offset` must either belong to a registered router region or
    /// point to a valid 32-bit aligned writable host memory location.
    fn cpp_arm_smmu_mmio_write32(base: usize, offset: u32, val: u32);
    /// Reads a 64-bit MMIO register at `base + offset` via the host `FakeSmmuMmioRouter` or raw
    /// memory fallback. `base + offset` must either belong to a registered router region or point
    /// to a valid 64-bit aligned host memory location.
    fn cpp_arm_smmu_mmio_read64(base: usize, offset: u32) -> u64;
    /// Writes `val` to a 64-bit MMIO register at `base + offset` via the host `FakeSmmuMmioRouter`
    /// or raw memory fallback. `base + offset` must either belong to a registered router region or
    /// point to a valid 64-bit aligned writable host memory location.
    fn cpp_arm_smmu_mmio_write64(base: usize, offset: u32, val: u64);
}

/// Opaque representation of a host `vm_page_t` whose leading field at offset 0 is its intrusive
/// doubly-linked list node (`queue_node`).
#[cfg(not(is_kernel))]
#[repr(C)]
pub struct HostVmPage {
    queue_node: DoublyLinkedListNode<HostVmPage>,
}

#[cfg(not(is_kernel))]
zr::static_assert!(core::mem::size_of::<HostVmPage>() == 16);
#[cfg(not(is_kernel))]
zr::static_assert!(core::mem::align_of::<HostVmPage>() == 8);

/// Non-null handle to a physical page descriptor (`VmPage` in kernel, mock `vm_page_t` on host).
#[cfg_attr(is_kernel, repr(transparent))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmmuPage {
    #[cfg(is_kernel)]
    ptr: crate::vm::page::VmPagePtr,
    #[cfg(not(is_kernel))]
    ptr: NonNull<HostVmPage>,
    #[cfg(not(is_kernel))]
    pa: u64,
}

impl SmmuPage {
    /// Constructs an `SmmuPage` from a raw `vm_page_t` pointer, returning `None` if `ptr` is null.
    ///
    /// # Safety
    /// If non-null, `ptr` must point to a valid, live `vm_page_t` managed by the kernel PMM or host
    /// `PmmMock`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `ptr` is null or a valid `VmPage` pointer.
            unsafe { crate::vm::page::VmPagePtr::from_raw(ptr.cast()).map(|ptr| Self { ptr }) }
        }
        #[cfg(not(is_kernel))]
        {
            NonNull::new(ptr.cast::<HostVmPage>()).map(|ptr| {
                // SAFETY: Caller guarantees non-null `ptr` points to a valid live host mock
                // `vm_page_t`. Caching its physical address inline avoids dereferencing `ptr` in
                // `SmmuPage::paddr`.
                let pa = unsafe { cpp_arm_smmu_vm_page_paddr(ptr.as_ptr().cast()) };
                Self { ptr, pa }
            })
        }
    }

    /// Returns the raw `vm_page_t` pointer.
    #[inline]
    pub fn as_raw(self) -> *mut c_void {
        #[cfg(is_kernel)]
        {
            self.ptr.as_raw().cast()
        }
        #[cfg(not(is_kernel))]
        {
            self.ptr.as_ptr().cast()
        }
    }

    /// Returns the physical address of this page.
    #[inline]
    pub fn paddr(self) -> u64 {
        #[cfg(is_kernel)]
        {
            // SAFETY: In the kernel, `VmPage` descriptors reside in static PMM arenas for the
            // lifetime of the system, and `paddr_priv` is initialized once at arena boot and never
            // mutated thereafter. Reading `paddr()` through `self.ptr` therefore cannot race or
            // dereference freed memory.
            unsafe { u64::from(self.ptr.as_non_null().as_ref().paddr()) }
        }
        #[cfg(not(is_kernel))]
        {
            self.pa
        }
    }

    /// Returns the `next` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), any non-null `next` pointer stored in `queue_node` must have
    ///   been written by [`SmmuPage::set_next`] and point to a live page (rather than an
    ///   `fbl::DoublyLinkedList` bit-0-tagged container sentinel), and the caller must have
    ///   exclusive access to the intrusive list containing `self`.
    #[inline]
    pub(crate) unsafe fn get_next(self) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                crate::vm::page::VmPagePtr::from_raw(*node.next.get()).map(|ptr| Self { ptr })
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.ptr` is a valid live `HostVmPage`, any non-null `next` pointer points
            // to a live `HostVmPage`, and caller owns the containing list.
            unsafe {
                let node = &self.ptr.as_ref().queue_node;
                Self::from_raw((*node.next.get()).cast())
            }
        }
    }

    /// Sets the `next` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` (and `next` when `Some`) must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), and the caller must have exclusive access to the intrusive
    ///   list containing `self`.
    #[inline]
    pub(crate) unsafe fn set_next(self, next: Option<Self>) {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                *node.next.get() = next.map_or(core::ptr::null_mut(), |p| p.ptr.as_raw());
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.ptr` is a valid live `HostVmPage` and caller owns the containing list.
            unsafe {
                let node = &self.ptr.as_ref().queue_node;
                *node.next.get() = next.map_or(core::ptr::null_mut(), |p| p.ptr.as_ptr());
            }
        }
    }

    /// Returns the `prev` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), any non-null `prev` pointer stored in `queue_node` must have
    ///   been written by [`SmmuPage::set_prev`] and point to a live page, and the caller must have
    ///   exclusive access to the intrusive list containing `self`.
    #[inline]
    pub(crate) unsafe fn get_prev(self) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                crate::vm::page::VmPagePtr::from_raw(*node.prev.get()).map(|ptr| Self { ptr })
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.ptr` is a valid live `HostVmPage`, any non-null `prev` pointer points
            // to a live `HostVmPage`, and caller owns the containing list.
            unsafe {
                let node = &self.ptr.as_ref().queue_node;
                Self::from_raw((*node.prev.get()).cast())
            }
        }
    }

    /// Sets the `prev` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` (and `prev` when `Some`) must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), and the caller must have exclusive access to the intrusive
    ///   list containing `self`.
    #[inline]
    pub(crate) unsafe fn set_prev(self, prev: Option<Self>) {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                *node.prev.get() = prev.map_or(core::ptr::null_mut(), |p| p.ptr.as_raw());
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.ptr` is a valid live `HostVmPage` and caller owns the containing list.
            unsafe {
                let node = &self.ptr.as_ref().queue_node;
                *node.prev.get() = prev.map_or(core::ptr::null_mut(), |p| p.ptr.as_ptr());
            }
        }
    }
}

/// A store into memory which the SMMU hardware reads, as reported to
/// [`SmmuEnv::observe_hw_visible_write`].
///
/// Today that memory is the stage 1 translation tables. SMMUv3 adds more in-memory structures
/// which the hardware reads (stream table entries, context descriptors, command queue entries);
/// they would be further variants here.
#[derive(Clone, Copy, Debug)]
pub enum HwVisibleWrite {
    /// One 64-bit translation table descriptor, in a table the writer believes is at `level`.
    Descriptor { level: u32, new_val: u64 },
    /// `len` bytes of a translation table being zero-filled, which clears every descriptor in
    /// the range.
    TableZeroFill { len: usize },
}

/// Zero-cost environment façade for SMMU page-table and page-cache operations.
///
/// [`SmmuEnv`] provides a unified static API over PMM allocation, physmap conversion, cache
/// maintenance, thread sleep, and PTE write observation that compiles to direct Zircon kernel
/// calls under `cfg(is_kernel)` and routes to the host test simulation mocks (`PmmMock`) under
/// `cfg(not(is_kernel))`.
pub struct SmmuEnv;

impl SmmuEnv {
    /// Allocates a single physical page from the PMM.
    #[inline]
    pub fn alloc_page(alloc_flags: u32) -> Result<SmmuPage, Status> {
        #[cfg(is_kernel)]
        {
            let (ptr, _pa) = crate::vm::pmm::alloc_page(alloc_flags)?;
            Ok(SmmuPage { ptr })
        }
        #[cfg(not(is_kernel))]
        {
            let mut raw_page: *mut c_void = core::ptr::null_mut();
            // SAFETY: `raw_page` is a valid stack out-pointer.
            let status = unsafe { cpp_arm_smmu_pmm_alloc_page(alloc_flags, &raw mut raw_page) };
            Status::ok(status)?;
            // SAFETY: `cpp_arm_smmu_pmm_alloc_page` succeeded and populated `raw_page`.
            unsafe { SmmuPage::from_raw(raw_page).ok_or(Status::NO_MEMORY) }
        }
    }

    /// Returns a single unlinked physical page to the PMM.
    ///
    /// # Safety
    /// `page` must have been allocated via [`SmmuEnv::alloc_page`], must not be linked into any
    /// intrusive list, and must no longer be referenced by any active translation table or IOTLB.
    #[inline]
    pub unsafe fn free_page(page: SmmuPage) {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `page` is unlinked and uniquely owned for reclamation.
            unsafe {
                crate::vm::pmm::free_page(page.ptr);
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Caller guarantees `page` is unlinked and uniquely owned for reclamation.
            unsafe {
                cpp_arm_smmu_pmm_free_page(page.as_raw());
            }
        }
    }

    /// Frees a chain of physical pages back to the PMM, taking the PMM lock once for the whole
    /// chain rather than once per page.
    ///
    /// `first` is the first page of a chain linked through the pages' intrusive `queue_node`
    /// `next` pointers and terminated by a null `next` pointer.
    ///
    /// # Safety
    /// Every page in the chain must be a valid allocated PMM page which is uniquely owned by the
    /// caller for reclamation, the chain must not be reachable from any other list, and
    /// `first`'s `prev` pointer must be null.
    pub unsafe fn free_page_chain(first: SmmuPage) {
        #[cfg(is_kernel)]
        {
            // Move the chain onto a proper `VmPageDoublyLinkedList`, which is what the PMM's
            // batched free consumes.
            pin_init::stack_pin_init!(let list = crate::vm::page::VmPageDoublyLinkedList::new());
            let mut cur = Some(first);
            while let Some(page) = cur {
                // SAFETY: Caller guarantees the chain is valid and uniquely owned. Each page is
                // fully unlinked from the chain before it is added to `list`, which is never
                // moved; `push_back_raw` only mutates its sentinel links and the page's node.
                unsafe {
                    cur = page.get_next();
                    page.set_next(None);
                    page.set_prev(None);
                    list.as_mut().push_back_raw(page.ptr.as_non_null());
                }
            }
            // SAFETY: Caller guarantees every page now on `list` is a valid allocated PMM page
            // owned by us. `free_list` drains `list`, leaving it empty.
            unsafe {
                crate::vm::pmm::free_list(list);
            }
        }
        #[cfg(not(is_kernel))]
        {
            let mut cur = Some(first);
            while let Some(page) = cur {
                // SAFETY: Caller guarantees the chain is valid and uniquely owned. Each page is
                // fully unlinked before being handed back to the host `PmmMock`.
                unsafe {
                    cur = page.get_next();
                    page.set_next(None);
                    page.set_prev(None);
                    cpp_arm_smmu_pmm_free_page(page.as_raw());
                }
            }
        }
    }

    /// Looks up the [`SmmuPage`] corresponding to physical address `pa`.
    #[inline]
    pub fn paddr_to_page(pa: u64) -> Option<SmmuPage> {
        #[cfg(is_kernel)]
        {
            crate::vm::pmm::paddr_to_vm_page(crate::kernel::types::PAddr(pa as usize))
                .map(|ptr| SmmuPage { ptr })
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Foreign call into `paddr_to_vm_page` on host mock PMM; result is null or a
            // valid `vm_page_t*`.
            unsafe { SmmuPage::from_raw(cpp_arm_smmu_paddr_to_vm_page(pa)) }
        }
    }

    /// Converts physical address `pa` to its virtual address in the kernel/mock physmap.
    #[inline]
    pub fn paddr_to_physmap(pa: u64) -> usize {
        #[cfg(is_kernel)]
        {
            crate::vm::physmap::paddr_to_physmap(crate::kernel::types::PAddr(pa as usize)).0
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Pure address conversion in host mock physmap.
            unsafe { cpp_arm_smmu_paddr_to_physmap(pa) }
        }
    }

    /// Converts virtual address `vaddr` in the kernel/mock physmap back to its physical address.
    #[inline]
    pub fn physmap_to_paddr(vaddr: usize) -> u64 {
        #[cfg(is_kernel)]
        {
            u64::from(crate::vm::physmap::physmap_to_paddr(crate::kernel::types::VAddr(vaddr)))
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Pure address conversion in host mock physmap.
            unsafe { cpp_arm_smmu_physmap_to_paddr(vaddr) }
        }
    }

    /// Cleans CPU data cache lines covering `[start, start + len)` to the Point of Coherency.
    ///
    /// # Safety
    /// `[start, start + len)` must be a mapped kernel virtual address range (in practice, the
    /// physmap address of a translation table page), or the mock physmap equivalent in host tests.
    #[inline]
    pub unsafe fn clean_dcache_range(start: usize, len: usize) {
        #[cfg(is_kernel)]
        {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                crate::arch_rs::clean_dcache_range(start, len);
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `cpp_arm_smmu_clean_dcache_range` records the cache clean in `PmmMock`.
            unsafe {
                cpp_arm_smmu_clean_dcache_range(start, len);
            }
        }
    }

    /// Tells the host simulation hardware model about a store into memory which the SMMU
    /// hardware reads, before the store is made. Compiles to a zero-cost no-op in production
    /// kernel builds.
    ///
    /// This is the single observation point for such memory. Every store into a translation
    /// table goes through it (see `vmsav8_64::write_pte` and `vmsav8_64::zero_table_page`), so
    /// the model sees each one. `vaddr` is the physmap address the store is about to write.
    ///
    /// # Safety
    /// - For [`HwVisibleWrite::Descriptor`], `vaddr` must be a non-null, 8-byte-aligned physmap
    ///   address with exposed provenance valid for reading a `u64` within an allocated 4 KiB
    ///   translation table page.
    /// - For [`HwVisibleWrite::TableZeroFill`], `vaddr` must be a non-null, 8-byte-aligned physmap
    ///   address with exposed provenance valid for reading `len / 8` initialized `u64` words
    ///   within a single allocated 4 KiB translation table page (`(vaddr & PAGE_MASK) + len <=
    ///   PAGE_SIZE`).
    #[inline]
    pub(super) unsafe fn observe_hw_visible_write(vaddr: usize, write: HwVisibleWrite) {
        #[cfg(is_kernel)]
        {
            let _ = (vaddr, write);
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                match write {
                    HwVisibleWrite::Descriptor { level, new_val } => {
                        cpp_arm_smmu_on_pte_write(
                            core::ptr::with_exposed_provenance::<u64>(vaddr),
                            new_val,
                            level,
                        );
                    }
                    HwVisibleWrite::TableZeroFill { len } => {
                        cpp_arm_smmu_on_table_zero(vaddr, len);
                    }
                }
            }
        }
    }

    /// Converts a peripheral physical address `paddr` to its mapped virtual address (`0` if
    /// unmapped).
    #[inline]
    pub fn periph_paddr_to_vaddr(paddr: u64) -> usize {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::periph_paddr_to_vaddr(crate::kernel::types::PAddr(paddr as usize))
                .map_or(0, |v| v.0)
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Pure address lookup in the host mock peripheral map.
            unsafe { cpp_arm_smmu_periph_paddr_to_vaddr(paddr) }
        }
    }

    /// Converts a peripheral virtual address `vaddr` back to its physical address (`0` if
    /// unmapped).
    #[inline]
    pub fn periph_vaddr_to_paddr(vaddr: usize) -> u64 {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::periph_vaddr_to_paddr(crate::kernel::types::VAddr(vaddr))
                .map_or(0, u64::from)
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Pure address lookup in the host mock peripheral map.
            unsafe { cpp_arm_smmu_periph_vaddr_to_paddr(vaddr) }
        }
    }

    /// Executes a full-system Device Memory Barrier (`dsb sy` on arm64, no-op on host).
    #[inline]
    pub fn device_memory_barrier() {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::device_memory_barrier();
        }
    }

    /// Hints to the processor that the caller is in a busy-wait spin loop.
    ///
    /// Issues the architectural spin-wait yield instruction (`yield` on aarch64, `pause` on
    /// x86_64) via `libarch::r#yield()` rather than `core::hint::spin_loop()`, which on aarch64
    /// emits an `isb` (a pipeline flush on every poll) and omits the compiler memory clobber.
    #[inline]
    pub fn yield_cpu() {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::r#yield();
        }
        #[cfg(not(is_kernel))]
        {
            libarch::r#yield();
        }
    }

    /// Reads a 32-bit register at `base + offset` in host test builds via `FakeSmmuMmioRouter`.
    #[cfg(not(is_kernel))]
    #[inline]
    pub fn host_mmio_read32(base: usize, offset: u32) -> u32 {
        // SAFETY: Routes through `FakeSmmuMmioRouter` or valid host test register buffer.
        unsafe { cpp_arm_smmu_mmio_read32(base, offset) }
    }

    /// Writes a 32-bit register at `base + offset` in host test builds via `FakeSmmuMmioRouter`.
    #[cfg(not(is_kernel))]
    #[inline]
    pub fn host_mmio_write32(base: usize, offset: u32, val: u32) {
        // SAFETY: Routes through `FakeSmmuMmioRouter` or valid host test register buffer.
        unsafe {
            cpp_arm_smmu_mmio_write32(base, offset, val);
        }
    }

    /// Reads a 64-bit register at `base + offset` in host test builds via `FakeSmmuMmioRouter`.
    #[cfg(not(is_kernel))]
    #[inline]
    pub fn host_mmio_read64(base: usize, offset: u32) -> u64 {
        // SAFETY: Routes through `FakeSmmuMmioRouter` or valid host test register buffer.
        unsafe { cpp_arm_smmu_mmio_read64(base, offset) }
    }

    /// Writes a 64-bit register at `base + offset` in host test builds via `FakeSmmuMmioRouter`.
    #[cfg(not(is_kernel))]
    #[inline]
    pub fn host_mmio_write64(base: usize, offset: u32, val: u64) {
        // SAFETY: Routes through `FakeSmmuMmioRouter` or valid host test register buffer.
        unsafe {
            cpp_arm_smmu_mmio_write64(base, offset, val);
        }
    }

    /// Sleeps the calling thread for `delay_ns` nanoseconds (relative).
    #[inline]
    pub fn sleep_relative(delay_ns: i64) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            crate::kernel::thread::sleep_relative(crate::platform_rs::timer::DurationMono(delay_ns))
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `cpp_arm_smmu_sleep_relative` sleeps the current thread for `delay_ns`.
            let status = unsafe { cpp_arm_smmu_sleep_relative(delay_ns) };
            Status::ok(status)
        }
    }

    /// Sleeps the calling thread for `usec` microseconds.
    #[inline]
    pub fn sleep_usec(usec: u64) {
        let delay_ns = i64::try_from(usec).unwrap_or(i64::MAX).saturating_mul(1_000);
        let _ = Self::sleep_relative(delay_ns);
    }

    /// Queries `BootOptions::Get()->arm_smmu_mode` and parses the mode for `base_addr`, falling
    /// back to [`ArmSmmuMode::Disabled`] if the boot option string is invalid.
    pub fn boot_option_smmu_mode(base_addr: Option<u64>) -> ArmSmmuMode {
        #[cfg(is_kernel)]
        {
            get_smmu_mode_bytes(&boot_options::BootOptions::get().arm_smmu_mode, base_addr)
                .unwrap_or(ArmSmmuMode::Disabled)
        }
        #[cfg(not(is_kernel))]
        {
            let mut buf = [0u8; 256];
            // SAFETY: `buf` is a valid stack buffer of length `buf.len()`.
            let len = unsafe { cpp_arm_smmu_get_boot_option_mode(buf.as_mut_ptr(), buf.len()) };
            get_smmu_mode_bytes(&buf[..len], base_addr).unwrap_or(ArmSmmuMode::Disabled)
        }
    }

    /// Adds `[base, base + size)` of resource `kind` to the root resource filter deny list.
    #[inline]
    pub fn root_resource_filter_add_deny_region(base: u64, size: usize, kind: u32) {
        #[cfg(is_kernel)]
        {
            crate::root_resource_filter::root_resource_filter_add_deny_region(
                base as usize,
                size,
                kind,
            );
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Registers a physical address range with the mock root resource filter.
            unsafe {
                cpp_arm_smmu_root_resource_filter_add_deny_region(base, size, kind);
            }
        }
    }

    /// Configures interrupt `vector` with `trigger_mode` and `polarity`.
    #[inline]
    pub fn configure_interrupt(
        vector: u32,
        trigger_mode: InterruptTriggerMode,
        polarity: InterruptPolarity,
    ) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `vector` is validated by the interrupt controller and global interrupt ops
            // are initialized before SMMU driver init.
            unsafe {
                crate::pdev_interrupt::configure_interrupt(
                    crate::dev_interrupt::InterruptVector(vector),
                    trigger_mode,
                    polarity,
                )
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Calls into host mock `configure_interrupt`.
            let status = unsafe {
                cpp_arm_smmu_configure_interrupt(vector, trigger_mode as u32, polarity as u32)
            };
            Status::ok(status)
        }
    }

    /// Masks interrupt `vector`.
    #[inline]
    pub fn mask_interrupt(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::mask_interrupt(crate::dev_interrupt::InterruptVector(vector))
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Calls into host mock `mask_interrupt`.
            let status = unsafe { cpp_arm_smmu_mask_interrupt(vector) };
            Status::ok(status)
        }
    }

    /// Unmasks interrupt `vector`.
    #[inline]
    pub fn unmask_interrupt(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::unmask_interrupt(crate::dev_interrupt::InterruptVector(
                    vector,
                ))
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Calls into host mock `unmask_interrupt`.
            let status = unsafe { cpp_arm_smmu_unmask_interrupt(vector) };
            Status::ok(status)
        }
    }

    /// Registers `handler(cookie)` for interrupt `vector`.
    ///
    /// # Safety
    /// `cookie` must remain valid for as long as the interrupt handler remains registered (until
    /// [`SmmuEnv::unregister_int_handler`] completes).
    #[inline]
    pub unsafe fn register_int_handler(
        vector: u32,
        cookie: *mut c_void,
        handler: extern "C" fn(*mut c_void),
    ) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `cookie` remains valid while registered.
            unsafe {
                crate::pdev_interrupt::register_int_handler(
                    crate::dev_interrupt::InterruptVector(vector),
                    crate::dev_interrupt::InterruptHandler { cookie, r#fn: Some(handler) },
                )
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Caller guarantees `cookie` remains valid while registered.
            let status =
                unsafe { cpp_arm_smmu_register_int_handler(vector, cookie, Some(handler)) };
            Status::ok(status)
        }
    }

    /// Unregisters the interrupt handler for `vector`.
    #[inline]
    pub fn unregister_int_handler(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::register_int_handler(
                    crate::dev_interrupt::InterruptVector(vector),
                    crate::dev_interrupt::InterruptHandler::DEFAULT,
                )
            }
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: Unregisters the handler for `vector` in host mock.
            let status = unsafe { cpp_arm_smmu_unregister_int_handler(vector) };
            Status::ok(status)
        }
    }

    /// Emits a fully rendered `DRIVER OOPS` `report` with a single console write in kernel builds,
    /// and suppresses it in host test builds so CI `tefmocheck` does not flag expected negative
    /// tests.
    ///
    /// `report` must start with [`DRIVER_OOPS_BANNER`]. Callers render the entire report into a
    /// buffer first instead of using `dprintf!` directly: `dprintf!` issues a separate console
    /// write for every literal fragment and argument of its format string, which would allow a
    /// multi-line report to be interleaved with output from other CPUs.
    #[inline]
    pub fn print_driver_oops_report(report: &str) {
        debug_assert!(report.starts_with(DRIVER_OOPS_BANNER));
        #[cfg(is_kernel)]
        {
            use core::fmt::Write as _;
            let _ = debug::dprintf::KernelConsoleWriter.write_str(report);
        }
        #[cfg(not(is_kernel))]
        {
            let _ = report;
        }
    }

    /// Logs a BTI quarantine warning for `bti_id` and `bti_name`.
    #[inline]
    pub fn print_quarantine_warning(
        bti_id: u64,
        bti_name: &[u8],
        reason: BtiPageLeakReason,
        total_leaked_pages: u64,
        total_leaked_vmos: usize,
    ) {
        // Temporary porting note: wired to `crate::iommu::print_quarantine_warning` once the common
        // `dev/iommu` module is ported in a follow-up commit (before the Rust SMMUv2 driver is
        // wired into the kernel).
        let _ = (bti_id, bti_name, reason, total_leaked_pages, total_leaked_vmos);
    }
}

/// Owned `PinnedVmObject` wrapper used by [`super::super::v2::SmmuBti`] and
/// [`super::super::v2::SmmuPmt`].
///
/// In kernel builds (`cfg(is_kernel)`), this wraps [`crate::vm::pinned_vm_object::PinnedVmObject`]
/// directly without any C++ FFI round-trip. In standalone host test builds (`cfg(not(is_kernel))`),
/// it wraps the 24-byte host mock `PinnedVmObject` storage.
#[repr(C)]
pub struct SmmuPinnedVmo {
    #[cfg(is_kernel)]
    inner: crate::vm::pinned_vm_object::PinnedVmObject,
    #[cfg(not(is_kernel))]
    storage: [usize; 3],
}

zr::static_assert!(core::mem::size_of::<SmmuPinnedVmo>() == 24);
zr::static_assert!(core::mem::align_of::<SmmuPinnedVmo>() == 8);

impl SmmuPinnedVmo {
    /// Creates an empty (unpinned) `SmmuPinnedVmo`.
    #[inline]
    pub const fn empty() -> Self {
        #[cfg(is_kernel)]
        {
            Self { inner: crate::vm::pinned_vm_object::PinnedVmObject::new() }
        }
        #[cfg(not(is_kernel))]
        {
            Self { storage: [0; 3] }
        }
    }

    /// Wraps a kernel [`crate::vm::pinned_vm_object::PinnedVmObject`] directly.
    #[cfg(is_kernel)]
    #[inline]
    pub const fn from_kernel_pinned_vmo(
        pinned_vmo: crate::vm::pinned_vm_object::PinnedVmObject,
    ) -> Self {
        Self { inner: pinned_vmo }
    }

    /// Move-constructs an `SmmuPinnedVmo` from a raw pointer to a C++ `PinnedVmObject`, leaving
    /// the source `PinnedVmObject` empty.
    ///
    /// # Safety
    /// `src` must point to a valid, initialized C++ `PinnedVmObject`.
    #[cfg(not(is_kernel))]
    #[inline]
    pub unsafe fn from_raw_move(src: *mut c_void) -> Self {
        let mut out = Self::empty();
        if !src.is_null() {
            // SAFETY: `out.storage` has the exact size and alignment of `PinnedVmObject`, and
            // caller guarantees `src` points to a valid `PinnedVmObject`.
            unsafe {
                cpp_arm_smmu_pinned_vmo_move_construct((&raw mut out.storage).cast(), src);
            }
        }
        out
    }

    /// Returns `true` if this `SmmuPinnedVmo` holds a non-null `VmObject`.
    #[inline]
    pub fn has_vmo(&self) -> bool {
        #[cfg(is_kernel)]
        {
            self.inner.vmo().is_some()
        }
        #[cfg(not(is_kernel))]
        {
            self.storage[0] != 0
        }
    }

    /// Resets this `SmmuPinnedVmo`, unpinning its pages and dropping its `VmObject` reference.
    #[inline]
    pub fn reset(&mut self) {
        #[cfg(is_kernel)]
        {
            self.inner = crate::vm::pinned_vm_object::PinnedVmObject::new();
        }
        #[cfg(not(is_kernel))]
        {
            if self.has_vmo() {
                // SAFETY: `self.storage` holds a valid `PinnedVmObject`.
                unsafe {
                    cpp_arm_smmu_pinned_vmo_reset((&raw mut self.storage).cast());
                }
            }
        }
    }

    /// Takes ownership of the underlying `PinnedVmObject`, leaving `self` empty.
    #[inline]
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

impl Default for SmmuPinnedVmo {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(not(is_kernel))]
impl Drop for SmmuPinnedVmo {
    #[inline]
    fn drop(&mut self) {
        self.reset();
    }
}

impl PinnedVmoSource for SmmuPinnedVmo {
    #[inline]
    fn offset(&self) -> u64 {
        #[cfg(is_kernel)]
        {
            self.inner.offset()
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.storage` is a valid C++ `PinnedVmObject`.
            unsafe { cpp_arm_smmu_pinned_vmo_offset((&raw const self.storage).cast()) }
        }
    }

    #[inline]
    fn size(&self) -> u64 {
        #[cfg(is_kernel)]
        {
            self.inner.size()
        }
        #[cfg(not(is_kernel))]
        {
            // SAFETY: `self.storage` is a valid C++ `PinnedVmObject`.
            unsafe { cpp_arm_smmu_pinned_vmo_size((&raw const self.storage).cast()) }
        }
    }

    #[inline]
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        #[cfg(is_kernel)]
        {
            PinnedVmoSource::lookup_contiguous(&self.inner, offset, size)
        }
        #[cfg(not(is_kernel))]
        {
            let mut pa = 0u64;
            // SAFETY: `self.storage` is a valid C++ `PinnedVmObject` and `pa` is a valid out-pointer.
            let status = unsafe {
                cpp_arm_smmu_pinned_vmo_lookup_contiguous(
                    (&raw const self.storage).cast(),
                    offset,
                    size as usize,
                    &raw mut pa,
                )
            };
            Status::ok(status)?;
            Ok(pa)
        }
    }
}

/// Trait abstracting a pinned VMO range for [`super::device_aspace::DeviceAspace::map`].
pub trait PinnedVmoSource {
    /// Returns the starting byte offset within the underlying VMO.
    fn offset(&self) -> u64;

    /// Returns the size in bytes of the pinned VMO range.
    fn size(&self) -> u64;

    /// Looks up the contiguous physical address for `[offset, offset + size)` in the VMO.
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status>;
}

#[cfg(is_kernel)]
impl PinnedVmoSource for crate::vm::pinned_vm_object::PinnedVmObject {
    #[inline]
    fn offset(&self) -> u64 {
        self.offset()
    }

    #[inline]
    fn size(&self) -> u64 {
        self.size()
    }

    #[inline]
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        let vmo = self.vmo().ok_or(Status::INVALID_ARGS)?;
        let pa = vmo.lookup_contiguous(offset, size)?;
        Ok(u64::from(pa))
    }
}

/// Borrowed reference wrapper around a C++ `PinnedVmObject` pointer for host test FFI callers.
#[cfg(not(is_kernel))]
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct CppPinnedVmObjectRef<'a> {
    raw: NonNull<c_void>,
    _marker: PhantomData<&'a c_void>,
}

#[cfg(not(is_kernel))]
impl<'a> CppPinnedVmObjectRef<'a> {
    /// Wraps a non-null pointer to a C++ `PinnedVmObject` for lifetime `'a`.
    ///
    /// # Safety
    /// `ptr` must point to a valid, initialized C++ `PinnedVmObject` that remains valid for `'a`.
    #[inline]
    pub unsafe fn from_raw(ptr: *const c_void) -> Option<Self> {
        NonNull::new(ptr.cast_mut()).map(|raw| Self { raw, _marker: PhantomData })
    }
}

#[cfg(not(is_kernel))]
impl PinnedVmoSource for CppPinnedVmObjectRef<'_> {
    #[inline]
    fn offset(&self) -> u64 {
        // SAFETY: `self.raw` points to a valid C++ `PinnedVmObject`.
        unsafe { cpp_arm_smmu_pinned_vmo_offset(self.raw.as_ptr()) }
    }

    #[inline]
    fn size(&self) -> u64 {
        // SAFETY: `self.raw` points to a valid C++ `PinnedVmObject`.
        unsafe { cpp_arm_smmu_pinned_vmo_size(self.raw.as_ptr()) }
    }

    #[inline]
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        let mut pa = 0u64;
        // SAFETY: `self.raw` points to a valid C++ `PinnedVmObject` and `pa` is a valid out-param.
        let status = unsafe {
            cpp_arm_smmu_pinned_vmo_lookup_contiguous(
                self.raw.as_ptr(),
                offset,
                size as usize,
                &raw mut pa,
            )
        };
        Status::ok(status)?;
        Ok(pa)
    }
}
