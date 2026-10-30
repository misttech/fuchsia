// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod extents;
pub mod file;
pub mod page_request;
pub mod protocol;
pub mod reader;
pub mod testing;

pub use extents::{DeviceExtent, Extent, Extents, ExtentsIterator};
pub use file::{
    DEFAULT_VERIFICATION_ALIGNMENT, DecodedBlobMetadata, DeliveryHandler, File, Files,
    NoopDeliveryHandler, PagerThread, TARGET_READ_AHEAD_SIZE, Transform, process_mapping_command,
    read_blob_metadata,
};
pub use fxfs_crypto::Cipher;
pub use page_request::{NullPageRequest, PagePrecommit, PageRequest};
pub use protocol::{
    CLOSE_BLOB_COMMAND, DELIVERY_CHILD_DATA_COMMAND, DELIVERY_CHILD_INLINE_TAIL_COMMAND,
    DELIVERY_DATA_COMMAND, DELIVERY_REGISTER_BLOB_COMMAND, DELIVERY_VMO_SIZE, DeliveryCommand,
    ENCRYPTION_KEY_SIZE, MAPPING_VMO_SIZE, MAPPINGS_COMMAND, MAPPINGS_FLAG_ENCRYPTED,
    MappingCommand, PENDING_COMMANDS_CAPACITY, PENDING_DELIVERY_COMMANDS_CAPACITY,
    RawDeliveryCommand, RawMappingCommand,
};
pub use reader::{ChildBlockService, read_buffer_from_extents};

pub const BLOCK_SIZE: u64 = 4096;
