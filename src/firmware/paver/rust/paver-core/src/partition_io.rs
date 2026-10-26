// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Server implementation for the `fuchsia.paver.PartitionIo` FIDL protocol, providing scoped
//! byte-granularity partition I/O backed by [`BoardPartitions`].

use crate::ResponderExt as _;
use crate::partition::{BoardPartitions, PartitionClient};
use anyhow::{Context as _, Error};
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_io as fio;
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_paver::partition_io;
use std::fmt;

/// Extracts the underlying [`zx::Status`] from an [`anyhow::Error`], defaulting to
/// [`zx::Status::INTERNAL`] if no [`zx::Status`] is in the error chain.
fn status_from_error(err: &Error) -> zx::Status {
    err.downcast_ref::<zx::Status>().copied().unwrap_or(zx::Status::INTERNAL)
}

/// `fuchsia.paver.PartitionIo` implementation backed by a board's [`BoardPartitions`] and a
/// caller-supplied `block_dir`.
pub struct PartitionIo {
    board_partitions: Box<dyn BoardPartitions>,
    block_dir: fidl_next::ClientEnd<fio::Directory>,
}

impl fmt::Debug for PartitionIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartitionIo").finish_non_exhaustive()
    }
}

impl PartitionIo {
    /// Creates a [`PartitionIo`] backed by `board_partitions` and a caller-supplied `block_dir`.
    pub fn new(
        board_partitions: Box<dyn BoardPartitions>,
        block_dir: fidl_next::ClientEnd<fio::Directory>,
    ) -> Self {
        Self { board_partitions, block_dir }
    }

    /// Opens `partition` through `self.board_partitions` using `self.block_dir`.
    async fn open_partition(
        &self,
        partition: &str,
    ) -> Result<Box<dyn PartitionClient>, zx::Status> {
        self.board_partitions.open_partition(partition, &self.block_dir).await
    }

    /// Returns the total size in bytes of `partition`.
    pub async fn get_partition_size(&self, partition: &str) -> Result<u64, zx::Status> {
        let client = self.open_partition(partition).await?;
        client.partition_size()
    }

    /// Reads `size` bytes from `partition` starting at byte `offset` into a newly allocated VMO.
    pub async fn read_partition(
        &self,
        partition: &str,
        offset: u64,
        size: u64,
    ) -> Result<zx::Vmo, zx::Status> {
        let result = self.read_partition_inner(partition, offset, size).await;
        match &result {
            Ok(_) => log::info!(partition, offset, size; "Partition read successfully"),
            Err(err) => log::warn!(partition, offset, size; "Partition read error: {err:#}"),
        }
        result.map_err(|err| status_from_error(&err))
    }

    /// Opens `partition`, allocates a VMO of `size` bytes, and reads `[offset, offset + size)` into
    /// it, attaching descriptive [`anyhow::Context`] on failure.
    async fn read_partition_inner(
        &self,
        partition: &str,
        offset: u64,
        size: u64,
    ) -> Result<zx::Vmo, Error> {
        if size == 0 {
            return Err(zx::Status::INVALID_ARGS).context("read size must be non-zero");
        }
        let client =
            self.open_partition(partition).await.context("failed to open partition for read")?;
        let vmo = zx::Vmo::create(size).context("failed to allocate VMO for partition read")?;
        client
            .read_vmo(&vmo, 0, offset, size)
            .await
            .context("failed to read partition into VMO")?;
        Ok(vmo)
    }

    /// Writes `payload` to `partition` starting at byte `offset`, handling bytes past the written
    /// range according to `trailing`.
    pub async fn write_partition(
        &self,
        partition: &str,
        offset: u64,
        payload: zx::Vmo,
        trailing: fpaver::TrailingBehavior,
    ) -> Result<(), zx::Status> {
        let size = match payload.get_content_size() {
            Ok(size) => size,
            Err(status) => {
                log::warn!(
                    partition,
                    offset;
                    "Partition write error: failed to get VMO content size: {status}"
                );
                return Err(status);
            }
        };
        let result = self.write_partition_inner(partition, offset, &payload, size, trailing).await;
        match &result {
            Ok(()) => log::info!(partition, offset, size; "Partition written successfully"),
            Err(err) => log::warn!(partition, offset, size; "Partition write error: {err:#}"),
        }
        result.map_err(|err| status_from_error(&err))
    }

