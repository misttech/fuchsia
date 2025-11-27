// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::crypt::zxcrypt::{UnsealOutcome, ZxcryptDevice};
use crate::device::constants::{
    self, BLOB_IMAGE_VOLUME_LABEL, BLOB_VOLUME_LABEL, DATA_PARTITION_LABEL,
    LEGACY_DATA_PARTITION_LABEL, ZXCRYPT_DRIVER_PATH,
};
use crate::device::{BlockDevice, Device, DeviceTag};
use crate::environment::{
    Container, Environment, FilesystemLauncher, FvmContainer, FxfsContainer, ServeFilesystemStatus,
};
use anyhow::{Context, Error, anyhow, ensure};
use device_watcher::recursive_wait_and_open;
use fidl::endpoints::{ClientEnd, Proxy, RequestStream, ServerEnd};
use fidl_fuchsia_device::{ControllerMarker, ControllerProxy};
use fidl_fuchsia_fs_startup::{CreateOptions, MountOptions};
use fidl_fuchsia_hardware_block_volume::VolumeManagerMarker;
use fidl_fuchsia_io::{self as fio, DirectoryMarker};
use fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream};
use fs_management::filesystem::BlockConnector;
use fs_management::format::{DiskFormat, detect_disk_format};
use fs_management::partition::{PartitionMatcher, find_partition, partition_matches_with_proxy};
use fs_management::{F2fs, Fvm, Fxfs, Minfs, filesystem};
use fuchsia_async::TimeoutExt as _;
use fuchsia_component::client::connect_to_protocol_at_dir_root;
use fuchsia_fs::file::write;
use fuchsia_runtime::HandleType;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::{StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use std::sync::Arc;
use vfs::service;
use zx::{self as zx, MonotonicDuration};
use {
    fidl_fuchsia_fshost as fshost, fidl_fuchsia_fxfs as ffxfs,
    fidl_fuchsia_storage_partitions as fpartitions, fuchsia_async as fasync,
};

pub enum FshostShutdownResponder {
    Lifecycle(
        // TODO(https://fxbug.dev/333319162): Implement me.
        #[allow(dead_code)] LifecycleRequestStream,
    ),
}

impl FshostShutdownResponder {
    pub fn close(self) -> Result<(), fidl::Error> {
        match self {
            FshostShutdownResponder::Lifecycle(_) => {}
        }
        Ok(())
    }
}

const FIND_PARTITION_DURATION: MonotonicDuration = MonotonicDuration::from_seconds(20);
const STARNIX_TEST_VOLUME_NAME: &str = "starnix_test_volume";
const IMAGE_FILE_NAME: &str = "blob.img";

fn data_partition_names() -> Vec<String> {
    vec![DATA_PARTITION_LABEL.to_string(), LEGACY_DATA_PARTITION_LABEL.to_string()]
}

async fn find_data_partition(ramdisk_prefix: Option<String>) -> Result<ControllerProxy, Error> {
    let fvm_matcher = PartitionMatcher {
        detected_disk_formats: Some(vec![DiskFormat::Fvm]),
        ignore_prefix: ramdisk_prefix,
        ..Default::default()
    };

    let fvm_controller =
        find_partition(fvm_matcher, FIND_PARTITION_DURATION).await.context("Failed to find FVM")?;
    let fvm_path = fvm_controller
        .get_topological_path()
        .await
        .context("fvm get_topo_path transport error")?
        .map_err(zx::Status::from_raw)
        .context("fvm get_topo_path returned error")?;

    let fvm_dir = fuchsia_fs::directory::open_in_namespace(&fvm_path, fio::Flags::empty())?;
    let fvm_volume_manager_proxy = recursive_wait_and_open::<VolumeManagerMarker>(&fvm_dir, "/fvm")
        .await
        .context("failed to connect to the VolumeManager")?;

    // **NOTE**: We must call VolumeManager::GetInfo() to ensure all partitions are visible when
    // we enumerate them below. See https://fxbug.dev/42077585 for more information.
    zx::ok(fvm_volume_manager_proxy.get_info().await.context("transport error on get_info")?.0)
        .context("get_info failed")?;

    find_data_partition_in(fvm_path).await
}

async fn find_data_partition_in(fvm_path: String) -> Result<ControllerProxy, Error> {
    let fvm_dir =
        fuchsia_fs::directory::open_in_namespace(&format!("{fvm_path}/fvm"), fio::PERM_READABLE)?;

    let data_matcher = PartitionMatcher {
        type_guids: Some(vec![constants::DATA_TYPE_GUID]),
        labels: Some(data_partition_names()),
        parent_device: Some(fvm_path),
        ignore_if_path_contains: Some("zxcrypt/unsealed".to_string()),
        ..Default::default()
    };

    // We can't use find_partition because it looks in /dev/class/block and we can't be sure that
    // it will show up there yet (the block driver is bound after fvm has published its
    // volumes). Instead, we enumerate the topological path directory whose entries should be
    // present thanks to calling get_info above.
    for entry in fuchsia_fs::directory::readdir(&fvm_dir).await? {
        // This will wait for the block entry to show up.
        let proxy = recursive_wait_and_open::<ControllerMarker>(
            &fvm_dir,
            &format!("{}/block/device_controller", entry.name),
        )
        .await
        .context("opening partition path")?;
        match partition_matches_with_proxy(&proxy, &data_matcher).await {
            Ok(true) => {
                return Ok(proxy);
            }
            Ok(false) => {}
            Err(error) => {
                log::info!(error:?; "Failure in partition match. Transient device?");
            }
        }
    }
    Err(anyhow!("Data partition not found"))
}

async fn mount_main_starnix_volume(
    environment: &Arc<Mutex<dyn Environment>>,
    starnix_volume_name: String,
    crypt: ClientEnd<ffxfs::CryptMarker>,
    exposed_dir: ServerEnd<fio::DirectoryMarker>,
) -> Result<[u8; 16], Error> {
    let mut env = environment.lock().await;
    if let Some(multi_vol_fs) = env.get_container() {
        let mounted_vol = if multi_vol_fs.has_volume(&starnix_volume_name).await? {
            multi_vol_fs
                .open_volume(
                    &starnix_volume_name,
                    MountOptions { crypt: Some(crypt), ..MountOptions::default() },
                )
                .await?
        } else {
            multi_vol_fs
                .create_volume(
                    &starnix_volume_name,
                    CreateOptions::default(),
                    MountOptions { crypt: Some(crypt), ..MountOptions::default() },
                )
                .await?
        };
        mounted_vol.exposed_dir().clone(exposed_dir.into_channel().into())?;
        multi_vol_fs
            .get_volume_info(&starnix_volume_name)
            .await
            .context("get_volume_info")?
            .guid
            .ok_or_else(|| anyhow!("No GUID returned"))
    } else {
        Err(anyhow!("Tried to mount starnix volume without container set"))
    }
}

async fn create_starnix_volume_impl(
    environment: &Arc<Mutex<dyn Environment>>,
    starnix_volume_name: &str,
    crypt: ClientEnd<ffxfs::CryptMarker>,
    exposed_dir: ServerEnd<fio::DirectoryMarker>,
) -> Result<[u8; 16], Error> {
    let mut env = environment.lock().await;
    if let Some(multi_vol_fs) = env.get_container() {
        // If the starnix volume already exists, unmount if mounted and then remove.
        if multi_vol_fs.has_volume(starnix_volume_name).await? {
            multi_vol_fs.remove_volume(starnix_volume_name).await?;
        }
        let mounted_vol = multi_vol_fs
            .create_volume(
                starnix_volume_name,
                CreateOptions::default(),
                MountOptions { crypt: Some(crypt), ..MountOptions::default() },
            )
            .await?;
        mounted_vol.exposed_dir().clone(exposed_dir.into_channel().into())?;
        multi_vol_fs
            .get_volume_info(&starnix_volume_name)
            .await
            .context("get_volume_info")?
            .guid
            .ok_or_else(|| anyhow!("No GUID returned"))
    } else {
        Err(anyhow!("Tried to mount starnix volume without container set"))
    }
}

async fn mount_starnix_volume(
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
    crypt: ClientEnd<ffxfs::CryptMarker>,
    exposed_dir: ServerEnd<fio::DirectoryMarker>,
) -> Result<[u8; 16], Error> {
    if config.starnix_volume_name.is_empty() {
        create_starnix_volume_impl(environment, STARNIX_TEST_VOLUME_NAME, crypt, exposed_dir).await
    } else {
        mount_main_starnix_volume(
            environment,
            config.starnix_volume_name.clone(),
            crypt,
            exposed_dir,
        )
        .await
    }
}

async fn create_starnix_volume(
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
    crypt: ClientEnd<ffxfs::CryptMarker>,
    exposed_dir: ServerEnd<fio::DirectoryMarker>,
) -> Result<[u8; 16], Error> {
    let volume_name = if config.starnix_volume_name.is_empty() {
        STARNIX_TEST_VOLUME_NAME
    } else {
        &config.starnix_volume_name
    };

    create_starnix_volume_impl(environment, volume_name, crypt, exposed_dir).await
}

async fn write_data_file(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
    ramdisk_prefix: Option<String>,
    launcher: &FilesystemLauncher,
    filename: &str,
    payload: zx::Vmo,
) -> Result<(), Error> {
    if !config.ramdisk_image {
        return Err(anyhow!(
            "Can't WriteDataFile from a non-recovery build;
            ramdisk_image must be set."
        ));
    }

    let content_size = if let Ok(content_size) = payload.get_content_size() {
        content_size
    } else if let Ok(content_size) = payload.get_size() {
        content_size
    } else {
        return Err(anyhow!("Failed to get content size"));
    };

    let content_size =
        usize::try_from(content_size).context("Failed to convert u64 content_size to usize")?;

    let _guard = system_partition_lock.lock().await;
    let (filesystem, mut data) = if config.fxfs_blob || config.storage_host {
        let (device, _) = get_system_container_for_recovery(environment).await?;
        let mut container: Box<dyn Container> = if config.fxfs_blob {
            Box::new(FxfsContainer::new(
                launcher
                    .serve_fxblob(device, Box::new(Fxfs::dynamic_child()))
                    .await
                    .context("serving Fxblob")?,
            ))
        } else {
            Box::new(FvmContainer::new(
                launcher
                    .serve_fvm(device, Box::new(Fvm::dynamic_child()))
                    .await
                    .context("serving Fvm")?,
                false,
            ))
        };
        let data = container.serve_data(&launcher).await.context("serving data from Fxblob")?;

        (Some(container.into_fs()), data)
    } else {
        let partition_controller = find_data_partition(ramdisk_prefix).await?;

        let format = match config.data_filesystem_format.as_ref() {
            "fxfs" => DiskFormat::Fxfs,
            "f2fs" => DiskFormat::F2fs,
            "minfs" => DiskFormat::Minfs,
            _ => panic!("unsupported data filesystem format type"),
        };

        let partition_path = partition_controller
            .get_topological_path()
            .await
            .context("get_topo_path transport error")?
            .map_err(zx::Status::from_raw)
            .context("get_topo_path returned error")?;
        log::info!(partition_path:%; "Found data partition");
        let mut device = Box::new(
            BlockDevice::from_proxy(partition_controller, &partition_path)
                .await
                .context("failed to make new device")?,
        );
        let mut device: &mut dyn Device = device.as_mut();
        let mut zxcrypt_device;
        if format != DiskFormat::Fxfs && !config.no_zxcrypt {
            launcher.attach_driver(device, ZXCRYPT_DRIVER_PATH).await?;
            log::info!("Ensuring device is formatted with zxcrypt");
            zxcrypt_device = Box::new(
                match ZxcryptDevice::unseal(device).await.context("Failed to unseal zxcrypt")? {
                    UnsealOutcome::Unsealed(device) => device,
                    UnsealOutcome::FormatRequired => ZxcryptDevice::format(device).await?,
                },
            );
            device = zxcrypt_device.as_mut();
        }

        let filesystem = match format {
            DiskFormat::Fxfs => {
                launcher.serve_data(device, Fxfs::dynamic_child()).await.context("serving fxfs")?
            }
            DiskFormat::F2fs => {
                launcher.serve_data(device, F2fs::dynamic_child()).await.context("serving f2fs")?
            }
            DiskFormat::Minfs => launcher
                .serve_data(device, Minfs::dynamic_child())
                .await
                .context("serving minfs")?,
            _ => unreachable!(),
        };
        let filesystem = match filesystem {
            ServeFilesystemStatus::Serving(fs) => fs,
            ServeFilesystemStatus::FormatRequired => {
                log::info!(
                    "Format required {:?} for device {:?}",
                    format,
                    device.topological_path()
                );
                match format {
                    DiskFormat::Fxfs => launcher
                        .format_data(device, Fxfs::dynamic_child())
                        .await
                        .context("serving fxfs")?,
                    DiskFormat::F2fs => launcher
                        .format_data(device, F2fs::dynamic_child())
                        .await
                        .context("serving f2fs")?,
                    DiskFormat::Minfs => launcher
                        .format_data(device, Minfs::dynamic_child())
                        .await
                        .context("serving minfs")?,
                    _ => unreachable!(),
                }
            }
        };

        (None, filesystem)
    };
    let data_root = data.root().context("Failed to get data root")?;
    let (directory_proxy, file_path) = match filename.rsplit_once("/") {
        Some((directory_path, relative_file_path)) => {
            let directory_proxy = fuchsia_fs::directory::create_directory_recursive(
                &data_root,
                directory_path,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .await
            .context("Failed to create directory")?;
            (directory_proxy, relative_file_path)
        }
        None => (data_root, filename),
    };

    let file_proxy = fuchsia_fs::directory::open_file(
        &directory_proxy,
        file_path,
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .context("Failed to open file")?;

    let mut contents = vec![0; content_size];
    payload.read(&mut contents, 0).context("reading payload vmo")?;
    write(&file_proxy, &contents).await.context("writing file contents")?;

    data.shutdown().await.context("shutting down data")?;
    if let Some(fs) = filesystem {
        fs.shutdown().await.context("shutting down filesystem")?;
    }
    return Ok(());
}

async fn shred_data_volume(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
) -> Result<(), zx::Status> {
    if !(config.data_filesystem_format == "fxfs" || (config.storage_host && !config.no_zxcrypt)) {
        return Err(zx::Status::NOT_SUPPORTED);
    }

    // If we expect the filesystems to be live, ask `environment` to shred the data volume.
    if (config.data || config.fxfs_blob) && !config.ramdisk_image {
        log::info!("Filesystem is running; shredding online.");
        environment.lock().await.shred_data().await.map_err(|error| {
            log::error!(error:?; "Failed to shred data");
            zx::Status::INTERNAL
        })?;
    } else {
        // Otherwise we need to find the system container and shred the encrypted volumes in it.
        log::info!("Filesystem is not running; shredding offline.");
        let _guard = system_partition_lock.lock().await;

        // Get the block connector for all filesystem types. This blocks until the matchers find
        // the system container, so even if we don't use it, we know after this the device exists.
        let (device, device_path) = get_system_container_for_recovery(environment).await?;
        let format = detect_disk_format(
            &device
                .connect_block()
                .map_err(|error| {
                    log::error!(error:?; "connect_block failed");
                    zx::Status::INTERNAL
                })?
                .into_proxy(),
        )
        .await;

        if config.fxfs_blob {
            if format != DiskFormat::Fxfs {
                return Ok(());
            }

            let fxfs = fs_management::filesystem::Filesystem::from_boxed_config(
                device,
                Box::new(Fxfs::dynamic_child()),
            );
            let serving_fxfs = fxfs.serve_multi_volume().await.map_err(|error| {
                log::error!(error:?; "Failed to serve fxfs");
                zx::Status::INTERNAL
            })?;
            let mut fxfs_container = Box::new(FxfsContainer::new(serving_fxfs));
            fxfs_container.shred_data().await.map_err(|error| {
                log::error!(error:?; "Failed to shred fxfs keybag");
                zx::Status::INTERNAL
            })?;
            fxfs_container.into_fs().shutdown().await.map_err(|error| {
                log::error!(error:?; "Failed to unmount fxfs");
                zx::Status::INTERNAL
            })?;
            log::info!("Deleted fxfs-data keybag");
        } else if config.storage_host && config.data_filesystem_format == "minfs" {
            if format != DiskFormat::Fvm {
                return Ok(());
            }

            let fvm = fs_management::filesystem::Filesystem::from_boxed_config(
                device,
                Box::new(Fvm::dynamic_child()),
            );
            let serving_fvm = fvm.serve_multi_volume().await.map_err(|error| {
                log::error!(error:?; "Failed to serve fvm");
                zx::Status::INTERNAL
            })?;
            let mut fvm_container = FvmContainer::new(serving_fvm, false);
            fvm_container.shred_data().await.map_err(|error| {
                log::error!(error:?; "Failed to call shred data on fvm component");
                zx::Status::INTERNAL
            })?;
            log::info!("Shredded zxcrypt instances in fvm");
        } else if !config.storage_host && config.data_filesystem_format == "fxfs" {
            // fvm+fxfs, fxblob is handled above.
            if format != DiskFormat::Fvm {
                return Ok(());
            }

            let controller_proxy = find_data_partition_in(device_path).await.map_err(|error| {
                log::error!(error:?; "Failed to find data partition");
                zx::Status::NOT_FOUND
            })?;
            let fxfs = fs_management::filesystem::Filesystem::from_boxed_config(
                Box::new(controller_proxy),
                Box::new(Fxfs::dynamic_child()),
            );
            let serving_fxfs = fxfs.serve_multi_volume().await.map_err(|error| {
                log::error!(error:?; "Failed to serve fxfs");
                zx::Status::INTERNAL
            })?;
            let mut fxfs_container = Box::new(FxfsContainer::new(serving_fxfs));
            fxfs_container.shred_data().await.map_err(|error| {
                log::error!(error:?; "Failed to shred fxfs keybag");
                zx::Status::INTERNAL
            })?;
            fxfs_container.into_fs().shutdown().await.map_err(|error| {
                log::error!(error:?; "Failed to unmount fxfs");
                zx::Status::INTERNAL
            })?;
            log::info!("Deleted fxfs-data keybag");
        }
    }
    log::info!("Shredded the data volume.  Data will be lost!!");
    Ok(())
}

async fn init_system_partition_table(
    system_partition_lock: &Arc<Mutex<()>>,
    partitions: Vec<fpartitions::PartitionInfo>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
) -> Result<(), zx::Status> {
    if !config.ramdisk_image {
        log::error!("init_system_partition_table only supported in ramdisk-image mode.");
        return Err(zx::Status::NOT_SUPPORTED);
    }
    if !config.storage_host {
        log::error!("init_system_partition_table only supported in storage-host mode.");
        return Err(zx::Status::NOT_SUPPORTED);
    }
    if !config.gpt {
        log::error!("init_system_partition_table called on a non-gpt system.");
        return Err(zx::Status::NOT_SUPPORTED);
    }

    let _guard = system_partition_lock.lock().await;
    let registered_devices = environment.lock().await.registered_devices().clone();
    const TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(10);
    let _ = registered_devices
        .get_block_connector(DeviceTag::SystemPartitionTable)
        .map_err(|error| {
            log::error!(error:?; "init_system_partition_table: unable to get block connector");
            zx::Status::NOT_FOUND
        })
        .on_timeout(TIMEOUT, || {
            log::error!("init_system_partition_table: Failed to find gpt within timeout");
            Err(zx::Status::NOT_FOUND)
        })
        .await?;

    log::info!("init_system_partition_table: Reformatting GPT...");
    let exposed_dir = environment.lock().await.partition_manager_exposed_dir().map_err(|err| {
        log::error!(
            err:?;
            "init_system_partition_table: Failed to connect to partition manager"
        );
        Err(zx::Status::BAD_STATE)
    })?;
    let client =
        connect_to_protocol_at_dir_root::<fpartitions::PartitionsAdminMarker>(&exposed_dir)
            .unwrap();
    client
        .reset_partition_table(&partitions[..])
        .await
        .map_err(|err| {
            log::error!(err:?; "init_system_partition_table: FIDL error");
            Err(zx::Status::PEER_CLOSED)
        })?
        .map_err(zx::Status::from_raw)
}

pub fn fshost_volume_provider(
    environment: Arc<Mutex<dyn Environment>>,
    config: Arc<fshost_config::Config>,
) -> Arc<service::Service> {
    service::host(move |mut stream: fshost::StarnixVolumeProviderRequestStream| {
        let env = environment.clone();
        let config = config.clone();
        async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(fshost::StarnixVolumeProviderRequest::Mount {
                        crypt,
                        mode,
                        exposed_dir,
                        responder,
                    }) => {
                        log::info!(mode:?; "volume provider mount");
                        let res = match mode {
                            fshost::MountMode::MaybeCreate => {
                                mount_starnix_volume(&env, &config, crypt, exposed_dir).await
                            }
                            fshost::MountMode::AlwaysCreate => {
                                create_starnix_volume(&env, &config, crypt, exposed_dir).await
                            }
                        };
                        let res = match res {
                            Ok(guid) => Ok(guid),
                            Err(error) => {
                                log::error!(error:?; "volume provider service: mount failed");
                                Err(zx::Status::INTERNAL.into_raw())
                            }
                        };
                        responder.send(res.as_ref().map_err(|e| *e)).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Err(e) => {
                        log::error!("volume provider server failed: {:?}", e);
                        return;
                    }
                }
            }
        }
    })
}

/// Make a new vfs service node that implements fuchsia.fshost.Admin
pub fn fshost_admin(
    system_partition_lock: Arc<Mutex<()>>,
    environment: Arc<Mutex<dyn Environment>>,
    config: Arc<fshost_config::Config>,
) -> Arc<service::Service> {
    service::host(move |mut stream: fshost::AdminRequestStream| {
        let system_partition_lock = system_partition_lock.clone();
        let env = environment.clone();
        let config = config.clone();
        async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(fshost::AdminRequest::ShredDataVolume { responder }) => {
                        log::info!("admin shred data volume called");
                        let res =
                            match shred_data_volume(&system_partition_lock, &env, &config).await {
                                Ok(()) => Ok(()),
                                Err(status) => {
                                    // If shredding is not supported, only emit a warning.
                                    if status == zx::Status::NOT_SUPPORTED {
                                        log::warn!("shred_data_volume not supported");
                                    } else {
                                        log::error!(status:?; "shred_data_volume failed");
                                    }
                                    Err(status.into_raw())
                                }
                            };
                        responder.send(res).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::AdminRequest::StorageHostEnabled { responder }) => {
                        responder.send(config.storage_host).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Err(e) => {
                        log::error!("admin service failed: {:?}", e);
                        return;
                    }
                }
            }
        }
    })
}

