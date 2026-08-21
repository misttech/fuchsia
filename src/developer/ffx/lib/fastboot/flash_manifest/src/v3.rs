// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::OemFile;
use crate::v1::{FlashManifest as FlashManifestV1, Partition as PartitionV1, Product as ProductV1};
use crate::v2::FlashManifest as FlashManifestV2;
use assembly_partitions_config::BootstrapCondition;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlashManifest {
    pub hw_revision: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub product_matches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub products: Vec<Product>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Product {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bootloader_partitions: Vec<Partition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<Partition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oem_files: Vec<ExplicitOemFile>,
    #[serde(default)]
    pub requires_unlock: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Partition {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition_json: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub variable: String,
    pub value: String,
}

impl From<&BootstrapCondition> for Condition {
    fn from(val: &BootstrapCondition) -> Self {
        Self { variable: val.variable.clone(), value: val.value.clone() }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExplicitOemFile {
    pub command: String,
    pub path: String,
}

impl From<&ExplicitOemFile> for OemFile {
    fn from(f: &ExplicitOemFile) -> OemFile {
        OemFile::new(f.command.clone(), f.path.clone())
    }
}

impl From<&Partition> for PartitionV1 {
    fn from(p: &Partition) -> PartitionV1 {
        PartitionV1::new_with_condition_json(
            p.name.clone(),
            p.path.clone(),
            p.condition.as_ref().map(|c| c.variable.clone()),
            p.condition.as_ref().map(|c| c.value.clone()),
            p.condition_json.clone(),
        )
    }
}

impl From<&Product> for ProductV1 {
    fn from(p: &Product) -> ProductV1 {
        ProductV1 {
            name: p.name.clone(),
            bootloader_partitions: p.bootloader_partitions.iter().map(|p| p.into()).collect(),
            partitions: p.partitions.iter().map(|p| p.into()).collect(),
            oem_files: p.oem_files.iter().map(|f| f.into()).collect(),
            requires_unlock: p.requires_unlock,
        }
    }
}

impl From<&FlashManifest> for FlashManifestV2 {
    fn from(p: &FlashManifest) -> FlashManifestV2 {
        FlashManifestV2 {
            hw_revision: p.hw_revision.clone(),
            product_matches: p.product_matches.clone(),
            credentials: p.credentials.iter().map(|c| c.clone()).collect(),
            v1: FlashManifestV1(p.products.iter().map(|p| p.into()).collect()),
        }
    }
}
