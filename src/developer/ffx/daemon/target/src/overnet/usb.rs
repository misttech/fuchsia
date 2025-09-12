// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::channel::mpsc::unbounded;
use futures::{AsyncReadExt, StreamExt};
use netext::TokioAsyncReadExt;
use std::sync::Arc;

use super::vsock::OVERNET_VSOCK_PORT;

/// Host-pipe-like task for communicating via vsock.
pub async fn spawn_usb(
    driver: Arc<usb_driver_api::Driver>,
    cid: u32,
    node: Arc<overnet_core::Router>,
) {
    log::debug!(cid; "Spawning USB VSOCK host pipe");

    let socket = match driver.connect(cid, OVERNET_VSOCK_PORT).await {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!(cid, error:?; "Could not connect to USB VSOCK");
            return;
        }
    };

    log::debug!(cid; "USB VSOCK connection established");

    let (error_sender, mut error_receiver) = unbounded();

    let (mut socket_reader, mut socket_writer) = socket.into_multithreaded_futures_stream().split();

    let conn = circuit::multi_stream::multi_stream_node_connection_to_async(
        node.circuit_node(),
        &mut socket_reader,
        &mut socket_writer,
        false,
        circuit::Quality::LOCAL_SOCKET,
        error_sender,
        format!("USB VSOCK connection cid:{cid}"),
    );

    let conn = async move {
        if let Err(error) = conn.await {
            log::warn!(cid, error:?; "USB VSOCK Connection failed");
        }
    };

    let error_logger = {
        async move {
            while let Some(error) = error_receiver.next().await {
                log::debug!(vsock_cid = cid, error:?; "Stream encountered an error");
            }
        }
    };

    futures::join!(conn, error_logger);
}
