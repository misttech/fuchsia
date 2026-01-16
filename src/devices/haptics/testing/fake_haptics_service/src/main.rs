// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The fake haptics service's goal is to provide a component that offers a fake
//! fuchsia.hardware.haptics.Service` FIDL service instance.

use anyhow::Context;
use fidl_fuchsia_hardware_haptics::{
    DeviceRequest, DeviceRequestStream, Effect, ServiceRequest, SupportedEffect,
};
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use log::error;

async fn handle_device_requests(mut requests: DeviceRequestStream) -> anyhow::Result<()> {
    while let Some(request) = requests.try_next().await.context("Failed to get request")? {
        match request {
            DeviceRequest::StartVibration { responder } => {
                responder.send(Ok(())).context("Failed to send response")?;
            }
            DeviceRequest::PlayVibration { duration: _, responder } => {
                responder.send(Ok(())).context("Failed to send response")?;
            }
            DeviceRequest::PlayEffect { effect: _, strength: _, responder } => {
                responder.send(Ok(())).context("Failed to send response")?;
            }
            DeviceRequest::StopVibration { responder } => {
                responder.send(Ok(())).context("Failed to send response")?;
            }
            DeviceRequest::SetAmplitude { amplitude, responder } => {
                if amplitude <= 0.0 || amplitude > 1.0 {
                    responder
                        .send(Err(zx::Status::INVALID_ARGS.into_raw()))
                        .context("Failed to send response")?;
                    continue;
                }

                responder.send(Ok(())).context("Failed to send response")?;
            }
            DeviceRequest::GetProperties { responder } => {
                // Value must be greater than 0 in order to pass tests.
                const FUNDAMENTAL_RESONANT_FREQUENCY_HZ: f32 = 123.0;

                // Value must be greater than 0 in order to pass tests.
                const QUALITY_FACTOR: f32 = 789.0;

                const SUPPORTED_EFFECTS: [SupportedEffect; 22] = [
                    SupportedEffect { effect: Effect::Click, duration: 0 },
                    SupportedEffect { effect: Effect::DoubleClick, duration: 0 },
                    SupportedEffect { effect: Effect::Tick, duration: 0 },
                    SupportedEffect { effect: Effect::Thud, duration: 0 },
                    SupportedEffect { effect: Effect::Pop, duration: 0 },
                    SupportedEffect { effect: Effect::HeavyClick, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone1, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone2, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone3, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone4, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone5, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone6, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone7, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone8, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone9, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone10, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone11, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone12, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone13, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone14, duration: 0 },
                    SupportedEffect { effect: Effect::Ringtone15, duration: 0 },
                    SupportedEffect { effect: Effect::TextureTick, duration: 0 },
                ];

                responder
                    .send(Ok((
                        FUNDAMENTAL_RESONANT_FREQUENCY_HZ,
                        QUALITY_FACTOR,
                        &SUPPORTED_EFFECTS,
                    )))
                    .context("Failed to send response")?;
            }
            DeviceRequest::_UnknownMethod { ordinal, .. } => {
                error!("Received unknown method {}", ordinal);
            }
        };
    }
    Ok(())
}

enum IncomingService {
    Haptics(ServiceRequest),
}

#[fuchsia::main]
async fn main() -> anyhow::Result<()> {
    // Initialize the outgoing services provided by this component
    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service_instance("default", IncomingService::Haptics);
    fs.take_and_serve_directory_handle()?;

    fs.for_each_concurrent(1, |IncomingService::Haptics(ServiceRequest::Device(device))| {
        handle_device_requests(device).unwrap_or_else(|e| println!("{:?}", e))
    })
    .await;

    Ok(())
}
