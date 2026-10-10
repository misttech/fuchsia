// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016, Google, Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::hw_rng_get_entropy;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use core::ffi::c_int;
use kprint::kprint;
use zerocopy::IntoBytes;
use zx_status::Status;

/// # Safety
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match
/// the console command signature.
unsafe extern "C" fn cmd_rng32(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    let mut val: u32 = 0;
    let fetched = hw_rng_get_entropy(val.as_mut_bytes());

    if fetched == 0 {
        kprint!("hw rng failed. Support may not exist on this platform\n");
        return Status::NOT_SUPPORTED.into_raw();
    }

    debug_assert!(fetched == size_of_val(&val));

    kprint!("Random val = {:u} (0x{:08x})\n", val, val);

    zx_status::sys::ZX_OK
}

/// # Safety
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_rng(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console framework passes `argc` valid arguments in `argv`.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    if !(2..=3).contains(&argc) {
        kprint!(
            concat!(
                "Invalid argument count\n\n",
                "Usage : {:cs} <N>\n",
                "N     : Number of bytes to generate.\n"
            ),
            args[0].arg_str
        );
        return Status::INVALID_ARGS.into_raw();
    }

    kprint!("Generating {:u} random bytes\n", args[1].arg_uint);

    let mut offset: u64 = 0;
    while offset < args[1].arg_uint {
        let mut bytes = [0u8; 16];

        let todo = core::cmp::min(bytes.len() as u64, args[1].arg_uint - offset) as usize;
        let done = hw_rng_get_entropy(&mut bytes[..todo]);
        debug_assert!(done <= todo);

        let _ = pretty::hexdump8_very_ex_rs(
            &mut debug::dprintf::KernelConsoleWriter,
            &bytes[..done],
            offset,
        );
        offset += done as u64;

        if done < todo {
            kprint!(
                "Entropy exhausted after {:u} byte{:s}\n",
                offset,
                if offset == 1 { "" } else { "s" }
            );
            break;
        }
    }

    zx_status::sys::ZX_OK
}

static_command!(
    CMD_RNG32,
    c"rng32".as_ptr(),
    c"Generate and print a random 32 bit unsigned integer using the HW RNG".as_ptr(),
    cmd_rng32,
    CMD_AVAIL_NORMAL
);

static_command!(
    CMD_RNG,
    c"rng".as_ptr(),
    c"Generate and print N random bytes using the HW RNG".as_ptr(),
    cmd_rng,
    CMD_AVAIL_NORMAL
);
