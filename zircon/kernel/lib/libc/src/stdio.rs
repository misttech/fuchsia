// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_size_t, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::{from_mut, from_raw_parts};

/// This represents the kernel <stdio.h> `FILE` type.  When a pointer is
/// received from C++ code, it's treated as &'static mut FILE.  When a FILE is
/// created in Rust, its lifetime flows from the reference it holds.
#[repr(C)]
pub struct FILE<'a> {
    write: extern "C" fn(*mut c_void, *const c_char, c_size_t) -> c_int,
    ptr: *mut c_void,

    // This does not actually affect the layout, but it must be present to tell
    // the compiler that 'a is used for something.  It's used to cast ptr back
    // into &'a mut T to call its write_str method.
    marker: PhantomData<&'a c_void>,
}

extern "C" fn write_to_file<'a, T: fmt::Write + 'a>(
    ptr: *mut c_void,
    chars: *const c_char,
    len: c_size_t,
) -> c_int {
    let u8_chars = chars as *const u8;

    // SAFETY: The caller warrants that the incoming string is valid UTF8.
    let s: &str = unsafe { &*from_raw_parts(u8_chars, len as usize) };

    // SAFETY: Recovering the type-erased pointer constructed by new().
    let write: &'a mut T = unsafe { &mut *(ptr as *mut T) };

    if write.write_str(s).is_ok() { s.len() as c_int } else { -1 }
}

impl<'a> FILE<'a> {
    /// Any Rust object that implements the core::fmt::Write trait can be
    /// wrapped into a FILE object to be passed into C++ code as &mut FILE.
    /// The new FILE object wraps a mutable reference and shares its lifetime.
    pub fn new<T: fmt::Write>(write: &'a mut T) -> FILE<'a> {
        FILE::<'a> {
            write: write_to_file::<'a, T>,
            ptr: from_mut(write) as *mut c_void,
            marker: PhantomData,
        }
    }
}

impl<'a> fmt::Write for FILE<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let c_chars = s.as_ptr() as *const c_char;
        let c_len = s.len() as c_size_t;
        let ret = (self.write)(self.ptr, c_chars, c_len);
        if ret < 0 { Err(fmt::Error) } else { Ok(()) }
    }
}

unsafe extern "C" {
    static mut gStdout: FILE<'static>;
}

pub struct StaticFileWrapper(*mut FILE<'static>);

impl fmt::Write for StaticFileWrapper {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // SAFETY: Pointer is &mut only by type; it's always treated as const.
        unsafe { &mut *self.0 }.write_str(s)
    }
}

/// This returns the kernel <stdio.h> `stdout` as reference, which can be used
/// as a core::fmt::Write traits object (e.g. with the write! macro).
#[inline]
pub fn stdout() -> StaticFileWrapper {
    StaticFileWrapper(&raw mut gStdout)
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        let _ = write!(stdout(), $($arg)*); ()
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        let _ = writeln!(stdout(), $($arg)*); ()
    };
}
