// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::vm_aspace::VmAspace;
use vmm_bindings as bindings;
use zr::ToMutPtr;

/// set the current user aspace as active on the current thread.
/// [`None`] is a valid argument, which unmaps the current user address space
///
/// # Safety
///
/// `aspace` remains valid while active and the previous active aspace is restored before `aspace`
/// is destroyed.
pub unsafe fn set_active_aspace(aspace: Option<&VmAspace>) {
    let aspace_ptr: *mut vm_aspace_bindings::VmAspace =
        aspace.map_or(core::ptr::null_mut(), |a| a.to_mut_ptr().cast());
    // SAFETY: Caller attests to the preconditions.
    unsafe { bindings::cpp_vmm_set_active_aspace(aspace_ptr) };
}

/// Kernel console `vmm` command implementation.
#[cfg(console_enabled)]
mod console {
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::thread;
    use crate::kernel::types::PAddr;
    use crate::platform_rs::timer::InstantMono;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
    };
    use crate::vm::vm_aspace::{Type, VmAspace};
    use core::ffi::{c_int, c_void};
    use fbl::RefPtr;
    use kprint::kprintln;
    use ksync::KCell;
    use zx_status::Status;

    ksync::declare_singleton_mutex!(TestAspaceLock);

    /// Synchronized static storage for the `vmm` console command's active test address space.
    struct TestAspaceStatic(KCell<Option<RefPtr<VmAspace>>, TestAspaceLock>);

    /// Active test address space manipulated by `k vmm` subcommands.
    static TEST_ASPACE: TestAspaceStatic = TestAspaceStatic(KCell::new(None));

    impl TestAspaceStatic {
        /// Returns a cloned reference to the current test address space, initializing it to the
        /// kernel address space if not yet set.
        fn get_or_init(&self) -> RefPtr<VmAspace> {
            ksync::lock!(let mut guard = TestAspaceLock::get().lock());
            // SAFETY: `guard.token_mut()` belongs to `TestAspaceLock`.
            let slot = unsafe { self.0.get_mut(guard.token_mut()) };
            slot.get_or_insert_with(|| RefPtr::from_ref(VmAspace::kernel_aspace())).clone()
        }

        /// Replaces the stored test address space with `val` and returns the previous value so it
        /// can be dropped outside `TestAspaceLock`.
        fn replace(&self, val: Option<RefPtr<VmAspace>>) -> Option<RefPtr<VmAspace>> {
            ksync::lock!(let mut guard = TestAspaceLock::get().lock());
            // SAFETY: `guard.token_mut()` belongs to `TestAspaceLock`.
            core::mem::replace(unsafe { self.0.get_mut(guard.token_mut()) }, val)
        }

        /// Clears and returns the stored test address space if it points to the same `VmAspace` as
        /// `target`.
        fn clear_if_eq(&self, target: &RefPtr<VmAspace>) -> Option<RefPtr<VmAspace>> {
            ksync::lock!(let mut guard = TestAspaceLock::get().lock());
            // SAFETY: `guard.token_mut()` belongs to `TestAspaceLock`.
            let slot = unsafe { self.0.get_mut(guard.token_mut()) };
            if slot
                .as_ref()
                .is_some_and(|t| core::ptr::eq(RefPtr::as_ptr(t), RefPtr::as_ptr(target)))
            {
                slot.take()
            } else {
                None
            }
        }
    }

    /// Constructs a `RefPtr<VmAspace>` from a raw address supplied on the console.
    ///
    /// Returns `None` if `addr` is `0` or misaligned.
    ///
    /// # Safety
    ///
    /// If `addr` is non-zero and aligned to `VmAspace`, it must be the address of a live
    /// `VmAspace` that remains valid for the duration of the call.
    unsafe fn aspace_from_addr(addr: u64) -> Option<RefPtr<VmAspace>> {
        let raw_ptr = core::ptr::with_exposed_provenance::<VmAspace>(addr as usize);
        if raw_ptr.is_null() || !raw_ptr.cast::<vm_aspace_bindings::VmAspace>().is_aligned() {
            None
        } else {
            // SAFETY: Caller guarantees `raw_ptr` points to a valid `VmAspace`.
            Some(unsafe { RefPtr::from_ref(&*raw_ptr) })
        }
    }

    /// Console command handler for `k vmm`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - For subcommands that take raw addresses (`alloc_physical`, `free_region`, `free_aspace`,
    ///   `set_test_aspace`), the caller-supplied address arguments must refer to valid physical
    ///   ranges, region addresses, or live `VmAspace` instances respectively.
    unsafe extern "C" fn cmd_vmm(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} aspaces", cmd_name);
            kprintln!("{:s} kaspace", cmd_name);
            kprintln!("{:s} alloc <size> <align_pow2>", cmd_name);
            kprintln!("{:s} alloc_physical <paddr> <size> <align_pow2>", cmd_name);
            kprintln!("{:s} alloc_contig <size> <align_pow2>", cmd_name);
            kprintln!("{:s} free_region <address>", cmd_name);
            kprintln!("{:s} create_aspace", cmd_name);
            kprintln!("{:s} create_test_aspace", cmd_name);
            kprintln!("{:s} free_aspace <address>", cmd_name);
            kprintln!("{:s} set_test_aspace <address>", cmd_name);
            Status::INTERNAL.into_raw()
        };
        let not_enough_args = || -> c_int {
            kprintln!("not enough arguments");
            usage()
        };

        if args.len() < 2 {
            return not_enough_args();
        }

        let test_aspace = TEST_ASPACE.get_or_init();
        let subcmd = args[1].as_str();

        if subcmd == "aspaces" {
            VmAspace::dump_all_aspaces(true);
        } else if subcmd == "kaspace" {
            VmAspace::kernel_aspace().dump(true);
        } else if subcmd == "alloc_physical" {
            if args.len() < 4 {
                return not_enough_args();
            }

            let mut ptr = core::ptr::without_provenance_mut::<c_void>(0x99);
            let align = if args.len() >= 5 { args[4].arg_uint as u8 } else { 0u8 };
            // SAFETY: Console debugging command mapping user-supplied physical address range into
            // the test address space.
            let err = Status::result_into_raw(unsafe {
                test_aspace.alloc_physical(
                    b"physical test",
                    args[3].arg_uint as usize,
                    &mut ptr,
                    align,
                    PAddr(args[2].arg_uint as usize),
                    0,
                    ARCH_MMU_FLAG_UNCACHED_DEVICE
                        | ARCH_MMU_FLAG_PERM_READ
                        | ARCH_MMU_FLAG_PERM_WRITE,
                )
            });
            kprintln!("VmAspace::AllocPhysical returns {}, ptr {:p}", err, ptr);
        } else if subcmd == "alloc_contig" {
            if args.len() < 3 {
                return not_enough_args();
            }

            let mut ptr = core::ptr::without_provenance_mut::<c_void>(0x99);
            let align = if args.len() >= 4 { args[3].arg_uint as u8 } else { 0u8 };
            // SAFETY: Console debugging command allocating contiguous memory in the test address
            // space.
            let err = Status::result_into_raw(unsafe {
                test_aspace.alloc_contiguous(
                    b"contig test",
                    args[2].arg_uint as usize,
                    &mut ptr,
                    align,
                    0,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                )
            });
            kprintln!("VmAspace::AllocContiguous returns {}, ptr {:p}", err, ptr);
        } else if subcmd == "free_region" {
            if args.len() < 3 {
                return not_enough_args();
            }

            // SAFETY: Console debugging command freeing a user-supplied region address in the test
            // address space.
            let err = Status::result_into_raw(unsafe {
                test_aspace.free_region(args[2].arg_uint as usize)
            });
            kprintln!("VmAspace::FreeRegion returns {}", err);
        } else if subcmd == "create_aspace" {
            let aspace = VmAspace::create(Type::User, b"test");
            kprintln!(
                "VmAspace::Create aspace {:p}",
                aspace.as_ref().map_or(core::ptr::null(), RefPtr::as_ptr)
            );
        } else if subcmd == "create_test_aspace" {
            let aspace = VmAspace::create(Type::User, b"test");
            kprintln!(
                "VmAspace::Create aspace {:p}",
                aspace.as_ref().map_or(core::ptr::null(), RefPtr::as_ptr)
            );
            let Some(aspace) = aspace else {
                return Status::NO_MEMORY.into_raw();
            };

            let old_aspace = TEST_ASPACE.replace(Some(aspace.clone()));
            // SAFETY: `aspace` is kept alive in `TEST_ASPACE` (and the local `aspace` binding)
            // while attached to the current thread, and `old_aspace` is kept alive until after the
            // address space switch and reschedule sleep complete.
            unsafe { thread::current_switch_aspace(&aspace) };
            // XXX hack to force it to reschedule and thus load the aspace
            let _ = thread::sleep(InstantMono(1));
            drop(old_aspace);
        } else if subcmd == "free_aspace" {
            if args.len() < 3 {
                return not_enough_args();
            }

            // SAFETY: Console debugging command operating on a user-supplied `VmAspace` address.
            let Some(aspace) = (unsafe { aspace_from_addr(args[2].arg_uint) }) else {
                kprintln!("invalid address");
                return usage();
            };
            let old_test_aspace = TEST_ASPACE.clear_if_eq(&aspace);
            drop(old_test_aspace);

            // SAFETY: The current thread's active aspace remains valid while attached to the
            // current thread during this pointer comparison.
            let is_active = unsafe { thread::current_active_aspace() }
                .is_some_and(|a| core::ptr::eq(a, RefPtr::as_ptr(&aspace)));
            if is_active {
                // SAFETY: Detaching the active user address space before destroying it.
                unsafe { thread::current_clear_aspace() };
                let _ = thread::sleep(InstantMono(1)); // hack
            }

            let err = Status::result_into_raw(aspace.destroy());
            kprintln!("VmAspace::Destroy() returns {}", err);
        } else if subcmd == "set_test_aspace" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if args[2].arg_uint == 0 {
                let old_aspace = TEST_ASPACE.replace(None);
                // SAFETY: Detaching the test user address space from this console kernel thread;
                // `old_aspace` is kept alive until after the reschedule sleep completes.
                unsafe { thread::current_clear_aspace() };
                // XXX hack to force it to reschedule and thus unload the aspace
                let _ = thread::sleep(InstantMono(1));
                drop(old_aspace);
            } else {
                // SAFETY: Console debugging command operating on a user-supplied `VmAspace`
                // address, kept alive in `TEST_ASPACE` while attached to the current thread.
                let Some(aspace) = (unsafe { aspace_from_addr(args[2].arg_uint) }) else {
                    kprintln!("invalid address");
                    return usage();
                };
                let old_aspace = TEST_ASPACE.replace(Some(aspace.clone()));
                // SAFETY: `aspace` is kept alive in `TEST_ASPACE` (and the local `aspace` binding)
                // while attached to the current thread, and `old_aspace` is kept alive until after
                // the address space switch and reschedule sleep complete.
                unsafe { thread::current_switch_aspace(&aspace) };
                // XXX hack to force it to reschedule and thus load the aspace
                let _ = thread::sleep(InstantMono(1));
                drop(old_aspace);
            }
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }

    static_command!(
        VMM,
        c"vmm".as_ptr(),
        c"virtual memory manager".as_ptr(),
        cmd_vmm,
        CMD_AVAIL_NORMAL
    );
}
