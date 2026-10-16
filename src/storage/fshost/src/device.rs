// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod constants;

use anyhow::{Context, Error, anyhow};
use async_trait::async_trait;
use fidl::endpoints::create_proxy;
use fidl_fuchsia_driver_framework as fdf;
use fidl_fuchsia_driver_token as ftoken;
use fidl_fuchsia_io::{self as fio, DirectoryProxy};
use fidl_fuchsia_storage_block::{BlockProxy, DeviceFlag};
use fs_management::filesystem::{BlockConnector, DirBasedBlockConnector};
use fs_management::format::{DiskFormat, detect_disk_format};
use fuchsia_async as fasync;
use fuchsia_async::condition::Condition;
use fuchsia_component::client::connect_to_named_protocol_at_dir_root;
use futures::stream::{AbortHandle, Abortable};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;
use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerConnector};

/// The parent of the block device.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Parent {
    /// The parent is the system partition table, this is a gpt partition in the main gpt.
    SystemPartitionTable,

    /// The parent is a df driver.
    Dev,

    /// The parent is fshost. This is mainly for the fshost ramdisk.
    Fshost,
}

#[async_trait]
pub trait Device: Send + Sync {
    /// Returns BlockInfo (the result of calling fuchsia.hardware.block/Block.Query).
    async fn get_block_info(&self) -> Result<fidl_fuchsia_storage_block::BlockInfo, Error>;

    /// Returns the format as determined by content sniffing. This should be used sparingly when
    /// other means of determining the format are not possible.
    async fn content_format(&mut self) -> Result<DiskFormat, Error>;

    /// Returns the topological path.
    fn topological_path(&self) -> &str;

    /// Returns the path in the local namespace.
    fn path(&self) -> &str;

    /// Returns the source of this block device. This is primarily a debugging path, and it's
    /// different for different device types. More specifically, for services it starts with the
    /// service path in the fshost namespace, and for component sources it starts with the
    /// component moniker.
    fn source(&self) -> &str;

    /// Get the parent of this block device. This is determined by the watch source when the device
    /// is discovered, if it's relevant for this device.
    fn parent(&self) -> Parent;

    /// If this device is a partition, this returns the label. Otherwise, an error is returned.
    async fn partition_label(&mut self) -> Result<&str, Error>;

    /// If this device is a partition, this returns the type GUID. Otherwise, an error is returned.
    async fn partition_type(&mut self) -> Result<&[u8; 16], Error>;

    /// Returns a DirectoryProxy connected to the fuchsia.hardware.block.volume.Service instance
    /// directory for the device.  The device must originate from a Service instance.
    fn service_instance_directory(&self) -> Result<DirectoryProxy, Error> {
        Err(anyhow!("Device is not a volume service instance"))
    }

    /// Returns a new controller for the block device.
    fn block_connector(&self) -> Result<Box<dyn BlockConnector>, Error>;

    /// Establish a new connection to the Block interface of the device.
    fn block_proxy(&self) -> Result<BlockProxy, Error>;

    /// True if this is the ramdisk device that fshost has created or a child of the ramdisk device.
    /// NOTE: This is *only* true for the ramdisk device that fshost creates and will not be true
    /// for other ramdisks.
    fn is_fshost_ramdisk(&self) -> bool;

    /// True if this device is removable (e.g. self-reported removable media flag or connected
    /// over a USB or removable hotplug bus).
    async fn is_removable(&self) -> bool {
        false
    }

    /// Marks the device as being backed by an fshost ramdisk.
    fn set_fshost_ramdisk(&mut self, v: bool);
}

/// A device which is backed by a fuchsia.hardware.block.volume.Service instance directory.
#[derive(Debug)]
pub struct VolumeServiceDevice {
    connector: Box<DirBasedBlockConnector>,
    source: String,

    // Cache a proxy to the device's Block interface so we can use it internally.
    block_proxy: BlockProxy,

    // Memoized fields.
    content_format: Option<DiskFormat>,
    partition_label: Option<String>,
    partition_type: Option<[u8; 16]>,
    is_removable: Mutex<Option<bool>>,

    parent: Parent,
}

