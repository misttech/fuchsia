// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod fuchsia {
    pub use fxfs_platform::fuchsia::*;
    pub mod testing;
}

pub use self::fuchsia::testing::*;
