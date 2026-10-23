// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides helper functions for reading, writing, and clearing the Android bootloader message
//! stored in the /misc partition.

use anyhow::{Context as _, Error, anyhow};
use block_client::{BlockClient as _, RemoteBlockClient};
use bootloader_message::{BootloaderMessage, BootloaderMessageRaw};
use fidl_fuchsia_storage_block::BlockMarker;
use zerocopy::{FromBytes, IntoBytes};

/// High level interface to operate on the bootloader message.
pub struct BootloaderMessageStore {
    client: RemoteBlockClient,
}

impl BootloaderMessageStore {
    pub async fn new() -> Result<Self, Error> {
        let device =
            fuchsia_component::client::connect_to_protocol_at::<BlockMarker>("/block/misc")
                .context("unable to open /misc device")?;
        let client =
            RemoteBlockClient::new(device).await.context("unable to connect to /misc device")?;
        Ok(Self { client })
    }

    fn buffer_size(&self) -> usize {
        std::mem::size_of::<BootloaderMessageRaw>()
            .next_multiple_of(self.client.block_size() as usize)
    }

    pub async fn read(&self) -> Result<BootloaderMessage, Error> {
        let mut buf = vec![0u8; self.buffer_size()];
        self.client.read_at(buf.as_mut_slice().into(), 0).await?;
        let (raw, _) = BootloaderMessageRaw::read_from_prefix(&buf[..])
            .map_err(|_| anyhow!("failed to deserialize bootloader message"))?;
        let message = BootloaderMessage::from(raw);
        Ok(message)
    }

    async fn write_raw(&self, raw: &BootloaderMessageRaw) -> Result<(), Error> {
        let raw_bytes = raw.as_bytes();
        let buf_size = self.buffer_size();
        if buf_size == raw_bytes.len() {
            self.client.write_at(raw_bytes.into(), 0).await?;
        } else {
            let mut buf = vec![0u8; buf_size];
            self.client.read_at(buf.as_mut_slice().into(), 0).await?;
            buf[..raw_bytes.len()].copy_from_slice(raw_bytes);
            self.client.write_at(buf.as_slice().into(), 0).await?;
        }
        Ok(())
    }

    pub async fn clear(&self) -> Result<(), Error> {
        self.write_raw(&BootloaderMessageRaw::default()).await
    }

    // TODO(https://fxbug.dev/450627605): Remove when this is used.
    #[allow(dead_code)]
    pub async fn write(&self, message: BootloaderMessage) -> Result<(), Error> {
        let raw: BootloaderMessageRaw = message.try_into()?;
        self.write_raw(&raw).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_storage_block::BlockProxy;
    use std::sync::Arc;
    use test_vmo_backed_block_server::VmoBackedServer;

    #[fuchsia::test]
    async fn test_read_write_clear_512_block_size() {
        let msg: BootloaderMessageRaw =
            BootloaderMessage::with_args("--wipe_data\n").try_into().unwrap();
        let server = Arc::new(VmoBackedServer::new(8, 512, msg.as_bytes()).unwrap());
        let client = RemoteBlockClient::new(server.connect::<BlockProxy>()).await.unwrap();
        let store = BootloaderMessageStore { client };

        let read_msg = store.read().await.unwrap();
        assert_eq!(BootloaderMessageRaw::try_from(read_msg).unwrap().as_bytes(), msg.as_bytes());

        store.clear().await.unwrap();
        let cleared_msg = store.read().await.unwrap();
        assert_eq!(
            BootloaderMessageRaw::try_from(cleared_msg).unwrap().as_bytes(),
            BootloaderMessageRaw::default().as_bytes()
        );
    }

    #[fuchsia::test]
    async fn test_read_write_clear_4096_block_size_preserves_trailing_bytes() {
        let msg: BootloaderMessageRaw =
            BootloaderMessage::with_args("--wipe_data\n").try_into().unwrap();
        let mut initial = vec![0xabu8; 4096];
        initial[..std::mem::size_of::<BootloaderMessageRaw>()].copy_from_slice(msg.as_bytes());
        let server = Arc::new(VmoBackedServer::new(2, 4096, &initial).unwrap());
        let client = RemoteBlockClient::new(server.connect::<BlockProxy>()).await.unwrap();
        let store = BootloaderMessageStore { client };

        let read_msg = store.read().await.unwrap();
        assert_eq!(BootloaderMessageRaw::try_from(read_msg).unwrap().as_bytes(), msg.as_bytes());

        store.clear().await.unwrap();
        let cleared_msg = store.read().await.unwrap();
        assert_eq!(
            BootloaderMessageRaw::try_from(cleared_msg).unwrap().as_bytes(),
            BootloaderMessageRaw::default().as_bytes()
        );

        let mut block = vec![0u8; 4096];
        store.client.read_at(block.as_mut_slice().into(), 0).await.unwrap();
        assert!(block[std::mem::size_of::<BootloaderMessageRaw>()..].iter().all(|&b| b == 0xab));
    }
}
