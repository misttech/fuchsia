// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::recyclable::{Recyclable, UninitRecyclable};
use crate::ref_counted::{HasRefCount, HasRefCountUpgradeable};
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::NonNull;
use kalloc::AllocError;

use pin_init::{Init, PinInit};

/// `RefPtr<T>` holds a reference to an intrusively-refcounted object of type
/// T that deletes the object when the refcount drops to 0.
///
/// T should be a struct that contains a `fbl::RefCounted` field and implements
/// `HasRefCount` and `Destroy` traits.
#[repr(transparent)]
pub struct RefPtr<T>
where
    T: HasRefCount + Recyclable,
{
    ptr: NonNull<T>,
}

impl<T: HasRefCount + Recyclable> RefPtr<T> {
    /// Constructs a `RefPtr` from a raw pointer that has already been adopted.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `ptr` is valid and has a ref count already
    ///   acquired.
    /// - `ptr` must have been allocated in such a way that calling `T::recycle(ptr)` is a
    ///   correct way to deallocate the pointer.
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // SAFETY: The caller must ensure that ptr is valid.
        unsafe { RefPtr { ptr: NonNull::new_unchecked(ptr as *mut T) } }
    }

    /// Constructs a `RefPtr` from a fresh object that has not been adopted yet.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid, non-null, and point to a freshly initialized `T` whose
    ///   `RefCounted` has not been adopted yet.
    /// - `ptr` must have been allocated in such a way that calling `T::recycle(ptr)` is a
    ///   correct way to deallocate the pointer.
    pub unsafe fn adopt_raw(ptr: *mut T) -> Self {
        // SAFETY: The caller ensures `ptr` is valid and has not been adopted yet.
        unsafe {
            (*ptr).ref_count().adopt();
            Self::from_raw(ptr)
        }
    }

    /// Constructs a `RefPtr` from a raw pointer that has already been adopted, unless the pointer
    /// is null.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is either null or, if not null that:
    /// - a ref count already acquired.
    /// - has been allocated in such a way that calling `T::recycle(ptr)` is a correct way to
    ///   deallocate the pointer.
    pub unsafe fn try_from_raw(ptr: *const T) -> Option<Self> {
        NonNull::new(ptr as *mut T).map(|ptr| RefPtr { ptr })
    }

    /// Helper function that allocates a new instance of `T` using `T::allocate` and
    /// returns a `RefPtr` wrapping it.
    ///
    /// This is an internal helper function that should not be used directly.
    /// Use the `make_ref_counted!(...)` macro instead of this function to properly
    /// initialize the ref count.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `T` has a RefCounted field that is not
    /// already adopted.
    pub unsafe fn try_new(value: T) -> Result<RefPtr<T>, AllocError> {
        let mut ptr = T::allocate(value)?;
        // SAFETY: The caller must ensure that T has a RefCounted field that is not
        // already adopted.
        unsafe { ptr.as_mut().ref_count().adopt() };
        Ok(RefPtr { ptr })
    }

    /// Returns the raw pointer to the object.
    pub fn as_ptr(this: &Self) -> *const T {
        this.ptr.as_ptr()
    }

    /// Returns `true` if the two `RefPtr`s point to the same object.
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    /// Consume the `RefPtr` and return the raw pointer without modifying the ref count.
    ///
    /// The caller is responsible for maintaining the reference count.
    pub fn into_raw(this: Self) -> *const T {
        let ptr = this.ptr;
        core::mem::forget(this);
        ptr.as_ptr()
    }

    /// Casts this `RefPtr` to point to a different type.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the object pointed to by this `RefPtr` can be safely
    /// treated as an instance of type `U`.
    pub unsafe fn cast<U>(self) -> RefPtr<U>
    where
        U: HasRefCount + Recyclable,
    {
        let ptr = self.ptr.cast::<U>();
        core::mem::forget(self);
        RefPtr { ptr }
    }

    /// Use the given pin-initializer to pin-initialize a `T` inside of a new `RefPtr`.
    #[inline]
    pub fn try_pin_init<E>(init: impl PinInit<T, E>) -> Result<Self, E>
    where
        T: UninitRecyclable,
        E: From<AllocError>,
    {
        let ptr = T::allocate_uninit()?;
        let guard = UninitRefGuard { ptr };
        let slot = guard.ptr.as_ptr() as *mut T;
        // SAFETY: `slot` is valid and will not be moved.
        unsafe { init.__pinned_init(slot)? };
        // SAFETY: The object is now initialized, and no `RefPtr` to it exists.
        unsafe { (*slot).ref_count().adopt() };
        let initialized_ptr = guard.ptr.cast::<T>();
        core::mem::forget(guard);
        let initialized_ref = RefPtr { ptr: initialized_ptr };
        Ok(initialized_ref)
    }

    /// Use the given initializer to in-place initialize a `T` inside of a new `RefPtr`.
    #[inline]
    pub fn try_init<E>(init: impl Init<T, E>) -> Result<Self, E>
    where
        T: UninitRecyclable,
        E: From<AllocError>,
    {
        let ptr = T::allocate_uninit()?;
        let guard = UninitRefGuard { ptr };
        let slot = guard.ptr.as_ptr() as *mut T;
        // SAFETY: `slot` is valid.
        unsafe { init.__init(slot)? };
        // SAFETY: The object is now initialized, and no `RefPtr` to it exists.
        unsafe { (*slot).ref_count().adopt() };
        let initialized_ptr = guard.ptr.cast::<T>();
        core::mem::forget(guard);
        Ok(RefPtr { ptr: initialized_ptr })
    }

    /// Use the given pin-initializer to pin-initialize a `T` inside of a new `RefPtr`.
    #[inline]
    pub fn pin_init(init: impl PinInit<T, core::convert::Infallible>) -> Result<Self, AllocError>
    where
        T: UninitRecyclable,
    {
        let init = unsafe {
            ::pin_init::pin_init_from_closure(|slot| {
                init.__pinned_init(slot).map_err(|i| match i {})
            })
        };
        Self::try_pin_init(init)
    }

    /// Use the given initializer to in-place initialize a `T` inside of a new `RefPtr`.
    #[inline]
    pub fn init(init: impl Init<T, core::convert::Infallible>) -> Result<Self, AllocError>
    where
        T: UninitRecyclable,
    {
        let init = unsafe {
            ::pin_init::init_from_closure(|slot| init.__init(slot).map_err(|i| match i {}))
        };
        Self::try_init(init)
    }

    /// Manually increments the reference count of `target`.
    ///
    /// Every call to `add_ref` should be balanced by a subsequent drop of a
    /// `RefPtr` (e.g. constructed via `RefPtr::from_raw`), otherwise memory will
    /// be leaked. It is safe to leak memory in Rust.
    #[inline]
    pub fn add_ref(target: &T) {
        target.ref_count().add_ref();
    }

    /// Constructs a `RefPtr` from a reference by incrementing its ref count.
    #[inline]
    pub fn from_ref(target: &T) -> Self {
        target.ref_count().add_ref();
        RefPtr { ptr: NonNull::from(target) }
    }
}

