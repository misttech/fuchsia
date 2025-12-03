// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::fuchsia::{RemoteFs, RemoteNode};
use crate::task::{CurrentTask, LockedAndTask};
use crate::vfs::{
    CacheConfig, CacheMode, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions,
    FsNodeHandle, FsStr,
};
use fidl::endpoints::{DiscoverableProtocolMarker, SynchronousProxy, create_sync_proxy};
use fidl_fuchsia_fshost::StarnixVolumeProviderMarker;
use fidl_fuchsia_fxfs::CryptMarker;
use fidl_fuchsia_io as fio;
use starnix_crypt::CryptService;
use starnix_logging::{log_error, log_info};
use starnix_sync::{FileOpsCore, Locked, Unlocked};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, from_status_like_fdio, statfs};
use std::sync::Arc;
use syncio::{Zxio, zxio_node_attr_has_t, zxio_node_attributes_t};

const CRYPT_THREAD_ROLE: &str = "fuchsia.starnix.remotevol.crypt";
// `KEY_FILE_PATH` determines where the volume-wide keys for the Starnix volume will live in the
// container's data storage capability.
const KEY_FILE_PATH: &str = "key_file";

pub struct RemoteVolume {
    remotefs: RemoteFs,
    exposed_dir_proxy: fio::DirectorySynchronousProxy,
    crypt_service: Arc<CryptService>,
}

impl RemoteVolume {
    pub fn remotefs(&self) -> &RemoteFs {
        &self.remotefs
    }
}

impl FileSystemOps for RemoteVolume {
    fn statfs(
        &self,
        locked: &mut Locked<FileOpsCore>,
        fs: &FileSystem,
        current_task: &CurrentTask,
    ) -> Result<statfs, Errno> {
        self.remotefs.statfs(locked, fs, current_task)
    }

    fn name(&self) -> &'static FsStr {
        "remotevol".into()
    }

    fn uses_external_node_ids(&self) -> bool {
        self.remotefs.uses_external_node_ids()
    }

    fn rename(
        &self,
        locked: &mut Locked<FileOpsCore>,
        fs: &FileSystem,
        current_task: &CurrentTask,
        old_parent: &FsNodeHandle,
        old_name: &FsStr,
        new_parent: &FsNodeHandle,
        new_name: &FsStr,
        renamed: &FsNodeHandle,
        replaced: Option<&FsNodeHandle>,
    ) -> Result<(), Errno> {
        self.remotefs.rename(
            locked,
            fs,
            current_task,
            old_parent,
            old_name,
            new_parent,
            new_name,
            renamed,
            replaced,
        )
    }

    fn unmount(&self) {
        let (proxy, server_end) = create_sync_proxy::<fidl_fuchsia_fs::AdminMarker>();
        if let Err(e) = fdio::service_connect_at(
            self.exposed_dir_proxy.as_channel(),
            &format!("svc/{}", fidl_fuchsia_fs::AdminMarker::PROTOCOL_NAME),
            server_end.into(),
        ) {
            log_error!(e:%; "StarnixVolumeProvider.Unmount failed to connect to fuchsia.fs.Admin");
            return;
        }

        if let Err(e) = proxy.shutdown(zx::MonotonicInstant::INFINITE) {
            log_error!(e:%; "StarnixVolumeProvider.Unmount failed at FIDL layer");
        }
    }

    fn crypt_service(&self) -> Option<Arc<CryptService>> {
        Some(self.crypt_service.clone())
    }
}

// Key file
// ========
//
// Version 1:
//
//   +------- 32 -------+------- 32 -------+
//   |   metadata key   |     data key     |
//   +------------------+------------------+
//
// Version 2:
//
//   +-2-+------- 32 -------+------- 32 -------+
//   | V |   metadata key   |     data key     |
//   +---+------------------+------------------+
//
// Version 2 includes a 16 bit version which indicates the version of the key file.  The key
// identifiers used for version 2 key files will use the lblk32 algorithm for derivation which
// differs from version 1, which uses a, deprecated, Fuchsia specific derivation.

struct VolumeKeys {
    metadata: [u8; 32],
    data: [u8; 32],
    use_lblk32_identifiers: bool,
}

