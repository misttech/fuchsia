// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration test suite for the `fuchsia.hardware.pci` Device protocol.
//!
//! Boots `DriverTestRealm` with `fake-bus-pci`, which exposes a single PCI device initialized
//! with the static Quadro K2200 configuration space from `test_device.h`.

use fidl_fuchsia_driver_test as fdt;
use fidl_fuchsia_hardware_pci as fpci;
use fuchsia_async as fasync;
use fuchsia_component::client::Service;
use fuchsia_component_test::{Capability, RealmBuilder, RealmInstance, Ref};
use fuchsia_driver_test::{
    DriverTestRealmBuilder2 as _, DriverTestRealmInstance2 as _, Options2 as Options,
};

// LINT.IfChange
// Constants matching driver_tests.h.
const PCI_TEST_DRIVER_VID: u16 = 0x0eff;
const PCI_TEST_DRIVER_DID: u16 = 0x0fff;
const PCI_TEST_BUS_ID: u8 = 0x00;
const PCI_TEST_DEV_ID: u8 = 0x01;
const PCI_TEST_FUNC_ID: u8 = 0x02;
// LINT.ThenChange(//src/devices/pci/drivers/pci/test/driver/driver_tests.h)

// PCIe configuration space sizing constants.
const EXTENDED_CONFIG_SIZE: u16 = 0x1000;
const CONFIG_HEADER_SIZE: u16 = 0x40;

// Test pattern boundaries for configuration space read/write verification in the upper
// half of extended configuration space (above all capabilities in kFakeQuadroDeviceConfig).
const TEST_PATTERN_START: u16 = 0x800;
const TEST_PATTERN_END: u16 = 0x1000;

// Generates a non-zero test pattern byte based on address for configuration space testing.
const fn test_pattern_value(address: u16) -> u8 {
    ((address % (u8::MAX as u16)) as u8) + 1
}

// LINT.IfChange
// Constants matching the Quadro K2200 configuration in test_device.h.
const MAX_BAR_COUNT: u32 = 6;
const BAR_0_SIZE: u64 = 16 * 1024 * 1024;
const BAR_1_SIZE: u64 = 256 * 1024 * 1024;
const BAR_2_SIZE: u64 = 1024 * 1024;
const BAR_3_SIZE: u64 = 32 * 1024 * 1024;
#[cfg(target_arch = "x86_64")]
const BAR_5_ADDRESS: u64 = 0x2000;
#[cfg(target_arch = "x86_64")]
const BAR_5_SIZE: u64 = 128;
#[cfg(not(target_arch = "x86_64"))]
const BAR_5_SIZE: u64 = 4096;

const FAKE_QUADRO_MSI_CAP_OFFSET: u16 = 0x68;
const FAKE_QUADRO_MSI_CTRL_OFFSET: u16 = FAKE_QUADRO_MSI_CAP_OFFSET + 2;
const FAKE_QUADRO_MSI_IRQ_CNT: u8 = 4;
const FAKE_QUADRO_MSIX_IRQ_CNT: u16 = 5;
// LINT.ThenChange(//src/devices/pci/drivers/pci/test/fakes/test_device.h)

struct TestFixture {
    _instance: RealmInstance,
    pci: fpci::DeviceProxy,
}

