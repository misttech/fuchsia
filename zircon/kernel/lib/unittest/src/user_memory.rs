// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use zx_status::Status;

// Opaque type for C++ UserMemory
#[repr(C)]
struct RawUserMemory {
    _facade: zr::OpaqueFacade,
}

unsafe extern "C" {
    fn unittest_user_memory_create(size: usize) -> *mut RawUserMemory;
    fn unittest_user_memory_destroy(mem: *mut RawUserMemory);
    fn unittest_user_memory_get_base(mem: *const RawUserMemory) -> usize;
    fn unittest_user_memory_commit_and_map(mem: *const RawUserMemory, size: usize) -> i32;
    fn unittest_user_memory_vmo_write(
        mem: *const RawUserMemory,
        ptr: *const c_void,
        offset: u64,
        len: u64,
    ) -> i32;
    fn unittest_user_memory_vmo_read(
        mem: *const RawUserMemory,
        ptr: *mut c_void,
        offset: u64,
        len: u64,
    ) -> i32;
}

/// UserMemory facilitates testing code that requires user memory.
///
/// Example:
///    let mem = UserMemory::create(size).unwrap();
///    mem.commit_and_map(size).unwrap();
///    let out_ptr = UserOutPtr::<u32>::new(mem.base() as *mut u32);
///    out_ptr.write(value).unwrap();
pub struct UserMemory {
    raw: NonNull<RawUserMemory>,
}

impl UserMemory {
    pub fn create(size: usize) -> Option<Self> {
        let raw = unsafe { unittest_user_memory_create(size) };
        NonNull::new(raw).map(|raw| Self { raw })
    }

    pub fn base(&self) -> usize {
        unsafe { unittest_user_memory_get_base(self.raw.as_ptr()) }
    }

    /// Ensures the mapping is committed and mapped such that usages will cause no faults.
    pub fn commit_and_map(&self, size: usize) -> Result<(), Status> {
        let status = unsafe { unittest_user_memory_commit_and_map(self.raw.as_ptr(), size) };
        Status::ok(status)
    }

    /// Write to the underlying VMO directly, bypassing the mapping.
    pub fn vmo_write(&self, data: &[u8], offset: u64) -> Result<(), Status> {
        let status = unsafe {
            unittest_user_memory_vmo_write(
                self.raw.as_ptr(),
                data.as_ptr() as *const c_void,
                offset,
                data.len() as u64,
            )
        };
        Status::ok(status)
    }

    /// Read from the underlying VMO directly, bypassing the mapping.
    pub fn vmo_read<'a>(
        &self,
        data: &'a mut [MaybeUninit<u8>],
        offset: u64,
    ) -> Result<&'a mut [u8], Status> {
        let status = unsafe {
            unittest_user_memory_vmo_read(
                self.raw.as_ptr(),
                data.as_mut_ptr() as *mut c_void,
                offset,
                data.len() as u64,
            )
        };
        Status::ok(status)?;
        // SAFETY: When `unittest_user_memory_vmo_read` returns ZX_OK, all `data.len()` bytes in
        // `data` have been initialized by the kernel.
        Ok(unsafe { data.assume_init_mut() })
    }
}

impl Drop for UserMemory {
    fn drop(&mut self) {
        unsafe { unittest_user_memory_destroy(self.raw.as_ptr()) };
    }
}
