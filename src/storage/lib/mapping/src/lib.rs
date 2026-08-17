// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod blob;
pub mod extents;
pub mod page_request;
pub mod pager;
pub mod protocol;
pub mod reader;

#[cfg(test)]
pub mod testing;

pub use blob::{Blob, Blobs, process_mapping_command, read_blob_metadata};
pub use extents::{Extent, Extents, ExtentsIterator};
pub use page_request::PageRequest;
pub use pager::{PagerThread, run_pager_loop};
pub use protocol::{
    CLOSE_BLOB_COMMAND, MAPPING_VMO_SIZE, MAPPINGS_COMMAND, MappingCommand,
    PENDING_COMMANDS_CAPACITY, RawMappingCommand,
};

pub const BLOCK_SIZE: u64 = 4096;
