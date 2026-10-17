// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CStr;

pub const PUBLISHER_SINK_NAME: &str = "data-publisher";
pub const PUBLISHER_SINK_NAME_CSTR: &CStr = c"data-publisher";
pub const VMO_CONTENTS: &str = "Hello World!";