impl VolumeKeys {
    // `KEYS_SIZE` is the size of the two keys (the metadata key, and the data key) stored in the
    // key file.
    const KEYS_SIZE: usize = 64;

    // Version 1 does not include a version.
    const V1_FILE_SIZE: usize = Self::KEYS_SIZE;

    // Includes 2 bytes for the version.
    const FILE_SIZE: usize = 2 + Self::KEYS_SIZE;

    const LATEST_VERSION: u16 = 2;

    /// Returns (keys, did_create).
    fn get_or_create(
        data: &fio::DirectorySynchronousProxy,
        key_path: &str,
    ) -> Result<(Self, bool), Errno> {
        if let Some(keys) = Self::get(data, key_path)? {
            Ok((keys, false))
        } else {
            log_info!("Creating key file at {key_path}");
            Ok((Self::create(data, key_path)?, true))
        }
    }

    /// Returns None rather than an error if the key file does not exist or is corrupt,
    /// but returns all other errors (e.g. if the connection to `data` is closed).
    fn get(data: &fio::DirectorySynchronousProxy, key_path: &str) -> Result<Option<Self>, Errno> {
        match syncio::directory_read_file(data, key_path, zx::MonotonicInstant::INFINITE) {
            Ok(bytes) => {
                if bytes.len() == Self::FILE_SIZE {
                    // Version 2
                    if u16::from_le_bytes(bytes[0..2].try_into().unwrap()) != Self::LATEST_VERSION {
                        return Ok(None);
                    }
                    Ok(Some(Self {
                        metadata: bytes[2..34].try_into().unwrap(),
                        data: bytes[34..66].try_into().unwrap(),
                        use_lblk32_identifiers: true,
                    }))
                } else if bytes.len() == Self::V1_FILE_SIZE {
                    // Version 1
                    Ok(Some(Self {
                        metadata: bytes[..32].try_into().unwrap(),
                        data: bytes[32..].try_into().unwrap(),
                        use_lblk32_identifiers: false,
                    }))
                } else {
                    Ok(None)
                }
            }
            Err(zx::Status::NOT_FOUND) => Ok(None),
            Err(status) => {
                log_error!("Failed to read key file: {status:?}");
                Err(from_status_like_fdio!(status))
            }
        }
    }

    /// Creates a new key file at the latest version, with new random metadata and data keys.
    fn create(data: &fio::DirectorySynchronousProxy, key_path: &str) -> Result<Self, Errno> {
        let mut bytes = [0; Self::FILE_SIZE];
        bytes[..2].copy_from_slice(&Self::LATEST_VERSION.to_le_bytes());
        zx::cprng_draw(&mut bytes[2..]);
        let tmp_file = syncio::directory_create_tmp_file(
            data,
            fio::PERM_READABLE,
            zx::MonotonicInstant::INFINITE,
        )
        .map_err(|e| {
            let err = from_status_like_fdio!(e);
            log_error!("Failed to create tmp file with error: {:?}", err);
            err
        })?;
        tmp_file
            .write(&bytes, zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!("FIDL transport error on File.Write {:?}", e);
                errno!(ENOENT)
            })?
            .map_err(|e| {
                let err = from_status_like_fdio!(zx::Status::from_raw(e));
                log_error!("File.Write failed with {:?}", err);
                err
            })?;
        tmp_file
            .sync(zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!("FIDL transport error on File.Sync {:?}", e);
                errno!(ENOENT)
            })?
            .map_err(|e| {
                let err = from_status_like_fdio!(zx::Status::from_raw(e));
                log_error!("File.Sync failed with {:?}", err);
                err
            })?;
        let (status, token) = data.get_token(zx::MonotonicInstant::INFINITE).map_err(|e| {
            log_error!("transport error on get_token for the data directory, error: {:?}", e);
            errno!(ENOENT)
        })?;
        zx::Status::ok(status).map_err(|e| {
            let err = from_status_like_fdio!(e);
            log_error!("Failed to get_token for the data directory, error: {:?}", err);
            err
        })?;

        tmp_file
            .link_into(
                zx::Event::from(token.ok_or_else(|| errno!(ENOENT))?),
                key_path,
                zx::MonotonicInstant::INFINITE,
            )
            .map_err(|e| {
                log_error!("FIDL transport error on File.LinkInto {:?}", e);
                errno!(EIO)
            })?
            .map_err(|e| {
                let err = from_status_like_fdio!(zx::Status::from_raw(e));
                log_error!("File.LinkInto failed with {:?}", err);
                err
            })?;
        Ok(Self {
            metadata: bytes[2..34].try_into().unwrap(),
            data: bytes[34..].try_into().unwrap(),
            use_lblk32_identifiers: true,
        })
    }
}

