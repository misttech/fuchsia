// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::dispatcher_ffi::{
    cpp_dispatcher_get_ref_counted, cpp_dispatcher_get_type, cpp_dispatcher_on_zero_handles,
    cpp_dispatcher_recycle, cpp_dispatcher_update_state, cpp_dispatcher_update_state_locked,
};
use crate::handle::HandleValue;
use crate::process_dispatcher_ffi::cpp_handle_table_get_dispatcher;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use kalloc::AllocError;
use ksync::LockToken;
use zx_status::Status;
use zx_types::zx_rights_t;

pub trait DispatcherOps {
    type LockClass;
    const TYPE: zx_types::zx_obj_type_t;

    fn dispatcher(&self) -> *const Dispatcher;

    fn on_zero_handles(&self) {
        unsafe {
            cpp_dispatcher_on_zero_handles(self.dispatcher());
        }
    }

    fn update_state(&self, clear_mask: u32, set_mask: u32) {
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
        unsafe {
            cpp_dispatcher_update_state_locked(self.dispatcher(), clear_mask, set_mask);
        }
    }
}

#[repr(C)]
pub struct Dispatcher {
    _facade: fbl::OpaqueRefCountedFacade,
}

impl Dispatcher {
    pub fn get_type(&self) -> zx_types::zx_obj_type_t {
        unsafe { cpp_dispatcher_get_type(self) }
    }

    pub fn get_with_rights<T>(
        handle: HandleValue,
        rights: zx_rights_t,
    ) -> Result<fbl::RefPtr<T>, Status>
    where
        T: DispatcherOps + fbl::HasRefCount + fbl::Recyclable,
    {
        let mut ref_ptr = MaybeUninit::<fbl::RefPtr<Dispatcher>>::zeroed();
        let mut actual_rights = MaybeUninit::<zx_rights_t>::zeroed();
        let (dispatcher, actual_rights) = unsafe {
            let status = cpp_handle_table_get_dispatcher(
                handle,
                ref_ptr.as_mut_ptr(),
                actual_rights.as_mut_ptr(),
            );
            Status::ok(status)?;
            (ref_ptr.assume_init(), actual_rights.assume_init())
        };
        // TODO(https://fxbug.dev/387324141): Currently, we don't have any use cases for
        // getting a generic Dispatcher. If we need to support this in the future, we will
        // need to change how this works (e.g. by allowing Dispatcher to bypass the type check).
        if dispatcher.get_type() != T::TYPE {
            return Err(Status::WRONG_TYPE);
        }
        if (actual_rights & rights) != rights {
            return Err(Status::ACCESS_DENIED);
        }
        // SAFETY: We verified the type of the dispatcher, so it is safe to cast.
        unsafe { Ok(dispatcher.cast::<T>()) }
    }
}

impl DispatcherOps for Dispatcher {
    type LockClass = ();
    const TYPE: zx_types::zx_obj_type_t = zx_types::ZX_OBJ_TYPE_NONE;

    fn dispatcher(&self) -> *const Dispatcher {
        self
    }
}

impl fbl::HasRefCount for Dispatcher {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe {
            let ptr = cpp_dispatcher_get_ref_counted(self);
            &*(ptr.cast::<fbl::RefCounted>())
        }
    }
}

unsafe impl fbl::Recyclable for Dispatcher {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_dispatcher_recycle(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