impl VolumeServiceDevice {
    pub async fn new(
        service_dir: &DirectoryProxy,
        instance_name: String,
        source: String,
        parent: Parent,
    ) -> Result<Self, Error> {
        let source = format!("{source}/{instance_name}");
        let instance_dir =
            fuchsia_fs::directory::open_directory(&service_dir, &instance_name, fio::PERM_READABLE)
                .await
                .context("Failed to open service instance")?;
        let connector = Box::new(DirBasedBlockConnector::new(instance_dir, "volume".to_string()));
        let block_proxy = connector.connect_block()?.into_proxy();
        Ok(Self {
            connector,
            source,
            block_proxy,
            content_format: None,
            partition_label: None,
            partition_type: None,
            is_removable: Mutex::new(None),
            parent,
        })
    }

    async fn check_is_removable(&self) -> bool {
        if self.parent == Parent::SystemPartitionTable {
            return false;
        }

        // Check 1: Self-reported removable media flag from block device info.
        if let Ok(info) = self.get_block_info().await {
            if info.flags.contains(DeviceFlag::REMOVABLE) {
                return true;
            }
        }

        // Check 2: Query Driver Framework NodeBusTopology via NodeToken.
        let token_proxy = match connect_to_named_protocol_at_dir_root::<ftoken::NodeTokenMarker>(
            self.connector.dir(),
            "token",
        ) {
            Ok(proxy) => proxy,
            Err(_) => return false,
        };
        let event_handle = match token_proxy.get().await {
            Ok(Ok(handle)) => handle,
            Ok(Err(status)) => {
                log::warn!(status:?; "Failed to get node token for device");
                return false;
            }
            Err(fidl::Error::ClientChannelClosed { epitaph, .. })
                if epitaph == zx::Status::NOT_FOUND =>
            {
                return false;
            }
            Err(err) => {
                log::warn!(err:?; "Failed to query node token for device");
                return false;
            }
        };
        let topology_proxy = match fuchsia_component::client::connect_to_protocol::<
            ftoken::NodeBusTopologyMarker,
        >() {
            Ok(proxy) => proxy,
            Err(err) => {
                log::warn!(err:?; "Failed to connect to NodeBusTopology protocol");
                return false;
            }
        };
        match topology_proxy.get(event_handle).await {
            Ok(Ok(resp)) => resp.iter().any(|bus_info| {
                matches!(
                    bus_info.bus,
                    Some(fdf::BusType::Usb)
                        | Some(fdf::BusType::UsbPeripheral)
                        | Some(fdf::BusType::Sdio)
                )
            }),
            Ok(Err(status)) => {
                log::warn!(status:?; "NodeBusTopology query returned error status");
                false
            }
            Err(err) => {
                log::warn!(err:?; "Failed to query NodeBusTopology for device");
                false
            }
        }
    }
}

#[async_trait]
impl Device for VolumeServiceDevice {
    async fn get_block_info(&self) -> Result<fidl_fuchsia_storage_block::BlockInfo, Error> {
        let block_proxy = self.block_proxy()?;
        let info = block_proxy.get_info().await?.map_err(zx::Status::err_from_raw)?;
        Ok(info)
    }

    async fn content_format(&mut self) -> Result<DiskFormat, Error> {
        if let Some(format) = self.content_format {
            return Ok(format);
        }
        let block = self.block_proxy()?;
        return Ok(detect_disk_format(&block).await);
    }

    fn path(&self) -> &str {
        &self.source
    }

    fn topological_path(&self) -> &str {
        self.connector.path()
    }

    fn source(&self) -> &str {
        &self.source
    }

    fn parent(&self) -> Parent {
        self.parent
    }

    async fn partition_label(&mut self) -> Result<&str, Error> {
        if self.partition_label.is_none() {
            let (status, name) = self.block_proxy.get_name().await?;
            zx::Status::ok(status)?;
            self.partition_label = Some(name.ok_or_else(|| anyhow!("Expected name"))?);
        }
        Ok(self.partition_label.as_ref().unwrap())
    }

    async fn partition_type(&mut self) -> Result<&[u8; 16], Error> {
        if self.partition_type.is_none() {
            let (status, partition_type) = self.block_proxy.get_type_guid().await?;
            zx::Status::ok(status)?;
            self.partition_type =
                Some(partition_type.ok_or_else(|| anyhow!("Expected type"))?.value);
        }
        Ok(self.partition_type.as_ref().unwrap())
    }

