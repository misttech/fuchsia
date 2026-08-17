// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::PageRequest;
use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use fuchsia_sync::Mutex;
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;

#[derive(Default)]
pub struct TestVecBufferInner {
    pub commits: Vec<(u64, usize)>,
    pub output: Vec<u8>,
}

#[derive(Clone)]
pub struct TestVecBufferReceiver(pub Arc<Mutex<TestVecBufferInner>>);

impl TestVecBufferReceiver {
    pub fn commits(&self) -> Vec<(u64, usize)> {
        self.0.lock().commits.clone()
    }

    pub fn output(&self) -> Vec<u8> {
        self.0.lock().output.clone()
    }
}

pub struct TestVecBuffer {
    pub data: Vec<u8>,
    pub range: Range<u64>,
    pub committed_len: usize,
    pub offset: u64,
    pub receiver: TestVecBufferReceiver,
}

impl TestVecBuffer {
    pub fn new(size: usize) -> (Self, TestVecBufferReceiver) {
        Self::new_with_offset(size, 0)
    }

    pub fn new_with_offset(size: usize, offset: u64) -> (Self, TestVecBufferReceiver) {
        let receiver = TestVecBufferReceiver(Arc::new(Mutex::new(TestVecBufferInner::default())));
        let range = offset..offset + size as u64;
        let buf = Self {
            data: vec![0u8; size],
            range,
            committed_len: 0,
            offset,
            receiver: receiver.clone(),
        };
        (buf, receiver)
    }

    pub fn new_unprepared() -> (Self, TestVecBufferReceiver) {
        let receiver = TestVecBufferReceiver(Arc::new(Mutex::new(TestVecBufferInner::default())));
        let buf = Self {
            data: Vec::new(),
            range: 0..0,
            committed_len: 0,
            offset: 0,
            receiver: receiver.clone(),
        };
        (buf, receiver)
    }
}

impl Drop for TestVecBuffer {
    fn drop(&mut self) {
        self.receiver.0.lock().output = std::mem::take(&mut self.data);
    }
}

impl DataBuffer for TestVecBuffer {
    fn range(&self) -> Range<u64> {
        self.range.clone()
    }

    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        let remaining = &mut self.data[self.committed_len..];
        MutPtrByteSlice::from(remaining)
    }

    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        self.receiver.0.lock().commits.push((self.offset, size));
        self.offset += size as u64;
        self.committed_len += size;
        Ok(())
    }
}

impl PageRequest for TestVecBuffer {
    fn prepare(&mut self, read_range: Range<u64>) -> Result<(), ChunkedArchiveError> {
        let size = (read_range.end - read_range.start) as usize;
        if self.data.len() < size {
            self.data.resize(size, 0);
        }
        self.range = read_range.clone();
        self.offset = read_range.start;
        Ok(())
    }
}
