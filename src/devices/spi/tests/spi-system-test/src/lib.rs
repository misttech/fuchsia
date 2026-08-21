// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_sharedmemory as fsharedmemory;
use fidl_fuchsia_hardware_spi as fspi;
use fidl_fuchsia_mem as fmem;
use fuchsia_component::client::{connect_to_service_instance, open_service_at};
use fuchsia_fs::directory::{WatchEvent, Watcher};
use futures::StreamExt;
use rand::Rng;
use spi_system_test_config::Config;
use zx::Status;

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

spi_test!(test_transmit_vector, device, {
    const BUFFER_SIZE: usize = 512;

    let mut txdata = vec![0u8; BUFFER_SIZE];
    rand::rng().fill(&mut txdata[..]);

    let status =
        device.transmit_vector(&txdata).await.context("TransmitVector FIDL call failed")?;

    assert_eq!(Status::from_raw(status), Status::OK);
    Ok(())
});

spi_test!(test_receive_vector, device, {
    const BUFFER_SIZE: usize = 512;

    let (status, rxdata) = device
        .receive_vector(BUFFER_SIZE as u32)
        .await
        .context("ReceiveVector FIDL call failed")?;

    assert_eq!(Status::from_raw(status), Status::OK);
    assert_eq!(rxdata.len(), BUFFER_SIZE);
    Ok(())
});

spi_test!(test_exchange_vector, device, {
    const BUFFER_SIZE: usize = 512;

    let mut txdata = vec![0u8; BUFFER_SIZE];
    rand::rng().fill(&mut txdata[..]);

    let (status, rxdata) =
        device.exchange_vector(&txdata).await.context("ExchangeVector FIDL call failed")?;

    assert_eq!(Status::from_raw(status), Status::OK);
    assert_eq!(txdata, rxdata);
    Ok(())
});

spi_test!(test_exchange_vector_multiple, device, {
    const BUFFER_SIZE: usize = 512;
    const CONCURRENT_REQUESTS: usize = 10;

    let mut futures = Vec::new();
    for _ in 0..CONCURRENT_REQUESTS {
        let mut txdata = vec![0u8; BUFFER_SIZE];
        rand::rng().fill(&mut txdata[..]);
        let device_clone = device.clone();

        futures.push(async move {
            let (status, rxdata) = device_clone
                .exchange_vector(&txdata)
                .await
                .context("ExchangeVector FIDL call failed")?;
            Ok::<_, anyhow::Error>((txdata, rxdata, status))
        });
    }

    let results = futures::future::try_join_all(futures).await?;
    for (txdata, rxdata, status) in results {
        assert_eq!(Status::from_raw(status), Status::OK);
        assert_eq!(txdata, rxdata);
    }
    Ok(())
});

