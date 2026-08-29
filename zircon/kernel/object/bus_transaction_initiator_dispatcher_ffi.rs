// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::bti::Bti;
use super::bus_transaction_initiator_dispatcher::{
    BusTransactionInitiatorDispatcher, BusTransactionInitiatorDispatcherState,
};
use super::iommu::Iommu;
use fbl::RefPtr;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    pub(crate) fn cpp_bti_recycle(bti: *mut Bti);
    pub(crate) fn cpp_bti_release_quarantine(bti: *mut Bti);
    pub(crate) fn cpp_bti_on_dispatcher_zero_handles(bti: *mut Bti);
    pub(crate) fn cpp_bti_minimum_contiguity(bti: *const Bti) -> u64;
    pub(crate) fn cpp_bti_aspace_size(bti: *const Bti) -> u64;
    pub(crate) fn cpp_bti_pmo_count(bti: *const Bti) -> u64;
    pub(crate) fn cpp_bti_quarantine_count(bti: *const Bti) -> u64;
    pub(crate) fn cpp_bti_in_fault_state(bti: *const Bti) -> bool;
    pub(crate) fn cpp_bti_bti_id(bti: *const Bti) -> u64;
    pub(crate) fn cpp_bti_set_name(
        bti: *mut Bti,
        name: *const core::ffi::c_char,
        len: usize,
    ) -> zx_status_t;
    pub(crate) fn cpp_bti_get_name(
        bti: *const Bti,
        out_name: *mut core::ffi::c_char,
    ) -> zx_status_t;

    pub(crate) fn cpp_bus_transaction_initiator_dispatcher_create(
        iommu: *const Iommu,
        bti_id: u64,
        handle_out: *mut core::mem::MaybeUninit<KernelHandle<BusTransactionInitiatorDispatcher>>,
    ) -> zx_status_t;
}

// Rust FFI trampolines for C++ calling into Rust BusTransactionInitiatorDispatcher

/// Initializes a `BusTransactionInitiatorDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `BusTransactionInitiatorDispatcherState`.
/// `bti_raw` must be a valid raw pointer exported from an `fbl::RefPtr<Bti>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_bus_transaction_initiator_dispatcher_state_init(
    state: *mut BusTransactionInitiatorDispatcherState,
    dispatcher: *const BusTransactionInitiatorDispatcher,
    bti_raw: *mut Bti,
) {
    // SAFETY: `bti_raw` is a valid raw pointer exported from `fbl::RefPtr<Bti>`.
    let bti = unsafe { RefPtr::from_raw(bti_raw) };
    let init = BusTransactionInitiatorDispatcherState::init(dispatcher, bti);
    // SAFETY: `state` points to uninitialized memory allocated for `BusTransactionInitiatorDispatcherState`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(init, state);
    }
}

/// Returns the raw pointer to the underlying C++ `iommu::Bti` object.
#[unsafe(no_mangle)]
pub extern "C" fn rust_bus_transaction_initiator_dispatcher_get_bti(
    disp: &BusTransactionInitiatorDispatcher,
) -> *mut Bti {
    disp.bti().as_ffi_mut()
}

/// Called when all handles to the dispatcher are closed.
#[unsafe(no_mangle)]
pub extern "C" fn rust_bus_transaction_initiator_dispatcher_on_zero_handles(
    disp: &BusTransactionInitiatorDispatcher,
) {
    disp.on_zero_handles();
}

/// Returns whether the dispatcher has hit zero handles.
///
/// # Safety
///
/// The caller must hold the dispatcher lock (`get_lock()`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_bus_transaction_initiator_dispatcher_zero_handles_locked(
    disp: &BusTransactionInitiatorDispatcher,
) -> bool {
    // SAFETY: The caller guarantees `disp.get_lock()` is held.
    let token = unsafe { ksync::LockToken::new() };
    disp.zero_handles_locked(&token)
}
