// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use core::mem::MaybeUninit;
use debug::ltrace_entry;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_PMT_RIGHTS, ZX_OBJ_TYPE_PMT, zx_rights_t};

use super::KernelHandle;
use super::bus_transaction_initiator_dispatcher::BusTransactionInitiatorDispatcher;
use super::pinned_memory_token_dispatcher_ffi::cpp_pinned_memory_token_dispatcher_create;
use super::pmt::Pmt;
pub use super::pmt::{QueryAddressResult, dev_vaddr_t};
use crate::vm::pinned_vm_object::PinnedVmObject;

use object_constants_rs as object_constants;

const LOCAL_TRACE: u32 = 0;

zr::static_assert_size_and_align!(
    PinnedMemoryTokenDispatcherState,
    object_constants::kPinnedMemoryTokenDispatcherStateSize,
    object_constants::kPinnedMemoryTokenDispatcherStateAlign,
);

define_kcounter!(
    DISPATCHER_PINNED_MEMORY_TOKEN_CREATE_COUNT,
    "dispatcher.pinned_memory_token.create",
    Sum
);
define_kcounter!(
    DISPATCHER_PINNED_MEMORY_TOKEN_DESTROY_COUNT,
    "dispatcher.pinned_memory_token.destroy",
    Sum
);

/// Internal state storage for `PinnedMemoryTokenDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PinnedMemoryTokenDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"PIMT") }>,

    /// Whether `on_zero_handles` needs to unpin `pmt` on behalf of the kernel
    /// because ownership has not yet been handed off to user space (and
    /// `unpin` has not yet been called).
    #[guarded_by(lock)]
    kernel_cleanup_needed: bool,

    /// Reference to the containing BTI dispatcher.
    bti: RefPtr<BusTransactionInitiatorDispatcher>,

    /// Reference to the underlying IOMMU PMT object.
    #[guarded_by(lock)]
    pmt: Option<RefPtr<Pmt>>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl PinnedMemoryTokenDispatcherState {
    /// Initializes the `PinnedMemoryTokenDispatcherState`.
    pub fn init(
        _dispatcher: *const PinnedMemoryTokenDispatcher,
        bti: RefPtr<BusTransactionInitiatorDispatcher>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_PINNED_MEMORY_TOKEN_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            kernel_cleanup_needed: true.into(),
            bti,
            pmt: None.into(),
            lock <- KMutex::init(),
        })
    }

    /// Stores the PMT reference into state.
    pub fn set_pmt(&self, pmt: RefPtr<Pmt>) {
        ksync::lock!(let mut guard = self.lock_lock());
        *guard.as_mut().fields_mut().pmt = Some(pmt);
    }
}

#[pinned_drop]
impl PinnedDrop for PinnedMemoryTokenDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_PINNED_MEMORY_TOKEN_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher representing a token of memory pinned by a Bus Transaction Initiator.
    pub struct PinnedMemoryTokenDispatcher,
    PinnedMemoryTokenDispatcherState,
    ZX_OBJ_TYPE_PMT,
    object_constants::kPinnedMemoryTokenDispatcherStateOffset
);

impl PinnedMemoryTokenDispatcher {
    /// Creates a new `PinnedMemoryTokenDispatcher` representing pinned pages in `pinned_vmo`.
    ///
    /// Sets the permissions of `pinned_vmo`'s pinned range to `perms` on behalf of `bti`.
    /// `perms` should be flags suitable for the `Iommu::map()` interface.
    pub fn create(
        bti: RefPtr<BusTransactionInitiatorDispatcher>,
        mut pinned_vmo: PinnedVmObject,
        perms: u32,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        ltrace_entry!();
        debug_assert!(page::is_aligned(pinned_vmo.offset() as usize));
        debug_assert!(page::is_aligned(pinned_vmo.size() as usize));
        debug_assert!(pinned_vmo.vmo().is_some());

        // Note: Do not move our BTI reference into the PMT dispatcher we are
        // creating. Instead, give the new PMT dispatcher its own reference. We
        // still need to hold onto our reference for now so that we can access the
        // underlying driver BTI instance to perform the map operation.
        let bti_driver = bti.bti().clone();
        let bti_raw = RefPtr::into_raw(bti);
        let mut handle_out = MaybeUninit::<KernelHandle<Self>>::uninit();
        // SAFETY: `bti_raw` is a valid pointer carrying an acquired refcount, and
        // `handle_out` points to valid uninitialized memory for `KernelHandle<Self>`.
        let status = unsafe {
            cpp_pinned_memory_token_dispatcher_create(bti_raw as *mut _, &raw mut handle_out)
        };
        Status::ok(status)?;
        // SAFETY: `cpp_pinned_memory_token_dispatcher_create` initialized `handle_out`.
        let handle = unsafe { handle_out.assume_init() };

        // TODO(b/502262026): It feels like whether or not we require a contiguous
        // mapping should come as a flag from user mode, not as an expectation based
        // on whether or not the underlying VMO is actually physically contiguous.
        let contiguous_vmo = pinned_vmo.vmo().expect("pinned_vmo must have a vmo").is_contiguous();
        let pmt = bti_driver.map(&mut pinned_vmo, perms, contiguous_vmo)?;

        handle.dispatcher().state().set_pmt(pmt);

        Ok((handle, ZX_DEFAULT_PMT_RIGHTS))
    }

