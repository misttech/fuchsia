// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::vfs::pseudo::simple_file::SimpleFileNode;
use crate::vfs::{
    FileOps, FsNodeOps, fileops_impl_dataless, fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use bstr::ByteSlice;
use starnix_logging::BugRef;
use starnix_sync::{FileOpsCore, Locked};
use std::panic::Location;

#[derive(Clone, Debug)]
pub struct StubEmptyFile {
    bug: BugRef,
    location: &'static Location<'static>,
}

impl StubEmptyFile {
    #[track_caller]
    pub fn new_node(bug: BugRef) -> impl FsNodeOps {
        // This ensures the caller of this fn is recorded instead of the location of the closure.
        let location = Location::caller();
        SimpleFileNode::new(move || Ok(StubEmptyFile { bug, location }))
    }
}

impl FileOps for StubEmptyFile {
    fileops_impl_dataless!();
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &crate::vfs::FileObject,
        current_task: &crate::task::CurrentTask,
    ) -> Result<(), starnix_uapi::errors::Errno> {
        let path = file.name.path(current_task);
        starnix_logging::__track_stub_inner(
            self.bug,
            path.to_str_lossy().as_ref(),
            None,
            &self.location,
        );
        Ok(())
    }
}