pub fn new_remote_vol(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    let kernel = current_task.kernel();
    let volume_provider = current_task
        .kernel()
        .connect_to_protocol_at_container_svc::<StarnixVolumeProviderMarker>()
        .map_err(|_| errno!(ENOENT))?
        .into_sync_proxy();

    let (crypt_client_end, crypt_proxy) = fidl::endpoints::create_endpoints::<CryptMarker>();

    let data = match kernel.container_namespace.get_namespace_channel("/data") {
        Ok(channel) => fio::DirectorySynchronousProxy::new(channel),
        Err(err) => {
            log_error!("Unable to find a channel for /data. Received error: {}", err);
            return Err(errno!(ENOENT));
        }
    };

    let (keys, created_key_file) = VolumeKeys::get_or_create(&data, KEY_FILE_PATH)?;

    let crypt_service =
        Arc::new(CryptService::new(&keys.metadata, &keys.data, keys.use_lblk32_identifiers, None));

    let (exposed_dir_client_end, exposed_dir_server) =
        fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();

    {
        let crypt_service = Arc::clone(&crypt_service);
        kernel.kthreads.spawner().spawn_async_with_role(
            CRYPT_THREAD_ROLE,
            async move |_: LockedAndTask<'_>| {
                if let Err(e) = crypt_service.handle_connection(crypt_proxy.into_stream()).await {
                    log_error!("Error while handling a Crypt request {e}");
                }
            },
        );
    }

    let mode = if created_key_file {
        fidl_fuchsia_fshost::MountMode::AlwaysCreate
    } else {
        fidl_fuchsia_fshost::MountMode::MaybeCreate
    };
    let guid = volume_provider
        .mount(crypt_client_end, mode, exposed_dir_server, zx::MonotonicInstant::INFINITE)
        .map_err(|e| {
            log_error!("FIDL transport error on StarnixVolumeProvider.Mount {:?}", e);
            errno!(ENOENT)
        })?
        .map_err(|e| {
            let error = from_status_like_fdio!(zx::Status::from_raw(e));
            log_error!(error:?; "StarnixVolumeProvider.Mount failed");
            error
        })?;

    crypt_service.set_uuid(guid);

    let exposed_dir_proxy = exposed_dir_client_end.into_sync_proxy();

    let root = syncio::directory_open_directory_async(
        &exposed_dir_proxy,
        "root",
        fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .map_err(|e| errno!(EIO, format!("Failed to open root: {e}")))?;

    let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;

    let (client_end, server_end) = zx::Channel::create();
    let remotefs = RemoteFs::new(root.into_channel(), server_end)?;
    let mut attrs = zxio_node_attributes_t {
        has: zxio_node_attr_has_t { id: true, ..Default::default() },
        ..Default::default()
    };
    let (remote_node, node_id) =
        match Zxio::create_with_on_representation(client_end.into(), Some(&mut attrs)) {
            Err(status) => return Err(from_status_like_fdio!(status)),
            Ok(zxio) => (RemoteNode::new(zxio, rights), attrs.id),
        };

    let use_remote_ids = remotefs.use_remote_ids();
    let remotevol = RemoteVolume { remotefs, exposed_dir_proxy, crypt_service };
    let fs = FileSystem::new(
        locked,
        kernel,
        CacheMode::Cached(CacheConfig::default()),
        remotevol,
        options,
    )?;
    if use_remote_ids {
        fs.create_root(node_id, remote_node);
    } else {
        let root_ino = fs.allocate_ino();
        fs.create_root(root_ino, remote_node);
    }
    Ok(fs)
}
