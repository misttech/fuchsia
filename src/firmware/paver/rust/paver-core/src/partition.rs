// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Traits ([`BoardPartitions`] and [`PartitionClient`]) for board-specific partition mapping and
//! byte-granularity storage I/O.

use async_trait::async_trait;
use fidl_next_fuchsia_io as fio;

/// Board-specific mapper for opening partitions by their logical paver name
/// (e.g. `"zircon_a"`, `"vbmeta_b"`, `"abl_a"`, `"super"`).
#[async_trait(?Send)]
pub trait BoardPartitions {
    /// Opens a partition client for `partition_name` using `block_dir`.
    ///
    /// Returns:
    /// - `Err(zx::Status::NOT_SUPPORTED)` if the partition name is not supported by this board.
    /// - `Err(zx::Status::NOT_FOUND)` if the partition is supported by the board, but cannot be
    ///   found on storage.
    ///
    /// # Single-Slot Firmware Mapping
    ///
    /// Neither `PartitionIo` nor [`crate::data_sink::DataSink`] performs a silent fallback from
    /// `Configuration::B` to `Configuration::A`. For boards that don't have A/B firmware slots,
    /// board backends must map both the `"_a"` and `"_b"` partition names (e.g.
    /// `"bootloader_a" | "bootloader_b"`) to the single underlying target here so `PartitionIo` and
    /// `DataSink` behave identically.
    async fn open_partition(
        &self,
        partition_name: &str,
        block_dir: &fidl_next::ClientEnd<fio::Directory>,
    ) -> Result<Box<dyn PartitionClient>, zx::Status>;

    /// Flushes any board-level buffered state across partitions to persistent storage.
    ///
    /// Standard block-backed [`PartitionClient`] implementations flush their block FIFO session
    /// during [`PartitionClient::write_vmo`]; boards with cross-partition buffered state (such as
    /// Astro buffering multiple sub-partitions in a shared NAND context) can override this method
    /// to flush when `fuchsia.paver.PartitionIo/Flush` is called.
    async fn flush(&self) -> Result<(), zx::Status> {
        Ok(())
    }
}

/// Object-safe partition client interface supporting byte-granularity offsets and payload sizes.
#[async_trait(?Send)]
pub trait PartitionClient {
    /// Returns the total usable size of the partition in bytes.
    fn partition_size(&self) -> Result<u64, zx::Status>;

    /// Reads `size` bytes starting at `dev_offset` on the partition into `vmo` starting at
    /// `vmo_offset`.
    ///
    /// Implementations must return `Err(zx::Status::OUT_OF_RANGE)` if `dev_offset + size` exceeds
    /// [`Self::partition_size`] or (when `size > 0`) `vmo_offset + size` exceeds `vmo.get_size()`
    /// (see [`validate_vmo_range`]).
    async fn read_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status>;

    /// Writes `size` bytes from `vmo` starting at `vmo_offset` to `dev_offset` on the partition,
    /// preserving existing bytes in any leading or trailing partial blocks via read-modify-write.
    ///
    /// Implementations must return `Err(zx::Status::OUT_OF_RANGE)` if `dev_offset + size` exceeds
    /// [`Self::partition_size`] or (when `size > 0`) `vmo_offset + size` exceeds `vmo.get_size()`
    /// (see [`validate_vmo_range`]).
    async fn write_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status>;

    /// Discards (trims) all full blocks starting at the next block boundary at or after `offset`
    /// up to the end of the partition.
    async fn trim_tail(&self, _offset: u64) -> Result<(), zx::Status> {
        Ok(())
    }
}

/// Validates that `[offset, offset + size)` is within `client.partition_size()`.
pub(crate) fn validate_range<P: PartitionClient + ?Sized>(
    client: &P,
    offset: u64,
    size: u64,
) -> Result<(), zx::Status> {
    let partition_size = client.partition_size()?;
    let end_offset = offset.checked_add(size).ok_or(zx::Status::OUT_OF_RANGE)?;
    if end_offset > partition_size {
        return Err(zx::Status::OUT_OF_RANGE);
    }
    Ok(())
}

