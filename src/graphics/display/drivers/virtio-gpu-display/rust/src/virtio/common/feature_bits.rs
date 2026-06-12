// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitfield::bitfield;

bitfield! {
    /// Configures virtio device operation.
    ///
    /// Each bit represents an optional feature or an alternative mode for an
    /// aspect of the virtio device's operation.
    ///
    /// virtio14 2.2 "Feature Bits" describes the general concept.
    /// virtio14 6 "Reserved Feature Bits" describes the feature bits applicable
    /// to all virtio devices.
    #[derive(Default, Copy, Clone, PartialEq, Eq)]
    pub struct VirtioFeatureBits(u128);
    impl Debug;

    /// The device notifies when a virtqueue has no submitted buffers.
    ///
    /// If this feature is negotiated, the device issues a virtqueue
    /// notification when it has completed all the commands submitted on the
    /// virtqueue.
    ///
    /// virtio14 name: VIRTIO_F_NOTIFY_ON_EMPTY
    pub bool, notify_on_empty_queue, set_notify_on_empty_queue: 24;

    /// Remove virtio queue memory layout constraints in legacy mode.
    ///
    /// The feature is only meaningful if [`uses_virtio1_standard`] is not
    /// negotiated. Flexible memory layout is required by the virtio 1.0+
    /// standard.
    ///
    /// If this feature or [`uses_virtio1_standard`] is negotiated, the driver
    /// does not need to support memory layout limitations in early virtio
    /// device implementations.
    ///
    /// If neither feature is negotiated, the driver must meet the constraints
    /// specified in the "Legacy Interface: Framing Requirements" sections of
    /// the relevant device type specification.
    ///
    /// This driver only supports virtio 1.0+ devices, and does not attempt to
    /// negotiate this feature.
    ///
    /// virtio14 name: VIRTIO_F_ANY_LAYOUT
    pub bool, supports_flexible_memory_layout, set_supports_flexible_memory_layout: 27;

    /// Indirect descriptors are supported.
    ///
    /// [`VirtioBufferRegionFlags::is_indirect`] may be set to true iff
    /// this feature is negotiated.
    ///
    /// Indirect descriptors are described in virtio14 2.7.5.3 "Indirect
    /// descriptors" and virtio14 2.8.7 "Indirect Flag: Scatter-Gather Support".
    ///
    /// virtio14 name: VIRTIO_F_INDIRECT_DESC
    pub bool, supports_indirect_descriptors, set_supports_indirect_descriptors: 28;

    /// Use selective virtqueue notifications.
    ///
    /// Selective notifications are described in virtio14 2.7.7 "Used Buffer
    /// Notification Suppression" and virtio14 2.7.10 "Available Buffer
    /// Notification Suppression".
    ///
    /// virtio14 name: VIRTIO_F_EVENT_IDX
    pub bool, uses_virtqueue_notification_index, set_uses_virtqueue_notification_index: 29;

    /// Workaround for bug in early QEMU implementations.
    ///
    /// This driver does not attempt to negotiate this feature.
    ///
    /// virtio14 name: UNUSED
    pub bool, unused_qemu_experimental, set_unused_qemu_experimental: 30;

    /// Use the virtio 1.0+ standard.
    ///
    /// Failing to negotiate this feature enables behaviors called "legacy" in
    /// virtio14.
    ///
    /// This driver only supports virtio 1.0+ devices, and fails to initialize
    /// if the device does not offer this feature. This behavior is explicitly
    /// allowed by virtio14 6.1 "Driver Requirements: Reserved Feature Bits".
    ///
    /// virtio14 name: VIRTIO_F_VERSION_1
    pub bool, uses_virtio1_standard, set_uses_virtio1_standard: 32;

    /// Signals that the device's memory accesses are limited or translated.
    ///
    /// By default, the driver may assume that the virtio implementation can
    /// access any physical memory address provided by the driver. If this
    /// feature is negotiated, the device's memory accesses may be gated by an
    /// IOMMU.
    ///
    /// virtio14 6.1 "Driver Requirements: Reserved Feature Bits" recommends
    /// accepting this feature if it is offered. virtio14 6.2 "Device
    /// Requirements: Reserved Feature Bits" states that a device may fail to
    /// operate if this feature is not accepted when it is offered.
    ///
    /// This driver does not currently support this feature.
    ///
    /// virtio14 name: VIRTIO_F_ACCESS_PLATFORM
    pub bool, has_limited_memory_access, set_has_limited_memory_access: 33;

    /// Use packed virtqueues, as opposed to split virtqueues.
    ///
    /// By default, virtqueues use the format described in virtio14 2.7 "Split
    /// Virtqueues". If this feature is negotiated, virtqueues use the format
    /// described in virtio14 2.8 "Packed Virtqueues".
    ///
    /// virtio14 6.1 "Driver Requirements: Reserved Feature Bits" recommends
    /// accepting this feature if it is offered. This driver currently only
    /// supports split virtqueues, and does not attempt to negotiate this
    /// feature.
    ///
    /// virtio14 name: VIRTIO_F_RING_PACKED
    pub bool, uses_packed_virtqueues, set_uses_packed_virtqueues: 34;

    /// The device always returns virtqueue buffers in submission order.
    ///
    /// By default, virtio devices are free to return submitted virtqueue
    /// buffers in any order. If this feature is negotiated, devices will follow
    /// submission order when returning the buffers. So, negotiating this
    /// feature can simplify the driver's buffer management, at the cost of
    /// reducing performance optimization opportunities for the device.
    ///
    /// Only some devices offer this feature. For example, the virtio-gpu
    /// implementation in QEMU and crosvm does not offer this feature.
    ///
    /// virtio14 name: VIRTIO_F_IN_ORDER
    pub bool, returns_buffers_in_submission_order, set_returns_buffers_in_submission_order: 35;

    /// Use the memory access ordering rules for device memory accesses.
    ///
    /// By default, the virtio device must behave as if it accesses memory
    /// directly from one of the CPU cores, which entails access to coherent
    /// caches.
    ///
    /// If this feature is negotiated, the virtio device may perform
    /// optimizations that assume the driver issues the memory access barriers
    /// needed for external hardware devices.
    ///
    /// virtio14 6.1 "Driver Requirements: Reserved Feature Bits" recommends
    /// accepting this feature if it is offered. virtio14 6.2 "Device
    /// Requirements: Reserved Feature Bits" states that a device may operate in
    /// a slower mode if this feature is not accepted when it is offered.
    ///
    /// This driver always issues the memory barriers required by the platform,
    /// even if the feature is not negotiated.
    ///
    /// virtio14 name: VIRTIO_F_ORDER_PLATFORM
    pub bool, assumes_hardware_memory_barriers, set_assumes_hardware_memory_barriers: 36;

    /// Enables device support for PCI Single Root I/O Virtualization (SR-IOV).
    ///
    /// If this feature is negotiated, the driver may use the PCI SR-IOV capability
    /// structure to enable virtual functions.
    ///
    /// This driver does not use SR-IOV.
    ///
    /// virtio14 name: VIRTIO_F_SR_IOV
    pub bool, supports_io_virtualization, set_supports_io_virtualization: 37;

    /// Enables extra data in the driver's device notifications.
    ///
    /// By default, the notifications issued by the driver only identify a
    /// target virtqueue. If this feature is negotiated, the notifications include
    /// more data.
    ///
    /// virtio14 2.9 "Driver Notifications" describes driver-issued
    /// notifications at a high level. virtio14 4.1.5.2 "Available Buffer
    /// Notifications" describes the impact of this feature on notifications
    /// used by the PCI transport.
    ///
    /// This driver does not attempt to negotiate this feature. None of the
    /// virtio implementations targeted by this driver take advantage of this
    /// feature.
    ///
    /// virtio14 name: VIRTIO_F_NOTIFICATION_DATA
    pub bool, uses_extended_notification_data, set_uses_extended_notification_data: 38;

    /// Enables custom virtqueue IDs in driver-issued notifications.
    ///
    /// By default, the driver identifies virtqueues using their index. If this
    /// feature is negotiated, the notifications include a custom virtqueue ID
    /// provided by the device.
    ///
    /// virtio14 2.9 "Driver Notifications" describes driver-issued
    /// notifications at a high level. virtio14 4.1.5.2 "Available Buffer
    /// Notifications" describes the impact of this feature on notifications
    /// used by the PCI transport.
    ///
    /// This driver does not attempt to negotiate this feature. The feature is
    /// not implemented in any of the virtio implementations targeted by this
    /// driver.
    ///
    /// virtio14 name: VIRTIO_F_NOTIF_CONFIG_DATA
    pub bool, uses_custom_virtqueue_ids, set_uses_custom_virtqueue_ids: 39;

    /// Enables virtqueue-level reset.
    ///
    /// If this feature is not negotiated, the driver only has a device-level
    /// reset mechanism.
    ///
    /// virtio14 name: VIRTIO_F_RING_RESET
    pub bool, supports_queue_reset, set_supports_queue_reset: 40;

    /// Enables driver usage of the device's administration virtqueues.
    ///
    /// If this feature is negotiated, the driver must configure the device's
    /// administration virtqueues during device initialization.
    ///
    /// virtio14 2.13 "Administration Virtqueues" describes the administration
    /// virtqueue concept.
    ///
    /// This driver does not support administration virtqueues.
    ///
    /// virtio14 name: VIRTIO_F_ADMIN_VQ
    pub bool, uses_admin_virtqueues, set_uses_admin_virtqueues: 41;

    /// Enables the device's suspend functionality.
    ///
    /// If this feature is enabled [`DeviceStatus::suspended`] may be set.
    ///
    /// virtio14 name: VIRTIO_F_SUSPEND
    pub bool, suspend_enabled, set_suspend_enabled: 43;
}
