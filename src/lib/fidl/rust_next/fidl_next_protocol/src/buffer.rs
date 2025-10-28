// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_constants::MAGIC_NUMBER_INITIAL;
use fidl_next_codec::{
    DecodeError, DecoderExt as _, EncodeError, EncoderExt as _, WireI32, WireU32, WireU64,
};

use crate::{
    MessageHeaderFlags0, MessageHeaderFlags1, MessageHeaderFlags2, Transport, WireEpitaph,
    WireMessageHeader,
};

/// The flexibility of a method.
#[derive(Clone, Copy, Debug)]
pub enum Flexibility {
    /// The method is strict.
    Strict,
    /// The method is flexible.
    Flexible,
}

/// Encodes a message into the given buffer.
pub fn encode_header<T: Transport>(
    buffer: &mut T::SendBuffer,
    txid: u32,
    ordinal: u64,
    flexibility: Flexibility,
) -> Result<(), EncodeError> {
    buffer.encode_next(
        WireMessageHeader {
            txid: WireU32(txid),
            flags_0: MessageHeaderFlags0::WIRE_FORMAT_V2,
            flags_1: MessageHeaderFlags1::empty(),
            flags_2: match flexibility {
                Flexibility::Strict => MessageHeaderFlags2::empty(),
                Flexibility::Flexible => MessageHeaderFlags2::FLEXIBLE_METHOD,
            },
            magic_number: MAGIC_NUMBER_INITIAL,
            ordinal: WireU64(ordinal),
        },
        (),
    )
}

/// Parses the transaction ID and ordinal from the given buffer.
pub fn decode_header<T: Transport>(
    mut buffer: &mut T::RecvBuffer,
) -> Result<(u32, u64, Flexibility), DecodeError> {
    let header = buffer.decode_owned::<WireMessageHeader>()?;

    let flexibility = if header.flags_2.contains(MessageHeaderFlags2::FLEXIBLE_METHOD) {
        Flexibility::Flexible
    } else {
        Flexibility::Strict
    };

    Ok((*header.txid, *header.ordinal, flexibility))
}

/// Encodes an epitaph into the given buffer.
pub fn encode_epitaph<T: Transport>(
    buffer: &mut T::SendBuffer,
    error: i32,
) -> Result<(), EncodeError> {
    buffer.encode_next(WireEpitaph { error: WireI32(error) }, ())
}

/// Parses the epitaph error from the given buffer.
pub fn decode_epitaph<T: Transport>(mut buffer: &mut T::RecvBuffer) -> Result<i32, DecodeError> {
    Ok(*buffer.decode_owned::<WireEpitaph>()?.error)
}
