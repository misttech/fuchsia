// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::vm_address_region::{VmAddressRegion, VmAddressRegionOrMapping};
use super::vm_aspace::VmAspaceLockClass;
use core::marker::PhantomData;
use core::pin::Pin;
use ksync::LockToken;
use pin_init::{PinInit, pin_data};
use vm_address_region_bindings as bindings;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};

pub use bindings::VmAddressRegionEnumeratorType;

/// Helper struct for performing enumeration of a VMAR.
///
/// The purpose of having a stateful enumerator is to have the option to not need to hold the aspace
/// lock over the entire enumeration, whilst guaranteeing forward progress and termination. If the
/// vmar is modified whilst enumeration is paused (due to dropping the lock or otherwise) then it is
/// not well defined whether the enumerator will return any new mappings. However, the enumerator
/// will never return `Dead` mappings, and will not return mappings in ranges it has already
/// enumerated.
///
/// Except between calls to `pause` and `resume`, the vmar should be considered immutable, and
/// sub-vmars and mappings should not be modified.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct VmAddressRegionEnumerator<'a> {
    opaque: Opaque<bindings::CppVmAddressRegionEnumerator>,
    phantom: PhantomData<&'a VmAddressRegion>,
}

unsafe_pinned_drop_ffi!(
    VmAddressRegionEnumerator<'_>,
    bindings::cpp_vm_address_region_enumerator_destroy
);

impl<'a> VmAddressRegionEnumerator<'a> {
    /// This requires the vmar lock to be held over the lifetime of the object, except where
    /// explicitly stated otherwise.
    pub fn new(
        enumerator_type: VmAddressRegionEnumeratorType,
        vmar: &'a VmAddressRegion,
        min_addr: usize,
        max_addr: usize,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_address_region_enumerator_construct,
            enumerator_type,
            vmar.as_ffi_ptr(),
            min_addr,
            max_addr
        )
    }

    fn as_raw(self: Pin<&mut Self>) -> *mut bindings::CppVmAddressRegionEnumerator {
        // SAFETY: Obtaining a raw pointer to `opaque` does not move the pinned object.
        unsafe { self.get_unchecked_mut().opaque.get() }
    }

    /// Yield the next region or mapping, or `None` if enumeration has completed. Regions are
    /// yielded in depth-first pre-order. The regions are yielded as references, which are
    /// guaranteed to be valid since the lock is held. It is the caller's responsibility to keep
    /// these references alive, by upgrading to `RefPtr`s or otherwise, if they want to `pause` and
    /// drop the lock.
    pub fn next<'b>(
        self: Pin<&mut Self>,
        _token: &'b LockToken<'_, VmAspaceLockClass>,
    ) -> Option<(&'b VmAddressRegionOrMapping, u32)> {
        let mut depth = 0;
        // SAFETY: `self.as_raw()` points to a live `CppVmAddressRegionEnumerator`, `depth` is valid
        // for writing, and the aspace lock is held.
        let ptr =
            unsafe { bindings::cpp_vm_address_region_enumerator_next(self.as_raw(), &mut depth) };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `ptr` points to a live `VmAddressRegionOrMapping` in the subregion tree
            // protected by the held aspace lock (`_token` with lifetime `'b`).
            Some((unsafe { &*ptr.cast::<VmAddressRegionOrMapping>() }, depth))
        }
    }

    /// Pause enumeration. Until `resume` is called `next` may not be called, but the vmar lock is
    /// permitted to be dropped, and the vmar is permitted to be modified.
    pub fn pause(self: Pin<&mut Self>, _token: &LockToken<'_, VmAspaceLockClass>) {
        // SAFETY: `self.as_raw()` points to a live `CppVmAddressRegionEnumerator` and the aspace
        // lock is held.
        unsafe {
            bindings::cpp_vm_address_region_enumerator_pause(self.as_raw());
        }
    }

    /// Resume enumeration allowing `next` to be called again.
    pub fn resume(self: Pin<&mut Self>, _token: &LockToken<'_, VmAspaceLockClass>) {
        // SAFETY: `self.as_raw()` points to a live `CppVmAddressRegionEnumerator` and the aspace
        // lock is held.
        unsafe {
            bindings::cpp_vm_address_region_enumerator_resume(self.as_raw());
        }
    }
}
