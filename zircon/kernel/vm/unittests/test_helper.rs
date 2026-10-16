// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::user_copy::UserInOutPtr;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_USER, ARCH_MMU_FLAG_PERM_WRITE, ArchMmuFlags,
};
use crate::vm::attribution::{AttributionCounts, FractionalBytes};
use crate::vm::page::VmPagePtr;
use crate::vm::pmm;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::num::NonZeroI64;
use core::ptr;
use fbl::RefPtr;
use kprint::kprintln;
use rand::TryRng;
use rand::rand_core::utils;
use test_helper_bindings as bindings;
use zx_status::Status;

pub const ARCH_RW_FLAGS: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
pub const ARCH_RW_USER_FLAGS: ArchMmuFlags = ARCH_RW_FLAGS | ARCH_MMU_FLAG_PERM_USER;

/// Creates a committed pager-backed VMO with `N` pages, returning `(vmo, initialized_pages_array)`.
pub fn make_committed_pager_vmo<const N: usize>(
    trap_dirty: bool,
    resizable: bool,
) -> Result<(RefPtr<VmObjectPaged>, [VmPagePtr; N]), Status> {
    let mut raw_vmo = core::ptr::null_mut();
    let mut page_ptrs = [core::ptr::null_mut(); N];

    // SAFETY: page_ptrs.as_mut_ptr() is valid for writing N pointers, and raw_vmo is a
    // valid out-pointer.
    let status = unsafe {
        bindings::cpp_make_committed_pager_vmo(
            page_ptrs.len(),
            trap_dirty,
            resizable,
            page_ptrs.as_mut_ptr(),
            &mut raw_vmo,
        )
    };
    Status::ok(status)?;

    // SAFETY: When cpp_make_committed_pager_vmo returns ZX_OK, raw_vmo is a valid exported
    // VmObjectPaged pointer.
    let vmo = unsafe { VmObjectPaged::from_raw(raw_vmo) };
    // Based on cpp_make_committed_pager_vmo returning ZX_OK, raw_vmo is guaranteed to be non-null
    // and valid.
    let vmo = vmo.unwrap();

    let pages = page_ptrs.map(|ptr| {
        // SAFETY: When cpp_make_committed_pager_vmo returns ZX_OK, ptr is a valid pointer to a
        //kernel page.
        let ptr = unsafe { VmPagePtr::from_ffi(ptr) };
        // Based on cpp_make_committed_pager_vmo returning ZX_OK, all page pointers are guaranteed
        // to be non-null and available.
        ptr.unwrap()
    });

    Ok((vmo, pages))
}

/// Same as make_committed_pager_vmo but does not commit any pages in the VMO.
pub fn make_uncommitted_pager_vmo(
    num_pages: usize,
    trap_dirty: bool,
    resizable: bool,
) -> Result<RefPtr<VmObjectPaged>, Status> {
    let (vmo, []) = make_partially_committed_pager_vmo(num_pages, trap_dirty, resizable, false)?;
    Ok(vmo)
}

/// Creates a partially committed pager-backed VMO with `num_pages` virtual
/// pages and `C` committed pages.
pub fn make_partially_committed_pager_vmo<const C: usize>(
    num_pages: usize,
    trap_dirty: bool,
    resizable: bool,
    ignore_requests: bool,
) -> Result<(RefPtr<VmObjectPaged>, [VmPagePtr; C]), Status> {
    assert!(C <= num_pages, "committed_pages ({C}) cannot exceed num_pages ({num_pages})");
    let mut raw_vmo = core::ptr::null_mut();
    let mut page_ptrs = [core::ptr::null_mut(); C];

    // SAFETY: The page pointer array and output VMO reference are valid for
    // their respective writes.
    let status = unsafe {
        bindings::cpp_make_partially_committed_pager_vmo(
            num_pages,
            C,
            trap_dirty,
            resizable,
            ignore_requests,
            page_ptrs.as_mut_ptr(),
            &mut raw_vmo,
        )
    };
    Status::ok(status)?;

    // SAFETY: When cpp_make_partially_committed_pager_vmo returns ZX_OK, raw_vmo is a valid
    // exported VmObjectPaged pointer.
    let vmo = unsafe { VmObjectPaged::from_raw(raw_vmo) }.expect("vmo pointer is non-null");
    let pages = page_ptrs.map(|ptr| {
        // SAFETY: When cpp_make_partially_committed_pager_vmo returns ZX_OK, ptr is a valid
        // pointer to a page.
        unsafe { VmPagePtr::from_ffi(ptr) }.expect("page pointer is non-null")
    });

    Ok((vmo, pages))
}

