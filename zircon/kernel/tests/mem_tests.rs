// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Interactive memory pattern and moving-inversion test console command.

use core::ffi::{c_int, c_void};
use kprint::kprintln;
use zx_status::Status;

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED,
};
use crate::vm::vm::vaddr_to_paddr;
use crate::vm::vm_aspace::{VmAspace, vmm_flag};

/// # Safety
///
/// The caller must ensure `base` is 4-byte aligned and valid for reads of `len_words` `u32` words
/// for the duration of the call, and `word_index < len_words`.
unsafe fn mem_test_fail(
    base: *const u32,
    len_words: usize,
    word_index: usize,
    should: u32,
    is: u32,
) {
    // SAFETY: Caller guarantees `word_index < len_words` and `base` is valid for `len_words` words.
    let fail_ptr = unsafe { base.add(word_index) };
    kprintln!("ERROR at {:p}: should be {:#x}, is {:#x}", fail_ptr, should, is);

    let base_addr = base.addr();
    let end_addr = base_addr.saturating_add(len_words.saturating_mul(core::mem::size_of::<u32>()));
    let dump_start = (fail_ptr.addr() & !(64 - 1)).max(base_addr);
    let dump_end = dump_start.saturating_add(128).min(end_addr);
    let dump_len = dump_end.saturating_sub(dump_start);

    // SAFETY: `dump_start` and `dump_end` are clamped to `[base_addr, end_addr]`, so
    // `dump_ptr` is in-bounds of `base` and valid for reads of `dump_len` bytes.
    let dump_ptr = unsafe { base.cast::<u8>().add(dump_start - base_addr) };
    let mut writer = debug::dprintf::KernelConsoleWriter;
    // SAFETY: `dump_ptr` is valid for reads of `dump_len` bytes.
    let _ =
        unsafe { pretty::hexdump_very_ex_raw(&mut writer, dump_ptr, dump_len, dump_start as u64) };
}

/// # Safety
///
/// The caller must ensure `ptr` is 4-byte aligned and valid for reads and writes of `len` bytes
/// for the duration of the call.
unsafe fn do_pattern_test(ptr: *mut c_void, len: usize, pat: u32) -> Result<(), Status> {
    let vbuf32 = ptr.cast::<u32>();
    let len_words = len / 4;

    kprintln!("\tpattern {:#010x}", pat);
    for i in 0..len_words {
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        unsafe { vbuf32.wrapping_add(i).write_volatile(pat) };
    }

    for i in 0..len_words {
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        let val = unsafe { vbuf32.wrapping_add(i).read_volatile() };
        if val != pat {
            // SAFETY: `vbuf32` is valid for `len_words` words and `i < len_words`.
            unsafe { mem_test_fail(vbuf32, len_words, i, pat, val) };
            return Err(Status::INTERNAL);
        }
    }

    Ok(())
}

/// # Safety
///
/// The caller must ensure `ptr` is 4-byte aligned and valid for reads and writes of `len` bytes
/// for the duration of the call.
unsafe fn do_moving_inversion_test(ptr: *mut c_void, len: usize, pat: u32) -> Result<(), Status> {
    let vbuf32 = ptr.cast::<u32>();
    let len_words = len / 4;

    kprintln!("\tpattern {:#010x}", pat);

    // fill memory
    for i in 0..len_words {
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        unsafe { vbuf32.wrapping_add(i).write_volatile(pat) };
    }

    // from the bottom, walk through each cell, inverting the value
    // printf("\t\tbottom up invert\n");
    for i in 0..len_words {
        let cell = vbuf32.wrapping_add(i);
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        let val = unsafe { cell.read_volatile() };
        if val != pat {
            // SAFETY: `vbuf32` is valid for `len_words` words and `i < len_words`.
            unsafe { mem_test_fail(vbuf32, len_words, i, pat, val) };
            return Err(Status::INTERNAL);
        }

        // SAFETY: `cell` is 4-byte aligned and in-bounds of `vbuf32`.
        unsafe { cell.write_volatile(!pat) };
    }

    // repeat, walking from top down
    // printf("\t\ttop down invert\n");
    for i in (0..len_words).rev() {
        let cell = vbuf32.wrapping_add(i);
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        let val = unsafe { cell.read_volatile() };
        if val != !pat {
            // SAFETY: `vbuf32` is valid for `len_words` words and `i < len_words`.
            unsafe { mem_test_fail(vbuf32, len_words, i, !pat, val) };
            return Err(Status::INTERNAL);
        }

        // SAFETY: `cell` is 4-byte aligned and in-bounds of `vbuf32`.
        unsafe { cell.write_volatile(pat) };
    }

    // verify that we have the original pattern
    // printf("\t\tfinal test\n");
    for i in 0..len_words {
        // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words` words,
        // and `i < len_words`.
        let val = unsafe { vbuf32.wrapping_add(i).read_volatile() };
        if val != pat {
            // SAFETY: `vbuf32` is valid for `len_words` words and `i < len_words`.
            unsafe { mem_test_fail(vbuf32, len_words, i, pat, val) };
            return Err(Status::INTERNAL);
        }
    }

    Ok(())
}

