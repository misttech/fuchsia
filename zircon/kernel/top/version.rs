// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013 Google, Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![cfg(console_enabled)]

use core::ffi::c_int;
use kprint::kprint;

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::top::version_ffi::cpp_boot_constants_kernel_version_ident;

/// # Safety
///
/// `_argv` must be a valid pointer to `_argc` elements if dereferenced.
unsafe extern "C" fn cmd_version(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    let mut size = 0;
    // SAFETY: `cpp_boot_constants_kernel_version_ident` returns a pointer and length to the
    // immutable `kBootConstants.kernel_version_ident` buffer in `.rodata` populated by physboot.
    let ptr = unsafe { cpp_boot_constants_kernel_version_ident(&mut size) };
    let ident = if size == 0 {
        &[]
    } else {
        // SAFETY: `ptr` is non-null and valid for `size` bytes for the lifetime of the kernel.
        unsafe { core::slice::from_raw_parts(ptr, size) }
    };
    kprint!("{:s}", ident);
    0
}

static_command!(
    VERSION_CMD,
    c"version".as_ptr(),
    c"print version".as_ptr(),
    cmd_version,
    CMD_AVAIL_NORMAL
);
