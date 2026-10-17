// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The sole purpose of this binary is to publish a VMO with known contents to
// the provided '/svc' handle through `__sanitizer_publish_data`.

use published_data::{PUBLISHER_SINK_NAME_CSTR, VMO_CONTENTS};
use std::fmt::Write as _;
use std::panic::set_hook;
use std::process::abort;
use zx::{Vmo, system_get_page_size};
use zx_libc::sanitizer::{Log, publish_data};

fn main() {
    set_hook(Box::new(|info| {
        let _ = writeln!(&mut Log::new(), "{info}");
        abort();
    }));

    let data = Vmo::create(u64::from(system_get_page_size())).expect("failed to create VMO");
    data.write(VMO_CONTENTS.as_bytes(), 0).expect("failed to write VMO contents");

    let _token = publish_data(PUBLISHER_SINK_NAME_CSTR, data);
}
