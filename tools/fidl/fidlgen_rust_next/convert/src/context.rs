// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_ir::Library;

pub struct Context {
    #[expect(unused)]
    pub library: Library,
    pub rust_crate: String,
    pub rust_next_crate: String,
}
