// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitflags::bitflags;

mod fifo;

pub use fifo::*;
pub const SENTINEL_SLOT_VALUE: u8 = fifo::_SENTINEL_SLOT_VALUE as u8;

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
/// InlineCryptoOptions only used if `slot` is not equal to its sentinel value (0xff).
pub struct InlineCryptoOptions {
    pub dun: u32,
    pub slot: u8,
}

impl Default for InlineCryptoOptions {
    fn default() -> Self {
        InlineCryptoOptions { dun: 0, slot: SENTINEL_SLOT_VALUE }
    }
}