    fn service_instance_directory(&self) -> Result<DirectoryProxy, Error> {
        let (instance_dir, server_end) = create_proxy::<fio::DirectoryMarker>();
        self.connector.dir().clone(server_end.into_channel().into())?;
        Ok(instance_dir)
    }

    fn block_connector(&self) -> Result<Box<dyn BlockConnector>, Error> {
        Ok(self.connector.clone())
    }

    fn block_proxy(&self) -> Result<BlockProxy, Error> {
        self.connector.connect_block().and_then(|c| Ok(c.into_proxy()))
    }

    fn is_fshost_ramdisk(&self) -> bool {
        false
    }

    async fn is_removable(&self) -> bool {
        if let Some(cached) = *self.is_removable.lock().unwrap() {
            return cached;
        }
        let is_removable = self.check_is_removable().await;
        *self.is_removable.lock().unwrap() = Some(is_removable);
        is_removable
    }

    fn set_fshost_ramdisk(&mut self, _v: bool) {}
}

/// A device backed by a local BlockServer running in fshost.
pub struct LocalBlockDevice {
    // The thread that runs the block server and handles connections.
    thread: Option<JoinHandle<()>>,
    // A handle which is used to shut down the executor running in `thread`.
    abort_handle: AbortHandle,

    connector: Arc<VmoBackedServerConnector>,

    // Cache a proxy to the device's Block interface so we can use it internally.
    block_proxy: BlockProxy,

    // Memoized fields.
    content_format: Option<DiskFormat>,
}

impl Drop for LocalBlockDevice {
    fn drop(&mut self) {
        self.abort_handle.abort();
        self.thread.take().map(|t| t.join());
    }
}

impl LocalBlockDevice {
    /// Runs `server` in a dedicated thread.
    ///
    /// This server runs in a dedicated thread to avoid issues with reentrant synchronous calls from
    /// fshost to the server.  For example, fshost might try to read key-bag contents from a
    /// filesystem running in the device, which is synchronous due to using regular POSIX filesystem
    /// APIs.
    pub async fn new(server: VmoBackedServer) -> Result<Self, Error> {
        let server = Arc::new(server);
        let (abort_handle, registration) = AbortHandle::new_pair();
        let (scope_tx, scope_rx) = futures::channel::oneshot::channel();
        // Note that to avoid deadlock, resources must not be shared between this thread and the
        // rest of the fshost executor.  Note that tasks spawned in `scope` run in this thread.
        // Take care when modifying this!
        let thread = std::thread::spawn(move || {
            let mut executor = fasync::LocalExecutor::default();
            scope_tx.send(executor.root_scope().clone()).unwrap();
            let _ = executor
                .run_singlethreaded(Abortable::new(std::future::pending::<()>(), registration));
        });
        let scope = scope_rx.await.unwrap();
        let connector = Arc::new(VmoBackedServerConnector::new_with_scope(server, scope));
        let block_proxy = connector.connect_block()?.into_proxy();
        Ok(Self {
            thread: Some(thread),
            abort_handle,
            connector,
            block_proxy,
            content_format: None,
        })
    }
}

#[async_trait]
impl Device for LocalBlockDevice {
    async fn get_block_info(&self) -> Result<fidl_fuchsia_storage_block::BlockInfo, Error> {
        let info = self.block_proxy.get_info().await?.map_err(zx::Status::err_from_raw)?;
        Ok(info)
    }

    async fn content_format(&mut self) -> Result<DiskFormat, Error> {
        if let Some(format) = self.content_format {
            return Ok(format);
        }
        return Ok(detect_disk_format(&self.block_proxy).await);
    }

    fn path(&self) -> &str {
        "fshost-ramdisk"
    }

    fn topological_path(&self) -> &str {
        "fshost-ramdisk"
    }

    fn source(&self) -> &str {
        "fshost-ramdisk"
    }

    fn parent(&self) -> Parent {
        Parent::Fshost
    }

    async fn partition_label(&mut self) -> Result<&str, Error> {
        Err(anyhow!("partition_label not supported for ramdisk"))
    }

    async fn partition_type(&mut self) -> Result<&[u8; 16], Error> {
        Err(anyhow!("partition_type not supported for ramdisk"))
    }

    fn block_connector(&self) -> Result<Box<dyn BlockConnector>, Error> {
        Ok(Box::new(self.connector.clone()))
    }