/// # Safety
///
/// The caller must ensure `ptr` is 4-byte aligned and valid for reads and writes of `len` bytes
/// for the duration of the call.
unsafe fn do_mem_tests(ptr: *mut c_void, len: usize) {
    let vbuf32 = ptr.cast::<u32>();
    let len_words = len / 4;
    'out: {
        // test 1: simple write address to memory, read back
        kprintln!("test 1: simple address write, read back");
        for i in 0..len_words {
            // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words`
            // words, and `i < len_words`.
            unsafe { vbuf32.wrapping_add(i).write_volatile(i as u32) };
        }

        for i in 0..len_words {
            // SAFETY: Caller guarantees `vbuf32` is 4-byte aligned and valid for `len_words`
            // words, and `i < len_words`.
            let val = unsafe { vbuf32.wrapping_add(i).read_volatile() };
            if val != i as u32 {
                // SAFETY: `vbuf32` is valid for `len_words` words and `i < len_words`.
                unsafe { mem_test_fail(vbuf32, len_words, i, i as u32, val) };
                break 'out;
            }
        }

        // test 2: write various patterns, read back
        kprintln!("test 2: write patterns, read back");

        const PATTERNS: [u32; 4] = [0x0, 0xffffffff, 0xaaaaaaaa, 0x55555555];

        for pattern in PATTERNS {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_pattern_test(ptr, len, pattern) }.is_err() {
                break 'out;
            }
        }
        // shift bits through 32bit word
        let mut p = 1u32;
        while p != 0 {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_pattern_test(ptr, len, p) }.is_err() {
                break 'out;
            }
            p <<= 1;
        }
        // shift bits through 16bit word, invert top of 32bit
        let mut p = 1u16;
        while p != 0 {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_pattern_test(ptr, len, (u32::from(!p) << 16) | u32::from(p)) }.is_err() {
                break 'out;
            }
            p <<= 1;
        }

        // test 3: moving inversion, patterns
        kprintln!("test 3: moving inversions with patterns");
        for pattern in PATTERNS {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_moving_inversion_test(ptr, len, pattern) }.is_err() {
                break 'out;
            }
        }
        // shift bits through 32bit word
        let mut p = 1u32;
        while p != 0 {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_moving_inversion_test(ptr, len, p) }.is_err() {
                break 'out;
            }
            p <<= 1;
        }
        // shift bits through 16bit word, invert top of 32bit
        let mut p = 1u16;
        while p != 0 {
            // SAFETY: Caller guarantees `ptr` is 4-byte aligned and valid for `len` bytes.
            if unsafe { do_moving_inversion_test(ptr, len, (u32::from(!p) << 16) | u32::from(p)) }
                .is_err()
            {
                break 'out;
            }
            p <<= 1;
        }
    }

    kprintln!("done with tests");
}

/// # Safety
///
/// The caller must ensure `argv` is valid for reads of `argc` initialized `CmdArgs` elements for
/// the duration of the call, and if `argc == 3`, `argv[1].arg_ptr` must be valid for reads and
/// writes of `argv[2].arg_uint` bytes for the duration of the call.
unsafe extern "C" fn mem_test(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 1 || argv.is_null() {
        return -1;
    }
    // SAFETY: `argc >= 1` and `argv` is non-null; the console dispatcher guarantees `argv` points
    // to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage: {:s} <length>", args[0].as_str());
        kprintln!("usage: {:s} <base> <length>", args[0].as_str());
        -1
    };

    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }

    if argc == 2 {
        let mut ptr: *mut c_void = core::ptr::null_mut();
        let len = args[1].arg_uint as usize;

        // rounding up len to the next page
        let len = len.checked_next_multiple_of(page::SIZE).unwrap_or(0);
        if len == 0 {
            kprintln!("invalid length");
            return -1;
        }

        // allocate a region to test in
        // SAFETY: Allocating an uncached contiguous kernel test region of `len` bytes.
        let status = unsafe {
            VmAspace::kernel_aspace().alloc_contiguous(
                b"memtest",
                len,
                &mut ptr,
                0,
                vmm_flag::COMMIT,
                ARCH_MMU_FLAG_UNCACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            )
        };
        if let Err(status) = status {
            kprintln!("error {} allocating test region", status.into_raw());
            return -1;
        }

        let pa = vaddr_to_paddr(ptr);
        kprintln!("physical address {:#x}", pa.0);

        kprintln!("got buffer at {:p} of length {:#x}", ptr, len);

        // run the tests
        // SAFETY: `ptr` was allocated and committed for `len` bytes above.
        unsafe { do_mem_tests(ptr, len) };

        // free the test memory
        // SAFETY: `ptr` is the base address of the region allocated above.
        let _ = unsafe { VmAspace::kernel_aspace().free_region(ptr.addr()) };
    } else if argc == 3 {
        let ptr = args[1].arg_ptr;
        let len = args[2].arg_uint as usize;
        if ptr.is_null() || !ptr.cast::<u32>().is_aligned() {
            kprintln!("invalid base address");
            return -1;
        }

        // run the tests
        // SAFETY: Console operator supplied `ptr` and `len` for memory testing.
        unsafe { do_mem_tests(ptr, len) };
    } else {
        return usage();
    }

    0
}

static_command!(
    MEM_TEST_CMD,
    c"mem_test".as_ptr(),
    c"test memory".as_ptr(),
    mem_test,
    CMD_AVAIL_NORMAL
);
