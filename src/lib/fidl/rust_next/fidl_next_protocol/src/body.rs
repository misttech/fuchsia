// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_codec::{AsDecoder, DecoderExt};

use crate::Transport;
use crate::wire::MessageHeader;

/// The body of an encoded FIDL message.
///
/// This is a simple wrapper around `T::RecvBuffer` that skips the transaction
/// header when `as_decoder` is called.
pub struct Body<T: Transport> {
    buffer: T::RecvBuffer,
}

impl<T: Transport> Body<T> {
    /// Returns a new `Body` wrapping a `RecvBuffer`.
    pub fn new(buffer: T::RecvBuffer) -> Self {
        Self { buffer }
    }
}

unsafe impl<'de, T: Transport> AsDecoder<'de> for Body<T> {
    type Decoder = <T::RecvBuffer as AsDecoder<'de>>::Decoder;

    fn as_decoder(&'de mut self) -> Self::Decoder {
        let mut decoder = self.buffer.as_decoder();
        let _ = decoder.take_slot::<MessageHeader>();
        decoder
    }
}
