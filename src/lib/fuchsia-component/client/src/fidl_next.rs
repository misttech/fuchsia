// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementations for new Rust bindings.

use fidl_next::{Client, Discoverable, ServerEnd};

use super::{Error, SVC_DIR, connect_channel_to_protocol_at_path};

/// Connect to a FIDL protocol using the provided server end and namespace
/// prefix.
pub fn connect_server_end_to_protocol_at<P: Discoverable>(
    server_end: ServerEnd<P>,
    service_directory_path: &str,
) -> Result<(), Error> {
    let protocol_path = format!("{}/{}", service_directory_path, P::PROTOCOL_NAME);
    connect_channel_to_protocol_at_path(server_end.into_untyped(), &protocol_path)
}

/// Connect to a FIDL protocol using the provided namespace prefix.
pub fn connect_to_protocol_at<P: Discoverable>(service_prefix: &str) -> Result<Client<P>, Error> {
    let (client_end, server_end) = fidl_next::fuchsia::create_channel();
    let () = connect_server_end_to_protocol_at(server_end, service_prefix)?;
    Ok(Client::new(client_end))
}

/// Connect to a FIDL protocol in the `/svc` directory of the application's root
/// namespace.
pub fn connect_to_protocol<P: Discoverable>() -> Result<Client<P>, Error> {
    connect_to_protocol_at(SVC_DIR)
}
