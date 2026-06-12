// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! PCI concepts that are not specific to virtio.

use fidl_next_fuchsia_hardware_pci as fidl_pci;
use zx::Status;

/// Maps a subset of a PCI device's BARs to VMOs covering the regions.
///
/// Each PCI Base Address Register (BAR) points to a memory region. In turn,
/// each BAR's memory region is mapped to a VMO.
///
/// Instances are obtained from [`PciDeviceBarMapBuilder`].
///
/// pci3 6.1 "Configuration Space Organization" describes the location of
/// Base Address Registers (BARs) in the PCI configuration space. pci3 6.2.5
/// "Base Addresses" describes the semantics.
pub struct PciDeviceBarMap {
    /// [`zx::Vmo`] is not wrapped in [`Option<>`] because it is nullable.
    ///
    /// Invalid VMOs represent unmapped BARs.
    bar_vmos: [zx::Vmo; PciDeviceBarMap::BAR_COUNT as usize],
}

impl PciDeviceBarMap {
    /// The number of BARs in a PCI device's configuration space.
    ///
    /// Limit stated in pci3 6.1 "Configuration Space Organization".
    pub const BAR_COUNT: u8 = 6;

    /// True iff the input is valid for [`get_vmo()`].
    pub fn is_valid_bar_index(bar_index: u8) -> bool {
        bar_index < Self::BAR_COUNT
    }

    /// Returns the VMO previously mapped to a BAR's memory region.
    ///
    /// `bar_index` must have a VMO mapping, which implies it must be less than
    /// [`BAR_COUNT`].
    pub fn get_vmo(&self, bar_index: u8) -> &zx::Vmo {
        debug_assert!(Self::is_valid_bar_index(bar_index), "BAR index too large: {}", bar_index);

        let bar_vmo: &zx::Vmo = &self.bar_vmos[bar_index as usize];
        debug_assert!(!bar_vmo.is_invalid(), "BAR index not mapped to a VMO");
        bar_vmo
    }
}

/// Builder pattern instantiation for [`PciDeviceBarMap`].
pub struct PciDeviceBarMapBuilder<'a> {
    bar_vmos: [zx::Vmo; PciDeviceBarMap::BAR_COUNT as usize],
    pci: &'a fidl_next::Client<fidl_pci::Device>,
}

impl<'a> PciDeviceBarMapBuilder<'a> {
    /// Returns a builder with no mappings.
    ///
    /// [`ensure_bar_memory_region_is_mapped()`] creates mappings.
    ///
    /// `pci` is used to retrieve VMOs for BARs.
    pub fn new(pci: &'a fidl_next::Client<fidl_pci::Device>) -> Self {
        Self {
            pci,
            bar_vmos: [
                zx::Vmo::invalid(),
                zx::Vmo::invalid(),
                zx::Vmo::invalid(),
                zx::Vmo::invalid(),
                zx::Vmo::invalid(),
                zx::Vmo::invalid(),
            ],
        }
    }

    pub fn build(self) -> PciDeviceBarMap {
        PciDeviceBarMap { bar_vmos: self.bar_vmos }
    }

    /// Retrieves the VMO for a BAR memory region, if necessary.
    ///
    /// `bar_index` must be less than [`PciDeviceBarMap::BAR_COUNT`].
    ///
    /// Errors out with [`Status::INTERNAL`] if the underlying PCI bus driver
    /// call fails.
    pub async fn ensure_bar_memory_region_is_mapped(
        &mut self,
        bar_index: u8,
    ) -> Result<(), Status> {
        if self.bar_memory_region_is_mapped(bar_index) {
            return Ok(());
        }
        self.map_bar_memory_region(bar_index).await
    }

    /// True iff there is a mapping for a BAR's memory region.
    fn bar_memory_region_is_mapped(&self, bar_index: u8) -> bool {
        debug_assert!(
            PciDeviceBarMap::is_valid_bar_index(bar_index),
            "BAR index too large: {}",
            bar_index
        );

        !self.bar_vmos[bar_index as usize].is_invalid()
    }

    /// Retrieves the PCI BAR region VMO at the given index.
    ///
    /// `bar_index` must be less than [`PciDeviceBarMap::BAR_COUNT`],
    /// and must not have an existing mapping.
    ///
    /// Errors out with [`Status::INTERNAL`] if the underlying PCI bus driver
    /// call fails.
    async fn map_bar_memory_region(&mut self, bar_index: u8) -> Result<(), Status> {
        debug_assert!(
            !self.bar_memory_region_is_mapped(bar_index),
            "BAR {}'s memory region was already mapped to a VMO",
            bar_index
        );

        // TODO(https://fxbug.dev/523333960): Refine error handling for PCI FIDL calls.

        let bar_info = self
            .pci
            .get_bar(bar_index as u32)
            .await
            .map_err(|_| Status::INTERNAL)?
            .map_err(|_| Status::INTERNAL)?;

        self.bar_vmos[bar_index as usize] = match bar_info.result.result {
            fidl_pci::BarResult::Vmo(vmo) => vmo,
            _ => return Err(Status::INTERNAL),
        };
        debug_assert!(
            self.bar_memory_region_is_mapped(bar_index),
            "GetBar() returned an invalid VMO"
        );

        Ok(())
    }
}
