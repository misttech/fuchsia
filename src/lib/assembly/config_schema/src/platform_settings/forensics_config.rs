// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assembly_container::WalkPaths;
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Configuration options for the forensics area.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(default, deny_unknown_fields)]
pub struct ForensicsConfig {
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub feedback: FeedbackConfig,

    #[walk_paths]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub cobalt: CobaltConfig,
}

/// Configuration options for the feedback configuration area.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FeedbackConfig {
    /// If true, Feedback will apply the config found at
    /// //src/developer/forensics/feedback/configs/product/large_disk.json. Compared to the
    /// default, this will persist snapshots to disk if the network is unavailable.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub large_disk: bool,

    /// If true, Feedback will retrieve the device ID via the fuchsia.feedback.DeviceIdProvider
    /// FIDL protocol rather than using its local implementation.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub remote_device_id_provider: bool,

    /// The URL of the component, if any, that exposes the fuchsia.feedback.DeviceIdProvider
    /// protocol and should be added to the core realm.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub feedback_id_component_url: FeedbackIdComponentUrl,

    /// Whether to include the last few kernel logs in the last reboot info.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub include_kernel_logs_in_last_reboot_info: bool,

    /// Configuration options for excluding items from snapshots.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub snapshot_exclusion: SnapshotExclusionConfig,

    /// How a device should interpret a spontaneous reboot, which occurs when a device did not cold
    /// boot and the kernel and bootloader cannot determine why a device shutdown (in other words,
    /// the Zircon reboot reason is "UNKNOWN").
    ///
    /// Note that this flag will only be used as the reboot reason for spontaneous reboots. For
    /// non-spontaneous reboots, other signals will be available and will be used to determine the
    /// reboot reason.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub spontaneous_reboot_reason: SpontaneousRebootReason,
}

/// Configuration options for excluding items from snapshots.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotExclusionConfig {
    /// A list of annotations that should not be collected.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub excluded_annotations: Vec<String>,
}

/// Configuration options for the cobalt configuration area.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(default, deny_unknown_fields)]
pub struct CobaltConfig {
    #[schemars(schema_with = "crate::option_path_schema")]
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<Utf8PathBuf>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackIdComponentUrl {
    #[default]
    None,

    /// The URL of the component, if any, that connects the fuchsia.feedback.DeviceIdProvider and
    /// google.flashts.Reader protocols.
    FlashTs(String),

    /// The URL of the component, if any, that connects the fuchsia.feedback.DeviceIdProvider and
    /// fuchsia.sysinfo.SysInfo protocols.
    SysInfo(String),
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
// LINT.IfChange
#[serde(rename_all = "snake_case")]
pub enum SpontaneousRebootReason {
    #[default]
    Spontaneous,

    /// The spontaneous reboot is likely because of a user disconnecting, then reconnecting their
    /// device's power supply in rapid succession.
    BriefPowerLoss,

    /// The spontaneous reboot is likely because of a user triggering a hardware reset.
    HardReset,
}
// LINT.ThenChange(//src/developer/forensics/feedback/config.cc)