    /// Unpins and unmaps the memory which was managed by this PMT.
    pub fn unpin(&self) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        // If kernel code calls `unpin()` directly without having cleared
        // `kernel_cleanup_needed` (for example, kernel tests or any in-kernel
        // caller that does not hand the PMT out to user space), clearing it here
        // avoids a redundant `release_pinned_memory()` call in `on_zero_handles()`.
        // When `unpin()` is called via `zx_pmt_unpin`, `kernel_cleanup_needed` was
        // already cleared before adding the handle to the user process's handle
        // table, so this assignment is a no-op in that case.
        *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
        let pmt = guard.fields().pmt.as_ref().expect("PMT must be initialized");
        pmt.release_pinned_memory();
    }

    /// Query the pinned and mapped VMO for a region specified by offset/size.
    pub fn query_address(&self, offset: u64, size: usize) -> Result<QueryAddressResult, Status> {
        ksync::lock!(let guard = self.state().lock_lock());
        let pmt = guard.fields().pmt.as_ref().expect("PMT must be initialized");
        pmt.query_address(offset, size)
    }

    /// Marks that the handle for this PMT is about to be added to the user
    /// process's handle table (after all fallible steps have succeeded), so user
    /// space is now responsible for calling `zx_pmt_unpin` before closing all
    /// handles.
    pub fn clear_kernel_cleanup_needed(&self) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
    }

    /// Callback invoked when all handles to this dispatcher are closed.
    pub fn on_zero_handles(&self) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        // We may not have a driver level PMT if we never fully constructed
        // successfully. In this case, we will be dropping the kernel handle to the
        // PMT Dispatcher without ever having assigned an iommu::Pmt to it.
        if *guard.fields().kernel_cleanup_needed {
            *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
            if let Some(pmt) = guard.fields().pmt.as_ref() {
                pmt.release_pinned_memory();
            }
        }
        if let Some(pmt) = guard.fields().pmt.as_ref() {
            pmt.on_dispatcher_zero_handles();
        }
    }
}

/// Kernel unit tests for `PinnedMemoryTokenDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "pinned_memory_token_dispatcher_tests")]
mod tests {
    use super::BusTransactionInitiatorDispatcher;
    use crate::object::{IOMMU_FLAG_PERM_READ, IommuDispatcher};
    use crate::vm::vm_object::VmObject;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use unittest::{expect_eq, expect_false, expect_true};
    use zx_types::{ZX_IOMMU_TYPE_STUB, zx_iommu_desc_stub_t};

    /// Tests PMT cleanup responsibility before and after `clear_kernel_cleanup_needed`.
    #[test]
    fn test_pmt_handoff_cleanup_responsibility() {
        let desc =
            fbl::Array::<u8>::try_new_zeroed_slice(core::mem::size_of::<zx_iommu_desc_stub_t>())
                .expect("failed to allocate stub iommu desc");
        let (iommu_handle, _iommu_rights) =
            IommuDispatcher::create(ZX_IOMMU_TYPE_STUB, desc).expect("failed to create stub iommu");

        let (bti_handle, _bti_rights) =
            BusTransactionInitiatorDispatcher::create(iommu_handle.dispatcher().iommu(), 0)
                .expect("failed to create bti");
        let bti_disp = bti_handle.dispatcher();

        let vmo_paged =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let vmo: fbl::RefPtr<VmObject> = fbl::RefPtr::from_ref(&vmo_paged);

        // 1. Pre-handoff drop: dropping the `KernelHandle` before `clear_kernel_cleanup_needed()`
        //    must unpin the PMT without quarantining it or faulting the BTI.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo.clone(), 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo");
            expect_eq!(bti_disp.pmo_count(), 1);
            expect_eq!(bti_disp.quarantine_count(), 0);
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());

        // 2. Post-handoff drop without `unpin()`: once `clear_kernel_cleanup_needed()` is called,
        //    dropping all handles without calling `unpin()` must quarantine the PMT.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo.clone(), 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo after pre-handoff drop");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 1);
        expect_eq!(bti_disp.quarantine_count(), 1);
        expect_true!(bti_disp.bti().in_fault_state());

        bti_disp.release_quarantine();
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());

        // 3. Post-handoff explicit `unpin()` followed by drop: must unpin cleanly without
        //    quarantining.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo, 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo after release_quarantine");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();
            pmt_handle.dispatcher().unpin();
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());
    }
}