/// Make a new vfs service node that implements fuchsia.fshost.Recovery
pub fn fshost_recovery(
    system_partition_lock: Arc<Mutex<()>>,
    environment: Arc<Mutex<dyn Environment>>,
    config: Arc<fshost_config::Config>,
    ramdisk_prefix: Option<String>,
    launcher: Arc<FilesystemLauncher>,
) -> Arc<service::Service> {
    service::host(move |mut stream: fshost::RecoveryRequestStream| {
        let system_partition_lock = system_partition_lock.clone();
        let env = environment.clone();
        let config = config.clone();
        let ramdisk_prefix = ramdisk_prefix.clone();
        let launcher = launcher.clone();
        async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(fshost::RecoveryRequest::WriteDataFile { responder, payload, filename }) => {
                        log::info!(filename:?; "recovery write data file called");
                        let res = match write_data_file(
                            &system_partition_lock,
                            &env,
                            &config,
                            ramdisk_prefix.clone(),
                            &launcher,
                            &filename,
                            payload,
                        )
                        .await
                        {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                log::error!(error:?; "write_data_file failed");
                                Err(zx::Status::INTERNAL.into_raw())
                            }
                        };
                        responder.send(res).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::RecoveryRequest::InitSystemPartitionTable {
                        partitions,
                        responder,
                    }) => {
                        log::info!("recovery init gpt called");
                        let res = match init_system_partition_table(
                            &system_partition_lock,
                            partitions,
                            &env,
                            &config,
                        )
                        .await
                        {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                log::error!(error:?; "init_system_partition_table failed");
                                Err(error.into_raw())
                            }
                        };
                        responder.send(res).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::RecoveryRequest::FormatSystemBlobVolume { responder }) => {
                        log::info!("formatting system blob volume");
                        let res = match format_system_blob_volume_impl(
                            &system_partition_lock,
                            &env,
                            &config,
                        )
                        .await
                        {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                log::error!(error:?; "format_system_blob_volume failed");
                                Err(if let Ok(status) = error.downcast::<zx::Status>() {
                                    status
                                } else {
                                    zx::Status::INTERNAL
                                })
                            }
                        };
                        responder.send(res.map_err(zx::Status::into_raw)).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::RecoveryRequest::MountSystemBlobVolume {
                        blob_exposed_dir,
                        responder,
                    }) => {
                        log::info!("mounting system blob volume");
                        let res = match mount_system_blob_volume_impl(
                            &system_partition_lock,
                            &env,
                            &config,
                            blob_exposed_dir,
                        )
                        .await
                        {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                log::error!(error:?; "mount_system_blob_volume failed");
                                Err(if let Ok(status) = error.downcast::<zx::Status>() {
                                    status
                                } else {
                                    zx::Status::INTERNAL
                                })
                            }
                        };
                        responder.send(res.map_err(zx::Status::into_raw)).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::RecoveryRequest::GetBlobImageHandle { responder }) => {
                        log::info!("getting image handle for new system blob volume");
                        let res = match get_blob_image_handle(&system_partition_lock, &env, &config)
                            .await
                        {
                            Ok(token) => Ok(token),
                            Err(error) => {
                                log::error!(error:?; "failed to get blob image handle");
                                Err(if let Ok(status) = error.downcast::<zx::Status>() {
                                    status
                                } else {
                                    zx::Status::INTERNAL
                                })
                            }
                        };
                        responder.send(res.map_err(zx::Status::into_raw)).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Ok(fshost::RecoveryRequest::InstallBlobImage { responder }) => {
                        log::info!("installing system blob volume");
                        let res =
                            match install_blob_image(&system_partition_lock, &env, &config).await {
                                Ok(()) => Ok(()),
                                Err(error) => {
                                    log::error!(error:?; "failed to install blob image");
                                    Err(if let Ok(status) = error.downcast::<zx::Status>() {
                                        status
                                    } else {
                                        zx::Status::INTERNAL
                                    })
                                }
                            };
                        responder.send(res.map_err(zx::Status::into_raw)).unwrap_or_else(|error| {
                            log::error!(error:?; "failed to send fidl response");
                        });
                    }
                    Err(e) => {
                        log::error!("recovery service failed: {:?}", e);
                        return;
                    }
                }
            }
        }
    })
}

