// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;
use counters_rs::define_kcounter;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_OBJ_TYPE_IOMMU, ZX_RIGHT_DUPLICATE, ZX_RIGHT_INSPECT, ZX_RIGHT_TRANSFER, zx_rights_t,
};

use super::KernelHandle;
use super::iommu::Iommu;
use super::iommu_dispatcher_ffi::cpp_iommu_dispatcher_create;

use object_constants_rs as object_constants;

/// Default rights assigned to an IommuDispatcher handle (ZX_RIGHTS_BASIC & ~ZX_RIGHT_WAIT).
pub const DEFAULT_RIGHTS: zx_rights_t = ZX_RIGHT_TRANSFER | ZX_RIGHT_DUPLICATE | ZX_RIGHT_INSPECT;

zr::static_assert_size_and_align!(
    IommuDispatcherState,
    object_constants::kIommuDispatcherStateSize,
    object_constants::kIommuDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_IOMMU_CREATE_COUNT, "dispatcher.iommu.create", Sum);
define_kcounter!(DISPATCHER_IOMMU_DESTROY_COUNT, "dispatcher.iommu.destroy", Sum);

/// Internal state storage for `IommuDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct IommuDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"IOMM") }>,

    /// Reference to the underlying C++ `iommu::Iommu` object (not protected by lock).
    iommu: RefPtr<Iommu>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl IommuDispatcherState {
    /// Initializes the `IommuDispatcherState`.
    pub fn init(
        _dispatcher: *const IommuDispatcher,
        iommu: RefPtr<Iommu>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_IOMMU_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            iommu,
            lock <- KMutex::init(),
        })
    }

    /// Returns a reference to the underlying `Iommu` facade object.
    pub fn iommu(&self) -> &RefPtr<Iommu> {
        &self.iommu
    }
}

#[pinned_drop]
impl PinnedDrop for IommuDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_IOMMU_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct IommuDispatcher,
    IommuDispatcherState,
    ZX_OBJ_TYPE_IOMMU,
    object_constants::kIommuDispatcherStateOffset
);

impl IommuDispatcher {
    /// Returns default rights for an IommuDispatcher handle.
    pub fn default_rights() -> zx_rights_t {
        DEFAULT_RIGHTS
    }

    /// Creates a new IommuDispatcher and returns its kernel handle and rights.
    pub fn create(
        type_param: u32,
        desc: kalloc::Box<[u8]>,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let desc_len = desc.len();
        let desc_ptr = kalloc::Box::into_raw(desc) as *const u8;
        let mut handle_out = MaybeUninit::<KernelHandle<Self>>::uninit();
        // SAFETY: `desc_ptr` points to `desc_len` bytes of valid heap-allocated memory that is
        // transferred to C++ (`cpp_iommu_dispatcher_create` takes ownership of `desc_ptr`), and
        // `handle_out` points to valid uninitialized memory for `KernelHandle<Self>`.
        let status = unsafe {
            cpp_iommu_dispatcher_create(type_param, desc_ptr, desc_len, &raw mut handle_out)
        };
        Status::ok(status)?;
        // SAFETY: cpp_iommu_dispatcher_create initialized handle_out.
        unsafe { Ok((handle_out.assume_init(), DEFAULT_RIGHTS)) }
    }

    /// Returns a reference to the underlying `Iommu` facade object.
    pub fn iommu(&self) -> &RefPtr<Iommu> {
        self.state().iommu()
    }
}
