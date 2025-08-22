// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetEvent;
use anyhow::Result;
use fastboot_file_discovery::{FastbootFileWatcher, GetFastbootEntriesError, get_fastboot_devices};
use futures::channel::mpsc::UnboundedSender;
use std::path::PathBuf;
use thiserror::Error;

pub(crate) struct FastbootWatcher {
    // Task for the drain loop
    _watcher: FastbootFileWatcher,
}

#[derive(Error, Debug)]
pub enum ParseFastbootDevicesError {
    #[error("Fastboot Devices file malformed. Please check file contents: \"{}\"", path)]
    MalformedFile { path: String, source: GetFastbootEntriesError },
}

impl FastbootWatcher {
    pub(crate) fn new(
        instance_root: PathBuf,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self> {
        let existing = get_fastboot_devices(&instance_root).map_err(|source| {
            ParseFastbootDevicesError::MalformedFile {
                path: instance_root.display().to_string(),
                source,
            }
        })?;
        for device in existing {
            let event = fastboot_file_discovery::FastbootEvent::Discovered(device);
            let handle = event.into();
            let _ = sender.unbounded_send(handle);
        }

        // FastbootFile (and therefore notify thread) lifetime should last as long as the task,
        // because it is moved into the loop
        let watcher = fastboot_file_discovery::recommended_watcher(
            move |res: fastboot_file_discovery::FastbootEvent| {
                // Translate the result to a TargetEvent
                log::trace!("discovery watcher got fastboot file event: {:#?}", res);
                let event = res.into();
                let _ = sender.unbounded_send(event);
            },
            instance_root,
        )?;
        Ok(Self { _watcher: watcher })
    }
}
