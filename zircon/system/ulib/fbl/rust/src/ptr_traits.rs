// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::recyclable::Recyclable;
use crate::ref_counted::HasRefCount;
use crate::ref_ptr::RefPtr;
use crate::unique_ptr::UniquePtr;
use core::ops::Deref;
use core::ptr::NonNull;

/// Trait for pointer types that can be stored in intrusive containers.
///
/// # Safety
///
/// Implementing this trait is unsafe because the container relies on the correctness of
/// the implementation to maintain memory safety:
/// - `into_raw` must return a valid, non-null, and dereferenceable pointer to `Target` that
///   uniquely identifies the object.
/// - `from_raw` must correctly reconstruct a valid instance of `Self` from a pointer previously
///   returned by `into_raw`.
/// - `from_raw` must reclaim the ownership previously yielded by `into_raw`.
/// - `get_ref` must return a valid reference to the `Target`.
pub unsafe trait PtrTraits {
    /// The type pointed to by this pointer.
    type Target;
    /// Whether the pointer type is managed (e.g., `UniquePtr` or `RefPtr`).
    ///
    /// If the pointer type is managed, containers can be dropped while they
    /// are non-empty because the pointers can free the underlying memory. If
    /// the pointer is not managed, then the containers need to be empty when
    /// they are dropped to avoid memory leaks.
    const IS_MANAGED: bool;
    /// Consumes the pointer and returns a raw pointer to the target.
    fn into_raw(self) -> *mut Self::Target;
    /// Creates an instance of `Self` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `raw` was returned by a previous call to
    /// `into_raw` on an instance of `Self`, and that it has not been used to
    /// create another instance of `Self`.
    unsafe fn from_raw(raw: *mut Self::Target) -> Self;
    /// Returns a reference to the target.
    fn get_ref(&self) -> &Self::Target;
}

// SAFETY: `*mut T` behaves as a raw pointer with identity mapped into/from raw conversions.
// It performs no lifecycle management and get_ref is safe if the pointer is valid.
unsafe impl<T> PtrTraits for *mut T {
    type Target = T;
    const IS_MANAGED: bool = false;
    #[inline]
    fn into_raw(self) -> *mut T {
        self
    }
    #[inline]
    unsafe fn from_raw(raw: *mut T) -> Self {
        raw
    }
    #[inline]
    fn get_ref(&self) -> &T {
        // SAFETY: The caller must ensure that `self` (which is `&(*mut T)`) points to a valid T.
        unsafe { &**self }
    }
}

// SAFETY: `NonNull<T>` behaves as a raw pointer with identity mapped into/from raw conversions.
// It performs no lifecycle management and get_ref is safe if the pointer is valid.
unsafe impl<T> PtrTraits for NonNull<T> {
    type Target = T;
    const IS_MANAGED: bool = false;
    #[inline]
    fn into_raw(self) -> *mut T {
        self.as_ptr()
    }
    #[inline]
    unsafe fn from_raw(raw: *mut T) -> Self {
        // SAFETY: The caller of `from_raw` must ensure that `raw` was returned
        // by a previous call to `into_raw` on an instance of `Self`. Since `into_raw`
        // for `NonNull` always returns a non-null pointer, `raw` is guaranteed to be non-null.
        unsafe { NonNull::new_unchecked(raw) }
    }
    #[inline]
    fn get_ref(&self) -> &T {
        // SAFETY: The caller must ensure that `self` points to a valid T.
        unsafe { self.as_ref() }
    }
}

// SAFETY: `UniquePtr<T>` owns the unique reference to `T`. Roundtrip between into_raw
// and from_raw is guaranteed to preserve unique ownership and lifetimes.
unsafe impl<T: Recyclable> PtrTraits for UniquePtr<T> {
    type Target = T;
    const IS_MANAGED: bool = true;
    #[inline]
    fn into_raw(self) -> *mut T {
        UniquePtr::into_raw(self)
    }
    #[inline]
    unsafe fn from_raw(raw: *mut T) -> Self {
        // SAFETY: The caller of `from_raw` must ensure that `raw` was returned
        // by a previous call to `into_raw` on an instance of `Self`, and that it
        // has not been used to create another instance of `Self`.
        unsafe { UniquePtr::from_raw(raw) }
    }
    #[inline]
    fn get_ref(&self) -> &T {
        self.deref()
    }
}

