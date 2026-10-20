// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Jump trace (`jtrace`) facade and console commands.

unsafe extern "C" {
    /// Dumps the specified jtrace buffer.
    ///
    /// # Safety
    ///
    /// Safe to call with any valid [`TraceBufferType`] variant.
    fn cpp_jtrace_dump(which: TraceBufferType);
}

/// Which jtrace buffer to dump.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceBufferType {
    /// Dump the active runtime trace buffer.
    Current = 0,
    /// Dump the trace buffer recovered from persistent RAM across a reboot.
    Recovered = 1,
}

/// Dumps the specified jtrace buffer (`jtrace_dump`).
pub fn jtrace_dump(which: TraceBufferType) {
    // SAFETY: Safe FFI call into `cpp_jtrace_dump`.
    unsafe { cpp_jtrace_dump(which) };
}

#[cfg(console_enabled)]
mod console {
    use super::{TraceBufferType, jtrace_dump};
    use crate::arch_rs::max_num_cpus;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use core::ffi::c_int;
    use kprint::kprintln;

    unsafe extern "C" {
        /// Returns a pointer to the allocated jtrace buffer and writes its size to `out_size`.
        ///
        /// # Safety
        ///
        /// `out_size` must point to a valid, writable `usize`.
        fn cpp_jtrace_get_location(out_size: *mut usize) -> *const u8;
    }

    /// Returns the data pointer and byte length of the allocated jtrace buffer.
    fn get_location() -> (*const u8, usize) {
        let mut size: usize = 0;
        // SAFETY: `size` is a valid out-pointer on the caller's stack.
        let data = unsafe { cpp_jtrace_get_location(&mut size) };
        (data, size)
    }

    const TARGET_BUFFER_SIZE: usize = zr::parse_usize(env!("JTRACE_TARGET_BUFFER_SIZE"))
        .expect("JTRACE_TARGET_BUFFER_SIZE invalid");
    const LAST_ENTRY_STORAGE: usize = zr::parse_usize(env!("JTRACE_LAST_ENTRY_STORAGE"))
        .expect("JTRACE_LAST_ENTRY_STORAGE invalid");
    const USE_LARGE_ENTRIES: bool = cfg!(jtrace_use_large_entries);
    const IS_PERSISTENT: bool = cfg!(jtrace_persistent);

    /// CLI command for `jtrace`.
    ///
    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where
    /// `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
    #[allow(clippy::absurd_extreme_comparisons)]
    unsafe extern "C" fn cmd_jtrace(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let program = argv[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage: {:s} [-r|-i]", program);
            kprintln!(
                "  -r : dump the recovered trace buffer instead of the current trace buffer."
            );
            kprintln!("  -i : dump information about the current JTRACE configuration.");
            -1
        };

        match argc {
            1 => {
                jtrace_dump(TraceBufferType::Current);
            }
            2 => {
                if argv[1].as_str() == "-r" {
                    jtrace_dump(TraceBufferType::Recovered);
                } else if argv[1].as_str() == "-i" {
                    if TARGET_BUFFER_SIZE == 0 {
                        kprintln!("Debug tracing is not enabled in this build.");
                    } else {
                        let (loc_data, loc_size) = get_location();
                        kprintln!("JTRACE configuration");
                        kprintln!("--------------------");
                        kprintln!("Requested Buffer Size  : {}", TARGET_BUFFER_SIZE);
                        kprintln!("Allocated Buffer Size  : {}", loc_size);
                        kprintln!("Allocated Buffer Loc   : {:p}", loc_data);
                        kprintln!("Per-CPU last entry cnt : {}", LAST_ENTRY_STORAGE);
                        kprintln!(
                            "Large entries          : {:s}",
                            if USE_LARGE_ENTRIES { "yes" } else { "no" }
                        );
                        kprintln!(
                            "Persistent             : {:s}",
                            if IS_PERSISTENT { "yes" } else { "no" }
                        );

                        if (LAST_ENTRY_STORAGE > 0)
                            && (LAST_ENTRY_STORAGE != max_num_cpus() as usize)
                        {
                            kprintln!(
                                concat!(
                                    "\nWarning! Configured per-cpu last entry count ({}) ",
                                    "does not match target's number of CPUs ({})",
                                ),
                                LAST_ENTRY_STORAGE,
                                max_num_cpus()
                            );
                        }
                    }
                } else {
                    return usage();
                }
            }
            _ => {
                return usage();
            }
        }

        0
    }

    static_command!(
        CMD_JTRACE,
        c"jtrace".as_ptr(),
        c"dump the current or recovered jtrace".as_ptr(),
        cmd_jtrace,
        CMD_AVAIL_NORMAL
    );
}