impl TestFixture {
    async fn new() -> Self {
        let builder = RealmBuilder::new().await.expect("RealmBuilder::new");
        builder
            .driver_test_realm_setup(
                Options::new()
                    .driver_offers(
                        Ref::parent(),
                        vec![
                            Capability::protocol_by_name("fuchsia.kernel.IoportResource")
                                .optional()
                                .into(),
                        ],
                    )
                    .driver_exposes(vec![Capability::service::<fpci::ServiceMarker>().into()]),
                fdt::RealmArgs {
                    root_driver: Some(
                        "fuchsia-boot:///platform-bus#meta/platform-bus.cm".to_string(),
                    ),
                    software_devices: Some(vec![fdt::SoftwareDevice {
                        device_name: "pci".to_string(),
                        device_id: 0,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .expect("driver_test_realm_setup");
        let instance = builder.build().await.expect("builder.build");
        instance.wait_for_bootup().await.expect("instance.wait_for_bootup");
        let pci = Service::open_from_dir(instance.root.get_exposed_dir(), fpci::ServiceMarker)
            .expect("Service::open_from_dir")
            .watch_for_any()
            .await
            .expect("watch_for_any")
            .connect_to_device()
            .expect("connect_to_device");
        Self { _instance: instance, pci }
    }

    async fn read_config8(&self, offset: u16) -> Result<u8, zx::Status> {
        self.pci
            .read_config8(offset)
            .await
            .expect("FIDL read_config8")
            .map_err(zx::Status::err_from_raw)
    }

    async fn read_config16(&self, offset: u16) -> Result<u16, zx::Status> {
        self.pci
            .read_config16(offset)
            .await
            .expect("FIDL read_config16")
            .map_err(zx::Status::err_from_raw)
    }

    async fn read_config32(&self, offset: u16) -> Result<u32, zx::Status> {
        self.pci
            .read_config32(offset)
            .await
            .expect("FIDL read_config32")
            .map_err(zx::Status::err_from_raw)
    }

    async fn write_config8(&self, offset: u16, value: u8) -> Result<(), zx::Status> {
        self.pci
            .write_config8(offset, value)
            .await
            .expect("FIDL write_config8")
            .map_err(zx::Status::err_from_raw)
    }

    async fn write_config16(&self, offset: u16, value: u16) -> Result<(), zx::Status> {
        self.pci
            .write_config16(offset, value)
            .await
            .expect("FIDL write_config16")
            .map_err(zx::Status::err_from_raw)
    }

    async fn write_config32(&self, offset: u16, value: u32) -> Result<(), zx::Status> {
        self.pci
            .write_config32(offset, value)
            .await
            .expect("FIDL write_config32")
            .map_err(zx::Status::err_from_raw)
    }

    async fn set_bus_mastering(&self, enable: bool) -> Result<(), zx::Status> {
        self.pci
            .set_bus_mastering(enable)
            .await
            .expect("FIDL set_bus_mastering")
            .map_err(zx::Status::err_from_raw)
    }

    async fn reset_device(&self) -> Result<(), zx::Status> {
        self.pci.reset_device().await.expect("FIDL reset_device").map_err(zx::Status::err_from_raw)
    }

    async fn get_bar(&self, bar_id: u32) -> Result<fpci::Bar, zx::Status> {
        self.pci.get_bar(bar_id).await.expect("FIDL get_bar").map_err(zx::Status::err_from_raw)
    }

    async fn get_bti(&self, index: u32) -> Result<zx::Bti, zx::Status> {
        self.pci.get_bti(index).await.expect("FIDL get_bti").map_err(zx::Status::err_from_raw)
    }

    async fn get_device_info(&self) -> fpci::DeviceInfo {
        self.pci.get_device_info().await.expect("FIDL get_device_info")
    }

    async fn get_capabilities(&self, id: fpci::CapabilityId) -> Vec<u8> {
        self.pci.get_capabilities(id).await.expect("FIDL get_capabilities")
    }

    async fn get_extended_capabilities(&self, id: fpci::ExtendedCapabilityId) -> Vec<u16> {
        self.pci.get_extended_capabilities(id).await.expect("FIDL get_extended_capabilities")
    }

    async fn get_interrupt_modes(&self) -> fpci::InterruptModes {
        self.pci.get_interrupt_modes().await.expect("FIDL get_interrupt_modes")
    }

    async fn set_interrupt_mode(
        &self,
        mode: fpci::InterruptMode,
        requested_irq_count: u32,
    ) -> Result<(), zx::Status> {
        self.pci
            .set_interrupt_mode(mode, requested_irq_count)
            .await
            .expect("FIDL set_interrupt_mode")
            .map_err(zx::Status::err_from_raw)
    }

    async fn map_interrupt(&self, which_irq: u32) -> Result<zx::Interrupt, zx::Status> {
        self.pci
            .map_interrupt(which_irq)
            .await
            .expect("FIDL map_interrupt")
            .map_err(zx::Status::err_from_raw)
    }

    async fn ack_interrupt(&self) -> Result<(), zx::Status> {
        self.pci
            .ack_interrupt()
            .await
            .expect("FIDL ack_interrupt")
            .map_err(zx::Status::err_from_raw)
    }
}

// Device reset is not supported and returns ZX_ERR_NOT_SUPPORTED.
#[fuchsia::test]
async fn reset_device_unsupported() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.reset_device().await, Err(zx::Status::NOT_SUPPORTED));
}

// Reads the Vendor and Device IDs from the Type 0 configuration header.
#[fuchsia::test]
async fn read_config_header() {
    let fixture = TestFixture::new().await;
    assert_eq!(
        fixture.read_config16(fpci::Config::VendorId.into_primitive()).await,
        Ok(PCI_TEST_DRIVER_VID)
    );
    assert_eq!(
        fixture.read_config16(fpci::Config::DeviceId.into_primitive()).await,
        Ok(PCI_TEST_DRIVER_DID)
    );
}

// Verifies configuration space bounds and header write protection:
// 1. Reads and writes at or beyond extended configuration space (0x1000) return
//    ZX_ERR_OUT_OF_RANGE.
// 2. Writes to the 64-byte Type 0 configuration header (0x00..0x40) return
//    ZX_ERR_ACCESS_DENIED so drivers cannot mutate header registers directly.
#[fuchsia::test]
async fn config_bounds() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.read_config8(EXTENDED_CONFIG_SIZE).await, Err(zx::Status::OUT_OF_RANGE));
    assert_eq!(fixture.read_config16(EXTENDED_CONFIG_SIZE).await, Err(zx::Status::OUT_OF_RANGE));
    assert_eq!(fixture.read_config32(EXTENDED_CONFIG_SIZE).await, Err(zx::Status::OUT_OF_RANGE));
    assert_eq!(
        fixture.write_config8(EXTENDED_CONFIG_SIZE, u8::MAX).await,
        Err(zx::Status::OUT_OF_RANGE)
    );
    assert_eq!(
        fixture.write_config16(EXTENDED_CONFIG_SIZE, u16::MAX).await,
        Err(zx::Status::OUT_OF_RANGE)
    );
    assert_eq!(
        fixture.write_config32(EXTENDED_CONFIG_SIZE, u32::MAX).await,
        Err(zx::Status::OUT_OF_RANGE)
    );

    for addr in 0..CONFIG_HEADER_SIZE {
        assert_eq!(fixture.write_config8(addr, u8::MAX).await, Err(zx::Status::ACCESS_DENIED));
        assert_eq!(fixture.write_config16(addr, u16::MAX).await, Err(zx::Status::ACCESS_DENIED));
        assert_eq!(fixture.write_config32(addr, u32::MAX).await, Err(zx::Status::ACCESS_DENIED));
    }
}

// Verifies 8-, 16-, and 32-bit configuration reads and writes across the upper half of
// extended configuration space (0x800..0x1000).
#[fuchsia::test]
async fn config_pattern_8() {
    let fixture = TestFixture::new().await;
    for addr in TEST_PATTERN_START..TEST_PATTERN_END {
        assert_eq!(fixture.write_config8(addr, 0).await, Ok(()));
    }
    for addr in TEST_PATTERN_START..TEST_PATTERN_END {
        assert_eq!(fixture.read_config8(addr).await, Ok(0));
    }
    for addr in TEST_PATTERN_START..TEST_PATTERN_END {
        assert_eq!(fixture.write_config8(addr, test_pattern_value(addr)).await, Ok(()));
    }
    for addr in TEST_PATTERN_START..TEST_PATTERN_END {
        assert_eq!(fixture.read_config8(addr).await, Ok(test_pattern_value(addr)));
    }
}

// 16-bit configuration pattern test across 0x800..0x1000.
#[fuchsia::test]
async fn config_pattern_16() {
    let fixture = TestFixture::new().await;
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 1).step_by(2) {
        assert_eq!(fixture.write_config16(addr, 0).await, Ok(()));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 1).step_by(2) {
        assert_eq!(fixture.read_config16(addr).await, Ok(0));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 1).step_by(2) {
        let pattern =
            (u16::from(test_pattern_value(addr + 1)) << 8) | u16::from(test_pattern_value(addr));
        assert_eq!(fixture.write_config16(addr, pattern).await, Ok(()));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 1).step_by(2) {
        let pattern =
            (u16::from(test_pattern_value(addr + 1)) << 8) | u16::from(test_pattern_value(addr));
        assert_eq!(fixture.read_config16(addr).await, Ok(pattern));
    }
}