    fn block_proxy(&self) -> Result<BlockProxy, Error> {
        Ok(self.connector.connect_block()?.into_proxy())
    }

    fn is_fshost_ramdisk(&self) -> bool {
        true
    }

    fn set_fshost_ramdisk(&mut self, _v: bool) {}
}

/// RegisteredDevices keeps track of significant devices so that they can be found later as
/// required.  Devices can be associated with a tag.
pub struct RegisteredDevices(Condition<HashMap<DeviceTag, Box<dyn Device>>>);

impl Default for RegisteredDevices {
    fn default() -> Self {
        Self(Condition::new(HashMap::default()))
    }
}

impl RegisteredDevices {
    /// Registers a device with the specified tag.  This *only* registers the first device with the
    /// tag.
    pub fn register_device(&self, tag: DeviceTag, device: Box<dyn Device>) {
        let mut map = self.0.lock();
        if let Entry::Vacant(v) = map.entry(tag) {
            v.insert(device);
        }
        for waker in map.drain_wakers() {
            waker.wake();
        }
    }

    /// Returns the topological path for the device with the specified tag, if registered.
    pub fn get_topological_path(&self, tag: DeviceTag) -> Option<String> {
        self.0.lock().get(&tag).map(|d| d.topological_path().to_string())
    }

    /// Returns a block_connector for the device with the specified tag.  This will wait till the
    /// device is registered.
    pub async fn get_block_connector(
        &self,
        tag: DeviceTag,
    ) -> Result<Box<dyn BlockConnector>, Error> {
        self.0
            .when(|map| map.get(&tag).map_or(Poll::Pending, |d| Poll::Ready(d.block_connector())))
            .await
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeviceTag {
    /// The fshost ramdisk device.
    Ramdisk,

    /// The block device containing the partition table in which the Fuchsia system resides.
    SystemPartitionTable,

    /// The non-ramdisk block device in which the Fuchsia system resides (which is either an FVM
    /// instance, or an Fxblob instance).  Only set on recovery (and volumes within the container
    /// will not be bound).
    SystemContainerOnRecovery,
}

#[cfg(test)]
mod tests {
    use super::{Device, Parent, VolumeServiceDevice};
    use fidl_fuchsia_storage_block::{BlockInfo, BlockRequest, BlockRequestStream, DeviceFlag};
    use futures::StreamExt;
    use vfs::execution_scope::ExecutionScope;

    #[fuchsia::test]
    async fn volume_service_device_is_removable_missing_token() {
        let service_dir = vfs::pseudo_directory! {
            "000" => vfs::pseudo_directory! {
                "volume" => vfs::service::host(move |mut stream: BlockRequestStream| async move {
                    while let Some(Ok(request)) = stream.next().await {
                        match request {
                            BlockRequest::GetInfo { responder } => {
                                let _ = responder.send(Ok(&BlockInfo {
                                    block_count: 100,
                                    block_size: 512,
                                    max_transfer_size: 1024 * 1024,
                                    flags: DeviceFlag::empty(),
                                }));
                            }
                            req => panic!("Unexpected request: {req:?}"),
                        }
                    }
                }),
            },
        };

        let dir_proxy = vfs::directory::serve_read_only(service_dir, ExecutionScope::new());
        let device = VolumeServiceDevice::new(
            &dir_proxy,
            "000".to_string(),
            "source".to_string(),
            Parent::Dev,
        )
        .await
        .unwrap();
        assert!(!device.is_removable().await);
    }

    #[fuchsia::test]
    async fn volume_service_device_system_partition_table_not_removable() {
        let service_dir = vfs::pseudo_directory! {
            "000" => vfs::pseudo_directory! {
                "volume" => vfs::service::host(move |mut stream: BlockRequestStream| async move {
                    if let Some(request) = stream.next().await {
                        match request {
                            Ok(BlockRequest::GetInfo { .. }) => {
                                panic!("GetInfo called unexpectedly");
                            }
                            req => panic!("Unexpected request: {req:?}"),
                        }
                    }
                }),
            },
        };

        let dir_proxy = vfs::directory::serve_read_only(service_dir, ExecutionScope::new());
        let device = VolumeServiceDevice::new(
            &dir_proxy,
            "000".to_string(),
            "source".to_string(),
            Parent::SystemPartitionTable,
        )
        .await
        .unwrap();
        assert!(!device.is_removable().await);
    }
}
