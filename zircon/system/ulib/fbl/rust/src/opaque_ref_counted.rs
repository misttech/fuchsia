// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::ref_counted::{HasRefCount, RefCounted};
use zr::Opaque;

/// A wrapper for C++ objects that are known to use `fbl::RefCounted`
/// and have their reference count at offset 0.
#[repr(transparent)]
pub struct OpaqueRefCounted<T>(Opaque<T>);

impl<T> OpaqueRefCounted<T> {
    /// Returns a raw pointer to the opaque data.
    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}

impl<T> HasRefCount for OpaqueRefCounted<T> {
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: OpaqueRefCounted guarantees that the ref count is at offset 0.
        unsafe { &*(self.get() as *const RefCounted) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recyclable::Recyclable;
    use crate::ref_ptr::RefPtr;
    use core::ffi::c_void;
    use core::ptr::NonNull;

    unsafe extern "C" {
        fn create_cpp_ref_counted_object(destroyed: *mut bool) -> *mut c_void;
        fn destroy_cpp_ref_counted_object(ptr: *mut c_void);
    }

    pub struct TestCppRefCountedObject;

    unsafe impl Recyclable for OpaqueRefCounted<TestCppRefCountedObject> {
        unsafe fn recycle(ptr: NonNull<Self>) {
            unsafe {
                destroy_cpp_ref_counted_object(ptr.as_ptr() as *mut c_void);
            }
        }

        fn allocate(_value: Self) -> Result<NonNull<Self>, ::kalloc::AllocError> {
            Err(::kalloc::AllocError)
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "miri does not support calling foreign functions")]
    fn test_cross_lang_ref_ptr() {
        use core::sync::atomic::{AtomicBool, Ordering};

        let destroyed = AtomicBool::new(false);
        unsafe {
            let raw_ptr = create_cpp_ref_counted_object(destroyed.as_ptr());
            assert!(!destroyed.load(Ordering::Relaxed));

            {
                let ref_ptr =
                    RefPtr::from_raw(raw_ptr as *mut OpaqueRefCounted<TestCppRefCountedObject>);
                assert!(!destroyed.load(Ordering::Relaxed));

                let ref_ptr_clone = ref_ptr.clone();
                assert!(!destroyed.load(Ordering::Relaxed));

                // Drop clone
                drop(ref_ptr_clone);
                assert!(!destroyed.load(Ordering::Relaxed));
            } // Drop ref_ptr -> count becomes 0 -> calls recycle -> calls C++ release!

            assert!(destroyed.load(Ordering::Relaxed));
        }
    }
}
