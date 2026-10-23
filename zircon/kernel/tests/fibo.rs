// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Multi-threaded Fibonacci console command.

use core::ffi::{c_char, c_int, c_void};
use kprint::{kformat, kprintln};

use crate::console::CmdArgs;
use crate::kernel::thread::create;
use crate::platform_rs::timer::{InstantMono, current_mono_time};

extern "C" fn fibo_thread(argv: *mut c_void) -> i32 {
    let fibo = argv.addr();

    if fibo == 0 {
        return 0;
    }
    if fibo == 1 {
        return 1;
    }

    let mut name = [0u8; 32];
    let _ = kformat!(&mut name, "fibo {}", fibo - 1);
    // SAFETY: `name` is NUL-terminated by `kformat!` (`snprintf`), and `fibo_thread` interprets
    // its argument as an integer value.
    let Ok(t0) = (unsafe {
        create(
            name.as_ptr().cast::<c_char>(),
            fibo_thread,
            core::ptr::without_provenance_mut::<c_void>(fibo - 1),
        )
    }) else {
        kprintln!("error creating thread for fibo {}", fibo - 1);
        return 0;
    };

    let _ = kformat!(&mut name, "fibo {}", fibo - 2);
    // SAFETY: `name` is NUL-terminated by `kformat!` (`snprintf`), and `fibo_thread` interprets
    // its argument as an integer value.
    let Ok(t1) = (unsafe {
        create(
            name.as_ptr().cast::<c_char>(),
            fibo_thread,
            core::ptr::without_provenance_mut::<c_void>(fibo - 2),
        )
    }) else {
        kprintln!("error creating thread for fibo {}", fibo - 2);
        // SAFETY: `t0` was just created above and has not yet been resumed.
        unsafe { t0.resume() };
        // SAFETY: `t0` was resumed above and is joined once before returning.
        let _ = unsafe { t0.join(InstantMono::INFINITE) };
        return 0;
    };

    // SAFETY: `t0` was created above and has not yet been resumed.
    unsafe { t0.resume() };
    // SAFETY: `t1` was created above and has not yet been resumed.
    unsafe { t1.resume() };

    // SAFETY: `t0` was resumed above and is joined once.
    let retcode0 = unsafe { t0.join(InstantMono::INFINITE) }.unwrap_or(0);
    // SAFETY: `t1` was resumed above and is joined once.
    let retcode1 = unsafe { t1.join(InstantMono::INFINITE) }.unwrap_or(0);

    retcode0.wrapping_add(retcode1)
}

/// # Safety
///
/// The caller must ensure `argv` is valid for reads of `argc` initialized `CmdArgs` elements for
/// the duration of the call.
pub(super) unsafe extern "C" fn fibo(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 2 || argv.is_null() {
        kprintln!("not enough args");
        return -1;
    }
    // SAFETY: `argc >= 2` and `argv` is non-null; the console dispatcher guarantees `argv` points
    // to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    let tim = current_mono_time();

    // SAFETY: `c"fibo"` is a valid NUL-terminated string, and `fibo_thread` interprets its
    // argument as an integer value.
    let Ok(t) = (unsafe {
        create(
            c"fibo".as_ptr(),
            fibo_thread,
            core::ptr::without_provenance_mut::<c_void>(args[1].arg_uint as usize),
        )
    }) else {
        kprintln!("error creating thread for fibo {}", args[1].arg_uint);
        return -1;
    };
    // SAFETY: `t` was just created above and has not yet been resumed.
    unsafe { t.resume() };

    // SAFETY: `t` was resumed above and is joined once.
    let retcode = unsafe { t.join(InstantMono::INFINITE) }.unwrap_or(0);

    let msec_duration = (current_mono_time() - tim).into_millis();

    kprintln!("fibo {}", retcode);
    kprintln!("took {} msecs to calculate", msec_duration);

    zx_status::sys::ZX_OK
}
