// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// An artifact reference.
#[derive(Debug, PartialEq)]
pub enum Artifact {
    /// A artifact that lives on the local host.
    Local(Utf8PathBuf),

    /// An artifact found in a CIPD package.
    CIPD(CIPDPackage),

    /// An artifact known by MOS.
    MOS(MOSIdentifier),
}

/// The type of assembly artifact.
#[derive(PartialEq, Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactType {
    /// Platform
    Platform,
    /// Product
    Product,
    /// Board
    Board,
    /// Product Input Bundle Set
    PIB,
    /// Board Input Bundle
    BIB,
}

impl FromStr for ArtifactType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "platform" => Ok(ArtifactType::Platform),
            "products" => Ok(ArtifactType::Product),
            "boards" => Ok(ArtifactType::Board),
            "product_input_bundles" => Ok(ArtifactType::PIB),
            "board_input_bundles" => Ok(ArtifactType::BIB),
            _ => Err(()), // Return an error for any other string
        }
    }
}

impl fmt::Display for ArtifactType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactType::Platform => write!(f, "platform"),
            ArtifactType::Product => write!(f, "products"),
            ArtifactType::Board => write!(f, "boards"),
            ArtifactType::PIB => write!(f, "product_input_bundles"),
            ArtifactType::BIB => write!(f, "board_input_bundles"),
        }
    }
}

/// A reference to an artifact in CIPD.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct CIPDPackage {
    pub path: Utf8PathBuf,
    pub tag: String,
}

impl std::fmt::Display for CIPDPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cipd://{}@{}", self.path, self.tag)
    }
}

/// A reference to an artifact known by MOS.
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub struct MOSIdentifier {
    /// name of this resource
    pub name: String,

    /// version of this resource
    pub version: String,

    /// repository where this artifact is defined
    pub repository: String,

    /// type of assembly artifact
    pub artifact_type: ArtifactType,

    /// location of this artifact in CIPD
    pub cipd: Option<CIPDPackage>,
}

impl MOSIdentifier {
    /// Return a string format representing this MOSIdentifier.
    pub fn id(&self) -> String {
        format!(
            "mos://{}/{}/{}@{}",
            self.repository.clone(),
            self.artifact_type,
            self.name.clone(),
            self.version.clone()
        )
    }
}
