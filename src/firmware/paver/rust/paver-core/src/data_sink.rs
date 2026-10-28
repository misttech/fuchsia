// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Server implementation for the legacy `fuchsia.paver.DataSink` and
//! `fuchsia.paver.DynamicDataSink` FIDL protocols backed by [`BoardPartitions`].

use crate::partition::{BoardPartitions, PartitionClient, validate_vmo_range};
use crate::{ResponderExt as _, status_from_error};
use anyhow::{Context as _, Error};
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_io as fio;
use fidl_next_fuchsia_mem as fmem;
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_paver::{data_sink, dynamic_data_sink};
use std::fmt;

/// Standard logical partition names used by [`DataSink`].
pub const ZIRCON_A_PARTITION: &str = "zircon_a";
pub const ZIRCON_B_PARTITION: &str = "zircon_b";
pub const ZIRCON_R_PARTITION: &str = "zircon_r";
pub const VBMETA_A_PARTITION: &str = "vbmeta_a";
pub const VBMETA_B_PARTITION: &str = "vbmeta_b";
pub const VBMETA_R_PARTITION: &str = "vbmeta_r";
pub const BOOTLOADER_A_PARTITION: &str = "bootloader_a";
pub const BOOTLOADER_B_PARTITION: &str = "bootloader_b";
pub const BOOTLOADER_R_PARTITION: &str = "bootloader_r";

/// Legacy `fuchsia.paver.DataSink` and `fuchsia.paver.DynamicDataSink` implementation backed by a
/// board's [`BoardPartitions`] and `/block` directory.
pub struct DataSink {
    board_partitions: Box<dyn BoardPartitions>,
    block_dir: fidl_next::ClientEnd<fio::Directory>,
}

impl fmt::Debug for DataSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataSink").finish_non_exhaustive()
    }
}

impl DataSink {
    /// Creates a [`DataSink`] backed by `board_partitions` and `block_dir`.
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

    /// Logs the outcome of reading `partition`, logging `NOT_SUPPORTED` at `info` level and other
    /// errors at `warn` level.
    fn log_read_result(partition: &str, result: &Result<(zx::Vmo, u64), Error>) {
        match result {
            Ok((_, size)) => log::info!(partition, size = *size; "Partition read successfully"),
            Err(err) if status_from_error(err) == zx::Status::NOT_SUPPORTED => {
                log::info!(partition; "Partition read not supported: {err:#}");
            }
            Err(err) => log::warn!(partition; "Partition read error: {err:#}"),
        }
    }

    /// Reads the entire contents of `partition` into a newly allocated VMO, returning the VMO and
    /// partition size in bytes.
    async fn read_full_partition(&self, partition: &str) -> Result<(zx::Vmo, u64), zx::Status> {
        let result = async {
            let client = self
                .open_partition(partition)
                .await
                .context("failed to open partition for read")?;
            Self::read_client_to_vmo(client.as_ref()).await
        }
        .await;
        Self::log_read_result(partition, &result);
        result.map_err(|err| status_from_error(&err))
    }

    /// Allocates a VMO of `client`'s full size and reads the partition into it, attaching
    /// descriptive [`anyhow::Context`] on failure.
    async fn read_client_to_vmo(client: &dyn PartitionClient) -> Result<(zx::Vmo, u64), Error> {
        let partition_size =
            client.partition_size().context("failed to query partition size for read")?;
        if partition_size == 0 {
            return Err(zx::Status::INVALID_ARGS).context("partition size must be non-zero");
        }
        let vmo =
            zx::Vmo::create(partition_size).context("failed to allocate VMO for partition read")?;
        client
            .read_vmo(&vmo, 0, 0, partition_size)
            .await
            .context("failed to read partition into VMO")?;
        Ok((vmo, partition_size))
    }

    /// Writes `size` bytes from `vmo` starting at offset 0 into `client` and best-effort trims
    /// trailing full blocks, attaching descriptive [`anyhow::Context`] on failure.
    ///
    /// Note: Partial-block trailing bytes in `[size, next_block_boundary)` are preserved via
    /// [`PartitionClient::write_vmo`] read-modify-write, and full trailing blocks are best-effort
    /// discarded via [`PartitionClient::trim_tail`], matching [`crate::partition_io::PartitionIo`]
    /// `TrailingBehavior::Discard` (rather than zero-filling the remainder of the partition).
    async fn write_client_from_vmo(
        client: &dyn PartitionClient,
        vmo: &zx::Vmo,
        size: u64,
    ) -> Result<(), Error> {
        if size == 0 {
            return Err(zx::Status::INVALID_ARGS).context("payload size must be non-zero");
        }
        validate_vmo_range(client, vmo, 0, 0, size)
            .context("payload exceeds partition or VMO bounds")?;

        // If `vmo` is pager-backed (e.g. from `fxblob`), create a local COW clone and commit its
        // pages before passing it to the block client so page faults cannot deadlock the storage
        // server mid-write and committed pages cannot be evicted by the pager source.
        let info = vmo.info().context("failed to get payload VMO info")?;
        let cow_vmo;
        let target_vmo = if info.flags.contains(zx::VmoInfoFlags::PAGER_BACKED) {
            cow_vmo = vmo
                .create_child(zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE, 0, size)
                .context("failed to create COW clone of pager-backed payload VMO")?;
            &cow_vmo
        } else {
            vmo
        };
        target_vmo.op_range(zx::VmoOp::COMMIT, 0, size).context("failed to commit payload VMO")?;

        client.write_vmo(target_vmo, 0, 0, size).await.context("failed to write partition VMO")?;
        let _ = client.trim_tail(size).await;
        Ok(())
    }