    /// Opens `partition` and writes `size` bytes from `payload` at `offset` according to
    /// `trailing`, attaching descriptive [`anyhow::Context`] on failure.
    async fn write_partition_inner(
        &self,
        partition: &str,
        offset: u64,
        payload: &zx::Vmo,
        size: u64,
        trailing: fpaver::TrailingBehavior,
    ) -> Result<(), Error> {
        let client =
            self.open_partition(partition).await.context("failed to open partition for write")?;
        match trailing {
            fpaver::TrailingBehavior::Retain => {
                client
                    .write_vmo(payload, 0, offset, size)
                    .await
                    .context("failed to write partition VMO")?;
            }
            fpaver::TrailingBehavior::Discard => {
                client
                    .write_vmo(payload, 0, offset, size)
                    .await
                    .context("failed to write partition VMO")?;
                // `write_vmo` validates that `offset + size <= partition_size`.
                let _ = client.trim_tail(offset + size).await;
            }
            fpaver::TrailingBehavior::UnknownOrdinal_(_) => {
                return Err(zx::Status::INVALID_ARGS).context("unknown TrailingBehavior ordinal");
            }
        }
        Ok(())
    }

    /// Flushes all previously written partition data to persistent storage.
    ///
    /// Standard block-backed partitions open and flush a [`PartitionClient`] during each
    /// [`write_partition`](Self::write_partition) call rather than keeping block FIFO sessions
    /// open across calls. This method delegates to [`BoardPartitions::flush`] so boards that
    /// maintain shared buffered state across partitions can flush when `PartitionIo::Flush` is
    /// called.
    pub async fn flush(&self) -> Result<(), zx::Status> {
        self.board_partitions.flush().await
    }
}

impl fpaver::PartitionIoLocalServerHandler for PartitionIo {
    async fn get_partition_size(
        &mut self,
        request: Request<partition_io::GetPartitionSize>,
        responder: Responder<partition_io::GetPartitionSize>,
    ) {
        let res = Self::get_partition_size(self, &request.payload().partition).await;
        responder.send_result(res).await;
    }

    async fn read_partition(
        &mut self,
        request: Request<partition_io::ReadPartition>,
        responder: Responder<partition_io::ReadPartition>,
    ) {
        let req = request.payload();
        let res = Self::read_partition(self, &req.partition, req.offset, req.size).await;
        responder.send_result(res).await;
    }

    async fn write_partition(
        &mut self,
        request: Request<partition_io::WritePartition>,
        responder: Responder<partition_io::WritePartition>,
    ) {
        let req = request.payload();
        let res =
            Self::write_partition(self, &req.partition, req.offset, req.payload, req.trailing)
                .await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<partition_io::Flush>) {
        let res = Self::flush(self).await;
        responder.send_result(res).await;
    }
}

