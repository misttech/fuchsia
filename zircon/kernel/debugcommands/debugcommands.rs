// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Raw memory debug console (`k`) commands (`dd`, `dw`, `dh`, `db`, `mw`, `mh`, `mb`, `fw`, `fh`,
//! `fb`, `mc`, `mtest`).

use core::ffi::{c_int, c_void};
use core::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use core::sync::atomic::{AtomicUsize, Ordering};

use kprint::{kprint, kprintln};

use crate::console::{CMD_AVAIL_ALWAYS, CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::types::PAddr;
use crate::vm::physmap::paddr_to_physmap;
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
