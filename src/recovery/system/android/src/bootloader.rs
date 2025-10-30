// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides helper functions for reading, writing, and clearing the Android bootloader message
//! stored in the /misc partition.

use anyhow::{Context as _, Error, anyhow};
use block_client::{BlockClient as _, RemoteBlockClient};
use bstr::ByteSlice as _;
use fidl_fuchsia_hardware_block_volume::VolumeMarker;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// See bootable/recovery/bootloader_message for the canonical format.
#[repr(C)]
#[derive(Copy, Clone, KnownLayout, FromBytes, IntoBytes, Immutable)]
struct BootloaderMessageRaw {
    command: [u8; 32],
    status: [u8; 32],
    recovery: [u8; 768],
    _stage: [u8; 32],
    _reserved: [u8; 1184],
}

impl Default for BootloaderMessageRaw {
    fn default() -> Self {
        Self {
            command: [0; _],
            status: [0; _],
            recovery: [0; _],
            _stage: [0; _],
            _reserved: [0; _],
        }
    }
}

/// Processed bootloader message.
#[derive(Debug)]
pub struct BootloaderMessage {
    command: String,
    status: String,
    recovery: String,
}

impl From<BootloaderMessageRaw> for BootloaderMessage {
    fn from(raw: BootloaderMessageRaw) -> Self {
        Self {
            command: bytes_to_string(&raw.command),
            status: bytes_to_string(&raw.status),
            recovery: bytes_to_string(&raw.recovery),
        }
    }
}

impl TryFrom<BootloaderMessage> for BootloaderMessageRaw {
    type Error = Error;

    fn try_from(message: BootloaderMessage) -> Result<Self, Error> {
        let mut raw = BootloaderMessageRaw::default();

        let BootloaderMessage { command, status, recovery } = message;

        // Ensure fields will fit before copying them.
        if command.len() > raw.command.len() {
            return Err(anyhow!("command field exceeds storage size"));
        }
        if status.len() > raw.status.len() {
            return Err(anyhow!("status field exceeds storage size"));
        }
        if recovery.len() > raw.recovery.len() {
            return Err(anyhow!("recovery field exceeds storage size"));
        }

        raw.command[0..command.len()].copy_from_slice(command.as_bytes());
        raw.status[0..status.len()].copy_from_slice(status.as_bytes());
        raw.recovery[0..recovery.len()].copy_from_slice(recovery.as_bytes());

        Ok(raw)
    }
}

/// High level interface to operate on the bootloader message.
pub struct BootloaderMessageStore {
    client: RemoteBlockClient,
}

impl BootloaderMessageStore {
    pub async fn new() -> Result<Self, Error> {
        let device =
            fuchsia_component::client::connect_to_protocol_at::<VolumeMarker>("/block/misc")
                .context("unable to open /misc device")?;
        let client =
            RemoteBlockClient::new(device).await.context("unable to connect to /misc device")?;
        Ok(Self { client })
    }

    pub async fn read(&self) -> Result<BootloaderMessage, Error> {
        let mut buf = vec![0u8; std::mem::size_of::<BootloaderMessageRaw>()];
        self.client.read_at(buf.as_mut_slice().into(), 0).await?;
        let raw = BootloaderMessageRaw::read_from_bytes(&buf[..])
            .map_err(|_| anyhow!("failed to deserialize bootloader message"))?;
        let message = BootloaderMessage::try_from(raw)?;
        Ok(message)
    }

    pub async fn clear(&self) -> Result<(), Error> {
        let raw = BootloaderMessageRaw::default();
        self.client.write_at(raw.as_bytes().into(), 0).await?;
        Ok(())
    }

    // TODO(https://fxbug.dev/450627605): Remove when this is used.
    #[allow(dead_code)]
    pub async fn write(&self, message: BootloaderMessage) -> Result<(), Error> {
        let raw: BootloaderMessageRaw = message.try_into()?;
        self.client.write_at(raw.as_bytes().into(), 0).await?;
        Ok(())
    }
}

/// Converts a byte buffer to a Rust [`String`], where `buf` is *possibly* a null-terminated UTF-8
/// string. Invalid UTF-8 characters will be emitted as "�" (U+FFFD). Bytes after the first null
/// character, if present, will be ignored, otherwise all of `buf` is used.
fn bytes_to_string(buf: &[u8]) -> String {
    if let Some((contents, _)) = buf.split_once_str(&[0u8]) {
        contents.as_bstr().to_string()
    } else {
        buf.as_bstr().to_string()
    }
}
