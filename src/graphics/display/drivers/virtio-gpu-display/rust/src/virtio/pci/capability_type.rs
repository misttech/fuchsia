// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(doc)]
use super::capabilities::PciCapabilityData;

#[cfg(doc)]
use super::common_configuration::VirtioPciCommonConfiguration;

/// Identifies the type of data stored by virtio in a PCI capability.
///
/// virtio14 4.1.4.1 "Driver Requirements: Virtio Structure PCI Capabilities"
/// states that all PCI capability structures are read-only for drivers, except
/// for structures with the type [`PCI_CONFIGURATION`].
///
/// virtio14 4.1.4 "Virtio Structure PCI Capabilities" >
/// struct virtio_pci_cap > cfg_type
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PciCapabilityType(pub u8);

impl PciCapabilityType {
    /// Points to data managed by [`VirtioPciCommonConfiguration`].
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_COMMON_CFG
    pub const COMMON_CONFIGURATION: Self = PciCapabilityType(1);

    /// Points to data managed by [`VirtioPciNotifications`].
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_NOTIFY_CFG
    pub const NOTIFICATIONS: Self = PciCapabilityType(2);

    /// Points to the device's Interrupt Status Register (ISR).
    ///
    /// Defined in virtio14 4.1.4.5 "ISR status capability".
    ///
    /// This driver does not access the ISR, because it only supports the PCI
    /// transport with Message Signalled Interrupts-Extended (MSI-X)
    /// capabilities. virtio14 4.1.4.5.2 "Driver Requirements: ISR status
    /// capability" mandates that the ISR is not accessed on devices where MSI-X
    /// is enabled.
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_ISR_CFG
    pub const INTERRUPT_STATUS_REGISTER: Self = PciCapabilityType(3);

    /// Points to a configuration area with data specific to each device type.
    ///
    /// The memory area described by the capability is exposed by
    /// [`VirtioPciDeviceBuilder::take_device_configuration()`].
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_DEVICE_CFG
    pub const DEVICE_CONFIGURATION: Self = PciCapabilityType(4);

    /// Alternative path for accessing configuration data.
    ///
    /// Defined in virtio14 4.1.4.9 "PCI configuration access capability".
    /// Not supported by this driver.
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_PCI_CFG
    ///
    /// Not supported by this driver.
    pub const PCI_CONFIGURATION: Self = PciCapabilityType(5);

    /// Points to data continuously shared between the device and the driver.
    ///
    /// Defined in virtio14 4.1.4.7 "Shared memory capability". The concept is
    /// defined in virtio14 2.10 "Shared Memory Regions". Not supported by this
    /// driver.
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_SHARED_MEMORY_CFG
    pub const SHARED_MEMORY: Self = PciCapabilityType(8);

    /// Vendor-specific information stored inline in the capability.
    ///
    /// Defined in virtio14 4.1.4.8 "Vendor data capability". Intended for
    /// vendor-specific data that facilitates debugging and reporting, and does
    /// not conflict with the functionality standardized in virtio. Not
    /// supported by this driver.
    ///
    /// virtio14 name: VIRTIO_PCI_CAP_VENDOR_CFG
    pub const VENDOR_SPECIFIC: Self = PciCapabilityType(9);
}

impl PciCapabilityType {
    /// True iff [`PciCapabilityData`] can represent the capability's data.
    pub fn has_bar_data(self) -> bool {
        match self {
            // virtio14 4.1.4.3 "Common configuration structure layout"
            Self::COMMON_CONFIGURATION => true,

            // virtio14 4.1.4.4 "Notification structure layout"
            Self::NOTIFICATIONS => true,

            // virtio14 4.1.4.5 "ISR status capability"
            Self::INTERRUPT_STATUS_REGISTER => true,

            // virtio14 4.1.4.6 "Device-specific configuration"
            Self::DEVICE_CONFIGURATION => true,

            // The capability's data is written by the driver, so we shouldn't
            // try to parse it here.
            //
            // virtio14 4.1.4.9 "PCI configuration access capability"
            Self::PCI_CONFIGURATION => false,

            // virtio14 4.1.4.7 "Shared memory capability"
            Self::SHARED_MEMORY => true,

            // virtio14 4.1.4.8 "Vendor data capability" >
            // struct virtio_pci_vndr_data
            Self::VENDOR_SPECIFIC => false,

            // Don't attempt to parse capabilities with unknown types.
            _ => false,
        }
    }
}

impl std::fmt::Debug for PciCapabilityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::COMMON_CONFIGURATION => write!(f, "COMMON_CONFIGURATION"),
            Self::NOTIFICATIONS => write!(f, "NOTIFICATIONS"),
            Self::INTERRUPT_STATUS_REGISTER => write!(f, "INTERRUPT_STATUS_REGISTER"),
            Self::DEVICE_CONFIGURATION => write!(f, "DEVICE_CONFIGURATION"),
            Self::PCI_CONFIGURATION => write!(f, "PCI_CONFIGURATION"),
            Self::SHARED_MEMORY => write!(f, "SHARED_MEMORY"),
            Self::VENDOR_SPECIFIC => write!(f, "VENDOR_SPECIFIC"),
            _ => write!(f, "UnknownPciCapabilityType({})", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pci_capability_type_debug() {
        assert_eq!(format!("{:?}", PciCapabilityType::PCI_CONFIGURATION), "PCI_CONFIGURATION");
        assert_eq!(format!("{:?}", PciCapabilityType(100)), "UnknownPciCapabilityType(100)");
    }
}
