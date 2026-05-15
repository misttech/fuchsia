// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use crate::SinglyLinkedListContainable;
use crate::opaque_ref_counted::OpaqueRefCounted;
use crate::recyclable::Recyclable;
use crate::ref_counted::{HasRefCount, RefCounted};
use crate::ref_ptr::RefPtr;
use crate::singly_linked_list::SinglyLinkedListNode;
use crate::tag::DefaultObjectTag;
use crate::unique_ptr::UniquePtr;
use core::ffi::c_void;
use core::ptr::NonNull;
use kalloc::Box;
use zr::Opaque;

pub trait TestValue: Sized {
    fn new(value: i32) -> Self {
        let _ = value;
        unimplemented!()
    }

    fn new_ref_counted(value: i32) -> RefPtr<Self>
    where
        Self: HasRefCount + Recyclable,
    {
        let _ = value;
        unimplemented!()
    }
}

pub struct RawFactory<T> {
    allocations: crate::Vector<*mut T>,
}

impl<T: Recyclable + TestValue> RawFactory<T> {
    pub fn new() -> Self {
        Self { allocations: crate::Vector::new() }
    }

    pub fn create(&mut self, value: i32) -> *mut T {
        let boxed = kalloc::Box::try_new(T::new(value)).unwrap();
        let raw = kalloc::Box::into_raw(boxed);
        self.allocations.push_back(raw).unwrap();
        raw
    }
}

impl<T> Drop for RawFactory<T> {
    fn drop(&mut self) {
        for &raw in self.allocations.iter() {
            // SAFETY: Reclaiming ownership to free the raw pointers leaked during tests.
            let _ = unsafe { kalloc::Box::from_raw(raw) };
        }
    }
}

pub struct UniqueFactory<T> {
    _phantom: core::marker::PhantomData<T>,
}

impl<T: Recyclable + TestValue> UniqueFactory<T> {
    pub fn new() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }

    pub fn create(&mut self, value: i32) -> UniquePtr<T> {
        UniquePtr::try_new(T::new(value)).unwrap()
    }
}

pub struct RefFactory<T> {
    _phantom: core::marker::PhantomData<T>,
}

impl<T: HasRefCount + Recyclable + TestValue> RefFactory<T> {
    pub fn new() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }

    pub fn create(&mut self, value: i32) -> RefPtr<T> {
        T::new_ref_counted(value)
    }
}

// Shared Interop Code (SLL only in this commit!)