// 32-bit configuration pattern test across 0x800..0x1000.
#[fuchsia::test]
async fn config_pattern_32() {
    let fixture = TestFixture::new().await;
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 3).step_by(4) {
        assert_eq!(fixture.write_config32(addr, 0).await, Ok(()));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 3).step_by(4) {
        assert_eq!(fixture.read_config32(addr).await, Ok(0));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 3).step_by(4) {
        let pattern = (u32::from(test_pattern_value(addr + 3)) << 24)
            | (u32::from(test_pattern_value(addr + 2)) << 16)
            | (u32::from(test_pattern_value(addr + 1)) << 8)
            | u32::from(test_pattern_value(addr));
        assert_eq!(fixture.write_config32(addr, pattern).await, Ok(()));
    }
    for addr in (TEST_PATTERN_START..TEST_PATTERN_END - 3).step_by(4) {
        let pattern = (u32::from(test_pattern_value(addr + 3)) << 24)
            | (u32::from(test_pattern_value(addr + 2)) << 16)
            | (u32::from(test_pattern_value(addr + 1)) << 8)
            | u32::from(test_pattern_value(addr));
        assert_eq!(fixture.read_config32(addr).await, Ok(pattern));
    }
}

// Toggles the Bus Master Enable bit in the Command register while preserving other bits.
#[fuchsia::test]
async fn set_bus_mastering() {
    let fixture = TestFixture::new().await;
    let cached = fixture.read_config16(fpci::Config::Command.into_primitive()).await.unwrap();
    assert_eq!(cached & fpci::Command::BUS_MASTER_EN.bits(), fpci::Command::BUS_MASTER_EN.bits());

    assert_eq!(fixture.set_bus_mastering(false).await, Ok(()));
    let val = fixture.read_config16(fpci::Config::Command.into_primitive()).await.unwrap();
    assert_eq!(val & fpci::Command::BUS_MASTER_EN.bits(), 0);
    assert_eq!(cached & !fpci::Command::BUS_MASTER_EN.bits(), val);

    assert_eq!(fixture.set_bus_mastering(true).await, Ok(()));
    let val = fixture.read_config16(fpci::Config::Command.into_primitive()).await.unwrap();
    assert_eq!(val & fpci::Command::BUS_MASTER_EN.bits(), fpci::Command::BUS_MASTER_EN.bits());
    assert_eq!(cached, val);
}

