// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_spi as fspi;
use fuchsia_component::client::{connect_to_service_instance, open_service_at};
use fuchsia_fs::directory::{WatchEvent, Watcher};
use futures::StreamExt;
use spi_integration_test_config::Config;

/// Discovers all available SPI devices using TestService.
async fn discover_devices(expected_count: usize) -> Result<Vec<fspi::DeviceProxy>> {
    if expected_count == 0 {
        return Ok(vec![]);
    }

    let service_directory = open_service_at(fspi::TestServiceMarker::SERVICE_NAME)
        .context("Failed to open service directory")?;
    let mut watcher = Watcher::new(&service_directory).await.context("Failed to create watcher")?;
    let mut device_names = Vec::new();

    while let Some(message) = watcher.next().await {
        let message = message.context("Watcher error")?;
        match message.event {
            WatchEvent::EXISTING | WatchEvent::ADD_FILE => {
                let filename = message.filename.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Invalid UTF-8 in filename: {:?}", message.filename)
                })?;
                if filename != "." && filename != ".." {
                    device_names.push(filename.to_string());
                }
            }
            _ => {}
        }

        if device_names.len() >= expected_count {
            break;
        }
    }

    let futures = device_names.into_iter().map(|name| async move {
        connect_to_device_by_name(&name)
            .await
            .with_context(|| format!("Failed to connect to device {}", name))
    });
    let devices = futures::future::try_join_all(futures).await?;
    Ok(devices)
}

/// Connects to a SPI device by its service instance name.
async fn connect_to_device_by_name(name: &str) -> Result<fspi::DeviceProxy> {
    let service_proxy = connect_to_service_instance::<fspi::TestServiceMarker>(name)
        .context("Failed to connect to TestService instance")?;
    let test_proxy =
        service_proxy.connect_to_test().context("Failed to connect to test protocol")?;

    let (device_client, device_server) = fidl::endpoints::create_proxy::<fspi::DeviceMarker>();
    test_proxy
        .connect_spi_loopback(device_server)
        .await
        .context("ConnectSpiLoopback FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("ConnectSpiLoopback failed: {:?}", status))?;

    Ok(device_client)
}

async fn run_with_devices<F, Fut>(test_func: F) -> Result<()>
where
    F: Fn(fspi::DeviceProxy) -> Fut + Sync + Send,
    Fut: futures::Future<Output = Result<()>> + Send,
{
    static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    let expected_count =
        CONFIG.get_or_init(|| Config::take_from_startup_handle()).spi_bus_count as usize;

    let devices = discover_devices(expected_count).await?;
    assert_eq!(
        devices.len(),
        expected_count,
        "Expected {} SPI devices, found {}",
        expected_count,
        devices.len()
    );

    let test_func = &test_func;
    let futures = devices.into_iter().enumerate().map(|(idx, device)| async move {
        test_func(device).await.with_context(|| format!("Test failed for device at index {}", idx))
    });

    let results = futures::future::join_all(futures).await;

    for (idx, result) in results.iter().enumerate() {
        assert!(result.is_ok(), "Test failed for device at index {}: {:?}", idx, result);
    }

    Ok(())
}

// Helper macro to define a test case that runs for every available SPI bus.
macro_rules! spi_test {
    ($name:ident, $device:ident, $body:block) => {
        #[fuchsia::test]
        async fn $name() -> Result<()> {
            run_with_devices(|$device| async move { $body }).await
        }
    };
}

spi_test!(test_can_assert_cs, device, {
    let can = device.can_assert_cs().await.context("CanAssertCs FIDL call failed")?;
    println!("CanAssertCs returned: {:?}", can);
    Ok(())
});