impl<T: HasRefCount + Recyclable + HasRefCountUpgradeable> RefPtr<T> {
    /// Constructs a RefPtr from a raw T* which is being held alive by RefPtr
    /// with the caveat that the existing RefPtr might be in the process of
    /// destructing the T object. When the T object is in the destructor, the
    /// resulting RefPtr is null, otherwise the resulting RefPtr points to T*
    /// with the updated reference count.
    ///
    /// The only way for this to be a valid pattern is that the call is made
    /// while holding some lock and that the same lock also is used to protect the
    /// value of T* .
    ///
    /// This pattern is needed in collaborating objects which cannot hold a
    /// RefPtr to each other because it would cause a reference cycle. Instead
    /// there is a raw pointer from one to the other and a RefPtr in the
    /// other direction. When needed the raw pointer can be upgraded via
    /// make_ref_ptr_upgrade_from_raw and operated outside the lock.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `ptr` is valid.
    /// - `ptr` must have been allocated in such a way that calling `T::recycle(ptr)` is a
    ///   correct way to deallocate the pointer.
    /// - Caller must hold a lock or otherwise know that `ptr` lives for the duration of this
    ///   method.
    pub unsafe fn make_ref_ptr_upgrade_from_raw(ptr: *const T) -> Option<Self> {
        // SAFETY: The caller must ensure that ptr is valid.
        unsafe {
            if !ptr.as_ref_unchecked().ref_count().add_ref_maybe_in_destructor() {
                None
            } else {
                Some(Self::from_raw(ptr))
            }
        }
    }
}

