// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::user_ptr::{UserInOutPtr, UserInPtr, UserOutPtr};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::zx_iovec_t;

#[repr(C)]
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, Default)]
struct RawIovec {
    buffer: usize,
    capacity: usize,
}

/// A wrapper around a userspace array of `zx_iovec_t` for read operations.
#[derive(Debug, Copy, Clone)]
pub struct UserInIovec {
    vector: UserInPtr<zx_iovec_t>,
    count: usize,
}

impl UserInIovec {
    /// Constructs a new `UserInIovec`.
    pub const fn new(vector: UserInPtr<zx_iovec_t>, count: usize) -> Self {
        Self { vector, count }
    }

    /// Returns true if the underlying pointer is null.
    pub fn is_null(&self) -> bool {
        self.vector.is_null()
    }

    /// Calculates the total capacity across all iovecs.
    pub fn get_total_capacity(&self) -> Result<usize, Status> {
        let mut total = 0usize;
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            total = total.checked_add(elem.capacity).ok_or(Status::INVALID_ARGS)?;
        }
        Ok(total)
    }

    /// Iterates through the iovecs and invokes the callback for each user pointer and capacity.
    pub fn for_each<F>(&self, mut cb: F) -> Result<(), Status>
    where
        F: FnMut(UserInPtr<u8>, usize) -> Result<(), Status>,
    {
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            let ptr = UserInPtr::new(elem.buffer as *const u8);
            cb(ptr, elem.capacity)?;
        }
        Ok(())
    }
}

/// A wrapper around a userspace array of `zx_iovec_t` for write operations.
#[derive(Debug, Copy, Clone)]
pub struct UserOutIovec {
    vector: UserInPtr<zx_iovec_t>,
    count: usize,
}

impl UserOutIovec {
    /// Constructs a new `UserOutIovec`.
    pub const fn new(vector: UserInPtr<zx_iovec_t>, count: usize) -> Self {
        Self { vector, count }
    }

    /// Returns true if the underlying pointer is null.
    pub fn is_null(&self) -> bool {
        self.vector.is_null()
    }

    /// Calculates the total capacity across all iovecs.
    pub fn get_total_capacity(&self) -> Result<usize, Status> {
        let mut total = 0usize;
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            total = total.checked_add(elem.capacity).ok_or(Status::INVALID_ARGS)?;
        }
        Ok(total)
    }

    /// Iterates through the iovecs and invokes the callback for each user pointer and capacity.
    pub fn for_each<F>(&self, mut cb: F) -> Result<(), Status>
    where
        F: FnMut(UserOutPtr<u8>, usize) -> Result<(), Status>,
    {
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            let ptr = UserOutPtr::new(elem.buffer as *mut u8);
            cb(ptr, elem.capacity)?;
        }
        Ok(())
    }
}

/// A wrapper around a userspace array of `zx_iovec_t` for read-write operations.
#[derive(Debug, Copy, Clone)]
pub struct UserInOutIovec {
    vector: UserInPtr<zx_iovec_t>,
    count: usize,
}

impl UserInOutIovec {
    /// Constructs a new `UserInOutIovec`.
    pub const fn new(vector: UserInPtr<zx_iovec_t>, count: usize) -> Self {
        Self { vector, count }
    }

    /// Returns true if the underlying pointer is null.
    pub fn is_null(&self) -> bool {
        self.vector.is_null()
    }

    /// Calculates the total capacity across all iovecs.
    pub fn get_total_capacity(&self) -> Result<usize, Status> {
        let mut total = 0usize;
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            total = total.checked_add(elem.capacity).ok_or(Status::INVALID_ARGS)?;
        }
        Ok(total)
    }

    /// Iterates through the iovecs and invokes the callback for each user pointer and capacity.
    pub fn for_each<F>(&self, mut cb: F) -> Result<(), Status>
    where
        F: FnMut(UserInOutPtr<u8>, usize) -> Result<(), Status>,
    {
        let raw_vec = self.vector.reinterpret::<RawIovec>();
        for i in 0..self.count {
            let elem = raw_vec.element_offset(i).read()?;
            let ptr = UserInOutPtr::new(elem.buffer as *mut u8);
            cb(ptr, elem.capacity)?;
        }
        Ok(())
    }
}
