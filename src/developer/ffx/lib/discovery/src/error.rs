// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not create emulator watcher from path {path:?}: {err}")]
    EmulatorWatcher { path: PathBuf, err: String },

    #[error("could not create fastboot watcher from path {path:?}: {err}")]
    FastbootWatcher { path: PathBuf, err: String },

    #[error("could not parse fastboot devices file {path:?}: {err}")]
    FastbootDiscovery { path: PathBuf, err: fastboot_file_discovery::GetFastbootEntriesError },

    #[error("could not start mdns watcher: {0}")]
    MdnsWatcher(String),

    #[error("could not start fastboot watcher: {0}")]
    FastbootUsbWatcher(String),

    #[error("could not start manual target watcher: {0}")]
    ManualTargetWatcher(String),

    #[error("could not send event: {0}")]
    Send(#[from] futures::channel::mpsc::SendError),

    #[error("could not send event: {0}")]
    TrySend(#[from] futures::channel::mpsc::TrySendError<crate::TargetEvent>),

    #[error("SocketBound events are not supported")]
    SocketBoundUnsupported,

    #[error("Target has no addresses")]
    TargetHasNoAddresses,
}
