// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod sizes {
    pub const KB: usize = 1024;
    pub const MB: usize = 1024 * KB;
    pub const GB: usize = 1024 * MB;
}

/// Address space tests duplicated from aspace_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod aspace_rs {
    use super::sizes::GB;
    use crate::arch_rs::vm::is_user_accessible_range;
    use crate::arch_rs::{
        KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, USER_ASPACE_BASE, USER_ASPACE_SIZE,
        USER_RESTRICTED_ASPACE_SIZE,
    };
    use crate::kernel::deadline::Deadline;
    use crate::kernel::event::AutounsignalEvent;
    use crate::kernel::thread;
    use crate::kernel::types::VAddr;
    use crate::platform_rs::timer::InstantMono;
    use crate::user_copy::internal::validate_user_accessible_range;
    use crate::user_memory::UserMemory;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_USER, ArchMmuFlags, NonTerminalAction,
        TerminalAction,
    };
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm::vaddr_to_paddr;
    use crate::vm::vm_address_region::{self as vmar, MemoryPriority, VmAddressRegionOpChildren};
    use crate::vm::vm_aspace::{ShareOpt, Type, VmAspace, vmm_flag};
    use crate::vm::vm_object::{Resizability, SnapshotType, VmObject, VmObjectReadWriteOptions};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm::{pmm, vmm};
    use crate::vm_unittests::test_helper::{
        ARCH_RW_FLAGS, ARCH_RW_USER_FLAGS, alloc_user, fill_and_test, fill_and_test_user,
        make_committed_pager_vmo,
    };
    use core::ffi::c_void;
    use core::mem::{MaybeUninit, size_of, size_of_val};
    use core::ptr::from_ref;
    use core::sync::atomic::{AtomicBool, Ordering};
    use fbl::RefPtr;
    use kprint::kprintln;
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_err, assert_nonnull, assert_ok, assert_true, expect_eq, expect_err,
        expect_false, expect_ne, expect_ok, expect_true, subtest, unwrap_ok, unwrap_some,
    };
    use zr::ToMutPtr;
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
                b"test",
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

    /// Allocates a new address space and creates a few regions in it, then destroys it.
    #[test]
    fn multiple_regions_test() {
        let alloc_size: usize = 16 * 1024;

        let aspace =
            unwrap_some!(VmAspace::create(Type::User, b"test aspace"), "VmAspace::Create pointer");

        // SAFETY: Active aspace reference is not invalidated during test.
        let old_aspace = unsafe { thread::current_active_aspace() };
        // SAFETY: `aspace` remains valid while active.
        unsafe { vmm::set_active_aspace(Some(&aspace)) };

        // allocate region 0
        let ptr0 = unwrap_ok!(
            alloc_user(&aspace, b"test0", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr0, alloc_size));

        // allocate region 1
        let ptr1 = unwrap_ok!(
            alloc_user(&aspace, b"test1", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr1, alloc_size));

        // allocate region 2
        let ptr2 = unwrap_ok!(
            alloc_user(&aspace, b"test2", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr2, alloc_size));

        // SAFETY: It is sound to restore the previous address space.
        unsafe { vmm::set_active_aspace(old_aspace) };

        // free the address space all at once
        let err = aspace.destroy();
        expect_ok!(err, "VmAspace::Destroy");
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
                b"test",
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
                b"test",
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
        let aspace = VmAspace::create(Type::User, b"test aspace").expect("VmAspace::create failed");
        let err = aspace.destroy();
        expect_ok!(err, "VmAspace::Destroy");
    }

    /// Verifies that creating an address space with out-of-range bounds fails.
    #[test]
    fn vmaspace_create_invalid_ranges() {
        // These are defined in vm_aspace.cc.
        const GUEST_PHYSICAL_ASPACE_BASE: usize = 0;
        const GUEST_PHYSICAL_ASPACE_SIZE: usize = 1usize << crate::arch_rs::MMU_GUEST_SIZE_SHIFT;

        // Test when base < valid base.
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE - 1,
                4096,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                KERNEL_ASPACE_BASE - 1,
                4096,
                Type::Kernel,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE.wrapping_sub(1),
                4096,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );

        // Test when base + size exceeds valid range.
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE,
                USER_ASPACE_SIZE + 1,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                KERNEL_ASPACE_BASE,
                KERNEL_ASPACE_SIZE + 1,
                Type::Kernel,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE,
                GUEST_PHYSICAL_ASPACE_SIZE + 1,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
    }

    /// Allocates a vm address space object directly, maps something on it, and drops it.
    #[test]
    fn vmaspace_alloc_smoke_test() {
        // Allocates a vm address space object directly, maps something on it,
        // allows it to go out of scope.
        let mut aspace = VmAspace::create(Type::User, b"test aspace2");

        let _ptr = unwrap_ok!(
            alloc_user(aspace.as_ref().unwrap(), b"test", PAGE_SIZE_USIZE),
            "allocating region\n"
        );

        // destroy the aspace, which should drop all the internal refs to it
        let err = aspace.as_ref().unwrap().destroy();
        expect_ok!(err, "VmAspace::Destroy");

        // drop the ref held by this pointer
        drop(aspace.take());
    }

    /// Wrapper for harvesting access bits that informs the page queues.
    fn harvest_access_bits(
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) {
        let _scanner_disable = AutoVmScannerDisable::new();
        VmAspace::harvest_all_user_accessed_bits(non_terminal_action, terminal_action);
    }

    /// Consume the (scalar) value, ensuring that the operation to calculate the value can not be
    /// optimized out/ deemed as unused by the compiler. I.e. this function can be used as a wrapper
    /// to a calculation to ensure it will be in the binary.
    fn consume_value<T>(value: T) {
        // The compiler must materialize the value into a register, since it doesn't
        // know that the register's value isn't actually used.
        core::hint::black_box(value);
    }

    /// Touch mappings in an aspace and ensure we can correctly harvest the accessed bits.
    /// This test takes an optional tag that is placed in the top byte of the address when
    /// performing a user_copy.
    fn vmaspace_accessed_test(tag: u8) -> bool {
        let run = subtest!(|tag: u8| {
            let _scanner_disable = AutoVmScannerDisable::new();

            // Create some memory we can map touch to test accessed tracking on. Needs to be created
            // from user pager backed memory as harvesting is allowed to be limited to just that.
            let (vmo, [page]) = unwrap_ok!(make_committed_pager_vmo::<1>(
                /*trap_dirty=*/ false, /*resizable=*/ false,
            ));
            let mem = unwrap_some!(UserMemory::create_from_vmo(
                VmObjectPaged::into_vm_object(vmo),
                tag,
                0
            ));

            assert_ok!(mem.commit_and_map(0..PAGE_SIZE_USIZE));

            // Initial accessed state is undefined, so harvest it away.
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

            // Grab the current queue for the page and then rotate the page queues. This means any
            // future, correct, access harvesting should result in a new page queue.
            // SAFETY: `page` is valid and attached to a VM object.
            let mut current_queue =
                unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };
            pmm::page_queues().rotate_reclaim_queues();

            // Read from the mapping to (hopefully) set the accessed bit.
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            // Harvest it to move it in the page queue.
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };

            // Rotating and harvesting again should not make the queue change since we have not
            // accessed it.
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_eq!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });

            // Set the accessed bit again, and make sure it does now harvest.
            pmm::page_queues().rotate_reclaim_queues();
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });

            // Set the accessed bit and update age without harvesting.
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };

            // Now if we rotate and update again, we should re-age the page.
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
        });
        run(tag)
    }

    /// Touch mappings in an aspace and ensure accessed bits are correctly harvested.
    #[test]
    fn vmaspace_accessed_test_untagged() {
        expect_true!(vmaspace_accessed_test(0));
    }

    /// Reruns the accessed-bit test with tagged user pointers.
    #[test]
    fn vmaspace_accessed_test_tagged() {
        // Rerun the `vmaspace_accessed_test` tests with tags in the top byte of user pointers. This
        // tests that the subsequent accessed faults are handled successfully, even if the FAR
        // contains a tag.

        // TODO(ethanws): Make this entire test conditional on #[cfg!(target_arch = "aarch64")] when
        // lib/unittest gets this functionality.
        if cfg!(target_arch = "aarch64") {
            expect_true!(vmaspace_accessed_test(0xAB));
        } else {
            kprintln!("Skipping vmaspace_accessed_test_tagged; not aarch64.");
        }
    }

    /// Ensure user requested VMO read/write handles faults after access bits are harvested.
    #[test]
    fn vmaspace_usercopy_accessed_fault_test() {
        // Ensure that if a user requested VMO read/write operation would hit a page that has had
        // its accessed bits harvested that any resulting fault (on ARM) can be handled.
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create some memory we can map touch to test accessed tracking on. Needs to be created
        // from user pager backed memory as harvesting is allowed to be limited to just that.
        let (mapping_vmo, [_page]) = unwrap_ok!(make_committed_pager_vmo::<1>(
            /*trap_dirty=*/ false, /*resizable=*/ false
        ));
        let mem = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(mapping_vmo),
            0,
            0
        ));

        assert_ok!(mem.commit_and_map(0..PAGE_SIZE_USIZE));

        // Need a separate VMO to read/write from.
        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        // Touch the mapping to make sure it is committed and mapped.
        unwrap_ok!(mem.put::<u8>(42, 0));

        // Harvest any accessed bits.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

        // Read from the VMO into the mapping that has been harvested.
        let (res, read_actual) =
            vmo.read_user(mem.user_out::<u8>(), 0, size_of::<u8>(), VmObjectReadWriteOptions::NONE);
        assert_ok!(res);
        assert_eq!(read_actual, size_of::<u8>());
    }

    /// Test that page tables that do not get accessed can be successfully unmapped and freed.
    #[test]
    fn vmaspace_free_unaccessed_page_tables_test() {
        // Test that page tables that do not get accessed can be successfully unmapped and freed.

        // Disable for RISC-V for now, since the `ArchMmmu` code for this architecture currently
        // does not track accessed bits in intermediate page tables, and thus has no reasonable
        // way to honor `NonTerminalAction::FreeUnaccessed` on harvest calls.
        if cfg!(target_arch = "riscv64") {
            kprintln!("Skipping on RISC-V");
            return true;
        }

        let _scanner_disable = AutoVmScannerDisable::new();

        let num_pages: usize = 512 * 3;
        let middle_page: usize = num_pages / 2;
        let middle_offset: usize = middle_page * PAGE_SIZE_USIZE;
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            0,
            PAGE_SIZE * (num_pages as u64)
        ));

        // Construct an additional aspace to use for mappings and touching pages. This allows us to
        // control whether the aspace is considered active, which can effect reclamation and
        // scanning.
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));

        let _cleanup_aspace = zr::defer(|| {
            let _ = aspace.destroy();
        });

        let mem = unwrap_some!(
            UserMemory::create_in_aspace(VmObjectPaged::into_vm_object(vmo), &aspace, 0, 0),
            "UserMemory::create_in_aspace"
        );

        // Put the state we need to share in a struct so we can easily share it with the thread.
        struct State<'a> {
            mem: &'a UserMemory,
            touch_event: &'a AutounsignalEvent,
            complete_event: &'a AutounsignalEvent,
            running: AtomicBool,
            middle_offset: usize,
        }

        stack_pin_init!(let touch_event = AutounsignalEvent::init_unsignaled());
        stack_pin_init!(let complete_event = AutounsignalEvent::init_unsignaled());

        let state = State {
            mem: &mem,
            touch_event: &touch_event,
            complete_event: &complete_event,
            running: AtomicBool::new(true),
            middle_offset,
        };

        // Spin up a kernel thread in the aspace we made. This thread will just continuously wait on
        // an event, touching the mapping whenever it is signaled.
        extern "C" fn thread_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a live `State` valid for the thread duration.
            let state = unsafe { arg.cast::<State<'_>>().as_ref_unchecked() };

            while state.running.load(Ordering::SeqCst) {
                let _ = state.touch_event.wait(&Deadline::infinite());
                // Check running again so we do not try and touch mem if attempting to shutdown
                // suddenly.
                if state.running.load(Ordering::SeqCst) {
                    state
                        .mem
                        .put::<u8>(42, state.middle_offset)
                        .expect("failed to put byte to UserMemory");
                    // Signal the event back
                    state.complete_event.signal();
                }
            }
            0
        }

        let arg = from_ref(&state).cast_mut().cast::<c_void>();
        // SAFETY: `thread_body` and `arg` are safe to execute on a new thread.
        let thread =
            unwrap_ok!(unsafe { thread::create(c"test-thread".as_ptr(), thread_body, arg) });
        aspace.attach_to_thread(thread);
        // SAFETY: `thread` is valid and not yet joined.
        unsafe { thread.resume() };

        let _cleanup_thread = zr::defer(|| {
            state.running.store(false, Ordering::SeqCst);
            state.touch_event.signal();
            // SAFETY: `thread` is valid and joined once.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        });

        // Helper to synchronously wait for the thread to perform a touch.
        let touch = || {
            state.touch_event.signal();
            let _ = state.complete_event.wait(&Deadline::infinite());
        };

        expect_ok!(mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Touch the mapping to ensure its accessed.
        touch();

        // Attempting to map should fail, as it's already mapped.
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        touch();
        // Harvest the accessed information, this should not actually unmap it, even if we ask it
        // to.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        touch();
        // Harvest the accessed information, then attempt to do it again so that it gets unmapped.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_ok!(mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Touch the mapping to ensure its accessed.
        touch();

        // Harvest the page accessed information, but retain the non-terminals.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        // We can do this a few times.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        // Now if we attempt to free unaccessed the non-terminal should still be accessed and so
        // nothing should get unmapped.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        // If we are not requesting a free, then we should be able to harvest repeatedly.
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
    }

    /// Touch mappings in a unified aspace and ensure accessed bits are correctly harvested.
    #[test]
    fn vmaspace_unified_accessed_test() {
        // Touch mappings in both the shared and restricted region of a unified aspace and ensure we
        // can correctly harvest accessed bits.

        // Disable for RISC-V for now, since the `ArchMmmu` code for this architecture currently
        // does not track accessed bits in intermediate page tables, and thus has no reasonable
        // way to honor `NonTerminalAction::FreeUnaccessed` on harvest calls.
        if cfg!(target_arch = "riscv64") {
            kprintln!("Skipping on RISC-V");
            return true;
        }

        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a unified aspace.
        let private_aspace_base = USER_ASPACE_BASE;
        let private_aspace_size = USER_RESTRICTED_ASPACE_SIZE;
        let shared_aspace_base = private_aspace_base + private_aspace_size + PAGE_SIZE_USIZE;
        let shared_aspace_size = USER_ASPACE_BASE + USER_ASPACE_SIZE - shared_aspace_base;
        let restricted_aspace = unwrap_some!(VmAspace::create_with_opts(
            private_aspace_base,
            private_aspace_size,
            Type::User,
            b"test restricted aspace",
            ShareOpt::Restricted,
        ));
        let shared_aspace = unwrap_some!(VmAspace::create_with_opts(
            shared_aspace_base,
            shared_aspace_size,
            Type::User,
            b"test shared aspace",
            ShareOpt::Shared,
        ));
        // SAFETY: `shared_aspace` and `restricted_aspace` are valid and were created as shared and
        // restricted respectively.
        let unified_aspace = unwrap_some!(unsafe {
            VmAspace::create_unified(
                (*shared_aspace).to_mut_ptr(),
                (*restricted_aspace).to_mut_ptr(),
                b"test unified aspace",
            )
        });

        let _cleanup_aspace = zr::defer(|| {
            let _ = unified_aspace.destroy();
            let _ = restricted_aspace.destroy();
            let _ = shared_aspace.destroy();
        });

        // Create regions of user memory that we can touch in both the shared and
        // restricted regions.
        let size: u64 = 4 * PAGE_SIZE;
        let shared_vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, size));
        let restricted_vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, size));

        let shared_mem = unwrap_some!(UserMemory::create_in_aspace(
            VmObjectPaged::into_vm_object(shared_vmo),
            &shared_aspace,
            0,
            0,
        ));
        let restricted_mem = unwrap_some!(UserMemory::create_in_aspace(
            VmObjectPaged::into_vm_object(restricted_vmo),
            &restricted_aspace,
            0,
            0,
        ));

        // Commit and map these regions to avoid page faults when we call `put` later on. We
        // have to do this because the `put` function invokes a `copy_to_user` that may trigger a
        // page fault, which the fault handler will try to resolve using the thread's current
        // aspace. That aspace, in turn, will be the unified aspace, which cannot resolve faults.
        let middle_offset: usize = (size / 2) as usize;
        expect_ok!(shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
        expect_ok!(restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Switch to the unified aspace.
        // NOTE: This test takes care to not trigger any page faults or accessed faults from this
        // point on, because the unified aspace cannot resolve faults. For a user thread, we would
        // rely on the process dispatcher to look up if the faulting address lies in the shared
        // aspace or the restricted aspace, and use one of those to resolve the fault instead.
        // We cannot exercise that behavior from within a kernel unit test.
        // SAFETY: Active aspace reference is not invalidated during test.
        let old_aspace = unsafe { thread::current_active_aspace() };
        // SAFETY: `unified_aspace` remains valid while active.
        unsafe { vmm::set_active_aspace(Some(&unified_aspace)) };
        let _reset_old_aspace = zr::defer(|| {
            // SAFETY: `old_aspace` remains valid while active.
            unsafe { vmm::set_active_aspace(old_aspace) };
        });

        #[cfg(target_arch = "x86_64")]
        {
            // Touch the shared and restricted regions via the unified aspace. This will
            // guarantee that the accessed bits are set on x86, where the hardware sets the
            // accessed bits. On ARM, where we use software managed accessed bits, the
            // `commit_and_map` above will already have set them.
            unwrap_ok!(shared_mem.put::<u8>(42, middle_offset));
            unwrap_ok!(restricted_mem.put::<u8>(42, middle_offset));
        }

        // Harvest the accessed information. This should not actually unmap the pages.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        expect_err!(
            restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        #[cfg(target_arch = "x86_64")]
        {
            // Touch the memory again so that the accessed bits are guaranteed to be set.
            // We must do this because `commit_and_map` does not set the accessed flag on x86.
            //
            // We specifically want to avoid doing this on ARM because the harvest will have
            // cleared accessed bits, and this `put` now will trigger an accessed fault on the
            // unified aspace, which cannot resolve any faults. Moreover, the `commit_and_map`
            // above will have already set the non-terminal accessed bits on the walk down
            // to the page.
            unwrap_ok!(shared_mem.put::<u8>(43, middle_offset));
            unwrap_ok!(restricted_mem.put::<u8>(43, middle_offset));
        }

        // Harvest the accessed information, then attempt to do it again so that it gets
        // unmapped. The first `harvest_access_bits` call will clear the accessed bits, and
        // the second will unmap the memory.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_ok!(shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
        expect_ok!(restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
    }

    /// Tests sparse VM mappings with an empty backing VMO.
    #[test]
    fn vm_mapping_sparse_mapping_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a large memory mapping with an empty backing VMO. Although this is a large virtual
        // address range, our later attempts to map it should be efficient.
        let memory_size = 16 * GB;
        let memory = UserMemory::create(memory_size).unwrap();

        // Memory backing the user memory is currently empty, so attempting to map in it should
        // succeed, albeit with nothing populated.
        expect_ok!(memory.map_existing(0..memory_size));

        // Commit a page in the middle, then re-map the whole thing and ensure the mapping is there.
        let val = 42u64;
        expect_ok!(
            memory.vmo_write(&val.to_ne_bytes()[..size_of_val(&val)], (memory_size / 2) as u64,)
        );
        expect_ok!(memory.map_existing(0..memory_size));
        expect_eq!(val, unwrap_ok!(memory.get::<u64>(memory_size / 2 / size_of::<u64>())));

        // Do the same test, but this time with the pages at the start and end of the range.
        expect_ok!(memory.vmo_write(&val.to_ne_bytes()[..size_of_val(&val)], 0));
        expect_ok!(memory.vmo_write(
            &val.to_ne_bytes()[..size_of_val(&val)],
            (memory_size - PAGE_SIZE_USIZE) as u64,
        ));
        expect_ok!(memory.map_existing(0..memory_size));
        expect_eq!(val, unwrap_ok!(memory.get::<u64>(0)));
        expect_eq!(
            val,
            unwrap_ok!(memory.get::<u64>((memory_size - PAGE_SIZE_USIZE) / size_of::<u64>()))
        );
    }

    /// Tests memory priority propagation with pager-backed VMOs and clones.
    #[test]
    fn vmaspace_priority_pager_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let (vmo, _) = unwrap_ok!(make_committed_pager_vmo::<1>(false, false));

        // Create a clone of the VMO.
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");

        // Map in the clone.
        let _mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            PAGE_SIZE_USIZE,
            PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC_OVERWRITE,
            vmo_child.clone(),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));

        // Validate the root and clone received the priority.
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Create a second child of the root.
        let vmo_child2: Option<RefPtr<VmObject>> = None;
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp2 = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");

        // This child should not have any priority.
        expect_false!(childp2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Destroy it should leave the rest of the tree unchanged.
        drop(vmo_child2);
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Remove priority and validate.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::Default));

        expect_false!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority propagation for VMO references.
    #[test]
    fn vmaspace_priority_reference_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace").expect("VmAspace::create failed");

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            PAGE_SIZE_USIZE,
            PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC_OVERWRITE,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a reference of the VMO.
        let (vmo_reference, _first_child) =
            unwrap_ok!(vmo.create_child_reference(Resizability::NonResizable, 0, 0, true));
        let refp = VmObject::downcast_paged(vmo_reference.clone()).expect("is paged");

        // Reference should have same priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Remove the original mapping.
        let _ = mapping_result.mapping.destroy();
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Now map in the reference.
        let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            PAGE_SIZE_USIZE,
            PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC_OVERWRITE,
            vmo_reference,
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));
        let _ = mapping_result;

        // Reference and vmo should have same priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority propagation through hierarchies.
    #[test]
    fn vmaspace_priority_propagation_test() {
        // Test that memory priority gets propagated through hierarchies and into newly created
        // objects.
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        let _mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));

        // Set the priority in our vmar and validate it propagates to the VMO and the aspace.
        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a new VMAR and VMO and map them into the high priority vmar. Memory priority
        // should propagate.
        let sub_vmar = unwrap_ok!(vmar.create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 16,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test sub-vmar",
        ));

        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        let _mapping2_result = unwrap_ok!(sub_vmar.create_vm_mapping(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo2.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));
        expect_true!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Change the priority of the sub vmar. It should not effect the original vmar / vmo
        // priority.
        let status = sub_vmar.set_memory_priority(MemoryPriority::Default);
        expect_ok!(status);
        expect_false!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(vmar.destroy());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_ok!(aspace.destroy());
    }

    /// Test that overwriting a mapping maintains priority counts.
    #[test]
    fn vmaspace_priority_mapping_overwrite_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            0,
            PAGE_SIZE_USIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));
        let mapping = mapping_result.mapping;

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Overwrite the mapping with a new one from a new VMO.
        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        let _mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            mapping.base().0 - vmar.base().0,
            mapping.size(),
            0,
            vmar::flag::SPECIFIC_OVERWRITE,
            VmObjectPaged::into_vm_object(vmo2.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping2",
        ));

        // Original VMO should have lost its priority, and the VMO for our new mapping should have
        // gained.
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Test that unmapping parts of a mapping preserves priority.
    #[test]
    fn vmaspace_priority_unmap_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 8));

        let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            0,
            PAGE_SIZE_USIZE * 8,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));

        // Set the priority in our vmar and validate it propagates to the VMO and the aspace.
        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        let base = mapping_result.base;

        // Unmap one page from either end of the mapping, ensuring memory priority did not change.
        expect_ok!(unsafe {
            vmar.unmap(VAddr(base), PAGE_SIZE_USIZE, VmAddressRegionOpChildren::No)
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 7),
                PAGE_SIZE_USIZE,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Unmap a page from the middle. This will split this into two mappings.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 4),
                PAGE_SIZE_USIZE,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());
        // Now completely unmap one portion. This will destroy one of the mappings, but the VMO
        // should still have priority from the other mapping that was previously split.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE),
                PAGE_SIZE_USIZE * 3,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Unmapping the rest of the other portion should finally cause the priority to be removed.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 5),
                PAGE_SIZE_USIZE * 2,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority propagation to a child VMO slice.
    #[test]
    fn vmaspace_priority_slice_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        let _mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            PAGE_SIZE_USIZE,
            PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC_OVERWRITE,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
            b"test-mapping",
        ));

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a slice of the VMO.
        let vmo_slice = unwrap_ok!(vmo.create_child_slice(0, PAGE_SIZE, true));
        let slicep = vmo_slice.as_paged().unwrap();

        // Slice inherits priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(slicep.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Change priority of the VMAR should remove from the VMO.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::Default));
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(aspace.is_high_memory_priority());

        // Re-enable priority and verify.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::High));
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(slicep.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Destroy slice and unmap.
        drop(vmo_slice);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
    }

    /// Attempt force a high priority region to be writeable.
    #[test]
    fn vm_mapping_force_writeable_high_priority() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        struct CleanupSubVmar<'a>(&'a vmar::VmAddressRegion);
        impl Drop for CleanupSubVmar<'_> {
            fn drop(&mut self) {
                let _ = self.0.destroy();
            }
        }
        let _cleanup_sub_vmar = CleanupSubVmar(&vmar);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        // Create a read-only user mapping. Since there is no ARCH_MMU_FLAG_PERM_WRITE,
        // force_writable won't trivially succeed.
        let arch_read_user_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_USER;
        let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo),
            0,
            arch_read_user_flags,
            b"test-mapping",
        ));

        let status = vmar.set_memory_priority(vmar::MemoryPriority::High);
        expect_ok!(status);

        let force_result = mapping_result.mapping.force_writable();
        expect_ok!(force_result.map(|_| ()));
    }

    /// Check if a range of addresses is accessible to the user. If `spectre_validation` is true, this is
    /// done by checking if `validate_user_accessible_range` returns `{0,0}`. Otherwise, check using
    /// `is_user_accessible_range`.
    fn check_user_accessible_range(
        mut vaddr: usize,
        mut len: usize,
        spectre_validation: bool,
    ) -> bool {
        if spectre_validation {
            // If the address and length were not modified, then the pair is valid.
            let old_vaddr = vaddr;
            let old_len = len;
            validate_user_accessible_range(&mut vaddr, &mut len);
            return vaddr == old_vaddr && len == old_len;
        }

        is_user_accessible_range(vaddr, len)
    }

    fn check_user_accessible_range_test(spectre_validation: bool) -> bool {
        let run = subtest!(|spectre_validation: bool| {
            use crate::arch_rs::USER_ASPACE_BASE;

            let mut va: usize;
            let mut len: usize;

            // Test address of zero.
            va = 0;
            len = PAGE_SIZE_USIZE;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test address and length of zero (both are valid).
            va = 0;
            len = 0;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test very end of address space and zero length (this is invalid since the start has bit 55 set
            // despite zero length).
            va = usize::MAX;
            len = 0;
            expect_false!(check_user_accessible_range(va, len, spectre_validation));

            // Test a regular user address.
            va = USER_ASPACE_BASE;
            len = PAGE_SIZE_USIZE;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test zero-length on a regular user address.
            va = USER_ASPACE_BASE;
            len = 0;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test overflow past 64 bits.
            va = USER_ASPACE_BASE;
            len = (usize::MAX - va).wrapping_add(1);
            expect_false!(check_user_accessible_range(va, len, spectre_validation));

            #[cfg(target_arch = "aarch64")]
            {
                use crate::arch_rs::vm::is_user_accessible;

                // On aarch64, an address is accessible to the user if bit 55 is zero.

                // Test starting on a bad user address.
                let bad_addr_mask = 1usize << 55;
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = PAGE_SIZE_USIZE;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test zero-length on a bad user address.
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = 0;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test 2^55 is in the range of `[va, va+len)`, ending on a bad user address.
                va = USER_ASPACE_BASE;
                len = bad_addr_mask;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test this returns false if any address within the range of `[va, va+len)`
                // contains a value where bit 55 is set. This also implies there are many
                // gaps in ranges above 2^56.
                //
                // Here both the start and end values are valid, but this range contains an
                // address that is invalid.
                va = 0;
                len = 0x017f_ffff_ffff_ffff; // Bits 0-56 (except 55) are set.
                assert_true!(is_user_accessible(va));
                assert_true!(is_user_accessible(va + len));
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test the range of the largest value less than 2^55 and the smallest value
                // greater than 2^55 where bit 55 == 0.
                va = (1usize << 55) - 1;
                len = 0x0080_0000_0000_0001; // End = `va` + `len` = 2^56.
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Be careful not to just check that 2^55 is in the range. We really want to
                // check whenever bit 55 is flipped in the range.
                va = 0x017f_ffff_ffff_ffff; // Start above 2^56. Bit 55 is not set.
                // End = `va` + `len` = `0x200_0000_0000_0000`. This is above 2^56 and bit 55 also is not set.
                len = 0x0080_0000_0000_0001;
                assert_true!(is_user_accessible(va));
                assert_true!(is_user_accessible(va + len));
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                va = USER_ASPACE_BASE;
                len = (1usize << 57) + 1;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test a range above 2^56 where bit 55 is never set.
                va = 0x0170_0000_0000_0000;
                len = 0x000f_ffff_ffff_ffff;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));

                // Test a range right below 2^55 where bit 55 is never set.
                va = 0x0070_0000_0000_0000;
                len = 0x000f_ffff_ffff_ffff;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));

                // Test the last valid user space address with a tag of 0.
                va = usize::MAX;
                va &= !(0xffusize << 56); // Set tag to zero.
                va &= !bad_addr_mask; // Ensure valid user address.
                len = 0;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));
            }

            #[cfg(target_arch = "x86_64")]
            {
                // On x86_64, an address is accessible to the user if bits 48-63 are zero.

                // Test a bad user address.
                let bad_addr_mask = 1usize << 48;
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = PAGE_SIZE_USIZE;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test zero-length on a bad user address.
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = 0;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test ending on a bad user address.
                va = USER_ASPACE_BASE;
                len = bad_addr_mask;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));
            }
        });
        run(spectre_validation)
    }

    /// Tests `is_user_accessible_range`.
    #[test]
    fn arch_is_user_accessible_range() {
        expect_true!(check_user_accessible_range_test(false));
    }

    /// Tests `validate_user_accessible_range`.
    #[test]
    fn validate_user_address_range() {
        expect_true!(check_user_accessible_range_test(true));
    }

    /// Doesn't do anything, just prints all aspaces.
    #[test]
    fn dump_all_aspaces() {
        // Doesn't do anything, just prints all aspaces.
        // Should be run after all other tests so that people can manually comb
        // through the output for leaked test aspaces.

        // Set to true for debugging.
        if false {
            kprintln!("verify there are no test aspaces left around");
            VmAspace::dump_all_aspaces(/*verbose*/ true);
        }
    }
}
