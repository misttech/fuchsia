// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use cbuf::Cbuf;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use pin_init::stack_pin_init;
use zx_status::Status;
use zx_types::ZX_TIME_INFINITE;

const ZX_ERR_INTERNAL_INTR_KILLED: i32 = -502;

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_constructor() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());
    if !cbuf.full() {
        return false;
    }

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is valid for cbuf lifetime.
    unsafe {
        if cbuf.initialize(buf.len(), buf.as_mut_ptr()).is_err() {
            return false;
        }
    }
    if cbuf.full() {
        return false;
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_read_write() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is valid for cbuf lifetime.
    unsafe {
        if cbuf.initialize(buf.len(), buf.as_mut_ptr()).is_err() {
            return false;
        }
    }

    if cbuf.full() {
        return false;
    }

    // Nothing to read, don't wait.
    if cbuf.read_char(false) != Err(Status::SHOULD_WAIT) {
        return false;
    }

    // Write some characters.
    let data = b"ABC";
    for &c in data {
        if cbuf.write_char(c) != 1 {
            return false;
        }
    }
    if !cbuf.full() {
        return false;
    }

    // Writing when full should return 0.
    if cbuf.write_char(b'D') != 0 {
        return false;
    }

    // Read them back.
    for (i, &expected) in data.iter().enumerate() {
        match cbuf.read_char_with_context(true) {
            Ok(res) => {
                if res.transitioned_from_full != (i == 0) {
                    return false;
                }
                if res.c != expected {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    if cbuf.full() {
        return false;
    }

    true
}

extern "C" fn reader_thread_entry(arg: *mut c_void) -> i32 {
    // SAFETY: arg is a valid pointer to a Cbuf pinned on the parent thread's stack.
    let cbuf = unsafe { &*(arg as *const Cbuf) };
    loop {
        match cbuf.read_char(true) {
            Ok(_) => {}
            Err(status) => return status.into_raw(),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_read_write_race() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is valid for cbuf lifetime.
    unsafe {
        if cbuf.initialize(buf.len(), buf.as_mut_ptr()).is_err() {
            return false;
        }
    }

    let thread_name = b"cbuf_rust_race\0".as_ptr() as *const c_char;
    let cbuf_ptr = &*cbuf as *const Cbuf as *mut c_void;

    // SAFETY: we pass reader_thread_entry and valid pointers. The thread is joined
    // before `cbuf` (and `buf`) goes out of scope.
    unsafe {
        let thread = match kernel::thread::spawn(thread_name, reader_thread_entry, cbuf_ptr) {
            Ok(t) => t,
            Err(_) => return false,
        };

        for _ in 0..1000 {
            while cbuf.write_char(b'A') == 0 {
                kernel::thread::r#yield();
            }
        }

        thread.kill();

        let ret = match thread.join(ZX_TIME_INFINITE) {
            Ok(r) => r,
            Err(_) => return false,
        };
        if ret != ZX_ERR_INTERNAL_INTR_KILLED {
            return false;
        }
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_init_limits() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());
    let mut buf = [0u8; 4];

    // Size 0 should fail.
    unsafe {
        if cbuf.initialize(0, buf.as_mut_ptr()) != Err(Status::INVALID_ARGS) {
            return false;
        }
    }

    // Non-power of two should fail.
    unsafe {
        if cbuf.initialize(3, buf.as_mut_ptr()) != Err(Status::INVALID_ARGS) {
            return false;
        }
        if cbuf.initialize(5, buf.as_mut_ptr()) != Err(Status::INVALID_ARGS) {
            return false;
        }
    }

    // Power of two should succeed.
    unsafe {
        if cbuf.initialize(4, buf.as_mut_ptr()).is_err() {
            return false;
        }
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_uninitialized() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());

    if !cbuf.full() {
        return false;
    }

    if cbuf.write_char(b'A') != 0 {
        return false;
    }

    if cbuf.read_char(false) != Err(Status::SHOULD_WAIT) {
        return false;
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_wrap_around() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());
    let mut buf = [0u8; 4];

    unsafe {
        if cbuf.initialize(buf.len(), buf.as_mut_ptr()).is_err() {
            return false;
        }
    }

    // Write 3 chars (capacity is 3)
    if cbuf.write_char(b'A') != 1 {
        return false;
    }
    if cbuf.write_char(b'B') != 1 {
        return false;
    }
    if cbuf.write_char(b'C') != 1 {
        return false;
    }

    if !cbuf.full() {
        return false;
    }

    // Read 3 chars
    if cbuf.read_char(false) != Ok(b'A') {
        return false;
    }
    if cbuf.read_char(false) != Ok(b'B') {
        return false;
    }
    if cbuf.read_char(false) != Ok(b'C') {
        return false;
    }

    if cbuf.full() {
        return false;
    }

    // Write 2 chars (wraps pointers)
    if cbuf.write_char(b'D') != 1 {
        return false;
    }
    if cbuf.write_char(b'E') != 1 {
        return false;
    }

    // Read 2 chars (wraps pointers)
    if cbuf.read_char(false) != Ok(b'D') {
        return false;
    }
    if cbuf.read_char(false) != Ok(b'E') {
        return false;
    }

    // Should be empty
    if cbuf.read_char(false) != Err(Status::SHOULD_WAIT) {
        return false;
    }

    true
}

struct BlockingReadContext {
    cbuf: *mut Cbuf,
    state: *const AtomicU32, // 0: init, 1: about to read, 2: read done, 3: error
    read_char: *mut u8,
}

// SAFETY: We only pass valid pointers and don't share mutability unsafely.
unsafe impl Send for BlockingReadContext {}

extern "C" fn blocking_reader_entry(arg: *mut c_void) -> i32 {
    let ctx = unsafe { &*(arg as *const BlockingReadContext) };
    let cbuf = unsafe { &*ctx.cbuf };
    let state = unsafe { &*ctx.state };

    state.store(1, Ordering::SeqCst);
    let c = cbuf.read_char(true); // Should block until written.

    match c {
        Ok(val) => {
            unsafe { *ctx.read_char = val };
            state.store(2, Ordering::SeqCst);
            0
        }
        Err(status) => {
            state.store(3, Ordering::SeqCst); // error
            status.into_raw()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_cbuf_blocking_read() -> bool {
    stack_pin_init!(let cbuf = Cbuf::init());

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is valid for cbuf lifetime.
    unsafe {
        if cbuf.initialize(buf.len(), buf.as_mut_ptr()).is_err() {
            return false;
        }
    }

    let state = AtomicU32::new(0);
    let mut read_char = 0u8;

    let mut ctx = BlockingReadContext {
        cbuf: &*cbuf as *const Cbuf as *mut Cbuf,
        state: &state,
        read_char: &mut read_char,
    };

    let thread_name = b"cbuf_blocking_read\0".as_ptr() as *const c_char;
    let ctx_ptr = &mut ctx as *mut BlockingReadContext as *mut c_void;

    unsafe {
        let thread = match kernel::thread::spawn(thread_name, blocking_reader_entry, ctx_ptr) {
            Ok(t) => t,
            Err(_) => return false,
        };

        // Wait until the reader thread is about to read.
        while state.load(Ordering::SeqCst) < 1 {
            kernel::thread::r#yield();
        }

        // Wait until the reader thread is actually blocked.
        while !thread.is_blocked() {
            kernel::thread::r#yield();
            // If it failed and exited, break.
            if state.load(Ordering::SeqCst) == 3 {
                break;
            }
        }

        if state.load(Ordering::SeqCst) == 3 {
            thread.join(ZX_TIME_INFINITE).ok();
            return false;
        }

        // Double check it is indeed blocked and state is 1.
        if !thread.is_blocked() || state.load(Ordering::SeqCst) != 1 {
            thread.join(ZX_TIME_INFINITE).ok();
            return false;
        }

        // Now write a char. This should wake it up.
        if cbuf.write_char(b'X') != 1 {
            thread.join(ZX_TIME_INFINITE).ok();
            return false;
        }

        // Wait for reader thread to complete.
        let ret = match thread.join(ZX_TIME_INFINITE) {
            Ok(r) => r,
            Err(_) => return false,
        };

        if ret != 0 {
            return false;
        }

        if state.load(Ordering::SeqCst) != 2 {
            return false;
        }

        if read_char != b'X' {
            return false;
        }
    }

    true
}
