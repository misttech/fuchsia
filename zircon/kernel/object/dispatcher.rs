// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher_ffi::{
    cpp_dispatcher_get_ref_counted, cpp_dispatcher_get_type, cpp_dispatcher_on_zero_handles,
    cpp_dispatcher_recycle, cpp_dispatcher_update_state, cpp_dispatcher_update_state_locked,
};
use super::handle::HandleValue;
use super::process_dispatcher_ffi::cpp_handle_table_get_dispatcher;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use fbl::{Recyclable, RefPtr, pin_make_ref_counted, ref_counted};
use kalloc::AllocError;
use ksync::{KMutex, LockToken, RawCriticalMutex, guarded};
use zx_status::Status;
use zx_types::zx_rights_t;

/// Common trait defining ops for kernel Dispatcher facades.
pub trait DispatcherOps {
    type LockClass;
    const TYPE: zx_types::zx_obj_type_t;

    fn dispatcher(&self) -> *const Dispatcher;

    fn on_zero_handles(&self) {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe {
            cpp_dispatcher_on_zero_handles(self.dispatcher());
        }
    }

    fn update_state(&self, clear_mask: u32, set_mask: u32) {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe {
            cpp_dispatcher_update_state(self.dispatcher(), clear_mask, set_mask);
        }
    }

    fn update_state_locked(
        &self,
        _token: &LockToken<'_, Self::LockClass>,
        clear_mask: u32,
        set_mask: u32,
    ) {
        // SAFETY: self.dispatcher() is valid, and the proof token guarantees the state lock is
        // held.
        unsafe {
            cpp_dispatcher_update_state_locked(self.dispatcher(), clear_mask, set_mask);
        }
    }
}

/// Helper macro to declare facade structs and implement common facade traits for Dispatcher
/// subtypes.
macro_rules! impl_dispatcher_facade {
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $obj_type:expr) => {
        $crate::object::dispatcher::impl_dispatcher_facade!($(#[$meta])* $vis struct $type, $obj_type, ());
    };
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $obj_type:expr, $lock_class:ty) => {
        $(#[$meta])*
        #[repr(C)]
        $vis struct $type {
            _facade: fbl::OpaqueRefCountedFacade<$crate::object::Dispatcher>,
        }

        impl core::ops::Deref for $type {
            type Target = $crate::object::Dispatcher;
            fn deref(&self) -> &Self::Target {
                // SAFETY: `self` is a valid facade reference, and the base `Dispatcher`
                // is part of the same allocation.
                unsafe { &*<Self as $crate::object::DispatcherOps>::dispatcher(self) }
            }
        }

        // SAFETY: `$type` is a `#[repr(C)]` facade struct that starts with `Dispatcher`
        // at offset 0 and is layout-compatible with `Dispatcher`.
        unsafe impl fbl::IsOpaqueRefCounted for $type {
            type TargetBase = $crate::object::Dispatcher;
        }

        impl $crate::object::DispatcherOps for $type {
            const TYPE: zx_types::zx_obj_type_t = $obj_type;
            type LockClass = $lock_class;

            fn dispatcher(&self) -> *const $crate::object::Dispatcher {
                self as *const Self as *const $crate::object::Dispatcher
            }
        }
    };
}
pub(crate) use impl_dispatcher_facade;

/// Helper macro to declare facade structs and implement common facade traits and state access
/// methods for Dispatcher subtypes with state.
macro_rules! impl_dispatcher_facade_with_state {
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $state:ident, $obj_type:expr, $offset_const:expr) => {
        paste::paste! {
            $crate::object::dispatcher::impl_dispatcher_facade!($(#[$meta])* $vis struct $type, $obj_type, [<$state LockClass>]);

            impl $type {
                /// Returns a reference to the underlying state object.
                pub fn state(&self) -> &$state {
                    // SAFETY: The state object is located at a verified offset within the
                    // same allocation as the facade.
                    unsafe {
                        let ptr = (self as *const Self)
                            .cast::<u8>()
                            .add($offset_const as usize)
                            .cast::<$state>();
                        &*ptr
                    }
                }
            }

            /// Returns a pointer to the mutex inside `$state`.
            ///
            /// # Safety
            ///
            /// `ptr` must point to an initialized `$state`.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_get_lock>](
                ptr: *const $state,
            ) -> *mut ksync::KMutex<[<$state LockClass>], ksync::RawCriticalMutex> {
                // SAFETY: The caller guarantees `ptr` points to a valid,
                // initialized `$state`.
                unsafe {
                    let lock_ref = &(*ptr).lock;
                    zr::ToMutPtr::to_mut_ptr(lock_ref)
                }
            }

            /// Destroys a `$state` in-place.
            ///
            /// # Safety
            ///
            /// The caller must ensure `state` is a valid reference to an initialized `$state`, and
            /// must not use the state (or the enclosing dispatcher) after this function returns.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_destroy>](
                state: &mut $state,
            ) {
                // SAFETY: The caller is destroying the dispatcher and will not use it again.
                unsafe {
                    core::ptr::drop_in_place(state);
                }
            }
        }
    };
}
pub(crate) use impl_dispatcher_facade_with_state;