// Helper to verify that GetBar returns an MMIO VMO matching the expected size.
async fn get_bar_test_helper(fixture: &TestFixture, bar_id: u32, expected_size: u64) {
    let info = fixture.get_bar(bar_id).await.unwrap();
    assert_eq!(info.bar_id, bar_id);
    assert_eq!(info.size, expected_size);
    match info.result {
        fpci::BarResult::Vmo(vmo) => {
            let size = vmo.get_size().expect("vmo get_size");
            assert_eq!(size, expected_size);
        }
        _ => panic!("expected Vmo for BAR {bar_id}"),
    }
}

// Rejects BAR indices at or above MAX_BAR_COUNT with ZX_ERR_INVALID_ARGS.
#[fuchsia::test]
async fn get_bar_argument_check() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.get_bar(MAX_BAR_COUNT).await, Err(zx::Status::INVALID_ARGS));
}

// BAR 0: 32-bit non-prefetchable MMIO (16 MiB).
#[fuchsia::test]
async fn get_bar_0() {
    let fixture = TestFixture::new().await;
    get_bar_test_helper(&fixture, 0, BAR_0_SIZE).await;
}

// BAR 1: 32-bit prefetchable MMIO (256 MiB).
#[fuchsia::test]
async fn get_bar_1() {
    let fixture = TestFixture::new().await;
    get_bar_test_helper(&fixture, 1, BAR_1_SIZE).await;
}