pub fn handle_lifecycle_requests(
    mut shutdown: mpsc::Sender<FshostShutdownResponder>,
) -> Result<(), Error> {
    if let Some(handle) = fuchsia_runtime::take_startup_handle(HandleType::Lifecycle.into()) {
        let mut stream =
            LifecycleRequestStream::from_channel(fasync::Channel::from_channel(handle.into()));
        fasync::Task::spawn(async move {
            if let Ok(Some(LifecycleRequest::Stop { .. })) = stream.try_next().await {
                shutdown.start_send(FshostShutdownResponder::Lifecycle(stream)).unwrap_or_else(
                    |e| log::error!("failed to send shutdown message. error: {:?}", e),
                );
            }
        })
        .detach();
    }
    Ok(())
}

async fn format_system_blob_volume_impl(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
) -> Result<(), Error> {
    ensure!(config.ramdisk_image, "format_system_blob_volume called in a non-Recovery build");
    ensure!(config.fxfs_blob, "format_system_blob_volume requires a fxblob-based product");

    let _guard = system_partition_lock.clone().lock_owned().await;
    let (device, _) = get_system_container_for_recovery(environment).await?;

    let fxfs = filesystem::Filesystem::from_boxed_config(device, Box::new(Fxfs::default()));
    let serving_fxfs = fxfs.serve_multi_volume().await.context("serving fxfs")?;

    if serving_fxfs.has_volume(BLOB_VOLUME_LABEL).await.context("checking for blob volume")? {
        log::info!("Removing existing blob volume.");
        serving_fxfs.remove_volume(BLOB_VOLUME_LABEL).await.context("removing blob volume")?;
    }
    log::info!("Creating new blob volume.");
    let blob_volume = serving_fxfs
        .create_volume(
            BLOB_VOLUME_LABEL,
            CreateOptions::default(),
            MountOptions { as_blob: Some(true), ..Default::default() },
        )
        .await
        .context("creating blob volume")?;

    blob_volume.shutdown().await.context("unmounting blob volume")?;
    serving_fxfs.shutdown().await.context("unmounting fxfs")?;
    Ok(())
}

