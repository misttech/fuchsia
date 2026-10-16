// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::ZX_MAX_NAME_LEN;

pub fn name_to_c_buf(name: &[u8]) -> [u8; ZX_MAX_NAME_LEN] {
    let mut buf = [0u8; ZX_MAX_NAME_LEN];
    let len = name.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&name[..len]);
    buf
}
