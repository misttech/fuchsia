// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_MSI_RIGHTS, ZX_OBJ_TYPE_MSI, zx_rights_t};

use super::KernelHandle;
use super::msi_allocation::MsiAllocation;
use super::msi_dispatcher_ffi::cpp_msi_dispatcher_create;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    MsiDispatcherState,
    object_constants::kMsiDispatcherStateSize,
    object_constants::kMsiDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_MSI_CREATE_COUNT, "dispatcher.msi.create", Sum);
define_kcounter!(DISPATCHER_MSI_DESTROY_COUNT, "dispatcher.msi.destroy", Sum);

/// Internal state storage for `MsiDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct MsiDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"MSID") }>,

    /// Reference to the underlying `MsiAllocation` object (not protected by lock).
    msi_alloc: RefPtr<MsiAllocation>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl MsiDispatcherState {
    /// Initializes the `MsiDispatcherState`.
    ///
    /// # Safety
    ///
    /// `msi_alloc_raw` must be a valid pointer exported from `RefPtr::into_raw`.
    pub unsafe fn init(
        _dispatcher: *const MsiDispatcher,
        msi_alloc_raw: *mut MsiAllocation,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_MSI_CREATE_COUNT.add(1);
        // SAFETY: `msi_alloc_raw` was exported from `RefPtr::into_raw` in `MsiDispatcher::create`.
        let msi_alloc = unsafe { RefPtr::from_raw(msi_alloc_raw) };
        pin_init!(Self {
            canary: Canary::new(),
            msi_alloc,
            lock <- KMutex::init(),
        })
    }

    /// Returns a reference to the underlying `MsiAllocation` ref pointer.
    pub fn msi_allocation(&self) -> &RefPtr<MsiAllocation> {
        self.canary.assert();
        &self.msi_alloc
    }
}

#[pinned_drop]
impl PinnedDrop for MsiDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_MSI_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Message Signaled Interrupts (MSI) allocation wrapper dispatcher.
    pub struct MsiDispatcher,
    MsiDispatcherState,
    ZX_OBJ_TYPE_MSI,
    object_constants::kMsiDispatcherStateOffset
);

impl MsiDispatcher {
    /// Creates a new MsiDispatcher wrapping an MsiAllocation and returns its handle and rights.
    pub fn create(
        msi_alloc: RefPtr<MsiAllocation>,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let msi_alloc_raw = RefPtr::into_raw(msi_alloc).cast_mut();
        // SAFETY: `msi_alloc_raw` is transferred to `rust_msi_dispatcher_state_init` if C++
        // allocates `MsiDispatcher`, and `cpp_msi_dispatcher_create` initializes `handle` on
        // success. If C++ allocation fails before invoking `MsiDispatcher::MsiDispatcher`,
        // we reclaim and drop `msi_alloc_raw`.
        let handle = unsafe {
            KernelHandle::create(|out| cpp_msi_dispatcher_create(msi_alloc_raw, out)).inspect_err(
                |_| {
                    drop(RefPtr::from_raw(msi_alloc_raw));
                },
            )
        }?;
        Ok((handle, ZX_DEFAULT_MSI_RIGHTS))
    }

    /// Returns `zx_info_msi_t` for this MSI dispatcher.
    pub fn get_info(&self) -> zx_types::zx_info_msi_t {
        self.msi_allocation().get_info()
    }

    /// Returns a reference to the underlying `MsiAllocation` ref pointer.
    pub fn msi_allocation(&self) -> &RefPtr<MsiAllocation> {
        self.state().msi_allocation()
    }
}