#[unsafe(no_mangle)]
pub extern "C" fn rust_recycle_shared_ref_object(ptr: *mut c_void) {
    unsafe {
        // Reclaim using Box
        let _ = Box::from_raw(ptr as *mut SharedRefObject);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_free_shared_unique_object(ptr: *mut c_void) {
    unsafe {
        // Reclaim using Box
        let _ = Box::from_raw(ptr as *mut SharedUniqueObject);
    }
}

#[derive(crate::SinglyLinkedListContainable)]
#[repr(C)]
pub struct SharedUniqueObject {
    pub value: i32,
    #[sll_node]
    sll_node: SinglyLinkedListNode<SharedUniqueObject>,
    pub allocated_in_rust: bool,
    pub destruction_flag: *mut bool,
}

impl SharedUniqueObject {
    pub fn new(value: i32) -> Self {
        Self {
            value,
            sll_node: SinglyLinkedListNode::new(),
            allocated_in_rust: true,
            destruction_flag: core::ptr::null_mut(),
        }
    }
}

impl Drop for SharedUniqueObject {
    fn drop(&mut self) {
        if !self.destruction_flag.is_null() {
            unsafe {
                *self.destruction_flag = true;
            }
        }
    }
}

unsafe extern "C" {
    fn cpp_destroy_unique_object(obj: *mut c_void);
    fn cpp_delete_ref_object(obj: *mut c_void);
}

unsafe impl Recyclable for SharedUniqueObject {
    unsafe fn recycle(ptr: NonNull<Self>) {
        let raw = ptr.as_ptr();
        unsafe {
            if (*raw).allocated_in_rust {
                let _ = Box::from_non_null(ptr);
            } else {
                cpp_destroy_unique_object(raw as *mut c_void);
            }
        }
    }
    fn allocate(value: Self) -> Result<NonNull<Self>, kalloc::AllocError> {
        let boxed = Box::try_new(value)?;
        let raw = Box::into_raw(boxed);
        unsafe { Ok(NonNull::new_unchecked(raw)) }
    }
}

impl TestValue for SharedUniqueObject {
    fn new(value: i32) -> Self {
        Self::new(value)
    }
}

#[repr(C)]
pub struct SharedRefObject {
    ref_count: RefCounted,
    __fbl_ref_counted_guard: (),
    pub value: i32,
    sll_node: SinglyLinkedListNode<SharedRefObject>,
    pub allocated_in_rust: bool,
    pub destruction_flag: *mut bool,
}

impl SinglyLinkedListContainable<SharedRefObject, DefaultObjectTag> for SharedRefObject {
    fn get_node(&self) -> &SinglyLinkedListNode<SharedRefObject> {
        &self.sll_node
    }
}

impl HasRefCount for SharedRefObject {
    fn ref_count(&self) -> &RefCounted {
        &self.ref_count
    }
}

impl Drop for SharedRefObject {
    fn drop(&mut self) {
        if !self.destruction_flag.is_null() {
            unsafe {
                *self.destruction_flag = true;
            }
        }
    }
}

unsafe impl Recyclable for SharedRefObject {
    unsafe fn recycle(ptr: NonNull<Self>) {
        let raw = ptr.as_ptr();
        unsafe {
            if (*raw).allocated_in_rust {
                let _ = Box::from_non_null(ptr);
            } else {
                cpp_delete_ref_object(raw as *mut c_void);
            }
        }
    }
    fn allocate(value: Self) -> Result<NonNull<Self>, kalloc::AllocError> {
        let boxed = Box::try_new(value)?;
        let raw = Box::into_raw(boxed);
        unsafe { Ok(NonNull::new_unchecked(raw)) }
    }
}

impl TestValue for SharedRefObject {
    fn new_ref_counted(value: i32) -> RefPtr<Self> {
        crate::make_ref_counted!(SharedRefObject {
            value: value,
            sll_node: SinglyLinkedListNode::new(),
            allocated_in_rust: true,
            destruction_flag: core::ptr::null_mut(),
        })
        .unwrap()
    }
}

pub struct CppUniqueObject;
unsafe impl Recyclable for Opaque<CppUniqueObject> {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_destroy_unique_object(ptr.as_ptr() as *mut c_void);
        }
    }
    fn allocate(_value: Self) -> Result<NonNull<Self>, kalloc::AllocError> {
        Err(kalloc::AllocError)
    }
}

pub struct CppRefObject;
unsafe impl Recyclable for OpaqueRefCounted<CppRefObject> {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_delete_ref_object(ptr.as_ptr() as *mut c_void);
        }
    }
    fn allocate(_value: Self) -> Result<NonNull<Self>, kalloc::AllocError> {
        Err(kalloc::AllocError)
    }
}

::zr::static_assert!(core::mem::offset_of!(SharedUniqueObject, value) == 0);
::zr::static_assert!(core::mem::offset_of!(SharedUniqueObject, sll_node) == 8);
::zr::static_assert!(core::mem::offset_of!(SharedUniqueObject, allocated_in_rust) == 16);
::zr::static_assert!(core::mem::offset_of!(SharedUniqueObject, destruction_flag) == 24);
::zr::static_assert!(core::mem::size_of::<SharedUniqueObject>() == 32);

::zr::static_assert!(core::mem::offset_of!(SharedRefObject, ref_count) == 0);
::zr::static_assert!(core::mem::offset_of!(SharedRefObject, value) == 4);
::zr::static_assert!(core::mem::offset_of!(SharedRefObject, sll_node) == 8);
::zr::static_assert!(core::mem::offset_of!(SharedRefObject, allocated_in_rust) == 16);
::zr::static_assert!(core::mem::offset_of!(SharedRefObject, destruction_flag) == 24);
::zr::static_assert!(core::mem::size_of::<SharedRefObject>() == 32);
