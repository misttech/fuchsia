// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitflags::bitflags;

mod fifo;

pub use fifo::*;

bitflags! {
    /// Options that may be used for writes.
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct WriteFlags: u32 {
        const FORCE_ACCESS = 1;
        const PRE_BARRIER = 2;
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions {
    pub flags: WriteFlags,
    pub inline_crypto_options: InlineCryptoOptions,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadOptions {
    pub inline_crypto_options: InlineCryptoOptions,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// InlineCryptoOptions is only used if `slot` is not equal to [`SENTINEL_SLOT_VALUE`].
pub struct InlineCryptoOptions {
    pub dun: u32,
    pub slot: u8,
}

impl Default for InlineCryptoOptions {
    fn default() -> Self {
        InlineCryptoOptions { dun: 0, slot: SENTINEL_SLOT_VALUE }
    }
}

impl InlineCryptoOptions {
    /// Returns true if this request should use inline encryption, false otherwise. Encryption is
    /// disabled by default, or when `slot` is set to [`SENTINEL_SLOT_VALUE`].
    pub fn is_enabled(&self) -> bool {
        return self.slot != SENTINEL_SLOT_VALUE;
    }
}

impl Default for BlockFifoRequest {
    fn default() -> Self {
        BlockFifoRequest {
            // Disable inline encryption by default by setting the slot to the sentinel value.
            slot: SENTINEL_SLOT_VALUE,
            // All remaining values can be default/zero constructed.
            command: Default::default(),
            reqid: Default::default(),
            group: Default::default(),
            vmoid: Default::default(),
            length: Default::default(),
            total_compressed_bytes: Default::default(),
            vmo_offset: Default::default(),
            dev_offset: Default::default(),
            trace_flow_id: Default::default(),
            dun: Default::default(),
            padding: Default::default(),
            compressed_prefix_bytes: Default::default(),
            uncompressed_bytes: Default::default(),
            padding2: Default::default(),
        }
    }
}
