// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_BTI_RIGHTS, ZX_OBJ_TYPE_BTI, zx_info_bti_t, zx_rights_t};

use super::bti::Bti;
use super::bus_transaction_initiator_dispatcher_ffi::cpp_bus_transaction_initiator_dispatcher_create;
use super::iommu::Iommu;
use super::pinned_memory_token_dispatcher::PinnedMemoryTokenDispatcher;
use super::{IOMMU_FLAG_PERM_WRITE, KernelHandle};
use crate::vm::pinned_vm_object::PinnedVmObject;
use crate::vm::vm_object::VmObject;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    BusTransactionInitiatorDispatcherState,
    object_constants::kBusTransactionInitiatorDispatcherStateSize,
    object_constants::kBusTransactionInitiatorDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_BTI_CREATE_COUNT, "dispatcher.bti.create", Sum);
define_kcounter!(DISPATCHER_BTI_DESTROY_COUNT, "dispatcher.bti.destroy", Sum);

/// Internal state storage for `BusTransactionInitiatorDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct BusTransactionInitiatorDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"BTID") }>,

    /// Reference to the underlying C++ `iommu::Bti` object (not protected by lock).
    bti: RefPtr<Bti>,

    #[guarded_by(lock)]
    zero_handles: bool,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl BusTransactionInitiatorDispatcherState {
    /// Initializes the `BusTransactionInitiatorDispatcherState`.
    pub fn init(
        _dispatcher: *const BusTransactionInitiatorDispatcher,
        bti: RefPtr<Bti>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_BTI_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            bti,
            zero_handles: false.into(),
            lock <- KMutex::init(),
        })
    }

    /// Returns a reference to the underlying `Bti` facade object.
    fn bti(&self) -> &RefPtr<Bti> {
        &self.bti
    }

    /// Marks the dispatcher as having zero handles.
    fn set_zero_handles(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        // Prevent new pinning from happening. The Dispatcher will stick around
        // until all of the PMTs are closed.
        *guard.as_mut().fields_mut().zero_handles = true;
    }

    /// Returns whether this dispatcher has hit zero handles.
    fn zero_handles_locked(
        &self,
        token: &ksync::LockToken<'_, BusTransactionInitiatorDispatcherStateLockClass>,
    ) -> bool {
        // SAFETY: `token` proves that `self.lock` is held.
        unsafe { *self.zero_handles.get(token) }
    }
}

#[pinned_drop]
impl PinnedDrop for BusTransactionInitiatorDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_BTI_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher for Bus Transaction Initiator (BTI) objects.
    pub struct BusTransactionInitiatorDispatcher,
    BusTransactionInitiatorDispatcherState,
    ZX_OBJ_TYPE_BTI,
    object_constants::kBusTransactionInitiatorDispatcherStateOffset
);

impl BusTransactionInitiatorDispatcher {
    /// Creates a new BusTransactionInitiatorDispatcher and returns its kernel handle and rights.
    pub fn create(iommu: &Iommu, bti_id: u64) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `iommu` points to a valid `Iommu`, and the FFI function initializes `handle`
        // on success.
        let handle = unsafe {
            KernelHandle::create(|out| {
                cpp_bus_transaction_initiator_dispatcher_create(iommu as *const Iommu, bti_id, out)
            })
        }?;
        Ok((handle, ZX_DEFAULT_BTI_RIGHTS))
    }

    /// Pins the given VMO range and returns a `PinnedMemoryTokenDispatcher` representing the
    /// pinned range.
    ///
    /// Returns `ZX_ERR_INVALID_ARGS` if `offset` or `size` are not page-aligned or `size == 0`.
    /// Returns `ZX_ERR_INVALID_ARGS` if `perms` is not suitable to pass to the `Iommu::map()`
    /// interface.
    /// Returns `ZX_ERR_BAD_STATE` if this BTI has hit zero handles or if the underlying driver is
    /// in a fault state.
    pub fn pin(
        &self,
        vmo: RefPtr<VmObject>,
        offset: u64,
        size: u64,
        perms: u32,
    ) -> Result<(KernelHandle<PinnedMemoryTokenDispatcher>, zx_rights_t), Status> {
        debug_assert!(page::is_aligned(offset as usize));
        debug_assert!(page::is_aligned(size as usize));

        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        let pinned_vmo =
            PinnedVmObject::create(vmo, offset, size, (perms & IOMMU_FLAG_PERM_WRITE) != 0)?;

        ksync::lock!(let guard = self.state().lock_lock());

        // User may not pin new memory if either our BTI has hit zero handles, or if
        // the underlying driver is in a fault state (usually because the BTI has
        // quarantined pages). In the case that the driver-level BTI is in a fault
        // state, user-mode driver code is expected to take the steps to stop their
        // DMA, and then call `zx_bti_release_quarantine` before proceeding to pin new
        // memory.
        if self.zero_handles_locked(guard.token()) || self.bti().in_fault_state() {
            return Err(Status::BAD_STATE);
        }

        let bti_ref = RefPtr::from_ref(self);
        PinnedMemoryTokenDispatcher::create(bti_ref, pinned_vmo, perms)
    }

    /// Returns a reference to the underlying `Bti` facade object.
    pub fn bti(&self) -> &RefPtr<Bti> {
        self.state().bti()
    }

    /// Returns the minimum contiguity guarantee in bytes.
    ///
    /// [`pin`](Self::pin) will always be able to return addresses that are contiguous for at
    /// least this many bytes. E.g. if this returns 1MB, then a call to [`pin`](Self::pin) with a
    /// size of 2MB will return at most two physically-contiguous runs. If the size were 2.5MB, it
    /// will return at most three physically-contiguous runs.
    pub fn minimum_contiguity(&self) -> u64 {
        self.bti().minimum_contiguity()
    }

    /// Returns the total size of the address space in bytes (`u64::MAX` if 2^64).
    pub fn aspace_size(&self) -> u64 {
        self.bti().aspace_size()
    }

    /// Returns the current total count of active and quarantined PMTs.
    pub fn pmo_count(&self) -> u64 {
        self.bti().pmo_count()
    }

    /// Returns the count of quarantined PMTs.
    pub fn quarantine_count(&self) -> u64 {
        self.bti().quarantine_count()
    }

    /// Releases all quarantined PMTs.
    ///
    /// The memory pins are released and the VMO references are dropped, so the underlying VMOs may
    /// be immediately destroyed, and the underlying physical memory may be reallocated.
    pub fn release_quarantine(&self) {
        self.bti().release_quarantine();
    }

    /// Called when the handle count drops to zero.
    pub fn on_zero_handles(&self) {
        self.state().set_zero_handles();
        self.bti().on_dispatcher_zero_handles();
    }

    /// Returns whether this dispatcher has hit zero handles.
    fn zero_handles_locked(
        &self,
        token: &ksync::LockToken<'_, BusTransactionInitiatorDispatcherStateLockClass>,
    ) -> bool {
        self.state().zero_handles_locked(token)
    }

    /// Returns the information for `zx_object_get_info(ZX_INFO_BTI, ...)`.
    pub fn get_info(&self) -> zx_info_bti_t {
        // TODO(johngro): Consider refactoring this so that the GetInfo operation can
        // be made in an atomic fashion.
        zx_info_bti_t {
            minimum_contiguity: self.minimum_contiguity(),
            aspace_size: self.aspace_size(),
            pmo_count: self.pmo_count(),
            quarantine_count: self.quarantine_count(),
        }
    }
}
