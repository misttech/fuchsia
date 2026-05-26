// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::DwSpiDriver;
use fake_clock::FakeClock;
use fake_gpio::FakeGpio;
use fake_pdev::FakePDev;
use fake_powerdomain::FakePowerDomain;
use fake_reset::FakeReset;
use fdf_component::testing::harness::TestHarness;
use fdf_fidl;
use fidl::Serializable;
use fidl_fuchsia_driver_metadata as fmetadata;
use fidl_fuchsia_hardware_spi_businfo as fspi_businfo;
use fidl_next_fuchsia_hardware_platform_device as fdevice;
use fidl_next_fuchsia_hardware_spiimpl as fspiimpl;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use zx::Vmo;

#[fuchsia::test]
async fn test_init() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let entries = vec![fmetadata::DictionaryEntry {
        key: "dw_spi_rx_sample_delay_ns".to_string(),
        value: fmetadata::DictionaryValue::Int64(25),
    }];
    let dict = fmetadata::Dictionary { entries: Some(entries), ..Default::default() };
    let serialized_config = fidl::persist(&dict).expect("Failed to serialize config");
    pdev.add_metadata("fuchsia.driver.metadata.Dictionary", serialized_config);

    let metadata = fspi_businfo::SpiBusMetadata {
        channels: Some(vec![fspi_businfo::SpiChannel {
            cs: Some(0),
            max_frequency_hz: Some(20_000_000),
            ..Default::default()
        }]),
        bus_id: Some(0),
        ..Default::default()
    };
    let serialized_metadata = fidl::persist(&metadata).expect("Failed to serialize metadata");
    pdev.add_metadata(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME, serialized_metadata);

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();

    let clock_bus = FakeClock::new();
    clock_bus.set_rate(200_000_000);
    let clock_regs = FakeClock::new();

    let mut reset = FakeReset::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "clock-bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "clock-registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "gpio-cs-0"))
        .set_driver_incoming(service_fs);

    let started_driver = harness.start_driver().await.expect("Failed to start driver");
    started_driver.stop_driver().await;

    assert!(powerdomain.enabled());
    assert!(clock_bus.enabled());
    assert!(clock_regs.enabled());
    assert!(reset.take_toggled());

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    let ctrlr0 = read_u32(0x0);
    assert_eq!(ctrlr0, 7);

    let ssi_enr = read_u32(0x8);
    assert_eq!(ssi_enr, 1);

    let baudr = read_u32(0x14);
    assert_eq!(baudr, 10);

    let rx_sample_dly = read_u32(0xf0);
    assert_eq!(rx_sample_dly, 5);
}

#[fuchsia::test]
async fn test_exchange_vector() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();

    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();

    let reset = FakeReset::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "clock-bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "clock-registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "gpio-cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    // Set SR.TFE and SR.RFNE to indicate that the FIFO can always be read from and written to.
    write_u32(0x28, 0x0c);

    let txdata = vec![1u8, 2, 3, 4, 5];
    let response = client.exchange_vector(0, &txdata).await;
    assert!(response.is_ok());

    let rxdata = response.unwrap().unwrap().rxdata;
    assert_eq!(rxdata.len(), txdata.len());

    // Check the last value that was written to the FIFO.
    assert_eq!(read_u32(0x60), 5);
    // The RX path should have read the last value that was written to the FIFO.
    assert_eq!(rxdata, vec![5, 5, 5, 5, 5]);

    started_driver.stop_driver().await;
}
