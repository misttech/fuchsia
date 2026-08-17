// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use std::ops::Range;

/// Trait for servicing a page request.
pub trait PageRequest: DataBuffer {
    /// Prepares the target buffer for reading `read_range`.
    ///
    /// Must be called at most once prior to accessing the buffer slice or committing data.
    fn prepare(&mut self, read_range: Range<u64>) -> Result<(), ChunkedArchiveError>;
}
