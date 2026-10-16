// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Debug console (`k`) commands (`dd`, `dw`, `dh`, `db`, `mw`, `mh`, `mb`, `fw`, `fh`, `fb`, `mc`,
//! `mtest`, `crash`, `sleep`, `sleepm`, `build_instrumentation`, `pop`, `ub`).

mod debugcommands_ffi;
mod undefined_behavior;

use core::ffi::{CStr, c_int, c_void};
use core::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use core::sync::atomic::{AtomicUsize, Ordering};

use kprint::{kprint, kprintln};
use zx_status::Status;

use crate::console::{CMD_AVAIL_ALWAYS, CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::debugcommands::debugcommands_ffi::{
    arch_sync_cache_range, cpp_before_stack_overflow, cpp_crash_assert,
    cpp_crash_shadow_call_stack_stomp, cpp_crash_stack_guard, cpp_crash_stackstomp,
};
use crate::kernel::thread::{sleep_relative, spawn};
use crate::kernel::types::PAddr;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use crate::top::debug::rust_debug_crash_panic;
use crate::user_memory::UserMemory;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_USER,
    ARCH_MMU_FLAG_PERM_WRITE,
};
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm;
use crate::vm::vm::vaddr_to_paddr;

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements, and the target memory range must be valid
/// to read if mapped.
unsafe extern "C" fn cmd_display_mem(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    // save the last address and len so we can continue where we left off
    static LAST_ADDRESS: AtomicUsize = AtomicUsize::new(0);
    static LAST_LEN: AtomicUsize = AtomicUsize::new(0);
    let mut address = LAST_ADDRESS.load(Ordering::Relaxed);
    let mut len = LAST_LEN.load(Ordering::Relaxed);

    if argc < 3 && len == 0 {
        kprintln!("not enough arguments");
        kprintln!("{:s} [-l] [-b] [-p] [address] [length]", args[0].as_str());
        return -1;
    }

    let size: usize = if args[0].as_str() == "dd" {
        8
    } else if args[0].as_str() == "dw" {
        4
    } else if args[0].as_str() == "dh" {
        2
    } else {
        1
    };

    let mut little_endian = cfg!(target_endian = "little");
    let mut argindex = 1usize;
    let mut read_address = false;
    let mut phys_addr = false;
    while (argc as usize) > argindex {
        if args[argindex].as_str() == "-l" {
            little_endian = true;
        } else if args[argindex].as_str() == "-b" {
            little_endian = false;
        } else if args[argindex].as_str() == "-p" {
            phys_addr = true;
        } else if !read_address {
            address = args[argindex].arg_uint as usize;
            LAST_ADDRESS.store(address, Ordering::Relaxed);
            read_address = true;
        } else {
            len = args[argindex].arg_uint as usize;
            LAST_LEN.store(len, Ordering::Relaxed);
        }

        argindex += 1;
    }

    if (address & (size - 1)) != 0 {
        kprintln!("unaligned address, cannot display");
        return -1;
    }

    if phys_addr {
        // Translate to a physmap address if present.
        let vaddr = paddr_to_physmap(PAddr(address)).0;
        if vaddr == 0 {
            kprintln!("ERROR: failed to translate physical address");
            return -1;
        }

        kprintln!(
            "physical address {:#x} translated to {:p}",
            address,
            with_exposed_provenance::<c_void>(vaddr)
        );
        address = vaddr;
        LAST_ADDRESS.store(address, Ordering::Relaxed);
    }

    // preflight the start address to see if it's mapped
    if vaddr_to_paddr(with_exposed_provenance::<c_void>(address)).0 == 0 {
        kprintln!("ERROR: address 0x{:x} is unmapped", address);
        return -1;
    }

    let stop = address.wrapping_add(len);
    let mut count = 0usize;
    let swap = little_endian != cfg!(target_endian = "little");

    while address < stop {
        if count == 0 {
            kprint!("0x{:08x}: ", address);
        }
        match size {
            8 => {
                // SAFETY: `address` is 8-byte aligned and preflighted as mapped kernel memory.
                let raw =
                    unsafe { core::ptr::read_volatile(with_exposed_provenance::<u64>(address)) };
                let val = if swap { raw.swap_bytes() } else { raw };
                kprint!("{:016x} ", val);
            }
            4 => {
                // SAFETY: `address` is 4-byte aligned and preflighted as mapped kernel memory.
                let raw =
                    unsafe { core::ptr::read_volatile(with_exposed_provenance::<u32>(address)) };
                let val = if swap { raw.swap_bytes() } else { raw };
                kprint!("{:08x} ", val);
            }
            2 => {
                // SAFETY: `address` is 2-byte aligned and preflighted as mapped kernel memory.
                let raw =
                    unsafe { core::ptr::read_volatile(with_exposed_provenance::<u16>(address)) };
                let val = if swap { raw.swap_bytes() } else { raw };
                kprint!("{:04x} ", val);
            }
            1 => {
                // SAFETY: `address` is preflighted as mapped kernel memory.
                let val =
                    unsafe { core::ptr::read_volatile(with_exposed_provenance::<u8>(address)) };
                kprint!("{:02x} ", val);
            }
            _ => unreachable!(),
        }
        count += size;
        if count == 16 {
            kprintln!("");
            count = 0;
        }
        address = address.wrapping_add(size);
    }
    LAST_ADDRESS.store(address, Ordering::Relaxed);

    if count != 0 {
        kprintln!("");
    }

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements, and the target address must be valid for
/// writing.
unsafe extern "C" fn cmd_modify_mem(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    if argc < 3 {
        kprintln!("not enough arguments");
        kprintln!("{:s} <address> <val>", args[0].as_str());
        return -1;
    }

    let size: usize = if args[0].as_str() == "mw" {
        4
    } else if args[0].as_str() == "mh" {
        2
    } else {
        1
    };

    let address = args[1].arg_uint as usize;
    let val = args[2].arg_uint as usize;

    if (address & (size - 1)) != 0 {
        kprintln!("unaligned address, cannot modify");
        return -1;
    }

    match size {
        4 => {
            // SAFETY: `address` is 4-byte aligned and specified by the console operator.
            unsafe {
                core::ptr::write_volatile(with_exposed_provenance_mut::<u32>(address), val as u32);
            }
        }
        2 => {
            // SAFETY: `address` is 2-byte aligned and specified by the console operator.
            unsafe {
                core::ptr::write_volatile(with_exposed_provenance_mut::<u16>(address), val as u16);
            }
        }
        1 => {
            // SAFETY: `address` is specified by the console operator.
            unsafe {
                core::ptr::write_volatile(with_exposed_provenance_mut::<u8>(address), val as u8);
            }
        }
        _ => unreachable!(),
    }

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements, and the target memory range must be valid
/// for writing.
unsafe extern "C" fn cmd_fill_mem(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    if argc < 4 {
        kprintln!("not enough arguments");
        kprintln!("{:s} <address> <len> <val>", args[0].as_str());
        return -1;
    }

    let size: usize = if args[0].as_str() == "fw" {
        4
    } else if args[0].as_str() == "fh" {
        2
    } else {
        1
    };

    let mut address = args[1].arg_uint as usize;
    let len = args[2].arg_uint as usize;
    let stop = address.wrapping_add(len);
    let val = args[3].arg_uint as usize;

    if (address & (size - 1)) != 0 {
        kprintln!("unaligned address, cannot modify");
        return -1;
    }

    while address < stop {
        match size {
            4 => {
                // SAFETY: `address` is 4-byte aligned and specified by the console operator.
                unsafe {
                    core::ptr::write_volatile(
                        with_exposed_provenance_mut::<u32>(address),
                        val as u32,
                    );
                }
            }
            2 => {
                // SAFETY: `address` is 2-byte aligned and specified by the console operator.
                unsafe {
                    core::ptr::write_volatile(
                        with_exposed_provenance_mut::<u16>(address),
                        val as u16,
                    );
                }
            }
            1 => {
                // SAFETY: `address` is specified by the console operator.
                unsafe {
                    core::ptr::write_volatile(
                        with_exposed_provenance_mut::<u8>(address),
                        val as u8,
                    );
                }
            }
            _ => unreachable!(),
        }
        address = address.wrapping_add(size);
    }

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements, and the source and target memory ranges
/// must be valid and non-overlapping.
unsafe extern "C" fn cmd_copy_mem(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    if argc < 4 {
        kprintln!("not enough arguments");
        kprintln!("{:s} <source address> <target address> <len>", args[0].as_str());
        return -1;
    }

    let source = args[1].arg_uint as usize;
    let target = args[2].arg_uint as usize;
    let len = args[3].arg_uint as usize;

    // SAFETY: `source` and `target` are non-overlapping raw memory ranges of `len` bytes
    // specified by the console operator.
    unsafe {
        core::ptr::copy_nonoverlapping(
            with_exposed_provenance::<u8>(source),
            with_exposed_provenance_mut::<u8>(target),
            len,
        );
    }

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements, and the target memory range must be
/// 4-byte aligned and valid for reading and writing.
unsafe extern "C" fn cmd_memtest(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    if argc < 3 {
        kprintln!("not enough arguments");
        kprintln!("{:s} <base> <len>", args[0].as_str());
        return -1;
    }

    let ptr = with_exposed_provenance_mut::<u32>(args[1].arg_uint as usize);
    let len = args[2].arg_uint as usize;

    // write out
    kprint!("writing first pass...");
    for i in 0..(len / 4) {
        // SAFETY: `ptr.add(i)` is within the caller-specified `[base, base + len)` range.
        unsafe {
            core::ptr::write_volatile(ptr.add(i), i as u32);
        }
    }
    kprintln!("done");

    // verify
    kprint!("verifying...");
    for i in 0..(len / 4) {
        // SAFETY: `ptr.add(i)` is within the caller-specified `[base, base + len)` range.
        let val = unsafe { core::ptr::read_volatile(ptr.add(i)) };
        if (val as usize) != i {
            // SAFETY: `ptr.add(i)` is within the caller-specified `[base, base + len)` range.
            kprintln!("error at {:p}", unsafe { ptr.add(i) });
        }
    }
    kprintln!("done");

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements.
unsafe extern "C" fn cmd_sleep(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    let mut t = DurationMono::from_seconds(1); // default to 1 second

    if argc >= 2 {
        t = DurationMono::from_millis(args[1].arg_uint as i64);
        if args[0].as_str() == "sleep" {
            t = t * 1000;
        }
    }

    let _ = sleep_relative(t);

    0
}

extern "C" fn crash_thread(_arg: *mut c_void) -> i32 {
    // should crash
    // SAFETY: Intentionally dereferences an invalid pointer (`1`) to crash the kernel.
    unsafe {
        core::ptr::write_volatile(core::ptr::without_provenance_mut::<u8>(1), 1);
    }

    0
}

fn crash_deref(thread: bool) -> c_int {
    if thread {
        // SAFETY: `c"crasher"` is a valid NUL-terminated string, and `crash_thread` does not
        // dereference its argument pointer.
        let t = unsafe { spawn(c"crasher".as_ptr(), crash_thread, core::ptr::null_mut()) }
            .expect("Thread::Create failed");

        // SAFETY: `t` was just created and has not been joined yet.
        let _ = unsafe { t.join(InstantMono::INFINITE) };

        return 0;
    }

    crash_thread(core::ptr::null_mut());

    // if it didn't, panic the system
    panic!("crash");
}

/// Crash by intentionally recursing to itself until the kernel
/// call stack is exceeded.
#[allow(unconditional_recursion)]
fn recurse() -> c_int {
    // The compiler doesn't know what function pointer this is anymore after
    // black_box, but it hasn't changed.
    let self_fn: fn() -> c_int = core::hint::black_box(recurse);
    self_fn().wrapping_add(1)
}

fn before_stack_overflow(what: &CStr) {
    // SAFETY: `what.as_ptr()` is a valid NUL-terminated C string.
    unsafe {
        cpp_before_stack_overflow(what.as_ptr());
    }
}

fn crash_recurse() -> c_int {
    before_stack_overflow(c"Recursing");

    recurse();

    kprintln!("survived.");

    0
}

fn crash_stackstomp() -> c_int {
    // SAFETY: Intentionally overflows the kernel stack in test-only crash command.
    unsafe { cpp_crash_stackstomp() }
}

fn crash_shadow_call_stack_stomp() -> c_int {
    // SAFETY: Intentionally overflows the shadow call stack in test-only crash command.
    unsafe { cpp_crash_shadow_call_stack_stomp() }
}

fn has_user_code_execution_protection() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::arch_rs::feature::{X86_FEATURE_SMEP, x86_feature_has_smap, x86_feature_test};
        x86_feature_test(X86_FEATURE_SMEP) || x86_feature_has_smap()
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Privilege Execute Never (PXN) is available on all aarch64 machines.
        true
    }
    #[cfg(target_arch = "riscv64")]
    {
        // Supervisor User Memory access control is always available on RISC-V.
        true
    }
}

fn crash_user_execute() -> c_int {
    if !has_user_code_execution_protection() {
        kprintln!(
            "missing protection to avoid executing userspace code from a privileged context; will \
             not crash."
        );
        return -1;
    }
    const USER_MEMORY_SIZE: usize = page::SIZE;

    let Some(mem) = UserMemory::create(USER_MEMORY_SIZE) else {
        kprintln!("failed to allocate user memory; will not crash.");
        return -1;
    };

    // Define a little fragment of code that we can copy.
    let begin_func: *const u8;
    let func_size: usize;
    // SAFETY: Places a `ret` instruction in `.rodata` and reads its address and byte length, then
    // forms a slice over those `func_size` static bytes.
    let code_fragment = unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            ".pushsection .rodata.crash_user_exec, \"a?\", %progbits",
            ".balign 4",
            "2:",
            "ret",
            "3:",
            ".popsection",
            "adrp {ptr}, 2b",
            "add {ptr}, {ptr}, :lo12:2b",
            "mov {size}, #3b - 2b",
            ptr = out(reg) begin_func,
            size = out(reg) func_size,
            options(nomem, nostack),
        );
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!(
            ".pushsection .rodata.crash_user_exec, \"a?\", %progbits",
            ".balign 4",
            "2:",
            "ret",
            "3:",
            ".popsection",
            "lla {ptr}, 2b",
            "li {size}, 3b - 2b",
            ptr = out(reg) begin_func,
            size = out(reg) func_size,
            options(nomem, nostack),
        );
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            ".pushsection .rodata.crash_user_exec, \"a?\", @progbits",
            "2:",
            "ret",
            "3:",
            ".popsection",
            "lea 2b(%rip), {ptr}",
            "mov $(3b - 2b), {size}",
            ptr = out(reg) begin_func,
            size = out(reg) func_size,
            options(att_syntax, nomem, nostack),
        );
        core::slice::from_raw_parts(begin_func, func_size)
    };

    assert!(
        code_fragment.len() <= USER_MEMORY_SIZE,
        "function does not fit in allocated user memory"
    );

    if let Err(status) = mem.vmo_write(code_fragment, 0) {
        kprintln!("failed to copy payload ({}); will not crash.", status.into_raw());
        return -1;
    }

    if let Err(status) = mem.commit_and_map(0..USER_MEMORY_SIZE) {
        kprintln!("failed to commit memory ({}); will not crash.", status.into_raw());
        return -1;
    }

    // Set the memory as executable. We need to also make it read-only because
    // in arm64, writable user mappings imply Privileged Execute Never (PXN).
    if let Err(status) = mem.make_rx() {
        kprintln!("failed to make memory executable ({}); will not crash.", status.into_raw());
        return -1;
    }

    let mmu_flags = match mem.aspace().arch_aspace().query(mem.base()) {
        Ok((_, flags)) => flags,
        Err(status) => {
            kprintln!("failed to query mmu flags ({}); will not crash.", status.into_raw());
            return -1;
        }
    };

    if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0
        || (mmu_flags
            & (ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE))
            == 0
    {
        kprintln!("incorrect memory permissions; will not crash.");
        return -1;
    }

    let p = mem.user_in::<u8>().as_ptr();
    if p.is_null() {
        kprintln!("failed to get pointer; will not crash.");
        return -1;
    }

    // SAFETY: `mem.base()..mem.base() + USER_MEMORY_SIZE` is a valid mapped user page.
    unsafe {
        arch_sync_cache_range(mem.base(), USER_MEMORY_SIZE);
    }

    kprintln!("about to crash..");
    // SAFETY: `p` was checked to be non-null at line 583; intentionally jumps to a userspace
    // address from kernel context to trigger a SMEP/PXN/SUM execution fault.
    unsafe {
        let user_func = core::mem::transmute::<*const u8, unsafe extern "C" fn()>(p);
        user_func();
    }
    kprintln!("executed userspace code from a kernel context; did not crash.");
    0
}

