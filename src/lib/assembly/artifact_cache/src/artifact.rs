// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use camino::Utf8PathBuf;

/// An artifact reference.
#[derive(Debug, PartialEq)]
pub enum Artifact {
    /// A artifact that lives on the local host.
    Local(Utf8PathBuf),

    /// An artifact found in a CIPD package.
    CIPD(CIPDPackage),

    /// An artifact known by MOS.
    #[allow(unused)]
    MOS(MOSIdentifier),
}

/// A reference to an artifact in CIPD.
#[derive(Debug, PartialEq)]
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
#[derive(Debug, PartialEq)]
pub struct MOSIdentifier {
    pub repo: String,
    pub version: String,
    pub name: String,
}
