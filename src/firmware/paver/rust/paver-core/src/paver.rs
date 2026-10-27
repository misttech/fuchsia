// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::boot_manager::serve_boot_manager;
use crate::data_sink::{DataSink, serve_data_sink, serve_dynamic_data_sink};
use crate::partition::BoardPartitions;
use crate::partition_io::{PartitionIo, serve_partition_io};
use anyhow::{Context as _, Error};
use fidl::epitaph::ChannelEpitaphExt as _;
use fidl_next::Request;
use fidl_next_fuchsia_io as fio;
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_paver::paver::{
    FindBootManager, FindDataSink, FindPartitionTableManager, FindSysconfig, OpenPartitionIo,
};
use fidl_next_fuchsia_process_lifecycle as fprocess_lifecycle;
use fuchsia_async as fasync;
use std::rc::Rc;

const BLOCK_DIR_PATH: &str = "/block";

/// Opens the paver component's `/block` directory for legacy [`DataSink`] and `DynamicDataSink`
/// connections.
fn open_block_dir() -> Result<fidl_next::ClientEnd<fio::Directory>, zx::Status> {
    let (block_dir, server_end) = fidl_next::fuchsia::create_channel::<fio::Directory>();
    fdio::open(
        BLOCK_DIR_PATH,
        fidl_fuchsia_io::Flags::PROTOCOL_DIRECTORY | fidl_fuchsia_io::PERM_READABLE,
        server_end.into_untyped(),
    )?;
    Ok(block_dir)
}

/// Trait representing the top-level `fuchsia.paver.Paver` and
/// `fuchsia.process.lifecycle.Lifecycle` FIDL protocols.
#[allow(async_fn_in_trait)]
pub trait Paver: 'static {
    /// Board-specific [`fpaver::BootManagerLocalServerHandler`] implementation.
    type BootManager: fpaver::BootManagerLocalServerHandler + 'static;

    /// Performs board-specific cleanup when the paver component receives a lifecycle stop request.
    async fn stop(&self) -> Result<(), zx::Status> {
        Ok(())
    }

    /// Opens the `/block` directory passed to [`DataSink`] by [`Self::find_data_sink`] and
    /// [`Self::find_partition_table_manager`].
    fn open_block_dir(&self) -> Result<fidl_next::ClientEnd<fio::Directory>, zx::Status> {
        open_block_dir()
    }

    /// Creates a board-specific [`BoardPartitions`] used by [`DataSink`], `DynamicDataSink`, and
    /// [`PartitionIo`].
    async fn create_board_partitions(&self) -> Result<Box<dyn BoardPartitions>, zx::Status>;

    /// Creates a [`DataSink`] for reading and writing boot assets, firmware, and partitions.
    async fn find_data_sink(&self) -> Result<DataSink, zx::Status> {
        let block_dir = self.open_block_dir()?;
        Ok(DataSink::new(self.create_board_partitions().await?, block_dir))
    }

    /// Creates a [`PartitionIo`] serving `fuchsia.paver.PartitionIo` backed by `block_dir`.
    async fn open_partition_io(
        &self,
        block_dir: fidl_next::ClientEnd<fio::Directory>,
    ) -> Result<PartitionIo, zx::Status> {
        Ok(PartitionIo::new(self.create_board_partitions().await?, block_dir))
    }

    /// Creates a [`DataSink`] serving `fuchsia.paver.DynamicDataSink` for partition table
    /// management.
    async fn find_partition_table_manager(&self) -> Result<DataSink, zx::Status> {
        let block_dir = self.open_block_dir()?;
        Ok(DataSink::new(self.create_board_partitions().await?, block_dir))
    }

    /// Creates a board-specific [`BootManager`](Self::BootManager) for managing A/B/R boot
    /// configuration slots.
    async fn find_boot_manager(&self) -> Result<Self::BootManager, zx::Status>;

    /// Handles a `FindSysconfig` request, closing `sysconfig` with `zx::Status::NOT_SUPPORTED` by
    /// default on boards without a sysconfig partition.
    async fn find_sysconfig(
        &self,
        sysconfig: fidl_next::ServerEnd<fpaver::Sysconfig>,
        _scope: &fasync::ScopeHandle,
    ) {
        log::warn!("FindSysconfig is not supported");
        let _ = sysconfig.into_untyped().close_with_epitaph(zx::Status::NOT_SUPPORTED);
    }
}

