// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail, ensure};
use async_trait::async_trait;
use std::ops::Range;
use std::sync::Arc;
use storage_device::buffer::{BufferRef, MutableBufferRef};
use storage_device::buffer_allocator::BufferFuture;
use storage_device::{Device, ReadOptions, WriteOptions};

/// Wrapper around a Device where we can only access a region within it.
pub struct RangedDevice {
    // The underlying device.
    source: Arc<dyn Device>,
    // Range (in bytes) of the accessible region in `source`.
    range: Range<u64>,
}

impl RangedDevice {
    pub fn new(source: Arc<dyn Device>, start_block: u64, num_blocks: u64) -> Result<Self, Error> {
        ensure!(
            start_block + num_blocks <= source.block_count(),
            "failed to create RangedDevice (out of range)"
        );
        ensure!(num_blocks > 0, "failed to create RangedDevice (no size)");
        let start = start_block
            .checked_mul(source.block_size() as u64)
            .ok_or_else(|| anyhow!("arithmetic overflow calculating ranged start"))?;
        let end = (start_block + num_blocks)
            .checked_mul(source.block_size() as u64)
            .ok_or_else(|| anyhow!("arithmetic overflow calculating ranged end"))?;

        Ok(Self { source: source.clone(), range: (start..end) })
    }

    fn num_blocks(&self) -> u64 {
        (self.range.end - self.range.start) / self.block_size() as u64
    }
}

#[async_trait]
impl Device for RangedDevice {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.source.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        self.source.block_size()
    }

    fn block_count(&self) -> u64 {
        self.num_blocks()
    }

    async fn read_with_opts(
        &self,
        offset: u64,
        buffer: MutableBufferRef<'_>,
        _read_opts: ReadOptions,
    ) -> Result<(), Error> {
        let adjusted_offset = self
            .range
            .start
            .checked_add(offset)
            .ok_or_else(|| anyhow!("arithmetic overflow calculating offset"))?;
        ensure!(
            adjusted_offset + buffer.len() as u64 <= self.range.end,
            "reading past end of device"
        );
        self.source.read(adjusted_offset, buffer).await
    }

    async fn write_with_opts(
        &self,
        offset: u64,
        buffer: BufferRef<'_>,
        opts: WriteOptions,
    ) -> Result<(), Error> {
        let adjusted_offset = self
            .range
            .start
            .checked_add(offset)
            .ok_or_else(|| anyhow!("arithmetic overflow calculating offset"))?;
        ensure!(
            adjusted_offset + buffer.len() as u64 <= self.range.end,
            "writing past end of device"
        );
        self.source.write_with_opts(adjusted_offset, buffer, opts).await
    }

    async fn trim(&self, _range: Range<u64>) -> Result<(), Error> {
        bail!("RangedDevice does not support trim");
    }

    async fn close(&self) -> Result<(), Error> {
        self.source.close().await
    }

    async fn flush(&self) -> Result<(), Error> {
        self.source.flush().await
    }

    fn barrier(&self) {
        self.source.barrier()
    }

    fn is_read_only(&self) -> bool {
        self.source.is_read_only()
    }

    fn supports_trim(&self) -> bool {
        false
    }

    fn reopen(&self, read_only: bool) {
        self.source.reopen(read_only)
    }
}

#[cfg(test)]
mod tests {
    use super::RangedDevice;
    use std::sync::Arc;
    use storage_device::Device;
    use storage_device::fake_device::FakeDevice;

    #[fuchsia::test]
    async fn test_ranged_device_reads() {
        const BLOCK_SIZE: usize = 512;
        let device = Arc::new(FakeDevice::new(8, BLOCK_SIZE as u32));

        let mut buffer = device.allocate_buffer(BLOCK_SIZE).await;
        buffer.as_mut_slice().copy_from_slice(&[1; 512]);
        device.write(BLOCK_SIZE as u64, buffer.as_ref()).await.expect("failed to write to device");

        buffer.as_mut_slice().copy_from_slice(&[2; 512]);
        device
            .write(2 * BLOCK_SIZE as u64, buffer.as_ref())
            .await
            .expect("failed to write to device");

        // Create a RangedDevice starting from block offset one, for three blocks.
        let sub_device =
            RangedDevice::new(device.clone(), 1, 3).expect("failed to create new RangedDevice");

        // Test reading from RangedDevice
        let mut ranged_device_buffer = sub_device.allocate_buffer(BLOCK_SIZE).await;
        sub_device
            .read(0, ranged_device_buffer.as_mut())
            .await
            .expect("failed to read from RangedDevice");
        assert_eq!(ranged_device_buffer.as_slice(), [1; 512]);

        sub_device
            .read(BLOCK_SIZE as u64, ranged_device_buffer.as_mut())
            .await
            .expect("failed to read from RangedDevice");
        assert_eq!(ranged_device_buffer.as_slice(), [2; 512]);

        sub_device
            .read(2 * BLOCK_SIZE as u64, ranged_device_buffer.as_mut())
            .await
            .expect("failed to read from RangedDevice");
        assert_eq!(ranged_device_buffer.as_slice(), [0; 512]);

        sub_device
            .read(3 * BLOCK_SIZE as u64, ranged_device_buffer.as_mut())
            .await
            .expect_err("unexepectedly passed reading out of range of RangedDevice");
    }

    #[fuchsia::test]
    async fn test_ranged_device_writes() {
        const BLOCK_SIZE: usize = 512;
        let device = Arc::new(FakeDevice::new(8, BLOCK_SIZE as u32));

        // Create a RangedDevice starting from block offset one, for three blocks.
        let block_offset = 1;
        let sub_device = RangedDevice::new(device.clone(), block_offset, 3)
            .expect("failed to create new RangedDevice");

        let mut invalid_buffer = sub_device.allocate_buffer(4 * BLOCK_SIZE).await;
        invalid_buffer.as_mut_slice().copy_from_slice(&[3; 2048]);
        sub_device
            .write(0, invalid_buffer.as_ref())
            .await
            .expect_err("unexpectedly passed writing a buffer that is too big");

        let mut write_buffer = sub_device.allocate_buffer(BLOCK_SIZE).await;
        write_buffer.as_mut_slice().copy_from_slice(&[3; 512]);
        let write_block_offset = 2;
        sub_device
            .write(write_block_offset * BLOCK_SIZE as u64, write_buffer.as_ref())
            .await
            .expect("failed to write to RangedDevice");

        // Verify write on underlying device.
        let mut read_buffer = device.allocate_buffer(BLOCK_SIZE).await;
        device
            .read((block_offset + write_block_offset) * BLOCK_SIZE as u64, read_buffer.as_mut())
            .await
            .expect("failed to read from device");
        assert_eq!(read_buffer.as_slice(), [3; 512]);
    }
}
