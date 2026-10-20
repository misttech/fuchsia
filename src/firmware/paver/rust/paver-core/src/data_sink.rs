// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ResponderExt as _;
use anyhow::{Context as _, Error};
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_mem as fmem;
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_paver::{data_sink, dynamic_data_sink, partition_io};

/// `DataSink` / `PartitionIo` / `DynamicDataSink` implementation.
#[derive(Debug, Default)]
pub struct DataSink;

impl DataSink {
    /// Creates a new [`DataSink`] instance.
    pub fn new() -> Self {
        Self
    }

    /// Returns the total size in bytes of `partition`.
    pub async fn get_partition_size(&mut self, _partition: &str) -> Result<u64, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Reads `size` bytes from `partition` starting at byte `offset` into a newly allocated VMO.
    pub async fn read_partition(
        &mut self,
        _partition: &str,
        _offset: u64,
        _size: u64,
    ) -> Result<zx::Vmo, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes `payload` to `partition` starting at byte `offset`, handling bytes past the written
    /// range according to `trailing`.
    pub async fn write_partition(
        &mut self,
        _partition: &str,
        _offset: u64,
        _payload: zx::Vmo,
        _trailing: fpaver::TrailingBehavior,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Reads the boot `asset` (kernel ZBI or verified boot metadata) for `configuration`.
    pub async fn read_asset(
        &mut self,
        _configuration: fpaver::Configuration,
        _asset: fpaver::Asset,
    ) -> Result<fmem::Buffer, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes `payload` to the boot `asset` partition for `configuration`.
    pub async fn write_asset(
        &mut self,
        _configuration: fpaver::Configuration,
        _asset: fpaver::Asset,
        _payload: fmem::Buffer,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes `payload` to the firmware partition of `firmware_type` for `configuration`.
    pub async fn write_firmware(
        &mut self,
        _configuration: fpaver::Configuration,
        _firmware_type: String,
        _payload: fmem::Buffer,
    ) -> fpaver::WriteFirmwareResult {
        fpaver::WriteFirmwareResult::Unsupported(true)
    }

    /// Reads the firmware image of `firmware_type` for `configuration`.
    pub async fn read_firmware(
        &mut self,
        _configuration: fpaver::Configuration,
        _firmware_type: String,
    ) -> Result<fmem::Buffer, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes a raw FVM/Fxfs image `payload` to the system volume partition.
    pub async fn write_opaque_volume(&mut self, _payload: fmem::Buffer) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Writes an Android sparse image `payload` to the system volume partition.
    pub async fn write_sparse_volume(&mut self, _payload: fmem::Buffer) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Flushes all previously written partition data to persistent storage.
    pub async fn flush(&mut self) -> Result<(), zx::Status> {
        Ok(())
    }

    /// Initializes the device partition tables.
    pub async fn initialize_partition_tables(&mut self) -> Result<(), zx::Status> {
        log::error!("Initializing partitions is not supported");
        Err(zx::Status::NOT_SUPPORTED)
    }

    /// Wipes all entries from the device partition tables.
    pub async fn wipe_partition_tables(&mut self) -> Result<(), zx::Status> {
        log::error!("Wiping partition tables is not supported");
        Err(zx::Status::NOT_SUPPORTED)
    }
}

impl fpaver::DataSinkLocalServerHandler for DataSink {
    async fn read_asset(
        &mut self,
        request: Request<data_sink::ReadAsset>,
        responder: Responder<data_sink::ReadAsset>,
    ) {
        let req = request.payload();
        let res = self.read_asset(req.configuration, req.asset).await;
        responder.send_result(res).await;
    }

    async fn write_asset(
        &mut self,
        request: Request<data_sink::WriteAsset>,
        responder: Responder<data_sink::WriteAsset>,
    ) {
        let req = request.payload();
        let res = self.write_asset(req.configuration, req.asset, req.payload).await;
        responder.send_response(res).await;
    }

    async fn write_firmware(
        &mut self,
        request: Request<data_sink::WriteFirmware>,
        responder: Responder<data_sink::WriteFirmware>,
    ) {
        let req = request.payload();
        let res = self.write_firmware(req.configuration, req.type_, req.payload).await;
        responder.send_response(res).await;
    }

    async fn read_firmware(
        &mut self,
        request: Request<data_sink::ReadFirmware>,
        responder: Responder<data_sink::ReadFirmware>,
    ) {
        let req = request.payload();
        let res = self.read_firmware(req.configuration, req.type_).await;
        responder.send_result(res).await;
    }

    async fn write_opaque_volume(
        &mut self,
        request: Request<data_sink::WriteOpaqueVolume>,
        responder: Responder<data_sink::WriteOpaqueVolume>,
    ) {
        let req = request.payload();
        let res = self.write_opaque_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn write_sparse_volume(
        &mut self,
        request: Request<data_sink::WriteSparseVolume>,
        responder: Responder<data_sink::WriteSparseVolume>,
    ) {
        let req = request.payload();
        let res = self.write_sparse_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<data_sink::Flush>) {
        let res = self.flush().await;
        responder.send_response(res).await;
    }
}

impl fpaver::PartitionIoLocalServerHandler for DataSink {
    async fn get_partition_size(
        &mut self,
        request: Request<partition_io::GetPartitionSize>,
        responder: Responder<partition_io::GetPartitionSize>,
    ) {
        let res = self.get_partition_size(&request.payload().partition).await;
        responder.send_result(res).await;
    }

    async fn read_partition(
        &mut self,
        request: Request<partition_io::ReadPartition>,
        responder: Responder<partition_io::ReadPartition>,
    ) {
        let req = request.payload();
        let res = self.read_partition(&req.partition, req.offset, req.size).await;
        responder.send_result(res).await;
    }

    async fn write_partition(
        &mut self,
        request: Request<partition_io::WritePartition>,
        responder: Responder<partition_io::WritePartition>,
    ) {
        let req = request.payload();
        let res = self.write_partition(&req.partition, req.offset, req.payload, req.trailing).await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<partition_io::Flush>) {
        let res = self.flush().await;
        responder.send_result(res).await;
    }
}

impl fpaver::DynamicDataSinkLocalServerHandler for DataSink {
    async fn read_asset(
        &mut self,
        request: Request<dynamic_data_sink::ReadAsset>,
        responder: Responder<dynamic_data_sink::ReadAsset>,
    ) {
        let req = request.payload();
        let res = self.read_asset(req.configuration, req.asset).await;
        responder.send_result(res).await;
    }

    async fn write_asset(
        &mut self,
        request: Request<dynamic_data_sink::WriteAsset>,
        responder: Responder<dynamic_data_sink::WriteAsset>,
    ) {
        let req = request.payload();
        let res = self.write_asset(req.configuration, req.asset, req.payload).await;
        responder.send_response(res).await;
    }

    async fn write_firmware(
        &mut self,
        request: Request<dynamic_data_sink::WriteFirmware>,
        responder: Responder<dynamic_data_sink::WriteFirmware>,
    ) {
        let req = request.payload();
        let res = self.write_firmware(req.configuration, req.type_, req.payload).await;
        responder.send_response(res).await;
    }

    async fn read_firmware(
        &mut self,
        request: Request<dynamic_data_sink::ReadFirmware>,
        responder: Responder<dynamic_data_sink::ReadFirmware>,
    ) {
        let req = request.payload();
        let res = self.read_firmware(req.configuration, req.type_).await;
        responder.send_result(res).await;
    }

    async fn write_opaque_volume(
        &mut self,
        request: Request<dynamic_data_sink::WriteOpaqueVolume>,
        responder: Responder<dynamic_data_sink::WriteOpaqueVolume>,
    ) {
        let req = request.payload();
        let res = self.write_opaque_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn write_sparse_volume(
        &mut self,
        request: Request<dynamic_data_sink::WriteSparseVolume>,
        responder: Responder<dynamic_data_sink::WriteSparseVolume>,
    ) {
        let req = request.payload();
        let res = self.write_sparse_volume(req.payload).await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<dynamic_data_sink::Flush>) {
        let res = self.flush().await;
        responder.send_response(res).await;
    }

    async fn initialize_partition_tables(
        &mut self,
        responder: Responder<dynamic_data_sink::InitializePartitionTables>,
    ) {
        let res = self.initialize_partition_tables().await;
        responder.send_response(res).await;
    }

    async fn wipe_partition_tables(
        &mut self,
        responder: Responder<dynamic_data_sink::WipePartitionTables>,
    ) {
        let res = self.wipe_partition_tables().await;
        responder.send_response(res).await;
    }
}

/// Serves the `fuchsia.paver.DataSink` FIDL protocol over `server_end` using `sink`.
pub async fn serve_data_sink(
    sink: DataSink,
    server_end: fidl_next::ServerEnd<fpaver::DataSink>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(sink)
        .await
        .map(|_| ())
        .context("serving DataSink")
}

/// Serves the `fuchsia.paver.PartitionIo` FIDL protocol over `server_end` using `sink`.
pub async fn serve_partition_io(
    sink: DataSink,
    server_end: fidl_next::ServerEnd<fpaver::PartitionIo>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(sink)
        .await
        .map(|_| ())
        .context("serving PartitionIo")
}

/// Serves the `fuchsia.paver.DynamicDataSink` FIDL protocol over `server_end` using `sink`.
pub async fn serve_dynamic_data_sink(
    sink: DataSink,
    server_end: fidl_next::ServerEnd<fpaver::DynamicDataSink>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(sink)
        .await
        .map(|_| ())
        .context("serving DynamicDataSink")
}