/// FIDL server handler for `fuchsia.paver.Paver` that dispatches requests to [`Paver`].
struct PaverHandler<P> {
    paver: Rc<P>,
    scope: fasync::ScopeHandle,
}

impl<P: Paver> fpaver::PaverLocalServerHandler for PaverHandler<P> {
    async fn find_data_sink(&mut self, request: Request<FindDataSink>) {
        let data_sink = request.payload().data_sink;
        match self.paver.find_data_sink().await {
            Ok(sink) => {
                self.scope.spawn_local(async move {
                    if let Err(err) = serve_data_sink(sink, data_sink).await {
                        log::error!(err:?; "Error serving DataSink");
                    }
                });
            }
            Err(status) => {
                log::warn!(status:?; "FindDataSink failed");
                let _ = data_sink.into_untyped().close_with_epitaph(status);
            }
        }
    }

    async fn open_partition_io(&mut self, request: Request<OpenPartitionIo>) {
        let req = request.payload();
        match self.paver.open_partition_io(req.block_dir).await {
            Ok(partition_io) => {
                let server_end = req.partition_io;
                self.scope.spawn_local(async move {
                    if let Err(err) = serve_partition_io(partition_io, server_end).await {
                        log::error!(err:?; "Error serving PartitionIo");
                    }
                });
            }
            Err(status) => {
                log::warn!(status:?; "OpenPartitionIo failed");
                let _ = req.partition_io.into_untyped().close_with_epitaph(status);
            }
        }
    }

    async fn find_partition_table_manager(&mut self, request: Request<FindPartitionTableManager>) {
        let data_sink = request.payload().data_sink;
        match self.paver.find_partition_table_manager().await {
            Ok(sink) => {
                self.scope.spawn_local(async move {
                    if let Err(err) = serve_dynamic_data_sink(sink, data_sink).await {
                        log::error!(err:?; "Error serving DynamicDataSink");
                    }
                });
            }
            Err(status) => {
                log::warn!(status:?; "FindPartitionTableManager failed");
                let _ = data_sink.into_untyped().close_with_epitaph(status);
            }
        }
    }

    async fn find_boot_manager(&mut self, request: Request<FindBootManager>) {
        let boot_manager = request.payload().boot_manager;
        match self.paver.find_boot_manager().await {
            Ok(manager) => {
                self.scope.spawn_local(async move {
                    if let Err(err) = serve_boot_manager(manager, boot_manager).await {
                        log::error!(err:?; "Error serving BootManager");
                    }
                });
            }
            Err(status) => {
                log::warn!(status:?; "FindBootManager failed");
                let _ = boot_manager.into_untyped().close_with_epitaph(status);
            }
        }
    }

    async fn find_sysconfig(&mut self, request: Request<FindSysconfig>) {
        let sysconfig = request.payload().sysconfig;
        self.paver.find_sysconfig(sysconfig, &self.scope).await;
    }
}

/// Serves the `fuchsia.paver.Paver` FIDL protocol over `server_end`, spawning sub-protocol
/// handlers onto `scope`.
pub async fn serve_paver<P: Paver>(
    paver: Rc<P>,
    server_end: fidl_next::ServerEnd<fpaver::Paver>,
    scope: fasync::ScopeHandle,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(PaverHandler { paver, scope })
        .await
        .map(|_| ())
        .context("serving Paver")
}

/// FIDL server handler for `fuchsia.process.lifecycle.Lifecycle`.
struct LifecycleHandler<P> {
    paver: Rc<P>,
    scope: Option<fasync::Scope>,
    server: fidl_next::Server<fprocess_lifecycle::Lifecycle>,
}

impl<P: Paver> fprocess_lifecycle::LifecycleLocalServerHandler for LifecycleHandler<P> {
    async fn stop(&mut self) {
        log::info!("Received shutdown command over lifecycle interface");
        if let Some(scope) = self.scope.take() {
            scope.cancel().await;
        }
        if let Err(status) = self.paver.stop().await {
            log::error!(status:?; "Lifecycle stop callback failed");
        }
        log::info!("Paver shutdown complete");
        self.server.close();
    }
}

