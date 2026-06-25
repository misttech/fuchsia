// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

// Keep dependency alive even when lockdep is disabled to satisfy unused dependency lints.
use zr as _;

/// Trait implemented by lock class types to expose their dynamic LockClassId pointer.
pub trait LockClass {
    const ID: *mut core::ffi::c_void;
}

#[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
mod enabled {
    core::cfg_select! {
        feature = "lock_dep" => {
            const LOCK_CLASS_STATE_SIZE: usize = 1608;
            const LOCK_CLASS_REGISTRATION_SIZE: usize = 1624;
        }
        feature = "lock_metadata_only" => {
            const LOCK_CLASS_STATE_SIZE: usize = 8;
            const LOCK_CLASS_REGISTRATION_SIZE: usize = 24;
        }
    }

    #[repr(C, align(8))]
    #[derive(Default)]
    struct LockClassStateStorage(zr::OpaqueBytes<LOCK_CLASS_STATE_SIZE>);

    impl LockClassStateStorage {
        const fn uninit() -> Self {
            Self(zr::OpaqueBytes::uninit())
        }
    }

    /// A registration entry for a Rust lock class.
    ///
    /// This struct is registered with the C++ lockdep implementation via a linker section. The
    /// layout of this struct is known to C++.
    #[repr(C)]
    pub struct LockClassRegistration {
        name: *const kstring::interned_string::InternedString,
        flags: u16,
        state_storage: LockClassStateStorage,
    }

    unsafe impl Sync for LockClassRegistration {}
    unsafe impl Send for LockClassRegistration {}

    impl LockClassRegistration {
        pub const fn new(name: &'static ::kstring::interned_string::InternedString) -> Self {
            Self { name, flags: 0, state_storage: LockClassStateStorage::uninit() }
        }

        #[inline]
        pub const fn get(&self) -> *mut core::ffi::c_void {
            self.state_storage.0.get() as *mut _
        }
    }

    zr::static_assert!(
        core::mem::size_of::<LockClassRegistration>() == LOCK_CLASS_REGISTRATION_SIZE
    );
    zr::static_assert!(core::mem::align_of::<LockClassRegistration>() == 8);
}

#[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
pub use enabled::LockClassRegistration;

#[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
mod disabled {
    /// A registration entry for a Rust lock class (stub for disabled lockdep).
    pub struct LockClassRegistration;

    impl LockClassRegistration {
        pub const fn new(_name: &'static ::kstring::interned_string::InternedString) -> Self {
            Self
        }

        #[inline]
        pub const fn get(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }
    }
}

#[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
pub use disabled::LockClassRegistration;
