// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_uint};
use core::panic::PanicInfo;

unsafe extern "C" {
    fn panic(fmt: *const c_char, ...) -> !;
}

#[panic_handler]
fn rust_panic(info: &PanicInfo<'_>) -> ! {
    let msg_str = info.message().as_str().unwrap_or("non-static Rust panic message");

    if let Some(location) = info.location() {
        let file = location.file_as_c_str();
        let line = location.line();
        unsafe {
            panic(
                c"%s:%u: %.*s".as_ptr(),
                file.as_ptr(),
                line as c_uint,
                msg_str.len() as c_int,
                msg_str.as_ptr(),
            );
        }
    } else {
        unsafe {
            panic(c"%.*s".as_ptr(), msg_str.len() as i32, msg_str.as_ptr());
        }
    }
}
