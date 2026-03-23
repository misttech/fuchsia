// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Generic traits for configuration.

use fuchsia_inspect::Node;
use fuchsia_runtime::{HandleInfo, HandleType, take_startup_handle};

pub trait Config: Sized {
    /// Take the config startup handle and parse its contents.
    ///
    /// # Panics
    ///
    /// If the config startup handle was already taken or if it is not valid.
    fn take_from_startup_handle() -> Self {
        let handle_info = HandleInfo::new(HandleType::ComponentConfigVmo, 0);
        let config_vmo: zx::Vmo =
            take_startup_handle(handle_info).expect("Config VMO handle must be present.").into();
        Self::from_vmo(&config_vmo).expect("Config VMO handle must be valid.")
    }

    /// Parse `Self` from `vmo`.
    fn from_vmo(vmo: &zx::Vmo) -> Result<Self, Error> {
        let config_size = vmo.get_content_size().map_err(Error::GettingContentSize)?;
        let config_bytes = vmo.read_to_vec(0, config_size).map_err(Error::ReadingConfigBytes)?;
        Self::from_bytes(&config_bytes)
    }

    /// Parse `Self` from `bytes`.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error>;

    /// Record config into inspect node.
    fn record_inspect(&self, inspector_node: &Node);
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to get content size of VMO")]
    GettingContentSize(#[source] zx::Status),
    #[error("Failed to read content of VMO")]
    ReadingConfigBytes(#[source] zx::Status),
    #[error("VMO is too small for this config library")]
    TooFewBytes,
    #[error(
        "ABI checksum mismatch, expected library checksum {expected_checksum:?}, got {observed_checksum:?}"
    )]
    ChecksumMismatch { expected_checksum: Vec<u8>, observed_checksum: Vec<u8> },
    #[error("Failed to unpersist the non-checksum bytes of the VMO as this library's FIDL type")]
    Unpersist(#[source] fidl::Error),
}
