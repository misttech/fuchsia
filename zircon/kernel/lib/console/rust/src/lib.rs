// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::ffi::{c_char, c_int, c_void};

pub const CMD_AVAIL_NORMAL: u8 = 1 << 0;
pub const CMD_AVAIL_PANIC: u8 = 1 << 1;
pub const CMD_AVAIL_ALWAYS: u8 = CMD_AVAIL_NORMAL | CMD_AVAIL_PANIC;

// Command is happening at crash time.
pub const CMD_FLAG_PANIC: u32 = 1 << 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CmdArgs {
    pub str: *const c_char,
    pub u: core::ffi::c_ulong,
    pub p: *mut c_void,
    pub i: core::ffi::c_long,
    pub b: bool,
}

pub type CmdCallback = unsafe extern "C" fn(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int;

#[repr(C)]
pub struct Cmd {
    pub cmd_str: *const c_char,
    pub help_str: *const c_char,
    pub cmd_callback: CmdCallback,
    pub availability_mask: u8,
}

// Safety: Cmd structures placed in the commands section are read-only
// after boot and safe to share between threads.
unsafe impl Sync for Cmd {}

// Verify size and alignment compatibility with C++ structs.
zr::static_assert!(core::mem::size_of::<CmdArgs>() == 40);
zr::static_assert!(core::mem::align_of::<CmdArgs>() == 8);
zr::static_assert!(core::mem::size_of::<Cmd>() == 32);
zr::static_assert!(core::mem::align_of::<Cmd>() == 8);

#[cfg(console_enabled)]
#[macro_export]
macro_rules! commands_section {
    () => {
        ".data.rel.ro.commands"
    };
}

#[macro_export]
macro_rules! static_command {
    ($var_name:ident, $cmd:expr, $help:expr, $func:expr, $mask:expr) => {
        #[cfg(console_enabled)]
        #[used]
        #[unsafe(link_section = $crate::commands_section!())]
        pub static $var_name: $crate::Cmd = $crate::Cmd {
            cmd_str: $cmd,
            help_str: $help,
            cmd_callback: $func,
            availability_mask: $mask,
        };
    };
}