async fn mount_system_blob_volume_impl(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
    blob_exposed_dir: ServerEnd<DirectoryMarker>,
) -> Result<(), Error> {
    ensure!(config.ramdisk_image, "mount_system_blob_volume called in a non-Recovery build");
    ensure!(config.fxfs_blob, "mount_system_blob_volume requires a fxblob-based product");

    let guard = system_partition_lock.clone().lock_owned().await;
    let (device, _) = get_system_container_for_recovery(environment).await?;

    log::info!("Mounting Fxfs.");
    let fxfs = filesystem::Filesystem::from_boxed_config(device, Box::new(Fxfs::default()));
    let serving_fxfs = fxfs.serve_multi_volume().await.context("serving fxfs")?;

    log::info!("Mounting blob volume.");
    if !serving_fxfs.has_volume(BLOB_VOLUME_LABEL).await.context("checking for blob volume")? {
        log::error!(
            "Blob volume missing! Use fuchsia.fshost/Recovery.FormatSystemBlobVolume to create one."
        );
        return Err(zx::Status::NOT_FOUND).context("missing blob volume");
    }
    let blob_volume = serving_fxfs
        .open_volume(BLOB_VOLUME_LABEL, MountOptions { as_blob: Some(true), ..Default::default() })
        .await
        .context("mounting blob volume")?;

    let blob_root = fuchsia_fs::directory::clone(blob_volume.root())?;
    let blob_svc =
        fuchsia_fs::directory::open_directory(blob_volume.exposed_dir(), "svc", fio::PERM_READABLE)
            .await?;

    // Create a pseudo-directory for us to forward what we want from the blob component's exposed
    // directory. When we stop serving the last connection to this directory, we can safely unmount
    // the filesystem.
    let exposed_dir = vfs::pseudo_directory! {
        "root" => vfs::remote::remote_dir(blob_root),
        "svc" => vfs::remote::remote_dir(blob_svc),
    };

    let scope = vfs::ExecutionScope::new();
    vfs::directory::serve_on(
        exposed_dir,
        fio::PERM_READABLE | fio::PERM_WRITABLE | fio::PERM_EXECUTABLE,
        scope.clone(),
        blob_exposed_dir,
    );

    // TODO(https://fxbug.dev/444486641): Is this the best way to manage state across calls to this
    // method? As we expand functionality of this protocol, we may want to consider a more holistic
    // approach to guarding the system container (e.g. extending the FshostEnvironment itself).
    fasync::Task::spawn(async move {
        let _guard = guard;
        scope.wait().await;
        log::info!("All handles to system container closed, unmounting...");
        if let Err(e) = blob_volume.shutdown().await {
            log::error!("Failed to unmount blob volume: {e:?}");
        }
        if let Err(e) = serving_fxfs.shutdown().await {
            log::error!("Failed to shutdown fxfs: {e:?}");
        }
        log::info!("System container unmounted.");
    })
    .detach();

    Ok(())
}

