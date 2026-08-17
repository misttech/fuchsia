// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const MAPPINGS_COMMAND: u32 = 1;
pub const CLOSE_BLOB_COMMAND: u32 = 2;

// The `vmo-fifo` divides the VMO into two regions: a fixed-size command slots region, and a
// dynamically allocated payload region where the actual extents are written.
//
// The following is the layout for a 512KB VMO with 256 capacity:
// [ Headers (64B) | Command Slots: 256 * 24B = 6,144B | .. Padding to 8KB .. | Payload (504KB) ]
// Note: Each command slot takes 24 bytes for `RawMappingCommand`.
//
// 504KB / 8-bytes per extent = 64,512 maximum extents bounded by the payload block.
pub const MAPPING_VMO_SIZE: u64 = 512 * 1024;

// With a maximum capacity of 256 pending mapping commands, this allows for an average of ~252
// extents per blob. In the worst case of maximum fragmentation (every 4KB block maps to one
// extent), 64,512 extents can map up to ~252MB of blob data (or ~504MB if block size is 8KB).
pub const PENDING_COMMANDS_CAPACITY: u32 = 256;

/// A command packet used to communicate extent mappings.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Copy, Clone, Debug, PartialEq)]
#[repr(C)]
pub struct RawMappingCommand {
    pub opcode: u32,
    pub offset: u32,
    pub key: u64,
    pub metadata_count: u32,
    pub blob_count: u32,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MappingCommand {
    /// Informs the driver of the extent mappings for a blob.
    /// The VMO payload contains `blob_count` data extent mappings followed by `metadata_count`
    /// Merkle extent mappings.
    Mappings {
        /// Session-unique identifier for the blob.
        key: u64,
        /// Byte offset within the shared VMO where the extent descriptors begin.
        offset: u32,
        /// Number of Merkle tree metadata extent mappings.
        metadata_count: u32,
        /// Number of Blob data extent mappings.
        blob_count: u32,
    },
    /// Informs the driver that the blob session is closed and mappings can be discarded.
    CloseBlob {
        /// Session-unique identifier for the blob.
        key: u64,
    },
}

impl From<MappingCommand> for RawMappingCommand {
    fn from(cmd: MappingCommand) -> Self {
        match cmd {
            MappingCommand::Mappings { key, offset, metadata_count, blob_count } => {
                RawMappingCommand {
                    opcode: MAPPINGS_COMMAND,
                    offset,
                    key,
                    metadata_count,
                    blob_count,
                }
            }
            MappingCommand::CloseBlob { key } => RawMappingCommand {
                opcode: CLOSE_BLOB_COMMAND,
                offset: 0,
                key,
                metadata_count: 0,
                blob_count: 0,
            },
        }
    }
}

impl TryFrom<RawMappingCommand> for MappingCommand {
    type Error = Error;

    fn try_from(cmd: RawMappingCommand) -> Result<Self, Self::Error> {
        match cmd.opcode {
            MAPPINGS_COMMAND => Ok(MappingCommand::Mappings {
                key: cmd.key,
                offset: cmd.offset,
                metadata_count: cmd.metadata_count,
                blob_count: cmd.blob_count,
            }),
            CLOSE_BLOB_COMMAND => Ok(MappingCommand::CloseBlob { key: cmd.key }),
            _ => Err(anyhow!("Unknown opcode: {}", cmd.opcode)),
        }
    }
}