/// Re-supplies `num_pages` to `vmo` starting at `page_offset`, returning the updated pages array.
pub fn supply_pager_vmo_pages<const N: usize>(
    vmo: &VmObjectPaged,
    page_offset: u64,
    num_pages: u64,
) -> Result<[VmPagePtr; N], Status> {
    let mut pages = [core::ptr::null_mut(); N];
    // SAFETY: `vmo.as_raw()` is a valid pointer to a live `VmObjectPaged`. `pages.as_mut_ptr()`
    // points to a stack array of length `N`.
    let status = unsafe {
        bindings::cpp_supply_pager_vmo_pages(
            vmo.as_raw(),
            page_offset,
            num_pages,
            pages.as_mut_ptr(),
        )
    };
    Status::ok(status)?;
    let pages = pages.map(|page| {
        // SAFETY: When `cpp_supply_pager_vmo_pages` returns ZX_OK, `page` is guaranteed
        // to be a valid pointer to a page.
        unsafe { VmPagePtr::from_ffi(page) }.expect("page pointer is non-null")
    });
    Ok(pages)
}

/// Verifies that `vmo` has `expected_bytes` tracked by continuous attribution.
pub fn verify_continuous_attribution_bytes(vmo: &VmObject, expected_bytes: u64) -> bool {
    // SAFETY: `vmo.as_raw()` points to a live `VmObjectPaged`.
    unsafe { bindings::cpp_verify_continuous_attribution_bytes(vmo.as_raw(), expected_bytes) }
}

/// Helper function that produces a filled out AttributionCounts for testing simple VMOs that just
/// have private and no shared content.
pub fn make_private_attribution_counts(uncompressed: u64, compressed: u64) -> AttributionCounts {
    AttributionCounts {
        uncompressed_bytes: uncompressed as usize,
        compressed_bytes: compressed as usize,
        private_uncompressed_bytes: uncompressed as usize,
        private_compressed_bytes: compressed as usize,
        scaled_uncompressed_bytes: FractionalBytes::from_whole(uncompressed),
        scaled_compressed_bytes: FractionalBytes::from_whole(compressed),
    }
}

/// Changes `vmo`'s high priority count by `delta`, taking care of the prepare and locking steps
/// required by the C++ `PriorityChanger`.
///
/// The count may never go negative, so callers must only subtract what they have already added,
/// and must remove any additions before `vmo` is destroyed. Violating either trips a kernel
/// `DEBUG_ASSERT`.
pub fn change_vmo_high_priority_count(vmo: &VmObjectPaged, delta: NonZeroI64) {
    // SAFETY: `vmo.as_raw()` points to a live `VmObjectPaged`.
    unsafe { bindings::cpp_change_vmo_high_priority_count(vmo.as_raw(), delta.get()) }
}

/// fill a region of memory with a pattern based on the address of the region
pub fn fill_region(seed: usize, buf: &mut [MaybeUninit<u8>]) -> &mut [u8] {
    let ptr: *mut MaybeUninit<u8> = buf.as_mut_ptr();
    let ptr: *mut c_void = ptr.cast();

    // SAFETY: `ptr` points to `buf.len()` bytes of valid memory allocated for writing.
    unsafe { bindings::cpp_fill_region(seed, ptr, buf.len()) };
    // SAFETY: `cpp_fill_region` initializes all `buf.len()` bytes of `buf`.
    unsafe { buf.assume_init_mut() }
}