    /// Reads the boot `asset` (kernel ZBI or verified boot metadata) for `configuration`.
    pub async fn read_asset(
        &self,
        configuration: fpaver::Configuration,
        asset: fpaver::Asset,
    ) -> Result<fmem::Buffer, zx::Status> {
        let partition = asset_partition_name(configuration, asset);
        let (vmo, partition_size) = self.read_full_partition(partition).await?;
        let size = extract_zbi_asset_size(&vmo, partition_size);
        Ok(fmem::Buffer { vmo, size })
    }

    /// Writes `payload` to the boot `asset` partition for `configuration`.
    pub async fn write_asset(
        &self,
        configuration: fpaver::Configuration,
        asset: fpaver::Asset,
        payload: fmem::Buffer,
    ) -> Result<(), zx::Status> {
        let partition = asset_partition_name(configuration, asset);
        let size = payload.size;
        let result = async {
            let client = self
                .open_partition(partition)
                .await
                .context("failed to open partition for write")?;
            Self::write_client_from_vmo(client.as_ref(), &payload.vmo, size).await
        }
        .await;
        match &result {
            Ok(()) => log::info!(partition, size; "Partition written successfully"),
            Err(err) => log::warn!(partition, size; "Partition write error: {err:#}"),
        }
        result.map_err(|err| status_from_error(&err))
    }

    /// Writes `payload` to the firmware partition of `firmware_type` for `configuration`.
    ///
    /// Note: Unlike legacy C++ `DataSinkImpl::GetFirmwarePartitionSpec`, this method intentionally
    /// does **not** fall back from `Configuration::B` to `Configuration::A` on `NOT_SUPPORTED`.
    /// Because `PartitionIo` callers address `"{type}_a"` and `"{type}_b"` directly without going
    /// through `DataSink`, boards with single-slotted firmware map both `"_a"` and `"_b"` variants
    /// to the single underlying slot in [`BoardPartitions::open_partition`].
    pub async fn write_firmware(
        &self,
        configuration: fpaver::Configuration,
        firmware_type: String,
        payload: fmem::Buffer,
    ) -> fpaver::WriteFirmwareResult {
        let partition = firmware_partition_name(configuration, &firmware_type);
        let size = payload.size;
        let client = match self.open_partition(&partition).await {
            Ok(client) => client,
            Err(zx::Status::NOT_SUPPORTED) => {
                log::info!(
                    partition = partition.as_str(),
                    firmware_type = firmware_type.as_str();
                    "Skipping unsupported firmware"
                );
                return fpaver::WriteFirmwareResult::Unsupported(true);
            }
            Err(status) => {
                log::warn!(
                    partition = partition.as_str(),
                    size,
                    status:?;
                    "Failed to open firmware partition for write"
                );
                return fpaver::WriteFirmwareResult::Status(Err(status));
            }
        };
        match Self::write_client_from_vmo(client.as_ref(), &payload.vmo, size).await {
            Ok(()) => {
                log::info!(partition = partition.as_str(), size; "Partition written successfully");
                fpaver::WriteFirmwareResult::Status(Ok(()))
            }
            Err(err) => {
                let status = status_from_error(&err);
                log::warn!(
                    partition = partition.as_str(),
                    size;
                    "Partition write error: {err:#}"
                );
                fpaver::WriteFirmwareResult::Status(Err(status))
            }
        }
    }

    /// Reads the firmware image of `firmware_type` for `configuration`.
    ///
    /// Note: Single-slotted firmware is mapped for both `"_a"` and `"_b"` in
    /// [`BoardPartitions::open_partition`]; `DataSink` does not fall back from `Configuration::B`
    /// to `Configuration::A`.
    pub async fn read_firmware(
        &self,
        configuration: fpaver::Configuration,
        firmware_type: String,
    ) -> Result<fmem::Buffer, zx::Status> {
        let partition = firmware_partition_name(configuration, &firmware_type);
        let (vmo, partition_size) = self.read_full_partition(&partition).await?;
        let size = extract_zbi_asset_size(&vmo, partition_size);
        Ok(fmem::Buffer { vmo, size })
    }

    /// Writes a raw FVM/Fxfs image `payload` to the system volume partition.
    pub async fn write_opaque_volume(&self, _payload: fmem::Buffer) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes an Android sparse image `payload` to the system volume partition.
    pub async fn write_sparse_volume(&self, _payload: fmem::Buffer) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Flushes all previously written partition data to persistent storage.
    pub async fn flush(&self) -> Result<(), zx::Status> {
        self.board_partitions.flush().await
    }

    /// Initializes the device partition tables.
    pub async fn initialize_partition_tables(&self) -> Result<(), zx::Status> {
        log::error!("Initializing partitions is not supported");
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Wipes all entries from the device partition tables.
    pub async fn wipe_partition_tables(&self) -> Result<(), zx::Status> {
        log::error!("Wiping partition tables is not supported");
        Err(zx::Status::NOT_SUPPORTED)
    }
}

/// FIDL server handler wrapper around [`DataSink`] so FIDL dispatch traits remain private.
struct DataSinkHandler(DataSink);

impl fpaver::DataSinkLocalServerHandler for DataSinkHandler {
    async fn read_asset(
        &mut self,
        request: Request<data_sink::ReadAsset>,
        responder: Responder<data_sink::ReadAsset>,
    ) {
        let req = request.payload();
        let res = self.0.read_asset(req.configuration, req.asset).await;
        responder.send_result(res).await;
    }