impl<T: HasRefCount + Recyclable> Deref for RefPtr<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: HasRefCount + Recyclable> Clone for RefPtr<T> {
    #[inline]
    fn clone(&self) -> Self {
        self.deref().ref_count().add_ref();
        RefPtr { ptr: self.ptr }
    }
}

impl<T: HasRefCount + Recyclable> Drop for RefPtr<T> {
    #[inline]
    fn drop(&mut self) {
        if self.deref().ref_count().release() {
            unsafe {
                T::recycle(self.ptr);
            }
        }
    }
}

impl<T: HasRefCount + Recyclable> PartialEq for RefPtr<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        RefPtr::ptr_eq(self, other)
    }
}

impl<T: HasRefCount + Recyclable> Eq for RefPtr<T> {}

unsafe impl<T: HasRefCount + Recyclable + Send + Sync> Send for RefPtr<T> {}
unsafe impl<T: HasRefCount + Recyclable + Send + Sync> Sync for RefPtr<T> {}

struct UninitRefGuard<T: UninitRecyclable> {
    ptr: NonNull<MaybeUninit<T>>,
}

impl<T: UninitRecyclable> Drop for UninitRefGuard<T> {
    fn drop(&mut self) {
        unsafe {
            T::recycle_uninit(self.ptr);
        }
    }
}

/// Macro to construct a RefPtr, automatically populating the ref_count field.
#[macro_export]
macro_rules! make_ref_counted {
    ($ty:ident { $($field:ident : $val:expr),* $(,)? }) => {
        // SAFETY: The macro creates a new object with a ref count of 1.
        unsafe {
            $crate::RefPtr::try_new($ty {
                ref_count: $crate::RefCounted::new(),
                __fbl_ref_counted_guard: (),
                $($field : $val),*
            })
        }
    };
}

/// Macro to construct a RefPtr with pin-initialization, automatically populating the ref_count
/// field.
#[macro_export]
macro_rules! pin_make_ref_counted {
    ($ty:ident { $($field:tt)* }) => {
        $crate::RefPtr::pin_init($crate::pin_init::pin_init!($ty {
            ref_count: $crate::RefCounted::new(),
            __fbl_ref_counted_guard: (),
            $($field)*
        }))
    };
}

