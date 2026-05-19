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
use fidl_next_fuchsia_hardware_platform_device as fdevice;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use zx::Vmo;

#[fuchsia::test]
async fn test_init() {
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

    let mut reset = FakeReset::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "default"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "clock-bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "clock-registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-gpio-0"))
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
    assert_eq!(baudr, 500);
}