/// Validates both partition bounds `[dev_offset, dev_offset + size)` and VMO bounds
/// `[vmo_offset, vmo_offset + size)`.
pub(crate) fn validate_vmo_range<P: PartitionClient + ?Sized>(
    client: &P,
    vmo: &zx::Vmo,
    vmo_offset: u64,
    dev_offset: u64,
    size: u64,
) -> Result<(), zx::Status> {
    validate_range(client, dev_offset, size)?;
    if size > 0 {
        let vmo_end = vmo_offset.checked_add(size).ok_or(zx::Status::OUT_OF_RANGE)?;
        if vmo_end > vmo.get_size()? {
            return Err(zx::Status::OUT_OF_RANGE);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    pub(crate) const TEST_BLOCK_SIZE: usize = 512;
    pub(crate) const TEST_PARTITION_SIZE: usize = 2560; // 5 blocks

    /// In-memory [`PartitionClient`] for unit tests.
    #[derive(Clone)]
    pub(crate) struct FakePartitionClient {
        data: Rc<RefCell<Vec<u8>>>,
        block_size: usize,
    }

    impl FakePartitionClient {
        /// Creates a [`FakePartitionClient`] with `size` bytes initialized to `fill` and a
        /// simulated `block_size` for `trim_tail`.
        pub(crate) fn new(size: usize, block_size: usize, fill: u8) -> Self {
            Self { data: Rc::new(RefCell::new(vec![fill; size])), block_size }
        }

        /// Returns a copy of the current in-memory partition contents.
        pub(crate) fn bytes(&self) -> Vec<u8> {
            self.data.borrow().clone()
        }
    }

    #[async_trait(?Send)]
    impl PartitionClient for FakePartitionClient {
        fn partition_size(&self) -> Result<u64, zx::Status> {
            Ok(self.data.borrow().len() as u64)
        }

        async fn read_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
            if size > 0 {
                let start = dev_offset as usize;
                let end = start + size as usize;
                vmo.write(&self.data.borrow()[start..end], vmo_offset)?;
            }
            Ok(())
        }

        async fn write_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
            if size > 0 {
                let info = vmo.info()?;
                if info.flags.contains(zx::VmoInfoFlags::PAGER_BACKED) {
                    assert_ne!(info.parent_koid, zx::Koid::from_raw(0));
                    assert!(info.committed_bytes >= size);
                }
                let start = dev_offset as usize;
                let end = start + size as usize;
                vmo.read(&mut self.data.borrow_mut()[start..end], vmo_offset)?;
            }
            Ok(())
        }

        async fn trim_tail(&self, offset: u64) -> Result<(), zx::Status> {
            validate_range(self, offset, 0)?;
            let aligned_start = (offset as usize).next_multiple_of(self.block_size);
            let mut data = self.data.borrow_mut();
            if aligned_start < data.len() {
                data[aligned_start..].fill(0xab);
            }
            Ok(())
        }
    }

    /// In-memory [`BoardPartitions`] for unit tests.
    pub(crate) struct FakeBoardPartitions {
        partitions: RefCell<HashMap<String, Option<FakePartitionClient>>>,
        pub(crate) flushes: Rc<Cell<u32>>,
    }

    impl FakeBoardPartitions {
        /// Creates an empty [`FakeBoardPartitions`].
        pub(crate) fn new() -> Self {
            Self { partitions: RefCell::new(HashMap::new()), flushes: Rc::new(Cell::new(0)) }
        }

        /// Registers an in-memory partition named `name` filled with `fill`.
        pub(crate) fn add_partition(&self, name: &str, fill: u8) -> FakePartitionClient {
            self.add_partition_with_size(name, TEST_PARTITION_SIZE, fill)
        }

        /// Registers an in-memory partition named `name` with `size` bytes filled with `fill`.
        pub(crate) fn add_partition_with_size(
            &self,
            name: &str,
            size: usize,
            fill: u8,
        ) -> FakePartitionClient {
            let client = FakePartitionClient::new(size, TEST_BLOCK_SIZE, fill);
            self.partitions.borrow_mut().insert(name.to_string(), Some(client.clone()));
            client
        }

        /// Registers `name` as a supported partition name that is missing on storage.
        pub(crate) fn add_missing_partition(&self, name: &str) {
            self.partitions.borrow_mut().insert(name.to_string(), None);
        }
    }

    #[async_trait(?Send)]
    impl BoardPartitions for FakeBoardPartitions {
        async fn open_partition(
            &self,
            partition_name: &str,
            _block_dir: &fidl_next::ClientEnd<fio::Directory>,
        ) -> Result<Box<dyn PartitionClient>, zx::Status> {
            match self.partitions.borrow().get(partition_name) {
                Some(Some(client)) => Ok(Box::new(client.clone())),
                Some(None) => Err(zx::Status::NOT_FOUND),
                None => Err(zx::Status::NOT_SUPPORTED),
            }
        }

        async fn flush(&self) -> Result<(), zx::Status> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }

    /// Verifies unaligned VMO reads and writes and out-of-range bounds validation.
    #[fuchsia::test]
    async fn test_vmo_read_and_write_unaligned_and_out_of_range() {
        let client = FakePartitionClient::new(4096, 512, 0x33);

        let payload: Vec<u8> = (0..1234).map(|i| ((i * 5 + 1) % 251) as u8).collect();
        let vmo = zx::Vmo::create(4096).expect("create vmo");
        vmo.write(&payload, 17).expect("write vmo at offset 17");

        // Write 1234 bytes from vmo offset 17 to device offset 333.
        client.write_vmo(&vmo, 17, 333, 1234).await.expect("write_vmo unaligned");

        let read_vmo = zx::Vmo::create(4096).expect("create read vmo");
        client.read_vmo(&read_vmo, 29, 333, 1234).await.expect("read_vmo unaligned");
        let mut read_back = vec![0u8; 1234];
        read_vmo.read(&mut read_back, 29).expect("read from vmo");
        assert_eq!(read_back, payload);

        // Surrounding bytes outside [333..1567) are untouched.
        let full = client.bytes();
        assert_eq!(&full[..333], &vec![0x33u8; 333][..]);
        assert_eq!(&full[333..1567], &payload[..]);
        assert_eq!(&full[1567..], &vec![0x33u8; 4096 - 1567][..]);

        // Out-of-range and u64::MAX overflow checks on partition and VMO bounds.
        assert_eq!(client.write_vmo(&vmo, 0, 4000, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 0, 4000, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, 4000, 0, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 4000, 0, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, 0, u64::MAX, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 0, u64::MAX, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, u64::MAX, 0, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, u64::MAX, 0, 1).await, Err(zx::Status::OUT_OF_RANGE));
    }
}