async fn get_system_container_for_recovery(
    env: &Arc<Mutex<dyn Environment>>,
) -> Result<(Box<dyn BlockConnector>, String), zx::Status> {
    log::info!("Finding system container...");
    let registered_devices = env.lock().await.registered_devices().clone();
    let block_connector = registered_devices
        .get_block_connector(DeviceTag::SystemContainerOnRecovery)
        .map_err(|error| {
            log::error!(error:?; "Unable to get block connector for system container");
            zx::Status::NOT_FOUND
        })
        .on_timeout(FIND_PARTITION_DURATION, || {
            log::warn!("Failed to find system container within timeout");
            Err(zx::Status::NOT_FOUND)
        })
        .await?;
    let topological_path =
        registered_devices.get_topological_path(DeviceTag::SystemContainerOnRecovery).unwrap();
    Ok((block_connector, topological_path))
}

/// Obtains a handle to a file which can be used to write a new system blob image, and a token that
/// will unmount the system container when dropped. The volume will be installed automatically on
/// the next system boot, or by calling [`install_blob_image`].
async fn get_blob_image_handle(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
) -> Result<(ClientEnd<fio::FileMarker>, zx::EventPair), Error> {
    ensure!(config.ramdisk_image, "get_blob_image_handle called in a non-Recovery build");
    ensure!(config.fxfs_blob, "get_blob_image_handle requires a fxblob-based product");

    let guard = system_partition_lock.clone().lock_owned().await;
    let (device, _) = get_system_container_for_recovery(environment).await?;

    let fxfs = filesystem::Filesystem::from_boxed_config(device, Box::new(Fxfs::default()));
    let serving_fxfs = fxfs.serve_multi_volume().await.context("serving fxfs")?;

    let pending = if serving_fxfs
        .has_volume(BLOB_IMAGE_VOLUME_LABEL)
        .await
        .context("checking for blob image volume")?
    {
        serving_fxfs
            .open_volume(BLOB_IMAGE_VOLUME_LABEL, Default::default())
            .await
            .context("mounting existing blob image volume")?
    } else {
        serving_fxfs
            .create_volume(BLOB_IMAGE_VOLUME_LABEL, Default::default(), Default::default())
            .await
            .context("creating blob image volume")?
    };

    let image_file = fuchsia_fs::directory::open_file(
        pending.root(),
        IMAGE_FILE_NAME,
        fio::PERM_READABLE
            | fio::PERM_WRITABLE
            | fio::Flags::PROTOCOL_FILE
            | fio::Flags::FLAG_MAYBE_CREATE
            | fio::Flags::FILE_TRUNCATE,
    )
    .await
    .context("opening image file")?
    .into_client_end()
    .map_err(|_| anyhow!("failed to get client end for image handle"))?;

    // Create an event pair which we use as a token to indicate when we should unmount the system
    // container. This is required since we won't be able to determine when clients are done writing
    // to the image file.
    let (our_mount_token, mount_token) = zx::EventPair::create();

    fasync::Task::spawn(async move {
        let _guard = guard;
        if let Err(error) =
            fasync::OnSignals::new(our_mount_token, zx::Signals::OBJECT_PEER_CLOSED).await
        {
            log::error!(error:?; "failed to wait for unmount token, unmounting");
        } else {
            log::info!("Unmount token closed, unmounting system container.");
        }
        if let Err(error) = pending.shutdown().await {
            log::error!(error:?; "failed to unmount blob image volume");
        }
        if let Err(error) = serving_fxfs.shutdown().await {
            log::error!(error:?; "failed to shutdown fxfs");
        }
        log::info!("System container unmounted.");
    })
    .detach();

    Ok((image_file, mount_token))
}

