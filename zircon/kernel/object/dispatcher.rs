// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::dispatcher_ffi::{
    cpp_dispatcher_get_ref_counted, cpp_dispatcher_on_zero_handles, cpp_dispatcher_recycle,
    cpp_dispatcher_update_state, cpp_dispatcher_update_state_locked,
};
use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use kalloc::AllocError;
use ksync::LockToken;

pub trait DispatcherOps {
    type LockClass;

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
    _marker: PhantomData<PhantomPinned>,
    _facade: zr::OpaqueFacade,
}

unsafe impl Send for Dispatcher {}
unsafe impl Sync for Dispatcher {}

impl DispatcherOps for Dispatcher {
    type LockClass = ();

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
