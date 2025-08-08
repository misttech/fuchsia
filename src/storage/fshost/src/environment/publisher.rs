// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::Device;
use anyhow::Error;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServerEnd};
use fidl_fuchsia_hardware_block_volume::VolumeMarker;
use fidl_fuchsia_io as fio;
use fs_management::filesystem::BlockConnector;
use pseudo_fs::{LazyPseudoDirectory, PseudoDirectory, PseudoFile, ToPseudoDirectory};
use std::sync::Arc;
use vfs::directory::helper::DirectlyMutable;
use vfs::service::endpoint;
use vfs::ExecutionScope;

pub trait SinglePublisher: Send + Sync {
    fn publish(self: Box<Self>, device: &dyn Device) -> Result<(), Error>;
}

/// A staged entry is one where the entry is placed in the block directory but it hasn't been
/// matched yet. Once it's matched, the matcher can use this to finish publishing the device. Until
/// it is completely published, open requests will be queued in the standard dangling-server-end
/// pipelining way.
pub struct StagedPublisher {
    scope: ExecutionScope,
    server_end: ServerEnd<fio::DirectoryMarker>,
}

impl StagedPublisher {
    fn new(scope: ExecutionScope) -> (fio::DirectoryProxy, Self) {
        let (proxy, server_end) = fidl::endpoints::create_proxy();
        (proxy, Self { scope, server_end })
    }
}

impl SinglePublisher for StagedPublisher {
    fn publish(self: Box<Self>, device: &dyn Device) -> Result<(), Error> {
        let volume = device.block_connector()?;
        let entry = vfs::pseudo_directory! {
            VolumeMarker::PROTOCOL_NAME => endpoint(move |_scope, channel| {
                volume.connect_channel_to_volume(channel.into_zx_channel().into())
                    .unwrap_or_else(|error| {
                        log::error!(error:%; "failed to open volume");
                    });
            }),
        };
        vfs::directory::serve_on(entry, fio::PERM_READABLE, self.scope, self.server_end);

        Ok(())
    }
}

pub struct DevicePublisher {
    scope: ExecutionScope,

    // The inner block directories are LazyPseudoDirectory directories and not this directory
    // because this directory is exposed from fshost and component_manager calls
    // fuchsia.unknown/Cloneable.Clone on it. The Clone call would force LazyPseudoDirectory to
    // materialize the directory eliminating any of the potential savings.
    debug_block_dir: Arc<PseudoDirectory>,

    block_dir: Arc<PseudoDirectory>,
}

impl DevicePublisher {
    pub fn new(scope: ExecutionScope) -> Self {
        Self { scope, debug_block_dir: PseudoDirectory::new(), block_dir: PseudoDirectory::new() }
    }

    /// Publishes *all* block devices.  Only suitable for routing outside of fshost on eng
    /// configurations.
    pub fn debug_block_dir(&self) -> Arc<PseudoDirectory> {
        self.debug_block_dir.clone()
    }

    /// Publishes block devices which are not already managed by fshost (i.e. block devices from the
    /// low-level storage drivers).
    pub fn block_dir(&self) -> Arc<PseudoDirectory> {
        self.block_dir.clone()
    }

    pub fn publish_to_debug_block_dir(&self, device: &dyn Device, name: &str) -> Result<(), Error> {
        let volume = device.block_connector()?;
        self.debug_block_dir.add_entry(
            name,
            LazyPseudoDirectory::new(BlockDirectoryInfo {
                volume,
                source: device.source().as_bytes().into(),
            }),
        )?;
        Ok(())
    }

    pub fn stage(&self, name: &str) -> Result<StagedPublisher, Error> {
        let (proxy, staged_publisher) = StagedPublisher::new(self.scope.clone());
        self.block_dir.add_entry(name, vfs::remote::remote_dir(proxy))?;
        Ok(staged_publisher)
    }

    pub fn publish(&mut self, volume: Box<dyn BlockConnector>, name: &str) -> Result<(), Error> {
        self.block_dir.add_entry(
            name,
            vfs::pseudo_directory! {
                VolumeMarker::PROTOCOL_NAME => endpoint(move |_scope, channel| {
                    volume.connect_channel_to_volume(channel.into_zx_channel().into())
                        .unwrap_or_else(|error| {
                            log::error!(error:%; "failed to open volume");
                        });
                }),
            },
        )?;
        Ok(())
    }
}

