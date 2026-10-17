// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

pub use boot_options_bindings::*;

/// See `kernel.page-scanner.eviction_policy`.
pub const PAGE_SCANNER_ENABLE_EVICTION_NAME: &str = "kernel.page-scanner.enable-eviction";