// BAR 2: 32-bit MMIO backing the MSI-X table and PBA (1 MiB).
#[fuchsia::test]
async fn get_bar_2() {
    let fixture = TestFixture::new().await;
    get_bar_test_helper(&fixture, 2, BAR_2_SIZE).await;
}

// BAR 3: 64-bit prefetchable MMIO spanning slots 3 and 4 (32 MiB).
#[fuchsia::test]
async fn get_bar_3() {
    let fixture = TestFixture::new().await;
    get_bar_test_helper(&fixture, 3, BAR_3_SIZE).await;
}

// BAR 4: upper 32 bits of 64-bit BAR 3, so fetching it directly returns ZX_ERR_NOT_FOUND.
#[fuchsia::test]
async fn get_bar_4() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.get_bar(4).await, Err(zx::Status::NOT_FOUND));
}

// BAR 5: 128-byte I/O port BAR at 0x2000 (returns an IOPORT resource on x86_64 and an MMIO VMO
// on other architectures).
#[fuchsia::test]
async fn get_bar_5() {
    let fixture = TestFixture::new().await;
    let info = fixture.get_bar(5).await.unwrap();
    assert_eq!(info.bar_id, 5);
    assert_eq!(info.size, BAR_5_SIZE);
    #[cfg(target_arch = "x86_64")]
    match info.result {
        fpci::BarResult::Io(io) => {
            assert_eq!(io.address, BAR_5_ADDRESS);
            let resource_info = io.resource.info().expect("resource info");
            assert_eq!(resource_info.kind, zx::sys::ZX_RSRC_KIND_IOPORT);
        }
        other => panic!("expected IoBar for BAR 5 on x86_64, got {other:?}"),
    }
    #[cfg(not(target_arch = "x86_64"))]
    match info.result {
        fpci::BarResult::Vmo(vmo) => {
            let size = vmo.get_size().expect("vmo get_size");
            assert!(size >= u64::from(zx::system_get_page_size()));
        }
        other => panic!("expected Vmo for BAR 5 on non-x86_64, got {other:?}"),
    }
}

// Walks the standard capability linked list and cross-checks each offset against its capability
// ID byte in config space.
#[fuchsia::test]
async fn get_capabilities() {
    let fixture = TestFixture::new().await;
    let offsets = fixture.get_capabilities(fpci::CapabilityId::PciPwrMgmt).await;
    assert_eq!(offsets, vec![0x60]);
    assert_eq!(
        fixture.read_config8(u16::from(offsets[0])).await,
        Ok(fpci::CapabilityId::PciPwrMgmt.into_primitive())
    );

    let offsets = fixture.get_capabilities(fpci::CapabilityId::Msi).await;
    assert_eq!(offsets, vec![0x68]);
    assert_eq!(
        fixture.read_config8(u16::from(offsets[0])).await,
        Ok(fpci::CapabilityId::Msi.into_primitive())
    );

    let offsets = fixture.get_capabilities(fpci::CapabilityId::PciExpress).await;
    assert_eq!(offsets, vec![0x78]);
    assert_eq!(
        fixture.read_config8(u16::from(offsets[0])).await,
        Ok(fpci::CapabilityId::PciExpress.into_primitive())
    );

    let offsets = fixture.get_capabilities(fpci::CapabilityId::Vendor).await;
    assert_eq!(offsets, vec![0xC4, 0xC8, 0xD0, 0xE8]);
    for offset in offsets {
        assert_eq!(
            fixture.read_config8(u16::from(offset)).await,
            Ok(fpci::CapabilityId::Vendor.into_primitive())
        );
    }

    let offsets = fixture.get_capabilities(fpci::CapabilityId::Msix).await;
    assert_eq!(offsets, vec![0xF0]);
    assert_eq!(
        fixture.read_config8(u16::from(offsets[0])).await,
        Ok(fpci::CapabilityId::Msix.into_primitive())
    );

    let offsets = fixture.get_capabilities(fpci::CapabilityId::Agp).await;
    assert_eq!(offsets, Vec::<u8>::new());
}

