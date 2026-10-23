// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! CPU data cache benchmark console command.

use core::alloc::Layout;
use core::ffi::c_int;
use kprint::kprintln;

use crate::arch_rs::ops::{clean_cache_range, dcache_line_size};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::platform_rs::timer::current_mono_time;

/// # Safety
///
/// The caller must ensure that `bufsize > 0` and that `buf` is either null or valid for writes and
/// cache operations over `bufsize` bytes for the duration of the call.
unsafe fn bench_cache(bufsize: usize, mut buf: *mut u8) {
    let line_size = dcache_line_size();
    kprintln!("cache line size {}", line_size);

    let alloc_layout = if buf.is_null() {
        (bufsize > 0)
            .then(|| Layout::from_size_align(bufsize, line_size as usize).ok())
            .flatten()
            .and_then(|layout| {
                // SAFETY: `bufsize > 0` is checked above, so `layout` has non-zero size.
                let ptr = unsafe { kalloc::alloc(layout) }?;
                buf = ptr.as_ptr();
                Some(layout)
            })
    } else {
        None
    };

    kprintln!("buf {:p}, size {}", buf, bufsize);

    if buf.is_null() {
        return;
    }

    let mut t = current_mono_time();
    // SAFETY: `buf` is non-null and valid for `bufsize` bytes.
    unsafe { clean_cache_range(buf, bufsize) };
    let mut duration = current_mono_time() - t;

    kprintln!("took {} nsecs to clean {} bytes (cold)", duration.into_nanos(), bufsize);

    // SAFETY: `buf` is valid for writes of `bufsize` bytes.
    unsafe { core::ptr::write_bytes(buf, 0x99, bufsize) };

    t = current_mono_time();
    // SAFETY: `buf` is non-null and valid for `bufsize` bytes.
    unsafe { clean_cache_range(buf, bufsize) };
    duration = current_mono_time() - t;

    if let Some(layout) = alloc_layout {
        // SAFETY: `buf` was allocated with `layout` via `kalloc::alloc` above and has not yet been
        // deallocated.
        unsafe { kalloc::dealloc(buf, layout) };
    }

    kprintln!("took {} nsecs to clean {} bytes (hot)", duration.into_nanos(), bufsize);
}

/// # Safety
///
/// `argv` must be valid for reads of `argc` initialized `CmdArgs` elements for the duration of the
/// call, and if `argc > 1`, `argv[1].arg_uint` must be `0` or a valid writable address for at
/// least 8 MiB.
unsafe extern "C" fn cache_tests(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    let buf = if argc > 1 && !argv.is_null() {
        // SAFETY: `argc > 1` and `argv` is non-null; the console dispatcher guarantees `argv`
        // points to `argc` valid `CmdArgs` elements.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        core::ptr::with_exposed_provenance_mut::<u8>(args[1].arg_uint as usize)
    } else {
        core::ptr::null_mut()
    };

    kprintln!("testing cache");

    // SAFETY: `buf` is either null (causing `bench_cache` to allocate its own buffer) or provided
    // by the console operator as a valid buffer of at least 8 MiB.
    unsafe {
        bench_cache(2 * 1024, buf);
        bench_cache(64 * 1024, buf);
        bench_cache(256 * 1024, buf);
        bench_cache(1024 * 1024, buf);
        bench_cache(8 * 1024 * 1024, buf);
    }
    0
}

static_command!(
    CACHE_TESTS_CMD,
    c"cache_tests".as_ptr(),
    c"test/bench the cpu cache".as_ptr(),
    cache_tests,
    CMD_AVAIL_NORMAL
);
