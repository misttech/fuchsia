// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::fs_registry::FsRegistry;
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_uapi::errors::Errno;
use std::sync::Arc;

#[derive(Clone)]
pub struct FilesystemsFile {
    fs_registry: Arc<FsRegistry>,
}

impl FilesystemsFile {
    pub fn new_node(fs_registry: &Arc<FsRegistry>) -> impl FsNodeOps {
        DynamicFile::new_node(Self { fs_registry: fs_registry.clone() })
    }
}

impl DynamicFileSource for FilesystemsFile {
    fn generate(&self, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        // TODO(https://fxbug.dev/441966997): Report nodev for filesystems that don't need a block
        // device.
        for entry in self.fs_registry.list_all() {
            writeln!(sink, "{:5}\t{}", "", entry)?;
        }

        Ok(())
    }
}