// Walks the PCIe extended capability linked list and cross-checks each offset against its 16-bit
// capability ID in config space.
#[fuchsia::test]
async fn get_extended_capabilities() {
    let fixture = TestFixture::new().await;
    let offsets =
        fixture.get_extended_capabilities(fpci::ExtendedCapabilityId::VirtualChannelNoMfvc).await;
    assert_eq!(offsets, vec![0x100]);
    assert_eq!(
        fixture.read_config16(offsets[0]).await,
        Ok(fpci::ExtendedCapabilityId::VirtualChannelNoMfvc.into_primitive())
    );

    let offsets = fixture
        .get_extended_capabilities(fpci::ExtendedCapabilityId::LatencyToleranceReporting)
        .await;
    assert_eq!(offsets, vec![0x250]);
    assert_eq!(
        fixture.read_config16(offsets[0]).await,
        Ok(fpci::ExtendedCapabilityId::LatencyToleranceReporting.into_primitive())
    );

    let offsets =
        fixture.get_extended_capabilities(fpci::ExtendedCapabilityId::L1PmSubstates).await;
    assert_eq!(offsets, vec![0x258]);
    assert_eq!(
        fixture.read_config16(offsets[0]).await,
        Ok(fpci::ExtendedCapabilityId::L1PmSubstates.into_primitive())
    );

    let offsets =
        fixture.get_extended_capabilities(fpci::ExtendedCapabilityId::PowerBudgeting).await;
    assert_eq!(offsets, vec![0x128]);
    assert_eq!(
        fixture.read_config16(offsets[0]).await,
        Ok(fpci::ExtendedCapabilityId::PowerBudgeting.into_primitive())
    );

    let offsets = fixture.get_extended_capabilities(fpci::ExtendedCapabilityId::Vendor).await;
    assert_eq!(offsets, vec![0x600]);
    assert_eq!(
        fixture.read_config16(offsets[0]).await,
        Ok(fpci::ExtendedCapabilityId::Vendor.into_primitive())
    );

    let offsets = fixture.get_extended_capabilities(fpci::ExtendedCapabilityId::ResizableBar).await;
    assert_eq!(offsets, Vec::<u16>::new());
}

// Verifies that GetDeviceInfo matches live configuration header registers and the device BDF.
#[fuchsia::test]
async fn get_device_info() {
    let fixture = TestFixture::new().await;
    let vendor_id = fixture.read_config16(fpci::Config::VendorId.into_primitive()).await.unwrap();
    let device_id = fixture.read_config16(fpci::Config::DeviceId.into_primitive()).await.unwrap();
    let base_class =
        fixture.read_config8(fpci::Config::ClassCodeBase.into_primitive()).await.unwrap();
    let sub_class =
        fixture.read_config8(fpci::Config::ClassCodeSub.into_primitive()).await.unwrap();
    let program_interface =
        fixture.read_config8(fpci::Config::ClassCodeIntr.into_primitive()).await.unwrap();
    let revision_id =
        fixture.read_config8(fpci::Config::RevisionId.into_primitive()).await.unwrap();

    let info = fixture.get_device_info().await;
    assert_eq!(info.vendor_id, PCI_TEST_DRIVER_VID);
    assert_eq!(info.device_id, PCI_TEST_DRIVER_DID);
    assert_eq!(vendor_id, info.vendor_id);
    assert_eq!(device_id, info.device_id);
    assert_eq!(base_class, info.base_class);
    assert_eq!(sub_class, info.sub_class);
    assert_eq!(program_interface, info.program_interface);
    assert_eq!(revision_id, info.revision_id);
    assert_eq!(PCI_TEST_BUS_ID, info.bus_id);
    assert_eq!(PCI_TEST_DEV_ID, info.dev_id);
    assert_eq!(PCI_TEST_FUNC_ID, info.func_id);
}