/// Helper macro to generate standard `rust_<type>_state_init` FFI trampolines.
macro_rules! impl_dispatcher_state_init {
    ($type:ident, $state:ident $(, $arg:ident : $arg_ty:ty)* $(,)?) => {
        paste::paste! {
            /// Initializes a `$state` in-place using `$state::init(dispatcher, ...)`.
            ///
            /// # Safety
            ///
            /// `ptr` must point to uninitialized memory of at least `size_of::<$state>()`
            /// bytes, and `dispatcher` must point to the enclosing `$type`.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_init>](
                ptr: *mut $state,
                dispatcher: *const $type,
                $( $arg : $arg_ty ),*
            ) {
                // SAFETY: `ptr` points to uninitialized memory allocated for `$state`.
                unsafe {
                    let _ = pin_init::PinInit::__pinned_init(
                        $state::init(dispatcher, $( $arg ),*),
                        ptr,
                    );
                }
            }
        }
    };
}
pub(crate) use impl_dispatcher_state_init;

fbl::impl_opaque_ref_counted_facade!(
    /// Base facade type for kernel Dispatchers.
    pub struct Dispatcher,
    cpp_dispatcher_recycle,
    cpp_dispatcher_get_ref_counted,
);

impl Dispatcher {
    /// Returns the ZX object type of this Dispatcher.
    pub fn get_type(&self) -> zx_types::zx_obj_type_t {
        // SAFETY: self is a valid reference to an initialized Dispatcher.
        unsafe { cpp_dispatcher_get_type(self) }
    }

    /// Returns the kernel object ID (KOID) of this Dispatcher.
    pub fn get_koid(&self) -> zx_types::zx_koid_t {
        // SAFETY: self is a valid reference to an initialized Dispatcher.
        unsafe { super::dispatcher_ffi::cpp_dispatcher_get_koid(self) }
    }

    /// Safely downcasts a `&Dispatcher` reference to a specific facade reference `&T` if the
    /// dispatcher types match.
    pub fn downcast<T: DispatcherOps>(&self) -> Option<&T> {
        if T::TYPE == zx_types::ZX_OBJ_TYPE_NONE || self.get_type() == T::TYPE {
            // SAFETY: `T` implements `DispatcherOps` and its `TYPE` matches `self.get_type()`.
            // All facade types (`ThreadDispatcher`, `ProcessDispatcher`, etc.) are `#[repr(C)]`
            // layout-compatible with `Dispatcher`.
            unsafe { Some(&*(self as *const Self as *const T)) }
        } else {
            None
        }
    }

    /// Resolves a handle to a dispatcher of type T without requiring any rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    pub fn get<T>(handle: HandleValue) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + fbl::HasRefCount + fbl::Recyclable,
    {
        Self::get_with_rights::<T>(handle, zx_types::ZX_RIGHT_NONE)
    }

    /// Resolves a handle to a dispatcher of type T with required rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    /// - `ZX_ERR_ACCESS_DENIED` if `handle` lacks the requested `rights`.
    pub fn get_with_rights<T>(handle: HandleValue, rights: zx_rights_t) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + fbl::HasRefCount + fbl::Recyclable,
    {
        let (dispatcher, actual_rights) = Self::get_dispatcher_and_rights(handle)?;
        if T::TYPE != zx_types::ZX_OBJ_TYPE_NONE && dispatcher.get_type() != T::TYPE {
            return Err(Status::WRONG_TYPE);
        }
        if (actual_rights & rights) != rights {
            return Err(Status::ACCESS_DENIED);
        }
        // SAFETY: We verified the type of the dispatcher, so it is safe to cast.
        unsafe { Ok(dispatcher.cast::<T>()) }
    }

    /// Resolves a handle to a dispatcher and returns its associated rights.
    pub fn get_dispatcher_and_rights(
        handle: HandleValue,
    ) -> Result<(RefPtr<Dispatcher>, zx_rights_t), Status> {
        let mut ref_ptr = MaybeUninit::<RefPtr<Dispatcher>>::zeroed();
        let mut actual_rights = MaybeUninit::<zx_rights_t>::zeroed();
        // SAFETY: ref_ptr and actual_rights point to valid, writable uninitialized memory.
        unsafe {
            let status = cpp_handle_table_get_dispatcher(
                handle,
                ref_ptr.as_mut_ptr(),
                actual_rights.as_mut_ptr(),
            );
            Status::ok(status)?;
            Ok((ref_ptr.assume_init(), actual_rights.assume_init()))
        }
    }
}

impl DispatcherOps for Dispatcher {
    type LockClass = ();
    const TYPE: zx_types::zx_obj_type_t = zx_types::ZX_OBJ_TYPE_NONE;

    fn dispatcher(&self) -> *const Dispatcher {
        self
    }
}

/// Peered dispatchers have opposing endpoints to coordinate state with. For example, writing into
/// one endpoint of a Channel needs to modify `zx_signals_t` state (for the readability bit) on the
/// opposite side. To coordinate their state, they share a mutex, which is held by the
/// `PeerHolder`. Both endpoints have a `RefPtr` back to the `PeerHolder`; no one else ever does.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[repr(C)]
pub struct PeerHolder<Endpoint> {
    #[mutex]
    pub mu: KMutex<RawCriticalMutex>,
    _phantom: PhantomData<Endpoint>,
}

impl<Endpoint> PeerHolder<Endpoint> {
    /// Creates a new `PeerHolder`.
    pub fn create() -> Result<RefPtr<Self>, AllocError> {
        pin_make_ref_counted!(Self {
            mu <- KMutex::init(),
            _phantom: PhantomData,
        })
    }
}
