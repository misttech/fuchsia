// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::sync::LazyLock;

pub static PKG_PATH: LazyLock<PathBuf> = LazyLock::new(|| PathBuf::from("/pkg"));