spi_test!(test_transmit_vmo, device, {
    const BUFFER_SIZE: usize = 512;
    const VMO_ID: u32 = 1;

    let mut txdata = vec![0u8; BUFFER_SIZE];
    rand::rng().fill(&mut txdata[..]);

    let vmo = zx::Vmo::create(BUFFER_SIZE as u64).context("Failed to create VMO")?;
    vmo.write(&txdata, 0).context("Failed to write to VMO")?;

    let vmo_dup =
        vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate VMO")?;

    device
        .register_vmo(
            VMO_ID,
            fmem::Range { vmo: vmo_dup, offset: 0, size: BUFFER_SIZE as u64 },
            fsharedmemory::SharedVmoRight::READ,
        )
        .await
        .context("RegisterVmo FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("RegisterVmo failed: {:?}", Status::from_raw(status)))?;

    device
        .transmit(&fsharedmemory::SharedVmoBuffer {
            vmo_id: VMO_ID,
            offset: 0,
            size: BUFFER_SIZE as u64,
        })
        .await
        .context("Transmit FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("Transmit failed: {:?}", Status::from_raw(status)))?;

    let _unregistered_vmo =
        device.unregister_vmo(VMO_ID).await.context("UnregisterVmo FIDL call failed")?.map_err(
            |status| anyhow::anyhow!("UnregisterVmo failed: {:?}", Status::from_raw(status)),
        )?;

    Ok(())
});

spi_test!(test_receive_vmo, device, {
    const BUFFER_SIZE: usize = 512;
    const VMO_ID: u32 = 1;

    let vmo = zx::Vmo::create(BUFFER_SIZE as u64).context("Failed to create VMO")?;
    let vmo_dup =
        vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate VMO")?;

    device
        .register_vmo(
            VMO_ID,
            fmem::Range { vmo: vmo_dup, offset: 0, size: BUFFER_SIZE as u64 },
            fsharedmemory::SharedVmoRight::WRITE,
        )
        .await
        .context("RegisterVmo FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("RegisterVmo failed: {:?}", Status::from_raw(status)))?;

    device
        .receive(&fsharedmemory::SharedVmoBuffer {
            vmo_id: VMO_ID,
            offset: 0,
            size: BUFFER_SIZE as u64,
        })
        .await
        .context("Receive FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("Receive failed: {:?}", Status::from_raw(status)))?;

    let _unregistered_vmo =
        device.unregister_vmo(VMO_ID).await.context("UnregisterVmo FIDL call failed")?.map_err(
            |status| anyhow::anyhow!("UnregisterVmo failed: {:?}", Status::from_raw(status)),
        )?;

    Ok(())
});

spi_test!(test_exchange_vmo, device, {
    const BUFFER_SIZE: usize = 512;
    const TX_VMO_ID: u32 = 1;
    const RX_VMO_ID: u32 = 2;

    let mut txdata = vec![0u8; BUFFER_SIZE];
    rand::rng().fill(&mut txdata[..]);

    let tx_vmo = zx::Vmo::create(BUFFER_SIZE as u64).context("Failed to create TX VMO")?;
    tx_vmo.write(&txdata, 0).context("Failed to write to TX VMO")?;
    let tx_vmo_dup =
        tx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate TX VMO")?;

    let rx_vmo = zx::Vmo::create(BUFFER_SIZE as u64).context("Failed to create RX VMO")?;
    let rx_vmo_dup =
        rx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate RX VMO")?;

    device
        .register_vmo(
            TX_VMO_ID,
            fmem::Range { vmo: tx_vmo_dup, offset: 0, size: BUFFER_SIZE as u64 },
            fsharedmemory::SharedVmoRight::READ,
        )
        .await
        .context("RegisterVmo TX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("RegisterVmo TX failed: {:?}", Status::from_raw(status))
        })?;

    device
        .register_vmo(
            RX_VMO_ID,
            fmem::Range { vmo: rx_vmo_dup, offset: 0, size: BUFFER_SIZE as u64 },
            fsharedmemory::SharedVmoRight::WRITE,
        )
        .await
        .context("RegisterVmo RX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("RegisterVmo RX failed: {:?}", Status::from_raw(status))
        })?;

    device
        .exchange(
            &fsharedmemory::SharedVmoBuffer {
                vmo_id: TX_VMO_ID,
                offset: 0,
                size: BUFFER_SIZE as u64,
            },
            &fsharedmemory::SharedVmoBuffer {
                vmo_id: RX_VMO_ID,
                offset: 0,
                size: BUFFER_SIZE as u64,
            },
        )
        .await
        .context("Exchange FIDL call failed")?
        .map_err(|status| anyhow::anyhow!("Exchange failed: {:?}", Status::from_raw(status)))?;

    let mut rxdata = vec![0u8; BUFFER_SIZE];
    rx_vmo.read(&mut rxdata, 0).context("Failed to read from RX VMO")?;
    assert_eq!(txdata, rxdata);

    let _unregistered_tx_vmo = device
        .unregister_vmo(TX_VMO_ID)
        .await
        .context("UnregisterVmo TX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("UnregisterVmo TX failed: {:?}", Status::from_raw(status))
        })?;

    let _unregistered_rx_vmo = device
        .unregister_vmo(RX_VMO_ID)
        .await
        .context("UnregisterVmo RX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("UnregisterVmo RX failed: {:?}", Status::from_raw(status))
        })?;

    Ok(())
});

