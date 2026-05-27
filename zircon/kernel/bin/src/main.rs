// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![no_main]

// This file exists mainly (ahem) just to be the `source_root` file to convince
// GN to make the kernel executable() targets be Rust-based linking instead of
// C++-based linking targets.  But it's also a convenient place to put the Rust
// panic_handler, which the compiler will insist exists somewhere.

use core::panic::PanicInfo;

unsafe extern "C" {
    fn panic(fmt: *const u8, line: i32, ...) -> !;
}

#[panic_handler]
fn rust_panic(info: &PanicInfo<'_>) -> ! {
    let msg_str = {
        if let Some(msg_str) = info.message().as_str() {
            msg_str
        } else {
            "non-static Rust panic message"
        }
    };

    if let Some(location) = info.location() {
        let file = location.file();
        let line = location.line();
        unsafe {
            panic(
                "%.*s:%d: %.*s\0".as_ptr(),
                file.len() as i32,
                file.as_ptr(),
                line as i32,
                msg_str.len() as i32,
                msg_str.as_ptr(),
            );
        }
    } else {
        unsafe {
            panic("%.*s\0".as_ptr(), msg_str.len() as i32, msg_str.as_ptr());
        }
    }
}

// The GN build machinery generates a file of `extern crate ...;` lines for
// each kernel_rust_crate() in the kernel's transitive deps graph.

extern crate rustenv_path;
use rustenv_path::envpath;

include!(envpath!("ZIRCON_EXTERN_CRATE_DECLS"));
