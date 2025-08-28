// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use thiserror::Error;

use fidl_next_codec::DecodeError;

/// Errors that can be produced by FIDL clients and servers.
#[derive(Error, Clone, Debug)]
pub enum ProtocolError<E> {
    /// The underlying transport encountered an error.
    #[error("the underlying transport encountered an error: {0}")]
    TransportError(E),

    /// The underlying transport was stopped gracefully.
    #[error("the transport was stopped gracefully")]
    Stopped,

    /// The underlying transport was closed by the peer.
    #[error("the underlying transport was closed by the peer")]
    PeerClosed,

    /// The underlying transport was closed by the peer with an epitaph.
    #[error("the underlying transport was closed by the peer with epitaph: {0}")]
    PeerClosedWithEpitaph(i32),

    /// The client or server received a message with an invalid protocol header.
    #[error("received a message with an invalid message header: {0}")]
    InvalidMessageHeader(DecodeError),

    /// The client received an epitaph with an invalid body.
    #[error("received an epitaph with an invalid body")]
    InvalidEpitaphBody(DecodeError),

    /// The client received a response for a two-way message which it did not send.
    #[error("received a response which did not correspond to a pending request: txid {txid}")]
    UnrequestedResponse {
        /// The transaction ID which there is no pending response for.
        txid: u32,
    },

    /// The client received a response with the wrong ordinal for the two-way message.
    #[error(
        "received a response with the wrong ordinal for the two-way message; expected ordinal \
        {expected}, but got ordinal {actual}"
    )]
    InvalidResponseOrdinal {
        /// The expected ordinal of the response
        expected: u64,
        /// The actual ordinal of the response
        actual: u64,
    },
}