/// Triggers installation of the system blob volume previously written by [`get_blob_image_handle`].
async fn install_blob_image(
    system_partition_lock: &Arc<Mutex<()>>,
    environment: &Arc<Mutex<dyn Environment>>,
    config: &fshost_config::Config,
) -> Result<(), Error> {
    ensure!(config.ramdisk_image, "install_blob_image called in a non-Recovery build");
    ensure!(config.fxfs_blob, "install_blob_image requires a fxblob-based product");

    let _guard = system_partition_lock.clone().lock_owned().await;
    let (device, _) = get_system_container_for_recovery(environment).await?;

    let fxfs = filesystem::Filesystem::from_boxed_config(device, Box::new(Fxfs::default()));
    let serving_fxfs = fxfs.serve_multi_volume().await.context("serving fxfs")?;

    if !serving_fxfs
        .has_volume(BLOB_IMAGE_VOLUME_LABEL)
        .await
        .context("checking for image volume")?
    {
        log::error!(
            "blob image missing, use fuchsia.fshost/Recovery.GetBlobImageHandle to write one"
        );
        return Err(zx::Status::NOT_FOUND).context("missing blob image volume");
    }

    let installer = connect_to_protocol_at_dir_root::<ffxfs::VolumeInstallerMarker>(
        serving_fxfs.exposed_dir(),
    )?;
    if let Err(error) = installer
        .install(BLOB_IMAGE_VOLUME_LABEL, IMAGE_FILE_NAME, BLOB_VOLUME_LABEL)
        .await
        .context("FIDL call to fuchsia.fxfs/VolumeInstaller.Install")?
        .map_err(zx::Status::from_raw)
    {
        log::error!(error:?; "failed to install blob volume, cleaning up...");
        if let Err(error) = serving_fxfs.remove_volume(BLOB_IMAGE_VOLUME_LABEL).await {
            log::error!(error:?; "could not remove blob image after failed installation");
        }
        // Return the original installation error.
        return Err(error).context("failed to install blob volume");
    } else {
        log::info!("Successfully installed system blob volume from image.");
    }

    Ok(())
}