fn crash_user_read() -> c_int {
    #[cfg(target_arch = "aarch64")]
    {
        // TODO(https://fxbug.dev/42137335): Once we support PAN enable this for arm64.
        kprintln!("ARM64 currently does not support PAN; will not crash.");
        -1
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        #[cfg(target_arch = "x86_64")]
        if !crate::arch_rs::feature::x86_feature_has_smap() {
            kprintln!("cpu does not support smap; will not crash.");
            return -1;
        }
        // RISCV implements the sstatus.sum bit, similar to x86 SMAP

        let Some(mem) = UserMemory::create(page::SIZE) else {
            kprintln!("failed to allocate user memory; will not crash.");
            return -1;
        };
        let p = mem.user_in::<u8>().as_ptr();
        if p.is_null() {
            kprintln!("failed to get pointer; will not crash.");
            return -1;
        }

        kprintln!("about to crash..");
        // SAFETY: Intentionally reads a userspace address directly from kernel context to trigger a
        // SMAP/PAN/SUM read fault.
        let b = unsafe { crate::debugcommands::debugcommands_ffi::cpp_read_byte(p) };

        kprintln!("read {:02x}; did not crash.", b);
        -1
    }
}

fn crash_pmm_use_after_free() -> c_int {
    // We want to corrupt one of the pages on the pmm's free list. To do so, we'll allocate a bunch
    // of pages, keep track of the address of the last page, then free them all. The free list is
    // LIFO so by allocating and freeing a bunch of pages we'll have a pointer "to the middle" and
    // our corrupted page will be less like to be immediately allocated.

    // Allocate.
    const NUM_PAGES: usize = 10000;
    pin_init::stack_pin_init!(let pages = VmPageDoublyLinkedList::new());
    if let Err(status) = pmm::alloc_pages(NUM_PAGES, 0, pages.as_mut()) {
        kprintln!("error: failed to allocate ({})", status.into_raw());
        return -1;
    }

    // Make note of address.
    let last_paddr = pages.back().unwrap().paddr();
    let va = paddr_to_physmap(last_paddr).0;

    // We're printing a little early because once we've returned the pages to the free list, we
    // want to avoid doing anything that might cause the target page to be allocated (by this
    // thread or some other thread).
    kprintln!("corrupting memory at address {:p}", with_exposed_provenance::<c_void>(va));

    // Free.
    // SAFETY: `pages` contains valid allocated PMM pages.
    unsafe {
        pmm::free_list(pages.as_mut());
    }

    // Corrupt!
    // SAFETY: Intentionally writes to a freed page in the physmap to test the PMM checker.
    unsafe {
        core::ptr::write_volatile(with_exposed_provenance_mut::<u8>(va), b'X');
    }

    kprintln!("crash_pmm_use_after_free done");
    -1
}