spi_test!(test_exchange_vmo_multiple, device, {
    const BUFFER_SIZE: usize = 512;
    const CONCURRENT_REQUESTS: usize = 10;
    const TX_VMO_ID: u32 = 1;
    const RX_VMO_ID: u32 = 2;
    const TOTAL_SIZE: usize = BUFFER_SIZE * CONCURRENT_REQUESTS;

    let tx_vmo = zx::Vmo::create(TOTAL_SIZE as u64).context("Failed to create TX VMO")?;
    let rx_vmo = zx::Vmo::create(TOTAL_SIZE as u64).context("Failed to create RX VMO")?;

    let mut txdata_all = vec![0u8; TOTAL_SIZE];
    rand::rng().fill(&mut txdata_all[..]);
    tx_vmo.write(&txdata_all, 0).context("Failed to write to TX VMO")?;

    let tx_vmo_dup =
        tx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate TX VMO")?;
    let rx_vmo_dup =
        rx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).context("Failed to duplicate RX VMO")?;

    device
        .register_vmo(
            TX_VMO_ID,
            fmem::Range { vmo: tx_vmo_dup, offset: 0, size: TOTAL_SIZE as u64 },
            fsharedmemory::SharedVmoRight::READ,
        )
        .await
        .context("RegisterVmo TX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("RegisterVmo TX failed: {:?}", Status::from_raw(status))
        })?;

    device
        .register_vmo(
            RX_VMO_ID,
            fmem::Range { vmo: rx_vmo_dup, offset: 0, size: TOTAL_SIZE as u64 },
            fsharedmemory::SharedVmoRight::WRITE,
        )
        .await
        .context("RegisterVmo RX FIDL call failed")?
        .map_err(|status| {
            anyhow::anyhow!("RegisterVmo RX failed: {:?}", Status::from_raw(status))
        })?;

    let mut futures = Vec::new();
    for i in 0..CONCURRENT_REQUESTS {
        let offset = (i * BUFFER_SIZE) as u64;
        let device_clone = device.clone();

        futures.push(async move {
            device_clone
                .exchange(
                    &fsharedmemory::SharedVmoBuffer {
                        vmo_id: TX_VMO_ID,
                        offset,
                        size: BUFFER_SIZE as u64,
                    },
                    &fsharedmemory::SharedVmoBuffer {
                        vmo_id: RX_VMO_ID,
                        offset,
                        size: BUFFER_SIZE as u64,
                    },
                )
                .await
                .context("Exchange FIDL call failed")?
                .map_err(|status| {
                    anyhow::anyhow!("Exchange failed: {:?}", Status::from_raw(status))
                })?;
            Ok::<_, anyhow::Error>(())
        });
    }

    futures::future::try_join_all(futures).await?;

    let mut rxdata_all = vec![0u8; TOTAL_SIZE];
    rx_vmo.read(&mut rxdata_all, 0).context("Failed to read from RX VMO")?;
    assert_eq!(txdata_all, rxdata_all);

    device.unregister_vmo(TX_VMO_ID).await.context("UnregisterVmo TX FIDL call failed")?.map_err(
        |status| anyhow::anyhow!("UnregisterVmo TX failed: {:?}", Status::from_raw(status)),
    )?;

    device.unregister_vmo(RX_VMO_ID).await.context("UnregisterVmo RX FIDL call failed")?.map_err(
        |status| anyhow::anyhow!("UnregisterVmo RX failed: {:?}", Status::from_raw(status)),
    )?;

    Ok(())
});
