// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use core::fmt::{self, Write as _};
use core::write;
use libc::stdio::FILE;

#[unsafe(no_mangle)]
pub extern "C" fn write_to_stdio_in_rust(f: &mut FILE<'_>) -> bool {
    write!(f, "{} {}!", "Hello", "world").is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn write_to_stdio_with_nul_in_rust(f: &mut FILE<'_>) -> bool {
    write!(f, "{}\0{}!", "Hello", "world").is_ok()
}

struct WriteBuf([u8; 16]);

impl fmt::Write for WriteBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > self.0.len() {
            fmt::Result::Err(fmt::Error)
        } else {
            self.0[..s.len()].copy_from_slice(s.as_bytes());
            fmt::Result::Ok(())
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn write_to_rust_from_stdio(f: extern "C" fn(&mut FILE<'_>) -> i32) -> bool {
    let mut buf = WriteBuf([0; _]);
    let wrote = f(&mut FILE::new(&mut buf));
    let hello = b"Hello world!";
    wrote == hello.len().try_into().unwrap() && &buf.0[0..hello.len()] == hello
}
