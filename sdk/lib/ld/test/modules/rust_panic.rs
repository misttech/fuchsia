// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::panic::PanicInfo;
use core::process::abort_immediate;

#[panic_handler]
fn rust_panic(_: &PanicInfo<'_>) -> ! {
    abort_immediate()
}
