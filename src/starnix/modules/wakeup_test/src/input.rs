// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow};
use fuchsia_component::client::connect_to_protocol;
use starnix_logging::{log_error, log_info};
use {fidl, fidl_fuchsia_ui_test_input as futinput};

pub(crate) async fn create_media_buttons_proxy() -> Result<futinput::MediaButtonsDeviceProxy> {
    // Create a proxy to the MediaButtonsDevice to send button press events.
    let registry_result = connect_to_protocol::<futinput::RegistryMarker>();

    match registry_result {
        Ok(registry) => {
            let (key_client, key_server) =
                fidl::endpoints::create_proxy::<futinput::MediaButtonsDeviceMarker>();

            let register_res = registry
                .register_media_buttons_device(
                    futinput::RegistryRegisterMediaButtonsDeviceRequest {
                        device: Some(key_server),
                        ..Default::default()
                    },
                )
                .await;
            match register_res {
                Ok(_) => Ok(key_client),
                Err(e) => {
                    Err(anyhow!("Uinput could not register Keyboard device to Registry: {e}"))
                }
            }
        }
        Err(e) => Err(anyhow!("Could not get registry proxy: {e}")),
    }
}

pub(crate) async fn wakeup_send_power_button(
    media_button_proxy: &futinput::MediaButtonsDeviceProxy,
) {
    log_info!("WakeupTestDevice::wakeup_send_power_button called.");

    if let Err(e) = media_button_proxy
        .simulate_button_press(&futinput::MediaButtonsDeviceSimulateButtonPressRequest {
            button: Some(fidl_fuchsia_input_report::ConsumerControlButton::Power),
            ..Default::default()
        })
        .await
    {
        log_error!("failed to send power press event: {:?}", e);
    } else {
        log_info!("successfully sent power press event");
    }
}
