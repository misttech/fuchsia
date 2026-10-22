// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::pseudo::simple_file::SimpleFileNode;
use crate::vfs::{
    FileObject, FileOps, FsNodeOps, fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use bstr::ByteSlice;
use starnix_logging::BugRef;
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use std::panic::Location;

#[derive(Clone, Debug)]
pub struct StubEmptyFile {
    bug: BugRef,
    location: &'static Location<'static>,
}

impl StubEmptyFile {
    #[track_caller]
    pub fn new_node(bug: BugRef) -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(StubEmptyFile::new(bug)))
    }

    #[track_caller]
    pub fn new(bug: BugRef) -> Self {
        // This ensures the caller of this fn is recorded instead of the location of the closure.
        let location = Location::caller();
        Self { bug, location }
    }
}

impl FileOps for StubEmptyFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn open(&self, file: &FileObject, current_task: &CurrentTask) -> Result<(), Errno> {
        let path = file.name.path(&current_task.fs());
        starnix_logging::__track_stub_inner(
            self.bug,
            path.to_str_lossy().as_ref(),
            None,
            &self.location,
        );
        Ok(())
    }

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{anon_test_file, spawn_kernel_and_run};
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use starnix_logging::bug_ref;
    use starnix_uapi::errno;
    use starnix_uapi::open_flags::OpenFlags;

    #[fuchsia::test]
    async fn test_stub_empty_file_read_returns_eof() {
        spawn_kernel_and_run(async |current_task| {
            let file = anon_test_file(
                &current_task,
                Box::new(StubEmptyFile::new(bug_ref!("https://fxbug.dev/574071555"))),
                OpenFlags::RDWR,
            );

            let mut out_buf = VecOutputBuffer::new(64);
            assert_eq!(file.read(&current_task, &mut out_buf), Ok(0));
            assert!(out_buf.data().is_empty());

            let mut in_buf = VecInputBuffer::new(b"test");
            assert_eq!(file.write(&current_task, &mut in_buf), Err(errno!(EINVAL)));
        })
        .await;
    }
}