/// test a region of memory against a known pattern
pub fn test_region(seed: usize, buf: &[u8]) -> bool {
    let ptr: *const u8 = buf.as_ptr();
    let ptr: *mut u8 = ptr.cast_mut();
    let ptr: *mut c_void = ptr.cast();

    // SAFETY: `ptr` points to `buf.len()` bytes of valid memory.
    unsafe { bindings::cpp_test_region(seed, ptr, buf.len()) }
}

pub fn fill_and_test(buf: &mut [MaybeUninit<u8>]) -> (&mut [u8], bool) {
    let seed = buf.as_ptr().addr();
    let buf = fill_region(seed, buf);
    let result = test_region(seed, buf);
    (buf, result)
}

pub fn test_rand(seed: u32) -> u32 {
    // SAFETY: `cpp_test_rand` has no preconditions.
    unsafe { bindings::cpp_test_rand(seed) }
}

/// just like [`fill_region`], but for user memory
pub fn fill_region_user(seed: usize, ptr: UserInOutPtr<c_void>, len: usize) {
    let ptr = ptr.reinterpret::<u32>();
    assert!(ptr.as_ptr().addr().is_multiple_of(4));
    let mut val = seed as u32;
    val ^= (seed >> 32) as u32;
    for i in 0..(len / 4) {
        let status = ptr.element_offset(i).copy_to_user(&val);
        assert!(status.is_ok());
        val = test_rand(val);
    }
}

/// just like [`test_region`], but for user memory
pub fn test_region_user(seed: usize, ptr: UserInOutPtr<c_void>, len: usize) -> bool {
    let ptr = ptr.reinterpret::<u32>();
    assert!(ptr.as_ptr().addr().is_multiple_of(4));
    let mut val = seed as u32;
    val ^= (seed >> 32) as u32;
    for i in 0..(len / 4) {
        let p = ptr.element_offset(i);
        let status = p.read();
        assert!(status.is_ok());
        let actual = status.unwrap();
        if actual != val {
            kprintln!(
                "value at {:p} ({}) is incorrect: {:#x} vs {:#x}",
                p.as_ptr(),
                i,
                actual,
                val
            );
            return false;
        }
        val = test_rand(val);
    }
    true
}

/// just like [`fill_and_test`], but for user memory
pub fn fill_and_test_user(ptr: UserInOutPtr<c_void>, len: usize) -> bool {
    let seed = ptr.as_ptr().addr();

    // fill it with a pattern
    fill_region_user(seed, ptr, len);

    // test that the pattern is read back properly
    test_region_user(seed, ptr, len)
}

/// Helper function to allocate memory in a user address space.
pub fn alloc_user(
    aspace: &VmAspace,
    name: &[u8],
    size: usize,
) -> Result<UserInOutPtr<c_void>, Status> {
    assert!(aspace.is_user());

    let size = page::round_up(size);
    if size == 0 {
        return Err(Status::INVALID_ARGS);
    }

    let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, size as u64)?;
    let _ = vmo.set_name(name);
    let mapping = aspace.root_vmar().unwrap().create_vm_mapping(
        0,
        size,
        0,
        0,
        VmObjectPaged::into_vm_object(vmo),
        0,
        ARCH_RW_USER_FLAGS,
        name,
    )?;

    Ok(UserInOutPtr::new(ptr::with_exposed_provenance_mut(mapping.base)))
}

#[derive(Debug)]
pub struct TestRand {
    state: u32,
}

impl TestRand {
    pub const fn new(init: u32) -> Self {
        Self { state: init }
    }
}

impl TryRng for TestRand {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        self.state = test_rand(self.state);
        Ok(self.state)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        utils::next_u64_via_u32(self)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Infallible> {
        utils::fill_bytes_via_next_word(dest, || self.try_next_u64())
    }
}
