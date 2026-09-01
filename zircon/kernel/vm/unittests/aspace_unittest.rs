// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Address space tests duplicated from aspace_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod aspace_rs {
    use crate::vm::vm::vaddr_to_paddr;
    use crate::vm::vm_aspace::{Type, VmAspace, vmm_flag};
    use crate::vm_unittests::test_helper::{ARCH_RW_FLAGS, fill_and_test};
    use core::mem::MaybeUninit;
    use kprint::kprintln;
    use page::SIZE as PAGE_SIZE_USIZE;
    use unittest::{assert_err, assert_nonnull, assert_ok, expect_eq, expect_ok, expect_true};
    use zx_status::Status;

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    /// Allocates a contiguous region in kernel space, reads/writes it, then destroys it.
    #[test]
    fn vmm_alloc_contiguous_smoke_test() {
        let alloc_size = 256 * 1024;

        // allocate a region of memory
        let mut ptr = core::ptr::null_mut();
        let kaspace = VmAspace::kernel_aspace();
        let err = unsafe {
            kaspace.alloc_contiguous(
                c"test",
                alloc_size,
                &mut ptr,
                0,
                vmm_flag::COMMIT,
                ARCH_RW_FLAGS,
            )
        };
        assert_ok!(err, "VmAspace::AllocContiguous region of memory");
        assert_nonnull!(ptr, "VmAspace::AllocContiguous region of memory");

        // fill with known pattern and test
        let slice: *mut MaybeUninit<u8> = ptr.cast();
        // SAFETY: `ptr` points to `alloc_size` bytes of valid memory allocated in `kaspace`.
        let slice = unsafe { core::slice::from_raw_parts_mut(slice, alloc_size) };
        let (_buf, result) = fill_and_test(slice);
        expect_true!(result);

        // test that it is indeed contiguous
        kprintln!("testing that region is contiguous");
        let mut last_pa = 0u64;
        for i in 0..(alloc_size / PAGE_SIZE_USIZE) {
            // SAFETY: `ptr` points to a contiguous allocation of at least `alloc_size` bytes.
            let va = unsafe { ptr.add(i * PAGE_SIZE_USIZE) };
            let pa = u64::from(vaddr_to_paddr(va));
            if last_pa != 0 {
                expect_eq!(pa, last_pa + PAGE_SIZE, "region is contiguous");
            }
            last_pa = pa;
        }

        // free the region
        // SAFETY: `ptr as usize` is the base virtual address of the allocation in `kaspace`.
        let err = unsafe { kaspace.free_region(ptr as usize) };
        expect_ok!(err, "VmAspace::FreeRegion region of memory");
    }

    /// Checks that AllocContiguous fails when missing VMM_FLAG_COMMIT.
    #[test]
    fn vmm_alloc_contiguous_missing_flag_commit_fails() {
        // should have VmAspace::VMM_FLAG_COMMIT
        let zero_vmm_flags = 0u32;
        let mut ptr = core::ptr::null_mut();
        // SAFETY: `ptr` points to a valid pointer storage for `alloc_contiguous`.
        let err = unsafe {
            VmAspace::kernel_aspace().alloc_contiguous(
                c"test",
                PAGE_SIZE_USIZE,
                &mut ptr,
                0,
                zero_vmm_flags,
                ARCH_RW_FLAGS,
            )
        };
        assert_err!(err, Status::INVALID_ARGS);
    }

    /// Checks that AllocContiguous fails with zero size.
    #[test]
    fn vmm_alloc_contiguous_zero_size_fails() {
        let zero_size = 0usize;
        let mut ptr = core::ptr::null_mut();
        // SAFETY: `ptr` points to a valid pointer storage for `alloc_contiguous`.
        let err = unsafe {
            VmAspace::kernel_aspace().alloc_contiguous(
                c"test",
                zero_size,
                &mut ptr,
                0,
                vmm_flag::COMMIT,
                ARCH_RW_FLAGS,
            )
        };
        assert_err!(err, Status::INVALID_ARGS);
    }

    /// Allocates a vm address space object directly, allows it to go out of scope.
    #[test]
    fn vmaspace_create_smoke_test() {
        let aspace = VmAspace::create(Type::User, c"test aspace").expect("VmAspace::create failed");
        let err = aspace.destroy();
        expect_ok!(err, "VmAspace::Destroy");
    }
}
