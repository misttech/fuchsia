// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel heap bindings and debug console command.

use core::ptr::NonNull;
use heap_bindings as bindings;

/// Returns the address and size of the heap profile buffer if it exists, and `None` otherwise.
///
/// The buffer is wired kernel memory that lives for the lifetime of the kernel and is written
/// concurrently by the heap profiler, so it must not be read through a Rust reference.
pub fn get_heap_profile() -> Option<(NonNull<u8>, usize)> {
    let mut ptr: *const core::ffi::c_void = core::ptr::null();
    let mut size: usize = 0;
    // SAFETY: `ptr` and `size` are valid local out-pointers.
    unsafe {
        bindings::cpp_get_heap_profile(&raw mut ptr, &raw mut size);
    }
    NonNull::new(ptr.cast_mut().cast::<u8>()).map(|ptr| (ptr, size))
}

/// Tracks an allocation of the given `size` in the heap profiler. This can be used to make visible
/// allocations that, while not from the general kernel heap, are notionally heap-like or from
/// separate slab / other non-general heap allocators.
/// The return value is a cookie that, along with `size` can be passed to `profile_track_free` to
/// indicate deallocation.
pub fn profile_track_alloc(size: usize) -> u32 {
    // SAFETY: There are no obligations.
    unsafe { bindings::cpp_profile_track_alloc(size) }
}

/// Tracks deallocation of `size` bytes associated with `cookie` in the heap profiler.
pub fn profile_track_free(cookie: u32, size: usize) {
    // SAFETY: There are no obligations.
    unsafe { bindings::cpp_profile_track_free(cookie, size) }
}

/// Gets stats about the heap.
/// Returns `(total_bytes, free_bytes)`, where `total_bytes` is the total size of the heap (the sum
/// of all pages allocated from the PMM) and `free_bytes` is the free portion.
pub fn get_info() -> (usize, usize) {
    let mut total_bytes = 0;
    let mut free_bytes = 0;
    // SAFETY: Passing valid, aligned, non-aliasing mutable pointers to local stack variables.
    unsafe { bindings::cpp_heap_get_info(&mut total_bytes, &mut free_bytes) };
    (total_bytes, free_bytes)
}

/// Kernel console `heap` command implementation.
#[cfg(console_enabled)]
mod console {
    use super::bindings;
    use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
    use bitflags::bitflags;
    use core::ffi::c_int;
    use core::sync::atomic::{AtomicBool, Ordering};
    use kprint::kprintln;

    bitflags! {
        /// Options controlling `heap_dump` output.
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
        struct CmpctDumpOptions: u32 {
            /// Standard dump output.
            const NONE = bindings::CmpctDumpOptions_None;
            /// Dump called at panic time; avoids blocking locks.
            const PANIC_TIME = bindings::CmpctDumpOptions_PanicTime;
            /// Include verbose free-block details.
            const VERBOSE = bindings::CmpctDumpOptions_Verbose;
        }
    }

    /// Whether per-callsite heap statistics collection is enabled at build time.
    const HEAP_COLLECT_STATS: bool = bindings::HEAP_COLLECT_STATS != 0;

    /// Dumps heap diagnostics using `options`.
    fn heap_dump(options: CmpctDumpOptions) {
        // SAFETY: `cpp_heap_dump` dumps heap state using the provided option flags.
        unsafe { bindings::cpp_heap_dump(options.bits()) }
    }

    /// Dumps collected per-callsite heap statistics.
    fn dump_stats() {
        // SAFETY: `cpp_heap_dump_stats` is internally synchronized by `stat_lock`.
        unsafe { bindings::cpp_heap_dump_stats() }
    }

    /// Runs the internal heap self-test.
    fn heap_test() {
        // SAFETY: `cpp_heap_test` runs the heap self-test.
        unsafe { bindings::cpp_heap_test() }
    }

    unsafe extern "C" {
        /// Whether heap allocation tracing is enabled.
        ///
        /// Defined in `zircon/kernel/lib/heap/heap_wrapper.cc` as
        /// `extern "C" constinit ktl::atomic<bool> heap_trace`.
        #[link_name = "heap_trace"]
        static HEAP_TRACE: AtomicBool;
    }

    /// Toggles heap allocation tracing and returns the new state.
    fn toggle_heap_trace() -> bool {
        // SAFETY: `HEAP_TRACE` links directly to `extern "C" constinit ktl::atomic<bool>
        // heap_trace` in C++, which matches Rust's `AtomicBool` type, size, alignment, and
        // lock-free atomic representation.
        // `Ordering::Relaxed` is sufficient as this is an independent diagnostic flag with no
        // dependent memory operations to synchronize.
        !unsafe { HEAP_TRACE.fetch_xor(true, Ordering::Relaxed) }
    }

    /// Console command handler for `k heap`.
    ///
    /// # Safety
    ///
    /// `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc` initialized
    /// `CmdArgs` entries for the duration of the call.
    unsafe extern "C" fn cmd_heap(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("\t{:s} info [-v]", cmd_name);
            if HEAP_COLLECT_STATS {
                kprintln!("\t{:s} stats", cmd_name);
            }
            if (flags & CMD_FLAG_PANIC) == 0 {
                kprintln!("\t{:s} trace", cmd_name);
                kprintln!("\t{:s} trim", cmd_name);
                kprintln!("\t{:s} test", cmd_name);
            }
            -1
        };

        if args.len() < 2 {
            return usage();
        }

        let subcmd = args[1].as_str();
        if subcmd == "info" {
            let mut options = if (flags & CMD_FLAG_PANIC) != 0 {
                CmpctDumpOptions::PANIC_TIME
            } else {
                CmpctDumpOptions::NONE
            };

            for arg in &args[2..] {
                let opt = arg.as_str();
                if opt == "-v" {
                    options |= CmpctDumpOptions::VERBOSE;
                } else {
                    kprintln!("unrecognized option (\"{:s}\") for info command", opt);
                    return usage();
                }
            }

            heap_dump(options);
        } else if HEAP_COLLECT_STATS && subcmd == "stats" {
            dump_stats();
        } else if (flags & CMD_FLAG_PANIC) == 0 && subcmd == "test" {
            heap_test();
        } else if (flags & CMD_FLAG_PANIC) == 0 && subcmd == "trace" {
            let heap_trace = toggle_heap_trace();
            kprintln!("heap trace is now {:s}", if heap_trace { "on" } else { "off" });
        } else {
            kprintln!("unrecognized command");
            return usage();
        }

        0
    }

    static_command!(
        HEAP,
        c"heap".as_ptr(),
        c"heap debug commands".as_ptr(),
        cmd_heap,
        CMD_AVAIL_ALWAYS
    );
}