/// Macro to construct a RefPtr with fallible pin-initialization, automatically populating the
/// ref_count field.
#[macro_export]
macro_rules! try_pin_make_ref_counted {
    ($ty:ident { $($field:tt)* }) => {
        $crate::RefPtr::try_pin_init($crate::pin_init::pin_init!($ty {
            ref_count: $crate::RefCounted::new(),
            __fbl_ref_counted_guard: (),
            $($field)*
        }))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;
    use core::pin::Pin;
    use core::ptr::null;
    use core::sync::atomic::{AtomicBool, Ordering};

    extern crate alloc;
    extern crate std;
    use alloc::sync::Arc;
    use std::sync::{Barrier, Mutex};
    use std::thread;

    #[unsafe(no_mangle)]
    pub extern "C" fn rust_recycle_test_rust_ref_counted(ptr: *mut c_void) {
        unsafe { TestRustRefCounted::recycle_ffi(ptr) }
    }

    unsafe extern "C" {
        fn test_import_rust_ref_counted(ptr: *mut c_void);
    }

    #[fbl::ref_counted]
    #[pin_init::pin_data(PinnedDrop)]
    #[derive(crate::Recyclable)]
    #[repr(C)]
    pub struct TestRustRefCounted {
        destroyed: Arc<AtomicBool>,
    }

    ::zr::static_assert!(core::mem::size_of::<RefPtr<TestRustRefCounted>>() == 8);
    ::zr::static_assert!(core::mem::align_of::<RefPtr<TestRustRefCounted>>() == 8);
    ::zr::static_assert!(core::mem::size_of::<Option<RefPtr<TestRustRefCounted>>>() == 8);
    ::zr::static_assert!(core::mem::align_of::<Option<RefPtr<TestRustRefCounted>>>() == 8);

    #[pin_init::pinned_drop]
    impl pin_init::PinnedDrop for TestRustRefCounted {
        fn drop(self: Pin<&mut Self>) {
            self.destroyed.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_rust_drops_reference() {
        let destroyed = Arc::new(AtomicBool::new(false));
        {
            let ref_ptr =
                make_ref_counted!(TestRustRefCounted { destroyed: destroyed.clone() }).unwrap();
            assert!(!destroyed.load(Ordering::Relaxed));
            let ref_ptr_clone = ref_ptr.clone();
            drop(ref_ptr_clone);
            assert!(!destroyed.load(Ordering::Relaxed));
        } // Drop ref_ptr -> count becomes 0 -> calls destroy -> triggers Drop trait!

        assert!(destroyed.load(Ordering::Relaxed));
    }

    #[test]
    #[cfg_attr(miri, ignore = "miri does not support calling foreign functions")]
    fn test_cpp_drops_reference() {
        let destroyed = Arc::new(AtomicBool::new(false));
        let ref_ptr =
            make_ref_counted!(TestRustRefCounted { destroyed: destroyed.clone() }).unwrap();
        let raw_ptr = RefPtr::into_raw(ref_ptr);

        unsafe {
            assert!(!destroyed.load(Ordering::Relaxed));
            // Pass to C++!
            test_import_rust_ref_counted(raw_ptr as *const TestRustRefCounted as *mut c_void);
            // C++ should have acquired reference and released it!
            // And since count was 1, it should have dropped it!
            assert!(destroyed.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn test_ref_ptr_compare() {
        let destroyed1 = Arc::new(AtomicBool::new(false));
        let destroyed2 = Arc::new(AtomicBool::new(false));
        let ptr1 = make_ref_counted!(TestRustRefCounted { destroyed: destroyed1.clone() }).unwrap();
        let ptr2 = make_ref_counted!(TestRustRefCounted { destroyed: destroyed2.clone() }).unwrap();
        let ptr1_clone = ptr1.clone();

        assert!(ptr1 == ptr1);
        assert!(ptr1 != ptr2);
        assert!(ptr1 == ptr1_clone);
    }

    #[test]
    fn test_rust_pin_init() {
        let destroyed = Arc::new(AtomicBool::new(false));
        let destroyed_clone = destroyed.clone();
        {
            let ref_ptr =
                pin_make_ref_counted!(TestRustRefCounted { destroyed: destroyed_clone }).unwrap();
            assert!(!destroyed.load(Ordering::Relaxed));
            let ref_ptr_clone = ref_ptr.clone();
            drop(ref_ptr_clone);
            assert!(!destroyed.load(Ordering::Relaxed));
        } // Drop ref_ptr
        assert!(destroyed.load(Ordering::Relaxed));
    }

    #[fbl::ref_counted]
    #[pin_init::pin_data]
    #[derive(crate::Recyclable)]
    #[repr(C)]
    struct FallibleInit {
        value: i32,
    }

    #[test]
    fn test_rust_try_pin_init_fail() {
        let init = unsafe {
            ::pin_init::pin_init_from_closure(
                |_slot: *mut FallibleInit| -> Result<(), AllocError> { Err(AllocError) },
            )
        };
        let res = RefPtr::try_pin_init(init);
        assert!(res.is_err());
    }

    #[test]
    fn test_null_try_from() {
        let maybe_ref_ptr = unsafe { RefPtr::try_from_raw(null::<TestRustRefCounted>()) };
        assert!(maybe_ref_ptr.is_none());
    }

    #[test]
    fn test_add_ref() {
        let destroyed = Arc::new(AtomicBool::new(false));
        {
            let ref_ptr =
                make_ref_counted!(TestRustRefCounted { destroyed: destroyed.clone() }).unwrap();
            assert!(!destroyed.load(Ordering::Relaxed));
            RefPtr::add_ref(&*ref_ptr);
            let ref_ptr2 = unsafe { RefPtr::from_raw(RefPtr::as_ptr(&ref_ptr)) };
            drop(ref_ptr);
            assert!(!destroyed.load(Ordering::Relaxed));
            drop(ref_ptr2);
            assert!(destroyed.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn test_from_ref() {
        let destroyed = Arc::new(AtomicBool::new(false));
        {
            let ref_ptr =
                make_ref_counted!(TestRustRefCounted { destroyed: destroyed.clone() }).unwrap();
            assert!(!destroyed.load(Ordering::Relaxed));
            let ref_ptr2 = RefPtr::from_ref(&*ref_ptr);
            assert!(ref_ptr == ref_ptr2);
            drop(ref_ptr);
            assert!(!destroyed.load(Ordering::Relaxed));
            drop(ref_ptr2);
            assert!(destroyed.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn test_adopt_raw() {
        let destroyed = Arc::new(AtomicBool::new(false));
        let ptr = TestRustRefCounted::allocate(TestRustRefCounted {
            ref_count: crate::RefCounted::new(),
            __fbl_ref_counted_guard: (),
            destroyed: destroyed.clone(),
        })
        .unwrap();

        {
            // SAFETY: `ptr` was freshly allocated by `TestRustRefCounted::allocate` and has not
            // been adopted yet.
            let ref_ptr = unsafe { RefPtr::adopt_raw(ptr.as_ptr()) };
            assert_eq!(ref_ptr.ref_count().ref_count_debug(), 1);
            assert!(!destroyed.load(Ordering::Relaxed));
        }

        assert!(destroyed.load(Ordering::Relaxed));
    }

    #[fbl::ref_counted]
    #[derive(crate::Recyclable)]
    #[repr(C)]
    struct RawUpgradeTester {
        mutex: Arc<Mutex<()>>,
        destroying: Arc<AtomicBool>,
        destroying_barrier: Option<Arc<Barrier>>,
    }

    impl HasRefCountUpgradeable for RawUpgradeTester {}

    impl Drop for RawUpgradeTester {
        fn drop(&mut self) {
            self.destroying.store(true, Ordering::SeqCst);
            if let Some(barrier) = &self.destroying_barrier {
                barrier.wait();
            }
            let _guard = self.mutex.lock().unwrap();
        }
    }

    #[test]
    fn test_upgrade_fail() {
        let mutex = Arc::new(Mutex::new(()));
        let destroying = Arc::new(AtomicBool::new(false));
        let destroying_barrier = Arc::new(Barrier::new(2));

        let ref_ptr = make_ref_counted!(RawUpgradeTester {
            mutex: mutex.clone(),
            destroying: destroying.clone(),
            destroying_barrier: Some(destroying_barrier.clone()),
        })
        .unwrap();
        let raw = RefPtr::as_ptr(&ref_ptr);

        let handle = {
            let _guard = mutex.lock().unwrap();
            let handle = thread::spawn(move || {
                // Dropping `ref_ptr` will call the destructor, which we expect to
                // block because `test_upgrade_fail` is holding the mutex.
                drop(ref_ptr);
            });

            // Wait until the thread is in the destructor.
            destroying_barrier.wait();
            assert!(destroying.load(Ordering::SeqCst));

            // The RawUpgradeTester must be blocked in the destructor, the upgrade will fail.
            // SAFETY: `raw` is valid because the destructor is blocked on `mutex`.
            let upgrade1 = unsafe { RefPtr::make_ref_ptr_upgrade_from_raw(raw) };
            assert!(upgrade1.is_none());

            // Verify that the previous upgrade attempt did not change the refcount.
            // SAFETY: `raw` is valid because the destructor is blocked on `mutex`.
            let upgrade2 = unsafe { RefPtr::make_ref_ptr_upgrade_from_raw(raw) };
            assert!(upgrade2.is_none());

            handle
        };

        handle.join().unwrap();
    }

    #[test]
    fn test_upgrade_success() {
        let mutex = Arc::new(Mutex::new(()));
        let destroying = Arc::new(AtomicBool::new(false));

        let ref_ptr = make_ref_counted!(RawUpgradeTester {
            mutex: mutex.clone(),
            destroying: destroying.clone(),
            destroying_barrier: None,
        })
        .unwrap();
        let raw = RefPtr::as_ptr(&ref_ptr);

        {
            let _guard = mutex.lock().unwrap();
            // RawUpgradeTester is not in the destructor so the upgrade should
            // succeed.
            // SAFETY: `raw` is valid because `ref_ptr` is alive.
            let upgrade = unsafe { RefPtr::make_ref_ptr_upgrade_from_raw(raw) };
            assert!(upgrade.is_some());
        }

        drop(ref_ptr);
        assert!(destroying.load(Ordering::SeqCst));
    }
}