fn crash_assert() -> c_int {
    // SAFETY: Intentionally triggers a C++ `ASSERT_MSG` failure.
    unsafe { cpp_crash_assert() }
}

fn crash_stack_guard() -> c_int {
    // SAFETY: Intentionally overwrites the stack guard to trigger a stack-protector crash.
    unsafe { cpp_crash_stack_guard() }
}

fn crash_illegal_instruction() -> c_int {
    kprintln!("attempting to crash with an illegal instruction");
    // SAFETY: Intentionally executes an illegal instruction to crash the kernel.
    unsafe {
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("unimp");
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("ud2");
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("udf #0");
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64", target_arch = "aarch64")))]
    kprintln!("not implemented for this architecture");

    0
}

fn crash_break_instruction() -> c_int {
    kprintln!("attempting to crash with a break instruction");
    // SAFETY: Intentionally executes a break instruction to crash the kernel.
    unsafe {
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("ebreak");
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("int3");
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("brk #0");
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64", target_arch = "aarch64")))]
    kprintln!("not implemented for this architecture");

    0
}

fn crash_syscall_instruction() -> c_int {
    kprintln!("attempting to crash with a syscall instruction");
    #[cfg(target_arch = "riscv64")]
    {
        // Actually not a good idea, this will probably call into firmware with undefined args.
        kprintln!(
            "not implemented for RISC-V, ecall instruction would probably be trapped by firmware"
        );
    }
    // SAFETY: Intentionally executes a syscall/svc instruction in kernel mode to crash the kernel.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("syscall");
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("svc #0");
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64", target_arch = "aarch64")))]
    kprintln!("not implemented for this architecture");

    0
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements.
unsafe extern "C" fn cmd_crash(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let cmd_name = args[0].as_str();

    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!(
            "{:s} deref [thread]      : intentionally dereference an invalid pointer",
            cmd_name
        );
        kprintln!("{:s} stackstomp          : intentionally overrun the stack", cmd_name);
        kprintln!(
            "{:s} recurse             : intentionally overrun the stack by recursing",
            cmd_name
        );
        kprintln!("{:s} user_read           : intentionally read user memory", cmd_name);
        kprintln!("{:s} user_execute        : intentionally execute user memory", cmd_name);
        kprintln!("{:s} pmm_use_after_free  : intentionally corrupt the pmm free list", cmd_name);
        kprintln!("{:s} assert              : intentionally crash by failing an assert", cmd_name);
        kprintln!("{:s} rust_panic          : intentionally crash by panicking in Rust", cmd_name);
        kprintln!(
            "{:s} stack_guard         : attempt to crash by overwriting the stack guard",
            cmd_name
        );
        kprintln!(
            "{:s} illegal_instruction : attempt to crash by running an illegal instruction",
            cmd_name
        );
        kprintln!(
            "{:s} break_instruction   : attempt to crash by running a break instruction",
            cmd_name
        );
        kprintln!(
            "{:s} syscall_instruction : attempt to crash by running a syscall instruction",
            cmd_name
        );
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }
    match args[1].as_str() {
        "deref" => {
            if argc > 2 && args[2].as_str() == "thread" {
                crash_deref(true)
            } else {
                crash_deref(false)
            }
        }
        "stackstomp" => crash_stackstomp(),
        "shadow_call_stack_stomp" => crash_shadow_call_stack_stomp(),
        "recurse" => crash_recurse(),
        "user_read" => crash_user_read(),
        "user_execute" => crash_user_execute(),
        "pmm_use_after_free" => crash_pmm_use_after_free(),
        "assert" => crash_assert(),
        "rust_panic" => {
            rust_debug_crash_panic();
        }
        "stack_guard" => crash_stack_guard(),
        "illegal_instruction" => crash_illegal_instruction(),
        "break_instruction" => crash_break_instruction(),
        "syscall_instruction" => crash_syscall_instruction(),
        _ => {
            kprintln!("unknown command");
            usage()
        }
    }
}

/// # Safety
///
/// `_argv` must be a valid pointer to `_argc` elements if dereferenced.
unsafe extern "C" fn cmd_build_instrumentation(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    let print_feature = |what: &str| {
        kprintln!("build_instrumentation: {:s}", what);
    };

    if cfg!(sanitize = "address") {
        print_feature("address_sanitizer");
    }
    if cfg!(lk_debuglevel = "2") {
        print_feature("debug_assert");
    }
    if cfg!(with_lock_dep) {
        print_feature("lockdep");
    }
    if cfg!(sanitize = "safestack") {
        print_feature("safe_stack");
    }
    if cfg!(sanitize = "shadow-call-stack") {
        print_feature("shadow_call_stack");
    }
    if cfg!(ubsan) {
        print_feature("undefined_behavior_sanitizer");
    }

    // missing: sancov, profile

    // This print statement is load-bearing! It is scraped in a panic test.
    // LINT.IfChange
    kprintln!("build_instrumentation: done");
    // LINT.ThenChange(//src/tests/kernel_panic/kernel_panic_test.go)
    0
}

/// # Safety
///
/// `_argv` must be a valid pointer to `argc` elements if dereferenced.
unsafe extern "C" fn cmd_pop(argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc > 1 {
        kprintln!("aren't there already enough arguments?");
        return -1;
    }

    kprint!("drama!");
    0
}

static_command!(
    DD_CMD,
    c"dd".as_ptr(),
    c"display memory in dwords".as_ptr(),
    cmd_display_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    DW_CMD,
    c"dw".as_ptr(),
    c"display memory in words".as_ptr(),
    cmd_display_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    DH_CMD,
    c"dh".as_ptr(),
    c"display memory in halfwords".as_ptr(),
    cmd_display_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    DB_CMD,
    c"db".as_ptr(),
    c"display memory in bytes".as_ptr(),
    cmd_display_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    MW_CMD,
    c"mw".as_ptr(),
    c"modify word of memory".as_ptr(),
    cmd_modify_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    MH_CMD,
    c"mh".as_ptr(),
    c"modify halfword of memory".as_ptr(),
    cmd_modify_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    MB_CMD,
    c"mb".as_ptr(),
    c"modify byte of memory".as_ptr(),
    cmd_modify_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    FW_CMD,
    c"fw".as_ptr(),
    c"fill range of memory by word".as_ptr(),
    cmd_fill_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    FH_CMD,
    c"fh".as_ptr(),
    c"fill range of memory by halfword".as_ptr(),
    cmd_fill_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    FB_CMD,
    c"fb".as_ptr(),
    c"fill range of memory by byte".as_ptr(),
    cmd_fill_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    MC_CMD,
    c"mc".as_ptr(),
    c"copy a range of memory".as_ptr(),
    cmd_copy_mem,
    CMD_AVAIL_ALWAYS
);
static_command!(
    MTEST_CMD,
    c"mtest".as_ptr(),
    c"simple memory test".as_ptr(),
    cmd_memtest,
    CMD_AVAIL_NORMAL
);
static_command!(
    CRASH_CMD,
    c"crash".as_ptr(),
    c"intentionally crash".as_ptr(),
    cmd_crash,
    CMD_AVAIL_NORMAL
);
static_command!(
    SLEEP_CMD,
    c"sleep".as_ptr(),
    c"sleep number of seconds".as_ptr(),
    cmd_sleep,
    CMD_AVAIL_NORMAL
);
static_command!(
    SLEEPM_CMD,
    c"sleepm".as_ptr(),
    c"sleep number of milliseconds".as_ptr(),
    cmd_sleep,
    CMD_AVAIL_NORMAL
);
static_command!(
    BUILD_INSTRUMENTATION_CMD,
    c"build_instrumentation".as_ptr(),
    c"display a non-exhaustive list of build instrumentations used to build this kernel image"
        .as_ptr(),
    cmd_build_instrumentation,
    CMD_AVAIL_NORMAL
);
static_command!(POP_CMD, c"pop".as_ptr(), c"start drama".as_ptr(), cmd_pop, CMD_AVAIL_NORMAL);
