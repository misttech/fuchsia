// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_codec::Constrained;
use fidl_next_protocol::{Flexibility, Transport};

/// A FIDL protocol which has associated connection handles.
///
/// # Safety
///
/// The associated `Client` and `Server` types must be `#[repr(transparent)]`
/// wrappers around `Client<T>` and `Server<T>` respectively.
pub unsafe trait HasConnectionHandles<T> {
    /// The client for the protocol. It must be a `#[repr(transparent)]` wrapper
    /// around `Client<T>`.
    type Client;

    /// The server for the protocol. It must be a `#[repr(transparent)]` wrapper
    /// around `Server<T>`.
    type Server;
}

/// A discoverable FIDL protocol.
pub trait Discoverable {
    /// The service name to use to connect to this discoverable protocol.
    const PROTOCOL_NAME: &'static str;
}

/// A method of a protocol.
pub trait Method {
    /// The ordinal associated with the method;
    const ORDINAL: u64;

    /// The flexibility of the method.
    const FLEXIBILITY: Flexibility;

    /// The protocol the method is a member of.
    type Protocol;

    /// The request payload for the method.
    type Request;

    /// The response payload for the method.
    type Response: Constrained;
}

/// A method which can be responded to with a single value.
///
/// For methods which return a result, this method implicitly returns `Ok` of
/// the given response.
pub trait Respond<R> {
    /// The returned response type.
    type Output;

    /// Makes a response from the given input.
    fn respond(response: R) -> Self::Output;
}

/// A method which can be responded `Err` to with a single value.
pub trait RespondErr<R> {
    /// The returned response type.
    type Output;

    /// Makes an `Err` response from the given input.
    fn respond_err(response: R) -> Self::Output;
}

/// A protocol which has a default transport type.
pub trait HasTransport {
    /// The default transport type for this protocol.
    type Transport: Transport;
}
