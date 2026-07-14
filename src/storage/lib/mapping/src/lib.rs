// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod extents;

pub use extents::{Extent, Extents, ExtentsIterator};

pub const BLOCK_SIZE: u64 = 4096;