    async fn write_asset(
        &mut self,
        request: Request<data_sink::WriteAsset>,
        responder: Responder<data_sink::WriteAsset>,
    ) {
        let req = request.payload();
        let res = self.0.write_asset(req.configuration, req.asset, req.payload).await;
        responder.send_response(res).await;
    }

    async fn write_firmware(
        &mut self,
        request: Request<data_sink::WriteFirmware>,
        responder: Responder<data_sink::WriteFirmware>,
    ) {
        let req = request.payload();
        let res = self.0.write_firmware(req.configuration, req.type_, req.payload).await;
        responder.send_response(res).await;
    }

    async fn read_firmware(
        &mut self,
        request: Request<data_sink::ReadFirmware>,
        responder: Responder<data_sink::ReadFirmware>,
    ) {
        let req = request.payload();
        let res = self.0.read_firmware(req.configuration, req.type_).await;
        responder.send_result(res).await;
    }

    async fn write_opaque_volume(
        &mut self,
        request: Request<data_sink::WriteOpaqueVolume>,
        responder: Responder<data_sink::WriteOpaqueVolume>,
    ) {
        let req = request.payload();
        let res = self.0.write_opaque_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn write_sparse_volume(
        &mut self,
        request: Request<data_sink::WriteSparseVolume>,
        responder: Responder<data_sink::WriteSparseVolume>,
    ) {
        let req = request.payload();
        let res = self.0.write_sparse_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<data_sink::Flush>) {
        let res = self.0.flush().await;
        responder.send_response(res).await;
    }
}

impl fpaver::DynamicDataSinkLocalServerHandler for DataSinkHandler {
    async fn read_asset(
        &mut self,
        request: Request<dynamic_data_sink::ReadAsset>,
        responder: Responder<dynamic_data_sink::ReadAsset>,
    ) {
        let req = request.payload();
        let res = self.0.read_asset(req.configuration, req.asset).await;
        responder.send_result(res).await;
    }

    async fn write_asset(
        &mut self,
        request: Request<dynamic_data_sink::WriteAsset>,
        responder: Responder<dynamic_data_sink::WriteAsset>,
    ) {
        let req = request.payload();
        let res = self.0.write_asset(req.configuration, req.asset, req.payload).await;
        responder.send_response(res).await;
    }

    async fn write_firmware(
        &mut self,
        request: Request<dynamic_data_sink::WriteFirmware>,
        responder: Responder<dynamic_data_sink::WriteFirmware>,
    ) {
        let req = request.payload();
        let res = self.0.write_firmware(req.configuration, req.type_, req.payload).await;
        responder.send_response(res).await;
    }

    async fn read_firmware(
        &mut self,
        request: Request<dynamic_data_sink::ReadFirmware>,
        responder: Responder<dynamic_data_sink::ReadFirmware>,
    ) {
        let req = request.payload();
        let res = self.0.read_firmware(req.configuration, req.type_).await;
        responder.send_result(res).await;
    }