/// Searches for a new blob volume ready for installation on the system container and attempts to
/// install it. On failure, the installation file will be cleaned up so we don't attempt the
/// installation again on subsequent boots.
pub async fn maybe_install_new_blob_volume(
    fs: &fs_management::filesystem::ServingMultiVolumeFilesystem,
) -> Result<(), Error> {
    if !fs.has_volume(BLOB_IMAGE_VOLUME_LABEL).await.context("checking for image volume")? {
        return Ok(());
    }
    log::info!("Installing system blob volume from image...");

    let installer =
        connect_to_protocol_at_dir_root::<ffxfs::VolumeInstallerMarker>(fs.exposed_dir())?;
    if let Err(error) = installer
        .install(BLOB_IMAGE_VOLUME_LABEL, IMAGE_FILE_NAME, BLOB_VOLUME_LABEL)
        .await
        .context("FIDL call to fuchsia.fxfs/VolumeInstaller.Install")?
        .map_err(zx::Status::from_raw)
    {
        log::error!(error:?; "failed to install blob volume, cleaning up...");
        if let Err(error) = fs.remove_volume(BLOB_IMAGE_VOLUME_LABEL).await {
            log::error!(error:?; "could not remove blob image after failed installation");
        }
        // Return the original installation error.
        return Err(error).context("failed to install blob volume");
    } else {
        log::info!("Successfully installed system blob volume from image.");
    }

    Ok(())
}