/// Serves the `fuchsia.process.lifecycle.Lifecycle` FIDL protocol over `server_end`, cancelling
/// `scope` and invoking [`Paver::stop`] on `paver` before closing the lifecycle channel.
pub async fn serve_lifecycle<P: Paver>(
    paver: Rc<P>,
    server_end: fidl_next::ServerEnd<fprocess_lifecycle::Lifecycle>,
    scope: fasync::Scope,
) -> Result<(), Error> {
    let dispatcher = fidl_next::ServerDispatcher::new(server_end);
    let server = dispatcher.server();
    let handler = dispatcher
        .run_local(LifecycleHandler { paver, scope: Some(scope), server })
        .await
        .context("serving Lifecycle")?;
    if let Some(scope) = handler.scope {
        log::info!("Lifecycle channel closed");
        scope.cancel().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResponderExt as _;
    use crate::partition::PartitionClient;
    use crate::sysconfig::serve_sysconfig;
    use async_trait::async_trait;
    use fidl_next::Responder;
    use fidl_next_fuchsia_paver::{boot_manager, sysconfig};
    use std::cell::Cell;

    struct EmptyBoardPartitions;

    #[async_trait(?Send)]
    impl BoardPartitions for EmptyBoardPartitions {
        async fn open_partition(
            &self,
            _partition_name: &str,
            _block_dir: &fidl_next::ClientEnd<fio::Directory>,
        ) -> Result<Box<dyn PartitionClient>, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }
    }

    struct FakePaver {
        stopped: Cell<bool>,
        has_block_dir: Cell<bool>,
        fail_board_partitions: Cell<Option<zx::Status>>,
    }

    impl Paver for FakePaver {
        type BootManager = FakeBootManager;

        async fn stop(&self) -> Result<(), zx::Status> {
            self.stopped.set(true);
            Ok(())
        }

        fn open_block_dir(&self) -> Result<fidl_next::ClientEnd<fio::Directory>, zx::Status> {
            if self.has_block_dir.get() {
                let (block_dir, _server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
                Ok(block_dir)
            } else {
                super::open_block_dir()
            }
        }

        async fn create_board_partitions(&self) -> Result<Box<dyn BoardPartitions>, zx::Status> {
            if let Some(status) = self.fail_board_partitions.get() {
                return Err(status);
            }
            Ok(Box::new(EmptyBoardPartitions))
        }

        async fn find_boot_manager(&self) -> Result<Self::BootManager, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }
    }

    struct FakeBootManager {
        active: fpaver::Configuration,
    }

    impl fpaver::BootManagerLocalServerHandler for FakeBootManager {
        async fn query_current_configuration(
            &mut self,
            responder: Responder<boot_manager::QueryCurrentConfiguration>,
        ) {
            responder.send_result(Ok(self.active)).await;
        }

        async fn query_active_configuration(
            &mut self,
            responder: Responder<boot_manager::QueryActiveConfiguration>,
        ) {
            responder.send_result(Ok(self.active)).await;
        }

        async fn query_configuration_last_set_active(
            &mut self,
            responder: Responder<boot_manager::QueryConfigurationLastSetActive>,
        ) {
            responder.send_result(Ok(self.active)).await;
        }

        async fn query_configuration_status(
            &mut self,
            _request: Request<boot_manager::QueryConfigurationStatus>,
            responder: Responder<boot_manager::QueryConfigurationStatus>,
        ) {
            responder.send_result(Ok(fpaver::ConfigurationStatus::Healthy)).await;
        }

        async fn query_configuration_status_and_boot_attempts(
            &mut self,
            _request: Request<boot_manager::QueryConfigurationStatusAndBootAttempts>,
            responder: Responder<boot_manager::QueryConfigurationStatusAndBootAttempts>,
        ) {
            responder
                .send_result(Ok(
                    fpaver::BootManagerQueryConfigurationStatusAndBootAttemptsResponse {
                        status: Some(fpaver::ConfigurationStatus::Healthy),
                        boot_attempts: Some(1),
                        unbootable_reason: Some(fpaver::UnbootableReason::None),
                    },
                ))
                .await;
        }

        async fn set_configuration_active(
            &mut self,
            request: Request<boot_manager::SetConfigurationActive>,
            responder: Responder<boot_manager::SetConfigurationActive>,
        ) {
            self.active = request.payload().configuration;
            responder.send_response(Ok(())).await;
        }

        async fn set_configuration_unbootable(
            &mut self,
            _request: Request<boot_manager::SetConfigurationUnbootable>,
            responder: Responder<boot_manager::SetConfigurationUnbootable>,
        ) {
            responder.send_response(Ok(())).await;
        }

        async fn set_configuration_healthy(
            &mut self,
            _request: Request<boot_manager::SetConfigurationHealthy>,
            responder: Responder<boot_manager::SetConfigurationHealthy>,
        ) {
            responder.send_response(Ok(())).await;
        }

        async fn set_one_shot_recovery(
            &mut self,
            responder: Responder<boot_manager::SetOneShotRecovery>,
        ) {
            responder.send_result::<()>(Err(zx::Status::NOT_SUPPORTED)).await;
        }

        async fn flush(&mut self, responder: Responder<boot_manager::Flush>) {
            responder.send_response(Ok(())).await;
        }
    }

    struct FakeSysconfig;

    impl fpaver::SysconfigLocalServerHandler for FakeSysconfig {
        async fn read(&mut self, responder: Responder<sysconfig::Read>) {
            responder
                .send_result::<fidl_next_fuchsia_mem::Buffer>(Err(zx::Status::NOT_SUPPORTED))
                .await;
        }

        async fn write(
            &mut self,
            _request: Request<sysconfig::Write>,
            responder: Responder<sysconfig::Write>,
        ) {
            responder.send_response(Err(zx::Status::NOT_SUPPORTED)).await;
        }

        async fn get_partition_size(&mut self, responder: Responder<sysconfig::GetPartitionSize>) {
            responder.send_result(Ok(4096)).await;
        }

        async fn flush(&mut self, responder: Responder<sysconfig::Flush>) {
            responder.send_response(Ok(())).await;
        }

        async fn wipe(&mut self, responder: Responder<sysconfig::Wipe>) {
            responder.send_response(Ok(())).await;
        }
    }

    /// Verifies `Paver` sub-protocol routing, error epitaphs, and `Lifecycle::Stop` shutdown.
    #[fuchsia::test]
    async fn test_serve_paver_and_lifecycle_stop() {
        let paver = Rc::new(FakePaver {
            stopped: Cell::new(false),
            has_block_dir: Cell::new(false),
            fail_board_partitions: Cell::new(None),
        });
        let scope = fasync::Scope::new_with_name("test");
        let (paver_client, paver_server) = fidl_next::fuchsia::create_channel::<fpaver::Paver>();
        let paver_client = paver_client.spawn();
        let paver_clone = paver.clone();
        let scope_handle = scope.to_handle();
        scope.spawn_local(async move {
            serve_paver(paver_clone, paver_server, scope_handle).await.expect("serve_paver");
        });

        // Without `/block` in the namespace, FindDataSink closes with NOT_FOUND.
        let (missing_ds_client, missing_ds_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DataSink>();
        let missing_ds_client = missing_ds_client.spawn();
        paver_client.find_data_sink(missing_ds_server).await.expect("find_data_sink");
        assert!(matches!(
            missing_ds_client.flush().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::NOT_FOUND
        ));

        // With `open_block_dir` succeeding, FindDataSink and FindPartitionTableManager spawn
        // DataSink / DynamicDataSink tasks.
        paver.has_block_dir.set(true);

        let (data_sink_client, data_sink_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DataSink>();
        let data_sink_client = data_sink_client.spawn();
        paver_client.find_data_sink(data_sink_server).await.expect("find_data_sink");
        assert_eq!(data_sink_client.flush().await.expect("flush fidl").status, Ok(()));

        let (dyn_sink_client, dyn_sink_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DynamicDataSink>();
        let dyn_sink_client = dyn_sink_client.spawn();
        paver_client
            .find_partition_table_manager(dyn_sink_server)
            .await
            .expect("find_partition_table_manager");
        assert_eq!(dyn_sink_client.flush().await.expect("dyn flush fidl").status, Ok(()));

        // OpenPartitionIo spawns a PartitionIo task.
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let (partition_io_client, partition_io_server) =
            fidl_next::fuchsia::create_channel::<fpaver::PartitionIo>();
        let partition_io_client = partition_io_client.spawn();
        paver_client
            .open_partition_io(block_dir_client, partition_io_server)
            .await
            .expect("open_partition_io");
        assert_eq!(partition_io_client.flush().await.expect("partition_io flush"), Ok(()));

        // When `create_board_partitions` fails, FindDataSink, FindPartitionTableManager, and
        // OpenPartitionIo close their server channels with that status epitaph.
        paver.fail_board_partitions.set(Some(zx::Status::INTERNAL));

        let (err_ds_client, err_ds_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DataSink>();
        let err_ds_client = err_ds_client.spawn();
        paver_client.find_data_sink(err_ds_server).await.expect("find_data_sink");
        assert!(matches!(
            err_ds_client.flush().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::INTERNAL
        ));

        let (err_dds_client, err_dds_server) =
            fidl_next::fuchsia::create_channel::<fpaver::DynamicDataSink>();
        let err_dds_client = err_dds_client.spawn();
        paver_client
            .find_partition_table_manager(err_dds_server)
            .await
            .expect("find_partition_table_manager");
        assert!(matches!(
            err_dds_client.flush().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::INTERNAL
        ));

        let (err_block_dir, _err_block_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let (err_pio_client, err_pio_server) =
            fidl_next::fuchsia::create_channel::<fpaver::PartitionIo>();
        let err_pio_client = err_pio_client.spawn();
        paver_client.open_partition_io(err_block_dir, err_pio_server).await.expect("open_pio");
        assert!(matches!(
            err_pio_client.flush().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::INTERNAL
        ));

        paver.fail_board_partitions.set(None);

        // FindBootManager and FindSysconfig close with NOT_SUPPORTED epitaph when unsupported.
        let (bm_client, bm_server) = fidl_next::fuchsia::create_channel::<fpaver::BootManager>();
        let bm_client = bm_client.spawn();
        paver_client.find_boot_manager(bm_server).await.expect("find_boot_manager");
        assert!(matches!(
            bm_client.query_current_configuration().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::NOT_SUPPORTED
        ));

        let (sc_client, sc_server) = fidl_next::fuchsia::create_channel::<fpaver::Sysconfig>();
        let sc_client = sc_client.spawn();
        paver_client.find_sysconfig(sc_server).await.expect("find_sysconfig");
        assert!(matches!(
            sc_client.flush().await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if zx::Status::err_from_raw(epitaph) == zx::Status::NOT_SUPPORTED
        ));

        // Verify lifecycle stop cancels scope and invokes Paver::stop.
        let (lc_client, lc_server) =
            fidl_next::fuchsia::create_channel::<fprocess_lifecycle::Lifecycle>();
        let lc_client = lc_client.spawn();
        lc_client.stop().await.expect("send stop");
        serve_lifecycle(paver.clone(), lc_server, scope).await.expect("serve_lifecycle");
        assert!(paver.stopped.get());
    }

    /// Verifies `BootManager` and `Sysconfig` FIDL server helpers and [`ResponderExt`] responses.
    #[fuchsia::test]
    async fn test_boot_manager_and_sysconfig_responder_ext() {
        let scope = fasync::Scope::new_with_name("test_bm_sc");

        let (bm_client, bm_server) = fidl_next::fuchsia::create_channel::<fpaver::BootManager>();
        let bm_client = bm_client.spawn();
        scope.spawn_local(async move {
            serve_boot_manager(FakeBootManager { active: fpaver::Configuration::A }, bm_server)
                .await
                .expect("serve_boot_manager");
        });

        assert_eq!(
            bm_client.query_active_configuration().await.expect("fidl").map(|r| r.configuration),
            Ok(fpaver::Configuration::A)
        );
        assert_eq!(
            bm_client
                .set_configuration_active(fpaver::Configuration::B)
                .await
                .expect("fidl")
                .status,
            Ok(())
        );
        assert_eq!(
            bm_client.query_active_configuration().await.expect("fidl").map(|r| r.configuration),
            Ok(fpaver::Configuration::B)
        );
        assert_eq!(
            bm_client.set_one_shot_recovery().await.expect("fidl"),
            Err(zx::Status::NOT_SUPPORTED)
        );

        let (sc_client, sc_server) = fidl_next::fuchsia::create_channel::<fpaver::Sysconfig>();
        let sc_client = sc_client.spawn();
        scope.spawn_local(async move {
            serve_sysconfig(FakeSysconfig, sc_server).await.expect("serve_sysconfig");
        });

        assert_eq!(sc_client.get_partition_size().await.expect("fidl").map(|r| r.size), Ok(4096));
        assert_eq!(sc_client.flush().await.expect("fidl").status, Ok(()));
        drop(bm_client);
        drop(sc_client);
        scope.cancel().await;
    }
}