// Verifies reported legacy, MSI, and MSI-X counts, cross-checking the MSI count against the
// Multiple Message Capable field in the MSI Control register.
#[fuchsia::test]
async fn get_interrupt_modes() {
    let fixture = TestFixture::new().await;
    let modes = fixture.get_interrupt_modes().await;
    assert!(modes.has_legacy);
    assert_eq!(modes.msi_count, FAKE_QUADRO_MSI_IRQ_CNT);

    let msi_ctrl = fixture.read_config16(FAKE_QUADRO_MSI_CTRL_OFFSET).await.unwrap();
    let mmc = (msi_ctrl >> 1) & 0b111;
    assert_eq!(1u8 << mmc, FAKE_QUADRO_MSI_IRQ_CNT);

    assert_eq!(modes.msix_count, FAKE_QUADRO_MSIX_IRQ_CNT);
}

// Verifies interrupt mode transitions (Legacy, Msi, MsiX, Disabled) and idempotent MSI
// reconfiguration when no vectors are mapped.
#[fuchsia::test]
async fn get_and_set_interrupt_mode() {
    let fixture = TestFixture::new().await;
    let modes = fixture.get_interrupt_modes().await;
    assert!(modes.has_legacy);
    assert_eq!(modes.msi_count, FAKE_QUADRO_MSI_IRQ_CNT);
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Legacy, 1).await, Ok(()));
    assert_eq!(
        fixture.set_interrupt_mode(fpci::InterruptMode::Msi, u32::from(modes.msi_count)).await,
        Ok(())
    );
    // Setting the same mode twice should work if no IRQs have been allocated off of this one.
    assert_eq!(
        fixture.set_interrupt_mode(fpci::InterruptMode::Msi, u32::from(modes.msi_count)).await,
        Ok(())
    );
    assert_eq!(
        fixture.set_interrupt_mode(fpci::InterruptMode::MsiX, u32::from(modes.msix_count)).await,
        Ok(())
    );
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await, Ok(()));
}

// Enabling MSI or MSI-X automatically enables bus mastering in the Command register.
#[fuchsia::test]
async fn msi_enables_bus_mastering() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.set_bus_mastering(false).await, Ok(()));
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Msi, 1).await, Ok(()));
    let val = fixture.read_config16(fpci::Config::Command.into_primitive()).await.unwrap();
    assert_eq!(val & fpci::Command::BUS_MASTER_EN.bits(), fpci::Command::BUS_MASTER_EN.bits());

    assert_eq!(fixture.set_bus_mastering(false).await, Ok(()));
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::MsiX, 1).await, Ok(()));
    let val = fixture.read_config16(fpci::Config::Command.into_primitive()).await.unwrap();
    assert_eq!(val & fpci::Command::BUS_MASTER_EN.bits(), fpci::Command::BUS_MASTER_EN.bits());
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await, Ok(()));
}

// AckInterrupt succeeds in Legacy mode and returns ZX_ERR_BAD_STATE in Msi, MsiX, and Disabled
// modes.
#[fuchsia::test]
async fn acking_irq_modes() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Legacy, 1).await, Ok(()));
    let interrupt = fixture.map_interrupt(0).await.unwrap();
    assert_eq!(fixture.ack_interrupt().await, Ok(()));
    drop(interrupt);

    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Msi, 1).await, Ok(()));
    assert_eq!(fixture.ack_interrupt().await, Err(zx::Status::BAD_STATE));

    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::MsiX, 1).await, Ok(()));
    assert_eq!(fixture.ack_interrupt().await, Err(zx::Status::BAD_STATE));

    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await, Ok(()));
    assert_eq!(fixture.ack_interrupt().await, Err(zx::Status::BAD_STATE));
}