struct BlockDirectoryInfo {
    volume: Box<dyn BlockConnector + 'static>,
    source: Box<[u8]>,
}

impl ToPseudoDirectory for BlockDirectoryInfo {
    fn to_pseudo_directory(self) -> Arc<PseudoDirectory> {
        vfs::pseudo_directory! {
            VolumeMarker::PROTOCOL_NAME => endpoint(move |_scope, channel| {
                self.volume.connect_channel_to_volume(channel.into_zx_channel().into())
                    .unwrap_or_else(|error| {
                        log::error!(error:%; "failed to open volume");
                    });
            }),
            "source" => PseudoFile::from_data(self.source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Parent;
    use async_trait::async_trait;
    use fidl::endpoints::Proxy;
    use fidl_fuchsia_hardware_block::BlockProxy;
    use fidl_fuchsia_hardware_block_volume::VolumeProxy;
    use fs_management::format::DiskFormat;
    use fuchsia_fs::directory::read_file_to_string;
    use futures::channel::mpsc;
    use futures::StreamExt;
    use zx::AsHandleRef;

    struct MockDevice {
        source: &'static str,
        volume: mpsc::UnboundedSender<ServerEnd<VolumeMarker>>,
    }

    #[async_trait]
    impl Device for MockDevice {
        async fn get_block_info(&self) -> Result<fidl_fuchsia_hardware_block::BlockInfo, Error> {
            unimplemented!()
        }
        fn is_nand(&self) -> bool {
            unimplemented!()
        }
        async fn content_format(&mut self) -> Result<DiskFormat, Error> {
            unimplemented!()
        }
        fn topological_path(&self) -> &str {
            unimplemented!()
        }
        fn path(&self) -> &str {
            unimplemented!()
        }
        fn source(&self) -> &str {
            self.source
        }
        fn parent(&self) -> Parent {
            unimplemented!()
        }
        async fn partition_label(&mut self) -> Result<&str, Error> {
            unimplemented!()
        }
        async fn partition_type(&mut self) -> Result<&[u8; 16], Error> {
            unimplemented!()
        }
        async fn partition_instance(&mut self) -> Result<&[u8; 16], Error> {
            unimplemented!()
        }
        fn block_connector(&self) -> Result<Box<dyn BlockConnector>, Error> {
            let volume = self.volume.clone();
            Ok(Box::new(move |server_end: ServerEnd<VolumeMarker>| {
                volume.unbounded_send(server_end).unwrap();
                Ok(())
            }))
        }
        fn block_proxy(&self) -> Result<BlockProxy, Error> {
            unimplemented!()
        }
        fn volume_proxy(&self) -> Result<VolumeProxy, Error> {
            unimplemented!()
        }
        async fn get_child(&self, _suffix: &str) -> Result<Box<dyn Device>, Error> {
            unimplemented!()
        }
        fn is_fshost_ramdisk(&self) -> bool {
            unimplemented!()
        }
        fn set_fshost_ramdisk(&mut self, _v: bool) {
            unimplemented!()
        }
    }

    #[fuchsia::test]
    async fn test_publish_to_debug_block_dir() {
        let scope = ExecutionScope::new();
        let device_publisher = DevicePublisher::new(scope.clone());
        let (client, server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        vfs::directory::serve_on(
            device_publisher.debug_block_dir(),
            fio::PERM_READABLE,
            scope,
            server_end,
        );
        let (sender, mut receiver) = mpsc::unbounded();
        let mock_device = MockDevice { source: "mock-source", volume: sender };
        device_publisher.publish_to_debug_block_dir(&mock_device, "001").unwrap();

        // Check that the "source" file is correct.
        assert_eq!(
            read_file_to_string(&client, "001/source").await.expect("failed to read file"),
            "mock-source"
        );

        // Check that connecting to the volume works.
        let path = "001/".to_string() + VolumeMarker::PROTOCOL_NAME;
        let volume = fuchsia_fs::directory::open_async::<VolumeMarker>(
            &client,
            &path,
            fio::Flags::PROTOCOL_SERVICE,
        )
        .unwrap();
        let volume_client = receiver.next().await.unwrap();
        assert_eq!(
            volume.as_channel().as_handle_ref().basic_info().unwrap().related_koid,
            volume_client.channel().as_handle_ref().get_koid().unwrap()
        );
    }
}
