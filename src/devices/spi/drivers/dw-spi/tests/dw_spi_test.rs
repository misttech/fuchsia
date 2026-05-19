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
}