/// Serves the `fuchsia.paver.PartitionIo` FIDL protocol over `server_end` using `partition_io`.
pub async fn serve_partition_io(
    partition_io: PartitionIo,
    server_end: fidl_next::ServerEnd<fpaver::PartitionIo>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(partition_io)
        .await
        .map(|_| ())
        .context("serving PartitionIo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::tests::FakePartitionClient;
    use async_trait::async_trait;
    use fuchsia_async as fasync;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    const TEST_BLOCK_SIZE: usize = 512;
    const TEST_PARTITION_SIZE: usize = 2560; // 5 blocks

    struct FakeBoardPartitions {
        partitions: RefCell<HashMap<String, Option<FakePartitionClient>>>,
        flushes: Rc<Cell<u32>>,
    }

    impl FakeBoardPartitions {
        /// Creates an empty [`FakeBoardPartitions`].
        fn new() -> Self {
            Self { partitions: RefCell::new(HashMap::new()), flushes: Rc::new(Cell::new(0)) }
        }

        /// Registers an in-memory partition named `name` filled with `fill`.
        fn add_partition(&self, name: &str, fill: u8) -> FakePartitionClient {
            self.add_partition_with_size(name, TEST_PARTITION_SIZE, fill)
        }

        /// Registers an in-memory partition named `name` with `size` bytes filled with `fill`.
        fn add_partition_with_size(
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
        fn add_missing_partition(&self, name: &str) {
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

    /// Creates a VMO containing `data` with its `ZX_PROP_VMO_CONTENT_SIZE` set to `data.len()`.
    fn make_vmo(data: &[u8]) -> zx::Vmo {
        let vmo = zx::Vmo::create(data.len().max(64) as u64).expect("create vmo");
        vmo.write(data, 0).expect("write vmo");
        vmo.set_content_size(&(data.len() as u64)).expect("set content size");
        vmo
    }

    #[fuchsia::test]
    async fn test_read_and_write_partition() {
        let board_partitions = FakeBoardPartitions::new();
        let raw = board_partitions.add_partition("raw_part", 0xaa);
        let _large = board_partitions.add_partition_with_size("large_part", 200 * 1024, 0xcc);
        board_partitions.add_missing_partition("missing_part");
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board_partitions), block_dir_client);

        let total_size = TEST_PARTITION_SIZE as u64;
        assert_eq!(pio.get_partition_size("raw_part").await, Ok(total_size));
        assert_eq!(pio.get_partition_size("missing_part").await, Err(zx::Status::NOT_FOUND));
        assert_eq!(pio.get_partition_size("unknown_part").await, Err(zx::Status::NOT_SUPPORTED));

        // Write 600 bytes at unaligned offset 100 with TrailingBehavior::Retain.
        let payload: Vec<u8> = (0..600).map(|i| ((i * 7 + 3) % 251) as u8).collect();
        pio.write_partition("raw_part", 100, make_vmo(&payload), fpaver::TrailingBehavior::Retain)
            .await
            .expect("write_partition at unaligned offset 100");

        let disk = raw.bytes();
        assert_eq!(&disk[..100], &[0xaau8; 100]);
        assert_eq!(&disk[100..700], &payload[..]);
        assert_eq!(&disk[700..], &[0xaau8; TEST_PARTITION_SIZE - 700]);

        // Read back exact 600 bytes at offset 100.
        let read_exact =
            pio.read_partition("raw_part", 100, 600).await.expect("read_partition 600 bytes");
        assert_eq!(read_exact.get_content_size().expect("content size"), 600);
        let mut read_vec = vec![0u8; 600];
        read_exact.read(&mut read_vec, 0).expect("read vmo");
        assert_eq!(read_vec, payload);

        // size == 0 is rejected with INVALID_ARGS.
        assert_eq!(
            pio.read_partition("raw_part", 100, 0).await.err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // Write 600 bytes at offset 0 with TrailingBehavior::Discard: preserves [600..1024) in the
        // trailing partial block and trims [1024..total_size) (filled with 0xab by
        // FakePartitionClient).
        pio.write_partition("raw_part", 0, make_vmo(&payload), fpaver::TrailingBehavior::Discard)
            .await
            .expect("write_partition with discard 600 bytes");
        let disk_after_discard = raw.bytes();
        assert_eq!(&disk_after_discard[..600], &payload[..]);
        assert_eq!(&disk_after_discard[600..700], &payload[500..600]);
        assert_eq!(&disk_after_discard[700..1024], &[0xaau8; 324]);
        assert_eq!(&disk_after_discard[1024..], &[0xabu8; TEST_PARTITION_SIZE - 1024]);

        // Zero-length payload with Retain is a no-op; with Discard it trims from the next block
        // boundary.
        pio.write_partition("raw_part", 0, make_vmo(&[]), fpaver::TrailingBehavior::Retain)
            .await
            .expect("write_partition empty retain");
        pio.write_partition("raw_part", 0, make_vmo(&[]), fpaver::TrailingBehavior::Discard)
            .await
            .expect("write_partition empty discard");
        assert_eq!(&raw.bytes()[..], &[0xabu8; TEST_PARTITION_SIZE]);

        // Out-of-range and u64::MAX overflow checks for read_partition and write_partition.
        assert_eq!(
            pio.read_partition("raw_part", total_size, 1).await.err(),
            Some(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.read_partition("raw_part", u64::MAX, 1).await.err(),
            Some(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.write_partition(
                "raw_part",
                total_size - 100,
                make_vmo(&payload),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.write_partition(
                "raw_part",
                u64::MAX,
                make_vmo(&payload),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );

        // Large (140 KiB) write_partition and read_partition.
        let large_payload: Vec<u8> = (0..140_000).map(|i| ((i * 11 + 5) % 251) as u8).collect();
        pio.write_partition(
            "large_part",
            300,
            make_vmo(&large_payload),
            fpaver::TrailingBehavior::Discard,
        )
        .await
        .expect("large write_partition");
        let read_large = pio
            .read_partition("large_part", 300, large_payload.len() as u64)
            .await
            .expect("large read_partition");
        let mut read_large_vec = vec![0u8; large_payload.len()];
        read_large.read(&mut read_large_vec, 0).expect("read large vmo");
        assert_eq!(read_large_vec, large_payload);
    }

    #[fuchsia::test]
    async fn test_serve_partition_io_fidl() {
        let board_partitions = FakeBoardPartitions::new();
        let board_flushes = board_partitions.flushes.clone();
        let vbmeta_a = board_partitions.add_partition("vbmeta_a", 0xbb);
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board_partitions), block_dir_client);

        let scope = fasync::Scope::new_with_name("test_partition_io");
        let (pio_client, pio_server) = fidl_next::fuchsia::create_channel::<fpaver::PartitionIo>();
        let pio_client = pio_client.spawn();
        scope.spawn_local(async move {
            serve_partition_io(pio, pio_server).await.expect("serve_partition_io");
        });

        let total_size = TEST_PARTITION_SIZE as u64;
        assert_eq!(
            pio_client.get_partition_size("vbmeta_a").await.expect("fidl").map(|r| r.size),
            Ok(total_size)
        );
        assert_eq!(
            pio_client.get_partition_size("unsupported_part").await.expect("fidl").map(|r| r.size),
            Err(zx::Status::NOT_SUPPORTED)
        );

        let payload: Vec<u8> = (0..600).map(|i| ((i * 5 + 9) % 251) as u8).collect();
        assert_eq!(
            pio_client
                .write_partition(
                    "vbmeta_a",
                    200,
                    make_vmo(&payload),
                    fpaver::TrailingBehavior::Retain,
                )
                .await
                .expect("write_partition fidl"),
            Ok(())
        );
        assert_eq!(pio_client.flush().await.expect("flush fidl"), Ok(()));
        assert_eq!(board_flushes.get(), 1);

        let read_vmo = pio_client
            .read_partition("vbmeta_a", 200, 600)
            .await
            .expect("read_partition fidl")
            .expect("read_partition ok")
            .payload;
        let mut read_bytes = vec![0u8; 600];
        read_vmo.read(&mut read_bytes, 0).expect("read vmo");
        assert_eq!(read_bytes, payload);

        let disk = vbmeta_a.bytes();
        assert_eq!(&disk[..200], &[0xbbu8; 200]);
        assert_eq!(&disk[200..800], &payload[..]);
        assert_eq!(&disk[800..1024], &[0xbbu8; 224]);

        drop(pio_client);
        scope.cancel().await;
    }
}
