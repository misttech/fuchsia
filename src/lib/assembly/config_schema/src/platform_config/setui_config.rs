// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assembly_file_relative_path::{FileRelativePathBuf, SupportsFileRelativePaths};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum ICUType {
    /// Use assembly to define the setui config.  Use the unflavored setui
    /// package, compiled without regard to a specific ICU version.
    #[serde(rename = "without_icu")]
    Unflavored,

    /// Use assembly to define the setui config. Use the ICU flavored setui
    /// package, compiled with the specific ICU commit ID.
    #[serde(rename = "with_icu")]
    #[default]
    Flavored,
}

/// Platform configuration options for the input area.
#[derive(
    Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema, SupportsFileRelativePaths,
)]
#[serde(default, deny_unknown_fields)]
pub struct SetUiConfig {
    /// If set, the setui config is added to the product configuration.
    pub use_icu: ICUType,

    /// If set, uses the setui configured with camera settings.  Else uses
    /// setui without camera.
    pub with_camera: bool,

    #[schemars(schema_with = "crate::option_path_schema")]
    #[file_relative_paths]
    pub display: Option<FileRelativePathBuf>,

    #[schemars(schema_with = "crate::option_path_schema")]
    #[file_relative_paths]
    pub interface: Option<FileRelativePathBuf>,

    #[schemars(schema_with = "crate::option_path_schema")]
    #[file_relative_paths]
    pub agent: Option<FileRelativePathBuf>,
}
