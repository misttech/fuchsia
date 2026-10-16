// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![cfg(console_enabled)]

unsafe extern "C" {
    pub(super) fn cpp_boot_constants_kernel_version_ident(size: &mut usize) -> *const u8;
}
