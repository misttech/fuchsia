// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use delivery_blob::{CompressionMode, Type1Blob};
use fidl_fuchsia_fxfs::BlobCreatorProxy;
use log::info;
use storage_stress_test_utils::data::FileFactory;
use stress_test::actor::{Actor, ActorError};
use thiserror::Error;
use zx::Status;

// Performs operations on blobs expected to exist on disk
pub struct BlobActor {
    // Factory used to generate blobs of specific size and compressibility
    pub factory: FileFactory,

    pub creator: BlobCreatorProxy,
}

#[derive(Debug, Error)]
enum WriteBlobError {
    #[error("failed to create blob: {0:?}")]
    CreateBlob(fidl_fuchsia_fxfs::CreateBlobError),
    #[error(transparent)]
    CreateBlobWriter(#[from] blob_writer::CreateError),
    #[error(transparent)]
    WriteBlob(#[from] blob_writer::WriteError),
}

impl From<fidl_fuchsia_fxfs::CreateBlobError> for WriteBlobError {
    fn from(error: fidl_fuchsia_fxfs::CreateBlobError) -> Self {
        WriteBlobError::CreateBlob(error)
    }
}

impl BlobActor {
    pub fn new(factory: FileFactory, creator: BlobCreatorProxy) -> Self {
        Self { factory, creator }
    }

    async fn create_blob(&mut self) -> Result<(), WriteBlobError> {
        // Create the root hash for the blob
        let data_bytes = self.factory.generate_bytes();
        let delivery_blob = Type1Blob::generate(&data_bytes, CompressionMode::Attempt);
        let merkle_root_hash = fuchsia_merkle::root_from_slice(&data_bytes);

        // Write the file to disk.
        let writer = self
            .creator
            .create(&merkle_root_hash.into(), false)
            .await
            .expect("Failed to make FIDL call")?;
        let mut writer =
            blob_writer::BlobWriter::create(writer.into_proxy(), delivery_blob.len() as u64)
                .await?;
        Ok(writer.write(&delivery_blob).await?)
    }
}

#[async_trait]
impl Actor for BlobActor {
    async fn perform(&mut self) -> Result<(), ActorError> {
        match self.create_blob().await {
            Ok(()) => Ok(()),
            Err(WriteBlobError::WriteBlob(blob_writer::WriteError::BytesReady(
                Status::NO_SPACE,
            ))) => Ok(()),
            // Any other error is assumed to come from an intentional crash.
            // The environment verifies that an intentional crash occurred
            // and will panic if that is not the case.
            Err(s) => {
                info!("Blob actor got status: {}", s);
                Err(ActorError::ResetEnvironment)
            }
        }
    }
}
