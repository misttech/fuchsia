// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_codec::{Constrained, Unconstrained};

/// A FIDL protocol.
///
/// # Safety
///
/// The associated `Client` and `Server` types must be `#[repr(transparent)]`
/// wrappers around `Client<T>` and `Server<T>` respectively.
pub unsafe trait Protocol<T> {
    /// The client for the protocol. It must be a `#[repr(transparent)]` wrapper
    /// around `Client<T>`.
    type Client;

    /// The server for the protocol. It must be a `#[repr(transparent)]` wrapper
    /// around `Server<T>`.
    type Server;
}

/// A discoverable protocol.
pub trait Discoverable {
    /// The service name to use to connect to this discoverable protocol.
    const PROTOCOL_NAME: &'static str;
}

/// A method of a protocol.
pub trait Method {
    /// The ordinal associated with the method;
    const ORDINAL: u64;

    /// The protocol the method is a member of.
    type Protocol;

    /// The request payload for the method.
    type Request;

    /// The response payload for the method.
    type Response: Constrained;
}

/// The request or response type of a method which does not have a request or
/// response.
pub enum Never {}

impl Unconstrained for Never {}
