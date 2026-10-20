// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Crashlog recovery and console commands.

use fbl::RefPtr;

use crate::vm::vm_object::VmObject;

unsafe extern "C" {
    /// Returns the stashed recovered crashlog VMO as a raw pointer, or null if none is stashed.
    ///
    /// # Safety
    ///
    /// Always safe to call; returns a raw `VmObject` pointer exported via `fbl::ExportToRawPtr`
    /// that transfers ownership of one strong reference count to the caller (or null if no
    /// crashlog is stashed).
    fn cpp_crashlog_get_stashed() -> *mut VmObject;
}

/// Returns the previously stashed recovered crashlog, or `None`.
pub fn crashlog_get_stashed() -> Option<RefPtr<VmObject>> {
    // SAFETY: `cpp_crashlog_get_stashed` returns a raw `VmObject` pointer exported via
    // `fbl::ExportToRawPtr` (or null), transferring one strong reference count.
    unsafe { RefPtr::try_from_raw(cpp_crashlog_get_stashed()) }
}

#[cfg(console_enabled)]
mod console {
    use super::crashlog_get_stashed;
    use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
    use core::ffi::{CStr, c_int};
    use core::mem::MaybeUninit;
    use kalloc::Box as UniquePtr;
    use kprint::kprintln;
    use zx_status::Status;
    use zx_types::ZX_OK;

    fn print_recovered_crashlog() {
        let Some(crashlog) = crashlog_get_stashed() else {
            kprintln!("no recovered crashlog");
            return;
        };

        // Allocate a temporary buffer so we can convert the VMO's contents to a C string.
        let buffer_size = (crashlog.size() as usize) + 1;
        let mut buffer = UniquePtr::<[MaybeUninit<u8>]>::default();
        if UniquePtr::try_grow(&mut buffer, buffer_size).is_err() {
            kprintln!("error: failed to allocate {} for crashlog", buffer_size);
            return;
        }

        buffer.fill(MaybeUninit::new(0));
        if let Err(status) = crashlog.read(0, &mut buffer[..buffer_size - 1]) {
            kprintln!("error: failed to read from recovered crashlog vmo: {}", status.into_raw());
            return;
        }

        kprintln!("recovered crashlog follows...");
        // SAFETY: `buffer` is zero-initialized of length `buffer_size` and `read` only writes at
        // most `buffer_size - 1` bytes, so `buffer[buffer_size - 1]` remains a NUL terminator.
        kprintln!("{:s}", unsafe { CStr::from_ptr(buffer.as_ptr().cast()) });
        kprintln!("... end of recovered crashlog");
    }

    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where
    /// `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn cmd_crashlog(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        'usage: {
            if argc < 2 {
                kprintln!("not enough arguments");
                break 'usage;
            }

            if argv[1].as_str() == "dump" {
                print_recovered_crashlog();
            } else {
                kprintln!("unknown command");
                break 'usage;
            }

            return ZX_OK;
        }
        kprintln!("usage:");
        kprintln!(
            "{:s} dump                              : dump the recovered crashlog",
            argv[0].as_str()
        );
        Status::INTERNAL.into_raw()
    }

    static_command!(
        CMD_CRASHLOG,
        c"crashlog".as_ptr(),
        c"crashlog".as_ptr(),
        cmd_crashlog,
        CMD_AVAIL_ALWAYS
    );
}
