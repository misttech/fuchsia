// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Boot options console commands (`setopt`, `showopt`).

use boot_options::BootOptions;
use core::ffi::c_int;
use core::ptr;
use kprint::kprintln;

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::vm_aspace::VmAspace;

// Note that using this can introduce data races on the member variables.
/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where `argc >= 1`
/// and each element's `arg_str` is a valid null-terminated C string.
unsafe extern "C" fn set(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        return -1;
    }
    // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
    // elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    if argc < 2 {
        kprintln!("Usage: {:s} <key>[=<value>]...", argv[0].as_str());
        return -1;
    }

    // BootOptions::get() returns a shared reference and can actually be a
    // read-only page mapping.  A mutable pointer is needed to set options, so
    // find the physical page and use it through the physmap.  Changing the
    // BootOptions object is inherently dangerous and racy, and should only be
    // done in a development context.
    let readonly_boot_options: *const BootOptions = BootOptions::as_ptr();
    let boot_options_vaddr = readonly_boot_options.addr();
    let res = VmAspace::kernel_aspace().arch_aspace().query(boot_options_vaddr);
    assert!(
        res.is_ok(),
        "Cannot find BootOptions {:p} in aspace!  {}",
        readonly_boot_options,
        res.unwrap_err().into_raw()
    );
    let (paddr, _) = res.unwrap();
    let boot_options = ptr::with_exposed_provenance_mut::<BootOptions>(paddr_to_physmap(paddr).0);

    for arg in &argv[1..] {
        // SAFETY: `boot_options` points to the writable physmap alias of `BootOptions`. As noted
        // above, mutating `BootOptions` via the console `setopt` command is racy and for
        // development only.
        unsafe { BootOptions::set_many(boot_options, arg.as_str()) };
    }

    0
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where `argc >= 1`
/// and each element's `arg_str` is a valid null-terminated C string.
unsafe extern "C" fn show(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        return -1;
    }
    // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
    // elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    if argc > 1 {
        let mut result: c_int = 0;
        for arg in &argv[1..] {
            result |= BootOptions::get().show_key(arg.as_str());
        }
        return result;
    }

    BootOptions::get().show();
    0
}

static_command!(
    CMD_SETOPT,
    c"setopt".as_ptr(),
    c"Set boot options (as from kernel cmdline)".as_ptr(),
    set,
    CMD_AVAIL_NORMAL
);
static_command!(
    CMD_SHOWOPT,
    c"showopt".as_ptr(),
    c"Show boot options (from kernel cmdline)".as_ptr(),
    show,
    CMD_AVAIL_NORMAL
);
