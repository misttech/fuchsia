// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitfield::bitfield;

bitfield! {
    /// Modified by both the driver and the device during lifecycle transitions.
    ///
    /// The bits are intended to represent stages in the device + driver
    /// lifecycle. After a device is reset, the value is guaranteed to be
    /// [`DeviceStatus::RESET`].
    ///
    /// With the exception of resetting the device (as described in virtio14 2.4
    /// "Device Reset"), the driver must not clear (write 0 to) a bit that is
    /// set (to 1).
    ///
    /// virtio14 2.4 "Device Reset" describes the field's behavior during a
    /// reset operation. virtio14 3 "General Initialization And Device
    /// Operation" describes the field's behavior during other operations.
    ///
    /// virtio14 2.1 "Device Status Field"
    #[derive(Default, Copy, Clone, PartialEq, Eq)]
    #[repr(transparent)]
    pub struct DeviceStatus(u8);
    impl Debug;

    /// True iff the driver recognized the hardware as a virtio device.
    ///
    /// Set to true (1) by the driver in step 2 of the process in virtio14 3.1.1
    /// "Driver Requirements: Device Initialization".
    ///
    /// virtio14 recommends that the virtualized operating system sets this bit.
    /// Fuchsia does not special-case virtio devices, so this bit is set by
    /// Fuchsia drivers.
    ///
    /// virtio14 name: ACKNOWLEDGE
    pub bool, virtio_device_detected, set_virtio_device_detected: 0;

    /// True iff the guest OS has found a driver for the device.
    ///
    /// Set to true (1) by the driver in step 3 of the process in virtio14 3.1.1
    /// "Driver Requirements: Device Initialization". Fuchsia does not
    /// special-case virtio devices, so the driver must perform this step.
    ///
    /// The driver may only read the feature bits offered by the device after
    /// setting this bit to true.
    ///
    /// virtio14 name: DRIVER
    pub bool, driver_found, set_driver_found: 1;

    /// True iff the driver is sufficiently initialized to drive the device.
    ///
    /// Set to true (1) by the driver in step 8 of the process in virtio14 3.1.1
    /// "Driver Requirements: Device Initialization".
    ///
    /// The driver must only set this bit to true after it has completed the
    /// device-specific initialization process.
    ///
    /// The driver may only operate the device's virtqueues while this bit is
    /// set to true (1).
    ///
    /// virtio14 name: DRIVER_OK
    pub bool, driver_initialized, set_driver_initialized: 2;

    /// True iff the feature negotiation is complete.
    ///
    /// Set to true (1) by the driver in step 5 of the process in virtio14 3.1.1
    /// "Driver Requirements: Device Initialization".
    ///
    /// The driver must only set this bit to true after it has acknowledged all
    /// the features it understands.
    ///
    /// virtio14 name: FEATURES_OK
    pub bool, feature_negotiation_complete, set_feature_negotiation_complete: 3;

    /// True iff the device has been suspended by the driver.
    ///
    /// The driver triggers device suspension by setting this bit to true (1).
    /// The device sets the [`driver_initialized`] bit to false (0) after the
    /// device completes suspending.
    ///
    /// The driver triggers resuming a suspended device by setting the
    /// [`driver_initialized`] bit to true (1). The device sets this bit to
    /// false (0) after the device completes resuming.
    ///
    /// The driver is only allowed to set this bit if
    /// [`VirtioFeatureBits::suspend_enabled`] is negotiated.
    ///
    /// Suspension is described in virtio14 3.4 "Device Suspend".
    ///
    /// virtio14 name: SUSPEND
    pub bool, suspended, set_suspended: 4;

    /// True iff the device has experienced an unrecoverable error.
    ///
    /// The device sends a configuration change notification when setting this
    /// bit to true (1). The driver cannot assume that virtqueue operations will
    /// complete (or, conversely, that the operations will be dropped) while
    /// this bit is set.
    ///
    /// virtio14 name: DEVICE_NEEDS_RESET
    pub bool, device_needs_reset, set_device_needs_reset: 6;

    /// True iff the driver has experienced an unrecoverable error.
    ///
    /// The driver sets this bit when it gives up on driving the device.
    ///
    /// virtio14 name: FAILED
    pub bool, driver_terminated, set_driver_terminated: 7;
}

impl DeviceStatus {
    /// The status value that indicates the device has been reset.
    ///
    /// virtio14 2.4 "Device Reset"
    pub const RESET: DeviceStatus = DeviceStatus(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_device_status() {
        let mut status = DeviceStatus(0);
        assert!(!status.virtio_device_detected());
        status.set_virtio_device_detected(true);
        assert!(status.virtio_device_detected());
        assert_eq!(status.0, 1);
    }

    #[fuchsia::test]
    fn test_device_status_debug() {
        let mut status = DeviceStatus(0);
        status.set_virtio_device_detected(true);
        let debug_str = format!("{:?}", status);
        assert_eq!(
            debug_str,
            "DeviceStatus { .0: 1, virtio_device_detected: true, driver_found: false, driver_initialized: false, feature_negotiation_complete: false, suspended: false, device_needs_reset: false, driver_terminated: false }"
        );

        let mut status3 = DeviceStatus(0);
        status3.set_virtio_device_detected(true);
        status3.set_driver_found(true);
        status3.set_driver_initialized(true);
        let debug_str3 = format!("{:?}", status3);
        assert_eq!(
            debug_str3,
            "DeviceStatus { .0: 7, virtio_device_detected: true, driver_found: true, driver_initialized: true, feature_negotiation_complete: false, suspended: false, device_needs_reset: false, driver_terminated: false }"
        );
    }
}
