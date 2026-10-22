// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use fidl::endpoints::ServerEnd;
use fidl::handle::AsyncChannel;
use fidl::prelude::*;
use fidl_fuchsia_process_lifecycle as flifecycle;
use fuchsia_component::runtime::{Capability, Data, DataValue, Dictionary};
use fuchsia_runtime::{HandleInfo, HandleType};
use futures_util::stream::TryStreamExt;
use log::{error, info};
use std::process;

// [START imports]
use fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream};
// [END imports]

/// This function is unused because we are only using it for documentation, so we just need to know
/// that it compiles.
async fn _escrow_example() -> Result<(), anyhow::Error> {
    // [START escrow_listening]
    let lifecycle =
        fuchsia_runtime::take_startup_handle(HandleInfo::new(HandleType::Lifecycle, 0)).unwrap();
    let lifecycle: zx::Channel = lifecycle.into();
    let lifecycle: ServerEnd<flifecycle::LifecycleMarker> = lifecycle.into();
    let (lifecycle_request_stream, lifecycle_control_handle) =
        lifecycle.into_stream_and_control_handle();

    let outgoing_dir = None;
    // Later, when `ServiceFs` has stalled and we have an `outgoing_dir`.
    lifecycle_control_handle
        .send_on_escrow(flifecycle::LifecycleOnEscrowRequest { outgoing_dir, ..Default::default() })
        .unwrap();
    // [END escrow_listening]

    let _ = lifecycle_request_stream;

    // [START escrow_create_dictionary]
    let dictionary = Dictionary::new().await;
    // [END escrow_create_dictionary]

    let outgoing_dir = None;

    // [START escrow_populate_dictionary]
    let bytes = vec![1, 2, 3];
    let data = Data::new(DataValue::Bytes(bytes)).await;
    dictionary.insert("my_data", data).await;
    // [END escrow_populate_dictionary]

    // [START escrow_send_dictionary]
    lifecycle_control_handle.send_on_escrow(flifecycle::LifecycleOnEscrowRequest {
        outgoing_dir,
        escrowed_dictionary_handle: Some(dictionary.handle),
        ..Default::default()
    })?;
    // [END escrow_send_dictionary]

    // [START escrow_receive_dictionary]
    let Some(dictionary) =
        fuchsia_runtime::take_startup_handle(HandleInfo::new(HandleType::EscrowedDictionary, 0))
    else {
        return Err(anyhow!("Couldn't find startup handle"));
    };

    let dictionary = Dictionary::from(zx::EventPair::from(dictionary));
    let Some(Capability::Data(data)) = dictionary.remove("my_data").await else {
        return Err(anyhow!("Bad capability type from dictionary"));
    };
    let DataValue::Bytes(bytes) = data.get_value().await else {
        return Err(anyhow!("Bad data value from dictionary"));
    };
    // Do something with the data...
    // [END escrow_receive_dictionary]

    let _ = bytes;
    Ok(())
}

// [START lifecycle_handler]
#[fuchsia::main(logging_tags = ["lifecycle", "example"])]
async fn main() {
    // Take the lifecycle handle provided by the runner
    match fuchsia_runtime::take_startup_handle(HandleInfo::new(HandleType::Lifecycle, 0)) {
        Some(lifecycle_handle) => {
            info!("Lifecycle channel received.");
            // Begin listening for lifecycle requests on this channel
            let x: zx::Channel = lifecycle_handle.into();
            let async_x = AsyncChannel::from(fuchsia_async::Channel::from_channel(x));
            let mut req_stream = LifecycleRequestStream::from_channel(async_x);
            info!("Awaiting request to close...");
            if let Some(request) =
                req_stream.try_next().await.expect("Failure receiving lifecycle FIDL message")
            {
                match request {
                    LifecycleRequest::Stop { control_handle: c } => {
                        info!("Received request to stop. Shutting down.");
                        c.shutdown();
                        process::exit(0);
                    }
                }
            }

            // We only arrive here if the lifecycle channel closed without
            // first sending the shutdown event, which is unexpected.
            process::abort();
        }
        None => {
            // We did not receive a lifecycle channel, exit abnormally.
            error!("No lifecycle channel received, exiting.");
            process::abort();
        }
    }
}
// [END lifecycle_handler]