    async fn write_opaque_volume(
        &mut self,
        request: Request<dynamic_data_sink::WriteOpaqueVolume>,
        responder: Responder<dynamic_data_sink::WriteOpaqueVolume>,
    ) {
        let req = request.payload();
        let res = self.0.write_opaque_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn write_sparse_volume(
        &mut self,
        request: Request<dynamic_data_sink::WriteSparseVolume>,
        responder: Responder<dynamic_data_sink::WriteSparseVolume>,
    ) {
        let req = request.payload();
        let res = self.0.write_sparse_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<dynamic_data_sink::Flush>) {
        let res = self.0.flush().await;
        responder.send_response(res).await;
    }

    async fn initialize_partition_tables(
        &mut self,
        responder: Responder<dynamic_data_sink::InitializePartitionTables>,
    ) {
        let res = self.0.initialize_partition_tables().await;
        responder.send_response(res).await;
    }

    async fn wipe_partition_tables(
        &mut self,
        responder: Responder<dynamic_data_sink::WipePartitionTables>,
    ) {
        let res = self.0.wipe_partition_tables().await;
        responder.send_response(res).await;
    }
}

/// Serves the `fuchsia.paver.DataSink` FIDL protocol over `server_end` using `sink`.
pub async fn serve_data_sink(
    sink: DataSink,
    server_end: fidl_next::ServerEnd<fpaver::DataSink>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(DataSinkHandler(sink))
        .await
        .map(|_| ())
        .context("serving DataSink")
}

/// Serves the `fuchsia.paver.DynamicDataSink` FIDL protocol over `server_end` using `sink`.
pub async fn serve_dynamic_data_sink(
    sink: DataSink,
    server_end: fidl_next::ServerEnd<fpaver::DynamicDataSink>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(DataSinkHandler(sink))
        .await
        .map(|_| ())
        .context("serving DynamicDataSink")
}

/// Returns the slot suffix (`"a"`, `"b"`, or `"r"`) for `configuration`.
fn configuration_suffix(configuration: fpaver::Configuration) -> &'static str {
    match configuration {
        fpaver::Configuration::A => "a",
        fpaver::Configuration::B => "b",
        fpaver::Configuration::Recovery => "r",
    }
}

/// Maps a `(configuration, asset)` pair to its standard logical partition name.
fn asset_partition_name(
    configuration: fpaver::Configuration,
    asset: fpaver::Asset,
) -> &'static str {
    match (configuration, asset) {
        (fpaver::Configuration::A, fpaver::Asset::Kernel) => ZIRCON_A_PARTITION,
        (fpaver::Configuration::B, fpaver::Asset::Kernel) => ZIRCON_B_PARTITION,
        (fpaver::Configuration::Recovery, fpaver::Asset::Kernel) => ZIRCON_R_PARTITION,
        (fpaver::Configuration::A, fpaver::Asset::VerifiedBootMetadata) => VBMETA_A_PARTITION,
        (fpaver::Configuration::B, fpaver::Asset::VerifiedBootMetadata) => VBMETA_B_PARTITION,
        (fpaver::Configuration::Recovery, fpaver::Asset::VerifiedBootMetadata) => {
            VBMETA_R_PARTITION
        }
    }
}

/// Maps a `(configuration, firmware_type)` pair to its logical firmware partition name.
///
/// Both `""` and `"bootloader"` map to `"bootloader_{a,b,r}"`, matching `system-updater` and board
/// partition configuration conventions.
fn firmware_partition_name(configuration: fpaver::Configuration, firmware_type: &str) -> String {
    let base_name = if firmware_type.is_empty() { "bootloader" } else { firmware_type };
    let suffix = configuration_suffix(configuration);
    format!("{base_name}_{suffix}")
}

/// Attempts to parse `vmo` (within `partition_size` bytes) as a [`zbi::ZbiContainer`], returning
/// the ZBI container size if valid or `partition_size` otherwise.
fn extract_zbi_asset_size(vmo: &zx::Vmo, partition_size: u64) -> u64 {
    try_extract_zbi_size(vmo, partition_size).unwrap_or(partition_size)
}

/// Maps `vmo` read-only and attempts to parse `[..partition_size]` as a [`zbi::ZbiContainer`],
/// returning its parsed byte length if valid.
fn try_extract_zbi_size(vmo: &zx::Vmo, partition_size: u64) -> Option<u64> {
    let len = usize::try_from(partition_size).ok()?;
    let mapping = mapped_vmo::ImmutableMapping::create_from_vmo(vmo, false).ok()?;
    let container = zbi::ZbiContainer::parse(mapping.get(..len)?).ok()?;
    u64::try_from(container.container_size().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::tests::{FakeBoardPartitions, TEST_PARTITION_SIZE};
    use async_trait::async_trait;
    use fuchsia_async as fasync;

    /// Creates an [`fmem::Buffer`] containing `data` with `size` set to `data.len()`.
    fn make_buffer(data: &[u8]) -> fmem::Buffer {
        let vmo = zx::Vmo::create(data.len().max(64) as u64).expect("create vmo");
        vmo.write(data, 0).expect("write vmo");
        fmem::Buffer { vmo, size: data.len() as u64 }
    }

    /// Creates a [`DataSink`] backed by `board_partitions` and a placeholder `/block` channel.
    fn make_sink(board_partitions: FakeBoardPartitions) -> DataSink {
        let (block_dir, _server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        DataSink::new(Box::new(board_partitions), block_dir)
    }

    /// Builds a valid serialized ZBI container containing a single `CmdLine` item with
    /// `item_payload`.
    fn make_test_zbi(item_payload: &[u8]) -> Vec<u8> {
        let mut raw = vec![0u8; 256 + item_payload.len()];
        let aligned = zbi::align_buffer(&mut raw[..]).expect("align zbi buffer");
        let mut container = zbi::ZbiContainer::new(&mut aligned[..]).expect("new zbi container");
        container
            .create_entry_with_payload(
                zbi::ZbiType::CmdLine,
                0,
                zbi::ZbiFlags::default(),
                item_payload,
            )
            .expect("create zbi entry");
        let size = container.container_size().expect("zbi container size");
        aligned[..size].to_vec()
    }

    /// Verifies reading and writing kernel and vbmeta assets, partial-block preservation, tail
    /// trimming, and error handling.
    #[fuchsia::test]
    async fn test_write_and_read_assets() {
        let board_partitions = FakeBoardPartitions::new();
        let zircon_a = board_partitions.add_partition(ZIRCON_A_PARTITION, 0xaa);
        let vbmeta_b = board_partitions.add_partition(VBMETA_B_PARTITION, 0xbb);
        let vbmeta_r = board_partitions.add_partition(VBMETA_R_PARTITION, 0xcc);
        board_partitions.add_missing_partition(ZIRCON_B_PARTITION);
        board_partitions.add_partition_with_size("empty_part", 0, 0x00);
        let sink = make_sink(board_partitions);

        // Write an unaligned payload (600 bytes) to zircon_a: preserves [600..1024) in the
        // trailing partial block and trims full trailing blocks [1024..TEST_PARTITION_SIZE).
        let payload_data: Vec<u8> = (0..600).map(|i| (i % 251) as u8).collect();
        sink.write_asset(
            fpaver::Configuration::A,
            fpaver::Asset::Kernel,
            make_buffer(&payload_data),
        )
        .await
        .expect("write zircon_a");

        let disk_bytes = zircon_a.bytes();
        assert_eq!(&disk_bytes[..600], &payload_data[..]);
        assert_eq!(&disk_bytes[600..1024], &[0xaau8; 424]);
        assert_eq!(&disk_bytes[1024..], &[0xabu8; TEST_PARTITION_SIZE - 1024]);

        // Zero-length asset write is rejected with INVALID_ARGS without trimming existing data.
        assert_eq!(
            sink.write_asset(fpaver::Configuration::A, fpaver::Asset::Kernel, make_buffer(&[]),)
                .await,
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(&zircon_a.bytes()[..600], &payload_data[..]);

        // Read back zircon_a (non-ZBI header -> returns full partition size).
        let read_buf = sink
            .read_asset(fpaver::Configuration::A, fpaver::Asset::Kernel)
            .await
            .expect("read zircon_a");
        assert_eq!(read_buf.size, TEST_PARTITION_SIZE as u64);
        assert_eq!(read_buf.vmo.get_content_size().expect("content size"), read_buf.size);
        let mut read_vec = vec![0u8; 600];
        read_buf.vmo.read(&mut read_vec, 0).expect("vmo read");
        assert_eq!(read_vec, payload_data);

        // Write vbmeta_b and vbmeta_r.
        let vbmeta_data = vec![0x55u8; 512];
        sink.write_asset(
            fpaver::Configuration::B,
            fpaver::Asset::VerifiedBootMetadata,
            make_buffer(&vbmeta_data),
        )
        .await
        .expect("write vbmeta_b");
        assert_eq!(&vbmeta_b.bytes()[..512], &vbmeta_data[..]);
        assert_eq!(&vbmeta_b.bytes()[512..], &[0xabu8; TEST_PARTITION_SIZE - 512]);

        sink.write_asset(
            fpaver::Configuration::Recovery,
            fpaver::Asset::VerifiedBootMetadata,
            make_buffer(&vbmeta_data),
        )
        .await
        .expect("write vbmeta_r");
        assert_eq!(&vbmeta_r.bytes()[..512], &vbmeta_data[..]);

        // Unsupported, missing, and out-of-range error paths.
        assert_eq!(
            sink.read_asset(fpaver::Configuration::Recovery, fpaver::Asset::Kernel).await.err(),
            Some(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            sink.read_asset(fpaver::Configuration::B, fpaver::Asset::Kernel).await.err(),
            Some(zx::Status::NOT_FOUND)
        );
        assert_eq!(
            sink.write_asset(
                fpaver::Configuration::A,
                fpaver::Asset::Kernel,
                make_buffer(&vec![0u8; TEST_PARTITION_SIZE + 1]),
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            sink.read_full_partition("empty_part").await.err(),
            Some(zx::Status::INVALID_ARGS)
        );
    }

    /// Verifies ZBI container size extraction on `read_asset` and `read_firmware` and fallback to
    /// full partition size for tiny, oversized, or corrupted headers.
    #[fuchsia::test]
    async fn test_read_asset_and_firmware_zbi_size_extraction() {
        let board_partitions = FakeBoardPartitions::new();
        let _zircon_b = board_partitions.add_partition(ZIRCON_B_PARTITION, 0x00);
        let _abl_a = board_partitions.add_partition("abl_a", 0x00);
        let _tiny_fw = board_partitions.add_partition_with_size("tiny_a", 16, 0x7f);
        let sink = make_sink(board_partitions);

        // Valid ZBI container trims `Buffer.size` while preserving full partition stream size on
        // the returned VMO.
        let zbi_image = make_test_zbi(b"kernel.serial=legacy");
        let expected_zbi_size = zbi_image.len() as u64;
        assert!(expected_zbi_size < TEST_PARTITION_SIZE as u64);

        sink.write_asset(fpaver::Configuration::B, fpaver::Asset::Kernel, make_buffer(&zbi_image))
            .await
            .expect("write zbi zircon_b");

        let read_buf = sink
            .read_asset(fpaver::Configuration::B, fpaver::Asset::Kernel)
            .await
            .expect("read zircon_b");
        assert_eq!(read_buf.size, expected_zbi_size);
        assert_eq!(
            read_buf.vmo.get_content_size().expect("content size"),
            TEST_PARTITION_SIZE as u64
        );

        // `read_firmware` also trims ZBI-formatted firmware payloads.
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::A,
                "abl".to_string(),
                make_buffer(&zbi_image),
            )
            .await,
            fpaver::WriteFirmwareResult::Status(Ok(()))
        );
        let fw_buf = sink
            .read_firmware(fpaver::Configuration::A, "abl".to_string())
            .await
            .expect("read abl_a");
        assert_eq!(fw_buf.size, expected_zbi_size);

        // Partition smaller than a 32-byte ZbiHeader falls back to partition_size.
        let tiny_buf = sink
            .read_firmware(fpaver::Configuration::A, "tiny".to_string())
            .await
            .expect("read tiny_a");
        assert_eq!(tiny_buf.size, 16);

        // ZBI header claiming length > partition_size - 32 falls back to partition_size without
        // attempting to allocate `header.length`.
        let mut oversized_zbi = zbi_image.clone();
        let huge_len = (TEST_PARTITION_SIZE as u32) + 4096;
        oversized_zbi[4..8].copy_from_slice(&huge_len.to_le_bytes());
        sink.write_asset(
            fpaver::Configuration::B,
            fpaver::Asset::Kernel,
            make_buffer(&oversized_zbi),
        )
        .await
        .expect("write oversized zbi header");
        let oversized_read = sink
            .read_asset(fpaver::Configuration::B, fpaver::Asset::Kernel)
            .await
            .expect("read oversized zbi header");
        assert_eq!(oversized_read.size, TEST_PARTITION_SIZE as u64);

        // Invalid/corrupted ZBI item payload falls back to full partition size.
        let mut invalid_zbi = zbi_image.clone();
        invalid_zbi[32..].fill(0xff);
        sink.write_asset(
            fpaver::Configuration::B,
            fpaver::Asset::Kernel,
            make_buffer(&invalid_zbi),
        )
        .await
        .expect("write invalid zbi");
        let invalid_read = sink
            .read_asset(fpaver::Configuration::B, fpaver::Asset::Kernel)
            .await
            .expect("read invalid zbi");
        assert_eq!(invalid_read.size, TEST_PARTITION_SIZE as u64);
    }

    /// Fake [`PartitionClient`] that succeeds on open/size queries but fails I/O with
    /// `zx::Status::NOT_SUPPORTED`.
    struct WriteUnsupportedClient;

    #[async_trait(?Send)]
    impl PartitionClient for WriteUnsupportedClient {
        fn partition_size(&self) -> Result<u64, zx::Status> {
            Ok(TEST_PARTITION_SIZE as u64)
        }

        async fn read_vmo(
            &self,
            _vmo: &zx::Vmo,
            _vmo_offset: u64,
            _dev_offset: u64,
            _size: u64,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        async fn write_vmo(
            &self,
            _vmo: &zx::Vmo,
            _vmo_offset: u64,
            _dev_offset: u64,
            _size: u64,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }
    }

    /// Fake [`BoardPartitions`] exposing `"ro_fw_a"` backed by [`WriteUnsupportedClient`].
    struct WriteUnsupportedBoard;

    #[async_trait(?Send)]
    impl BoardPartitions for WriteUnsupportedBoard {
        async fn open_partition(
            &self,
            partition_name: &str,
            _block_dir: &fidl_next::ClientEnd<fio::Directory>,
        ) -> Result<Box<dyn PartitionClient>, zx::Status> {
            if partition_name == "ro_fw_a" {
                Ok(Box::new(WriteUnsupportedClient))
            } else {
                Err(zx::Status::NOT_SUPPORTED)
            }
        }
    }

    /// Verifies reading and writing named firmware partitions and default `""` (`"bootloader"`)
    /// type mapping across configurations.
    #[fuchsia::test]
    async fn test_write_and_read_firmware() {
        let board_partitions = FakeBoardPartitions::new();
        let abl_a = board_partitions.add_partition("abl_a", 0x00);
        let dtbo_b = board_partitions.add_partition("dtbo_b", 0x00);
        let bootloader_a = board_partitions.add_partition(BOOTLOADER_A_PARTITION, 0x00);
        let bootloader_b = board_partitions.add_partition(BOOTLOADER_B_PARTITION, 0x00);
        let bootloader_r = board_partitions.add_partition(BOOTLOADER_R_PARTITION, 0x00);
        let sink = make_sink(board_partitions);

        let fw_data = vec![0x5au8; 1024];
        assert_eq!(
            sink.write_firmware(fpaver::Configuration::A, "abl".to_string(), make_buffer(&fw_data))
                .await,
            fpaver::WriteFirmwareResult::Status(Ok(()))
        );
        assert_eq!(&abl_a.bytes()[..1024], &fw_data[..]);

        let read_fw =
            sink.read_firmware(fpaver::Configuration::A, "abl".to_string()).await.expect("read");
        let mut read_vec = vec![0u8; 1024];
        read_fw.vmo.read(&mut read_vec, 0).expect("read vmo");
        assert_eq!(read_vec, fw_data);

        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::B,
                "dtbo".to_string(),
                make_buffer(&fw_data)
            )
            .await,
            fpaver::WriteFirmwareResult::Status(Ok(()))
        );
        assert_eq!(&dtbo_b.bytes()[..1024], &fw_data[..]);

        // Empty firmware type maps to bootloader_{a,b,r}.
        for (cfg, part) in [
            (fpaver::Configuration::A, &bootloader_a),
            (fpaver::Configuration::B, &bootloader_b),
            (fpaver::Configuration::Recovery, &bootloader_r),
        ] {
            assert_eq!(
                sink.write_firmware(cfg, String::new(), make_buffer(&fw_data)).await,
                fpaver::WriteFirmwareResult::Status(Ok(()))
            );
            assert_eq!(&part.bytes()[..1024], &fw_data[..]);
        }
    }

    /// Verifies `WriteFirmwareResult::Unsupported(true)` and `NOT_SUPPORTED` for unsupported
    /// firmware types and unmapped configurations without falling back to `Configuration::A`.
    #[fuchsia::test]
    async fn test_write_and_read_firmware_unsupported() {
        let board_partitions = FakeBoardPartitions::new();
        let abl_a = board_partitions.add_partition("abl_a", 0x5a);
        let sink = make_sink(board_partitions);

        let fw_data = vec![0x6bu8; 1024];

        // `DataSink` does NOT silently fall back from `Configuration::B` or `Recovery` to
        // `Configuration::A` when `abl_b` / `abl_r` is `NOT_SUPPORTED` (single-slot firmware maps
        // both `_a` and `_b` in `BoardPartitions`).
        for cfg in [fpaver::Configuration::B, fpaver::Configuration::Recovery] {
            assert_eq!(
                sink.write_firmware(cfg, "abl".to_string(), make_buffer(&fw_data)).await,
                fpaver::WriteFirmwareResult::Unsupported(true)
            );
            assert_eq!(
                sink.read_firmware(cfg, "abl".to_string()).await.err(),
                Some(zx::Status::NOT_SUPPORTED)
            );
        }
        assert_eq!(&abl_a.bytes()[..], &[0x5au8; TEST_PARTITION_SIZE]);

        // Unsupported firmware type returns Unsupported(true) across configurations (even when
        // payload size is 0) and NOT_SUPPORTED for read.
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::A,
                "unsupported_fw".to_string(),
                make_buffer(&[]),
            )
            .await,
            fpaver::WriteFirmwareResult::Unsupported(true)
        );
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::B,
                "unsupported_fw".to_string(),
                make_buffer(&fw_data),
            )
            .await,
            fpaver::WriteFirmwareResult::Unsupported(true)
        );
        assert_eq!(
            sink.read_firmware(fpaver::Configuration::A, "unsupported_fw".to_string()).await.err(),
            Some(zx::Status::NOT_SUPPORTED)
        );
    }

    /// Verifies `WriteFirmwareResult::Status(Err(...))` and `read_firmware` errors for missing
    /// partitions, invalid payload sizes, and post-open I/O failures.
    #[fuchsia::test]
    async fn test_write_and_read_firmware_errors() {
        let board_partitions = FakeBoardPartitions::new();
        let abl_a = board_partitions.add_partition("abl_a", 0x5a);
        let dual_missing_a = board_partitions.add_partition("dual_missing_a", 0x42);
        board_partitions.add_missing_partition("dual_missing_b");
        board_partitions.add_missing_partition("missing_fw_a");
        let sink = make_sink(board_partitions);

        let fw_data = vec![0x6bu8; 1024];

        // Supported `_b` partition that is missing on disk (`NOT_FOUND`) fails with `NOT_FOUND`
        // without touching `_a`.
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::B,
                "dual_missing".to_string(),
                make_buffer(&fw_data),
            )
            .await,
            fpaver::WriteFirmwareResult::Status(Err(zx::Status::NOT_FOUND))
        );
        assert_eq!(&dual_missing_a.bytes()[..], &[0x42u8; TEST_PARTITION_SIZE]);
        assert_eq!(
            sink.read_firmware(fpaver::Configuration::B, "dual_missing".to_string()).await.err(),
            Some(zx::Status::NOT_FOUND)
        );

        // Supported firmware partition that is missing on disk returns Status(Err(NOT_FOUND)).
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::A,
                "missing_fw".to_string(),
                make_buffer(&fw_data),
            )
            .await,
            fpaver::WriteFirmwareResult::Status(Err(zx::Status::NOT_FOUND))
        );

        // Zero-length payload on a supported firmware partition returns Status(Err(INVALID_ARGS))
        // without modifying existing partition data.
        assert_eq!(
            sink.write_firmware(fpaver::Configuration::A, "abl".to_string(), make_buffer(&[]))
                .await,
            fpaver::WriteFirmwareResult::Status(Err(zx::Status::INVALID_ARGS))
        );
        assert_eq!(&abl_a.bytes()[..], &[0x5au8; TEST_PARTITION_SIZE]);

        // Payload exceeding partition size returns Status(Err(OUT_OF_RANGE)).
        assert_eq!(
            sink.write_firmware(
                fpaver::Configuration::A,
                "abl".to_string(),
                make_buffer(&vec![0u8; TEST_PARTITION_SIZE + 1]),
            )
            .await,
            fpaver::WriteFirmwareResult::Status(Err(zx::Status::OUT_OF_RANGE))
        );

        // Post-open `write_vmo` returning `NOT_SUPPORTED` must surface `Status(Err(NOT_SUPPORTED))`
        // rather than `Unsupported(true)`.
        let (ro_block_dir, _ro_server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        let ro_sink = DataSink::new(Box::new(WriteUnsupportedBoard), ro_block_dir);
        assert_eq!(
            ro_sink
                .write_firmware(
                    fpaver::Configuration::A,
                    "ro_fw".to_string(),
                    make_buffer(&fw_data)
                )
                .await,
            fpaver::WriteFirmwareResult::Status(Err(zx::Status::NOT_SUPPORTED))
        );
    }

    /// Verifies FIDL dispatch over [`serve_data_sink`] and [`serve_dynamic_data_sink`].
    #[fuchsia::test]
    async fn test_serve_data_sink_and_dynamic_data_sink_fidl() {
        let scope = fasync::Scope::new_with_name("test_data_sink_fidl");

        // 1. Test `serve_data_sink`.
        let board_partitions = FakeBoardPartitions::new();
        let flushes = board_partitions.flushes.clone();
        let _zircon_a = board_partitions.add_partition(ZIRCON_A_PARTITION, 0x11);
        let abl_a = board_partitions.add_partition("abl_a", 0x22);
        let sink = make_sink(board_partitions);

        let (ds_client, ds_server) = fidl_next::fuchsia::create_channel::<fpaver::DataSink>();
        let ds_client = ds_client.spawn();
        scope.spawn_local(async move {
            serve_data_sink(sink, ds_server).await.expect("serve_data_sink");
        });

        let kernel_payload = vec![0x77u8; 700];
        assert_eq!(
            ds_client
                .write_asset(
                    fpaver::Configuration::A,
                    fpaver::Asset::Kernel,
                    make_buffer(&kernel_payload),
                )
                .await
                .expect("write_asset fidl")
                .status,
            Ok(())
        );
        let read_asset_resp = ds_client
            .read_asset(fpaver::Configuration::A, fpaver::Asset::Kernel)
            .await
            .expect("read_asset fidl")
            .expect("read_asset ok")
            .asset;
        let mut read_back = vec![0u8; 700];
        read_asset_resp.vmo.read(&mut read_back, 0).expect("read vmo");
        assert_eq!(read_back, kernel_payload);

        let fw_payload = vec![0x88u8; 512];
        assert_eq!(
            ds_client
                .write_firmware(fpaver::Configuration::A, "abl", make_buffer(&fw_payload))
                .await
                .expect("write_firmware fidl")
                .result,
            fpaver::WriteFirmwareResult::Status(Ok(()))
        );
        // Both unmapped `Configuration::B` and `Configuration::Recovery` return `Unsupported(true)`
        // without falling back to `abl_a`.
        let fw_payload_b = vec![0x89u8; 512];
        assert_eq!(
            ds_client
                .write_firmware(fpaver::Configuration::B, "abl", make_buffer(&fw_payload_b))
                .await
                .expect("write_firmware B unsupported fidl")
                .result,
            fpaver::WriteFirmwareResult::Unsupported(true)
        );
        assert_eq!(&abl_a.bytes()[..512], &fw_payload[..]);
        assert_eq!(
            ds_client
                .write_firmware(fpaver::Configuration::Recovery, "abl", make_buffer(&fw_payload))
                .await
                .expect("write_firmware unsupported fidl")
                .result,
            fpaver::WriteFirmwareResult::Unsupported(true)
        );
        let read_fw_resp = ds_client
            .read_firmware(fpaver::Configuration::A, "abl")
            .await
            .expect("read_firmware fidl")
            .expect("read_firmware ok")
            .firmware;
        let mut read_fw_bytes = vec![0u8; 512];
        read_fw_resp.vmo.read(&mut read_fw_bytes, 0).expect("read fw vmo");
        assert_eq!(read_fw_bytes, fw_payload);

        assert_eq!(ds_client.flush().await.expect("flush fidl").status, Ok(()));
        assert_eq!(flushes.get(), 1);
        assert_eq!(
            ds_client.write_opaque_volume(make_buffer(&[0u8; 64])).await.expect("opaque fidl"),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            ds_client.write_sparse_volume(make_buffer(&[0u8; 64])).await.expect("sparse fidl"),
            Err(zx::Status::NOT_SUPPORTED)
        );

        // 2. Test `serve_dynamic_data_sink`.
        let dyn_partitions = FakeBoardPartitions::new();
        let dyn_flushes = dyn_partitions.flushes.clone();
        let _vbmeta_a = dyn_partitions.add_partition(VBMETA_A_PARTITION, 0x33);
        let dyn_sink = make_sink(dyn_partitions);

        let (dds_client, dds_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DynamicDataSink>();
        let dds_client = dds_client.spawn();
        scope.spawn_local(async move {
            serve_dynamic_data_sink(dyn_sink, dds_server).await.expect("serve_dynamic_data_sink");
        });

        let vbmeta_payload = vec![0x99u8; 256];
        assert_eq!(
            dds_client
                .write_asset(
                    fpaver::Configuration::A,
                    fpaver::Asset::VerifiedBootMetadata,
                    make_buffer(&vbmeta_payload),
                )
                .await
                .expect("dyn write_asset fidl")
                .status,
            Ok(())
        );
        assert_eq!(dds_client.flush().await.expect("dyn flush fidl").status, Ok(()));
        assert_eq!(dyn_flushes.get(), 1);
        assert_eq!(
            dds_client.initialize_partition_tables().await.expect("init gpt fidl").status,
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            dds_client.wipe_partition_tables().await.expect("wipe gpt fidl").status,
            Err(zx::Status::NOT_SUPPORTED)
        );

        drop(ds_client);
        drop(dds_client);
        scope.cancel().await;
    }

    /// Verifies that [`DataSink::write_asset`] clones and commits pager-backed payload VMOs before
    /// invoking [`PartitionClient::write_vmo`].
    #[fuchsia::test]
    async fn test_write_asset_commits_pager_backed_vmo() {
        use std::sync::Arc;

        let page_size = u64::from(zx::system_get_page_size());
        let vmo_size = page_size * 2;
        let port = zx::Port::create();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).expect("create pager"));
        let pager_vmo = Arc::new(
            pager
                .create_vmo(zx::VmoOptions::empty(), &port, 1, vmo_size)
                .expect("create pager vmo"),
        );

        // `system-updater` passes resizable COW clones of pager-backed blob VMOs.
        let payload_vmo = pager_vmo
            .create_child(
                zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE | zx::VmoChildOptions::RESIZABLE,
                0,
                vmo_size,
            )
            .expect("create child vmo");
        let initial_info = payload_vmo.info().expect("vmo info");
        assert!(initial_info.flags.contains(zx::VmoInfoFlags::PAGER_BACKED));
        assert_eq!(initial_info.committed_bytes, 0);

        let pager_thread = std::thread::spawn({
            let pager = pager.clone();
            let pager_vmo = pager_vmo.clone();
            move || {
                let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("port wait");
                let zx::PacketContents::Pager(request) = packet.contents() else {
                    panic!("unexpected packet: {packet:?}");
                };
                assert_eq!(
                    request.command(),
                    zx::sys::zx_page_request_command_t::ZX_PAGER_VMO_READ
                );
                let range = request.range();
                let len = range.end - range.start;
                let aux_vmo = zx::Vmo::create(len).expect("create aux vmo");
                aux_vmo.write(&vec![0x5au8; len as usize], 0).expect("fill aux vmo");
                pager.supply_pages(&pager_vmo, range, &aux_vmo, 0).expect("supply pages");
            }
        });

        let board_partitions = FakeBoardPartitions::new();
        let zircon_a =
            board_partitions.add_partition_with_size(ZIRCON_A_PARTITION, vmo_size as usize, 0x00);
        let sink = make_sink(board_partitions);

        // `FakePartitionClient::write_vmo` asserts that `vmo` has `parent_koid != 0` and
        // `committed_bytes >= size` when `PAGER_BACKED` is set.
        sink.write_asset(
            fpaver::Configuration::A,
            fpaver::Asset::Kernel,
            fmem::Buffer { vmo: payload_vmo, size: vmo_size },
        )
        .await
        .expect("write pager-backed zircon_a");

        pager_thread.join().expect("join pager thread");
        assert_eq!(zircon_a.bytes(), vec![0x5au8; vmo_size as usize]);
    }
}