// SAFETY: `RefPtr<T>` manages a shared atomic ref-count on `T`. Roundtrip between
// into_raw and from_raw correctly transfers a single reference.
unsafe impl<T: HasRefCount + Recyclable> PtrTraits for RefPtr<T> {
    type Target = T;
    const IS_MANAGED: bool = true;
    #[inline]
    fn into_raw(self) -> *mut T {
        RefPtr::into_raw(self) as *mut T
    }
    #[inline]
    unsafe fn from_raw(raw: *mut T) -> Self {
        // SAFETY: The caller of `from_raw` must ensure that `raw` was returned
        // by a previous call to `into_raw` on an instance of `Self`, and that it
        // has not been used to create another instance of `Self`.
        unsafe { RefPtr::from_raw(raw) }
    }
    #[inline]
    fn get_ref(&self) -> &T {
        self.deref()
    }
}

/// Marker trait for managed pointer types (like `UniquePtr` and `RefPtr`).
///
/// # Safety
///
/// Implementing this trait is unsafe because:
/// - The implementer must guarantee that the pointer type `P` actually manages the lifetime
///   of its `Target` (i.e., it holds exclusive or shared ownership of the target).
/// - Dropping the pointer `P` must correctly release the reference/ownership, causing the
///   target to be dropped and cleaned up once there are no more owners.
/// - The target object must be guaranteed to outlive its reference in the list while contained.
pub unsafe trait ManagedPtr: PtrTraits {}

// SAFETY: `UniquePtr<T>` uniquely manages `T`'s lifetime.
unsafe impl<T: Recyclable> ManagedPtr for UniquePtr<T> {}
// SAFETY: `RefPtr<T>` manages `T`'s lifetime via atomic reference counting.
unsafe impl<T: HasRefCount + Recyclable> ManagedPtr for RefPtr<T> {}

/// Marker trait for pointer types through which intrusive containers may hand out mutable
/// (pinned) access to their elements (e.g. `front_mut()`, `iter_mut()`, `CursorMut::get_mut()`).
///
/// # Safety
///
/// Implementing this trait is unsafe because containers rely on it to soundly produce
/// `Pin<&mut Self::Target>` from their stored pointers. The implementer must guarantee that
/// exclusive access to a container holding `Self` (i.e. `Pin<&mut Container<Self>>`) implies
/// exclusive mutable access to every `Self::Target` stored in it. In particular:
/// - `Self` must not represent shared ownership of the target (as `RefPtr<T>` does), since other
///   owners could otherwise observe the target while it is being mutated through the container.
/// - For unmanaged pointers (e.g. `*mut T`, `NonNull<T>`), when mutable element accessors
///   (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on the container,
///   callers of the container's `unsafe` raw insertion APIs take on the obligation that no other
///   references to the element are accessed while it is mutably borrowed through the container.
/// - Because mutable element accessors expose `Pin<&mut Self::Target>`, elements accessed through
///   them must satisfy `Pin`'s drop guarantee: unless `Self::Target: Unpin`, an element must not be
///   moved out of its memory location (or have that memory reused) before it is dropped. For
///   managed pointers this is upheld by the pointer type itself; for unmanaged pointers it is an
///   obligation of the callers of the container's `unsafe` raw insertion APIs.
pub unsafe trait MutPtr: PtrTraits {}

// SAFETY: Raw pointers carry no ownership. When mutable element accessors are used, callers of the
// `unsafe` raw insertion APIs guarantee that no other references to the pointee are accessed while
// it is mutably borrowed through the container, and that the pointee satisfies `Pin`'s drop
// guarantee (it is not moved before being dropped).
unsafe impl<T> MutPtr for *mut T {}
// SAFETY: `NonNull<T>` carries no ownership. When mutable element accessors are used, callers of
// the `unsafe` raw insertion APIs guarantee that no other references to the pointee are accessed
// while it is mutably borrowed through the container, and that the pointee satisfies `Pin`'s drop
// guarantee (it is not moved before being dropped).
unsafe impl<T> MutPtr for NonNull<T> {}
// SAFETY: `UniquePtr<T>` holds the only reference to `T`, so exclusive access to the container
// that owns it implies exclusive access to `T`.
unsafe impl<T: Recyclable> MutPtr for UniquePtr<T> {}