// Mapped MSI-X interrupt handles block disabling interrupts (ZX_ERR_BAD_STATE) until all handles
// are closed.
#[fuchsia::test]
async fn msix() {
    let fixture = TestFixture::new().await;
    let modes = fixture.get_interrupt_modes().await;
    assert_eq!(modes.msix_count, FAKE_QUADRO_MSIX_IRQ_CNT);
    assert_eq!(
        fixture.set_interrupt_mode(fpci::InterruptMode::MsiX, u32::from(modes.msix_count)).await,
        Ok(())
    );
    {
        let mut ints = Vec::new();
        for i in 0..modes.msix_count {
            let interrupt = fixture.map_interrupt(u32::from(i)).await.unwrap();
            ints.push(interrupt);
        }
        assert_eq!(
            fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await,
            Err(zx::Status::BAD_STATE)
        );
    }
    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await, Ok(()));
}

// Verifies MapInterrupt behavior in MSI mode:
// 1. Mapped interrupt handles block SetInterruptMode with ZX_ERR_BAD_STATE.
// 2. Destroying a mapped handle unblocks a waiting thread with ZX_ERR_CANCELED.
// 3. Out-of-bounds vector indices return ZX_ERR_INVALID_ARGS.
// 4. Mapping an already-mapped vector returns ZX_ERR_ALREADY_BOUND until the handle is closed.
#[fuchsia::test]
async fn map_interrupt() {
    let fixture = TestFixture::new().await;
    let modes = fixture.get_interrupt_modes().await;
    assert_eq!(
        fixture.set_interrupt_mode(fpci::InterruptMode::Msi, u32::from(modes.msi_count)).await,
        Ok(())
    );

    for int_id in 0..modes.msi_count {
        let interrupt = fixture.map_interrupt(u32::from(int_id)).await.unwrap();
        assert_eq!(
            fixture.set_interrupt_mode(fpci::InterruptMode::Msi, u32::from(modes.msi_count)).await,
            Err(zx::Status::BAD_STATE)
        );

        // Concurrently wait on duplicate interrupt handle while destroying original interrupt to
        // verify waiter thread safely unblocks with ZX_ERR_CANCELED.
        let int_dup =
            interrupt.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let waiter_thrd = std::thread::spawn(move || {
            let thread = fuchsia_runtime::with_thread_self(|t| {
                t.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate thread handle")
            });
            tx.send(thread).unwrap();
            let first = int_dup.wait().map(|_| ());
            let second = int_dup.wait().map(|_| ());
            (first, second)
        });
        let waiter_thread = rx.recv().unwrap();
        let deadline = zx::MonotonicInstant::get() + zx::Duration::from_seconds(5);
        while zx::MonotonicInstant::get() < deadline {
            if waiter_thread.info().unwrap().state
                == zx::ThreadState::Blocked(zx::ThreadBlockType::Interrupt)
            {
                break;
            }
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_micros(100),
            ))
            .await;
        }
        assert_eq!(
            waiter_thread.info().unwrap().state,
            zx::ThreadState::Blocked(zx::ThreadBlockType::Interrupt)
        );
        interrupt.destroy().unwrap();
        assert_eq!(
            waiter_thrd.join().expect("join waiter thread"),
            (Err(zx::Status::CANCELED), Err(zx::Status::CANCELED))
        );
    }

    // Invalid ids
    assert_eq!(fixture.map_interrupt(u32::MAX).await, Err(zx::Status::INVALID_ARGS));
    assert_eq!(
        fixture.map_interrupt(u32::from(modes.msi_count) + 1).await,
        Err(zx::Status::INVALID_ARGS)
    );

    // Duplicate ids
    let int_0 = fixture.map_interrupt(0).await.unwrap();
    assert_eq!(fixture.map_interrupt(0).await, Err(zx::Status::ALREADY_BOUND));
    drop(int_0);

    assert_eq!(fixture.set_interrupt_mode(fpci::InterruptMode::Disabled, 0).await, Ok(()));
}

// Returns ZX_ERR_NOT_SUPPORTED when the underlying pciroot does not provide a BTI.
#[fuchsia::test]
async fn get_bti() {
    let fixture = TestFixture::new().await;
    assert_eq!(fixture.get_bti(0).await, Err(zx::Status::NOT_SUPPORTED));
}
