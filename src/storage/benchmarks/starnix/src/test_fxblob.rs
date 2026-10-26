// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::CompressionMode;
use erofs_serializer::SerializerNode;
use fidl::endpoints::{ControlHandle as _, RequestStream as _};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream};
use fidl_fuchsia_storage_partitions as fpartitions;
use fuchsia_component::client::{Service, connect_to_protocol};
use fuchsia_component::server::ServiceFs;
use fuchsia_merkle::Hash;
use fuchsia_storage_benchmarks::block_devices::BenchmarkVolumeFactory;
use fuchsia_storage_benchmarks::filesystems::{
    BlobFilesystem as _, DeliveryBlob, Fxblob, FxblobInstance,
};
use futures::StreamExt as _;
use futures::lock::Mutex as AsyncMutex;
use rand::{RngExt as _, SeedableRng as _};
use rand_xorshift::XorShiftRng;
use std::ops::Range;
use std::sync::Arc;
use storage_benchmarks::{Filesystem as _, FilesystemConfig as _};
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::directory::entry_container::Directory;
use vfs::directory::immutable::connection::ImmutableConnection;
use vfs::directory::traversal_position::TraversalPosition;
use vfs::execution_scope::ExecutionScope;
use vfs::file::{GetVmo as _, StreamIoConnection};
use vfs::{ObjectRequestRef, immutable_attributes};
use vmo_blob::VmoBlob;

const RNG_SEED: u64 = 0xda782a0c3ce1819a;
const EROFS_FILE_OFFSET: usize = 4096;
const IMAGE_INODE_NUM: u64 = 3;
const IO_FILE_SIZE: usize = 8 * 1024 * 1024;
const COLD_PAGE_FAULT_SIZES: [usize; 3] = [1024, 4096, 131072];
const STAT_OPEN_FILE_COUNT: usize = 100;
const NESTED_FILE_COUNT: usize = 100;
const NESTED_DEPTH: usize = 5;

fn create_compressible_bytes(size: usize, rng: &mut XorShiftRng) -> Vec<u8> {
    const RUN_RANGE: Range<usize> = 2..8;
    let mut data = vec![0u8; size];
    let mut rest = data.as_mut_slice();
    while !rest.is_empty() {
        let chunk =
            if rest.len() < RUN_RANGE.end { rest.len() } else { rng.random_range(RUN_RANGE) };
        let value: u8 = rng.random();
        let (l, r) = rest.split_at_mut(chunk);
        rest = r;
        l.fill(value);
    }
    data
}

fn build_directory_tree(
    files_per_dir: usize,
    dirs_per_dir: usize,
    depth: usize,
    max_depth: usize,
) -> Vec<SerializerNode> {
    let mut entries = Vec::with_capacity(files_per_dir + dirs_per_dir);
    for i in 0..files_per_dir {
        entries
            .push(SerializerNode::File { name: format!("file-{i:03}.txt"), data: vec![b'x'; 64] });
    }
    if depth < max_depth {
        for i in 0..dirs_per_dir {
            entries.push(SerializerNode::Directory {
                name: format!("dir-{i:03}"),
                entries: build_directory_tree(files_per_dir, dirs_per_dir, depth + 1, max_depth),
            });
        }
    }
    entries
}

fn build_nested_file_chain(n: usize, depth: usize, max_depth: usize) -> SerializerNode {
    let name = format!("dir-{n:03}-{depth:03}");
    let entries = if depth + 1 == max_depth {
        vec![SerializerNode::File { name: format!("file-{n:03}.txt"), data: vec![b'x'; 64] }]
    } else {
        vec![build_nested_file_chain(n, depth + 1, max_depth)]
    };
    SerializerNode::Directory { name, entries }
}

fn serialize_into_delivery_blob(nodes: &[SerializerNode], mode: CompressionMode) -> DeliveryBlob {
    let erofs_image = erofs_serializer::serialize(nodes);
    let mut blob_bytes = vec![0xABu8; EROFS_FILE_OFFSET];
    blob_bytes.extend_from_slice(&erofs_image);
    DeliveryBlob::new(blob_bytes, mode)
}

fn build_erofs_blob() -> DeliveryBlob {
    let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);

    let io_dir = SerializerNode::Directory {
        name: "io".to_string(),
        entries: vec![SerializerNode::File {
            name: "file".to_string(),
            data: create_compressible_bytes(IO_FILE_SIZE, &mut rng),
        }],
    };

    let cold_page_fault_entries = COLD_PAGE_FAULT_SIZES
        .into_iter()
        .map(|size| SerializerNode::File {
            name: format!("file_{size}"),
            data: create_compressible_bytes(size, &mut rng),
        })
        .collect();
    let cold_page_fault_dir = SerializerNode::Directory {
        name: "cold_page_fault".to_string(),
        entries: cold_page_fault_entries,
    };

    let stat_open_entries = (0..STAT_OPEN_FILE_COUNT)
        .map(|i| SerializerNode::File { name: format!("file-{i:03}.txt"), data: vec![b'x'; 64] })
        .collect();
    let stat_open_dir =
        SerializerNode::Directory { name: "stat_open".to_string(), entries: stat_open_entries };

    let nested_entries =
        (0..NESTED_FILE_COUNT).map(|n| build_nested_file_chain(n, 0, NESTED_DEPTH)).collect();
    let nested_dir =
        SerializerNode::Directory { name: "nested".to_string(), entries: nested_entries };

    let walk_tree_dir = SerializerNode::Directory {
        name: "walk_tree".to_string(),
        entries: build_directory_tree(3, 2, 0, 5),
    };

    let git_tree_dir = SerializerNode::Directory {
        name: "git_tree".to_string(),
        entries: build_directory_tree(4, 2, 0, 7),
    };

    serialize_into_delivery_blob(
        &[io_dir, cold_page_fault_dir, stat_open_dir, nested_dir, walk_tree_dir, git_tree_dir],
        CompressionMode::Always,
    )
}

fn build_bundle_metadata() -> Arc<[u8]> {
    let mut metadata = ext4_metadata::Metadata::new();
    metadata.insert_directory(
        ext4_metadata::ROOT_INODE_NUM,
        ext4_metadata::S_IFDIR | 0o755,
        0,
        0,
        Default::default(),
    );
    metadata.insert_file(IMAGE_INODE_NUM, ext4_metadata::S_IFREG | 0o644, 0, 0, Default::default());
    metadata.add_child(&["image.erofs"], IMAGE_INODE_NUM);
    metadata.serialize().into()
}

struct ErofsBlobSubdir {
    fs: Arc<FxblobInstance>,
    debug_proxy: fidl_fuchsia_fxfs::DebugProxy,
    blob_hash: Hash,
    metadata_bytes: Arc<[u8]>,
    current_blob: AsyncMutex<Option<Arc<VmoBlob>>>,
}

impl ErofsBlobSubdir {
    fn new(
        fs: Arc<FxblobInstance>,
        debug_proxy: fidl_fuchsia_fxfs::DebugProxy,
        blob_hash: Hash,
        metadata_bytes: Arc<[u8]>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            debug_proxy,
            blob_hash,
            metadata_bytes,
            current_blob: AsyncMutex::new(None),
        })
    }
}

impl GetEntryInfo for ErofsBlobSubdir {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(fio::INO_UNKNOWN, fio::DirentType::Directory)
    }
}

impl DirectoryEntry for ErofsBlobSubdir {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), zx::Status> {
        request.open_dir(self)
    }
}

impl vfs::node::Node for ErofsBlobSubdir {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        Ok(immutable_attributes!(
            requested_attributes,
            Immutable {
                protocols: fio::NodeProtocolKinds::DIRECTORY,
                abilities: fio::Operations::GET_ATTRIBUTES
                    | fio::Operations::ENUMERATE
                    | fio::Operations::TRAVERSE,
                id: 1,
            }
        ))
    }
}

impl Directory for ErofsBlobSubdir {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        path: vfs::Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        if path.is_empty() {
            object_request
                .take()
                .create_connection_sync::<ImmutableConnection<_>, _>(scope, self, flags);
            return Ok(());
        }
        if path.is_dir() {
            return Err(zx::Status::NOT_DIR);
        }

        let path_str = path.as_ref();
        if path_str == "metadata.v1" {
            let this = self.clone();
            let object_request = object_request.take();
            scope.clone().spawn(object_request.handle_async(async move |object_request| {
                let mut blob_guard = this.current_blob.lock().await;
                if let Some(prev_blob) = blob_guard.take() {
                    while Arc::strong_count(&prev_blob) > 1 {
                        fuchsia_async::Timer::new(std::time::Duration::from_millis(1)).await;
                    }
                    if let Ok(info) = prev_blob.get_vmo().info()
                        && info.num_children > 0
                    {
                        let _ = fuchsia_async::OnSignals::new(
                            prev_blob.get_vmo(),
                            zx::Signals::VMO_ZERO_CHILDREN,
                        )
                        .await;
                    }
                    drop(prev_blob);
                }
                this.debug_proxy
                    .clear_caches()
                    .await
                    .expect("ClearCaches transport error")
                    .expect("ClearCaches failed");
                let vmo = this.fs.get_vmo(&this.blob_hash).await;
                *blob_guard = Some(VmoBlob::new(vmo, this.blob_hash.into()));
                drop(blob_guard);
                let metadata_file = vfs::file::vmo::read_only(this.metadata_bytes.clone());
                vfs::file::serve(metadata_file, scope, &flags, object_request)
            }));
            return Ok(());
        }

        if path_str == IMAGE_INODE_NUM.to_string() {
            let this = self.clone();
            let object_request = object_request.take();
            scope.clone().spawn(object_request.handle_async(async move |object_request| {
                let blob_file = this
                    .current_blob
                    .lock()
                    .await
                    .as_ref()
                    .expect("metadata.v1 must be opened before inode 3")
                    .clone();
                object_request
                    .create_connection::<StreamIoConnection<_>, _>(scope, blob_file, flags)
                    .await
            }));
            return Ok(());
        }

        Err(zx::Status::NOT_FOUND)
    }

    async fn read_dirents(
        &self,
        pos: &TraversalPosition,
        sink: Box<dyn vfs::directory::dirents_sink::Sink + 'static>,
    ) -> Result<
        (TraversalPosition, Box<dyn vfs::directory::dirents_sink::Sealed + 'static>),
        zx::Status,
    > {
        Ok((pos.clone(), sink.seal()))
    }

    fn register_watcher(
        self: Arc<Self>,
        _: ExecutionScope,
        _: fio::WatchMask,
        _: vfs::directory::entry_container::DirectoryWatcher,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn unregister_watcher(self: Arc<Self>, _: usize) {}
}

enum IncomingRequest {
    Lifecycle(LifecycleRequestStream),
}

#[fuchsia::main]
async fn main() {
    let partitions = Service::open(fpartitions::PartitionServiceMarker)
        .expect("failed to open PartitionService");
    let manager = connect_to_protocol::<fpartitions::PartitionsManagerMarker>()
        .expect("failed to connect to PartitionsManager");
    let partition = BenchmarkVolumeFactory::connect_to_test_partition(partitions, manager)
        .await
        .expect("failed to open or create test partition in GPT");

    let fs = Arc::new(Fxblob::new().start_filesystem(&partition).await);
    let erofs_blob = build_erofs_blob();
    fs.write_blob(&erofs_blob).await;

    // Warm up decompression threads once before benchmarks begin.
    {
        let vmo = fs.get_vmo(&erofs_blob.name).await;
        let mut buf = [0u8; 1];
        vmo.read(&mut buf, 0).expect("warm-up read failed");
    }

    let metadata_bytes = build_bundle_metadata();
    let debug_proxy = fs.connect_to_debug();
    let scope = ExecutionScope::new();
    let erofs_blobs_dir = vfs::pseudo_directory! {
        "bundle" => ErofsBlobSubdir::new(
            fs.clone(),
            debug_proxy,
            erofs_blob.name,
            metadata_bytes,
        ),
    };

    let (dir_client, dir_server) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
    vfs::directory::serve_on(
        erofs_blobs_dir,
        fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        scope.clone(),
        dir_server,
    );

    let mut svc = ServiceFs::new();
    svc.add_remote("erofs_blobs", dir_client.into_proxy());
    svc.dir("svc").add_service_at("fuchsia.process.lifecycle.Lifecycle", |chan| {
        Some(IncomingRequest::Lifecycle(LifecycleRequestStream::from_channel(
            fidl::AsyncChannel::from_channel(chan),
        )))
    });
    svc.take_and_serve_directory_handle().expect("failed to serve outgoing directory");

    let mut stop_control_handle = None;
    'outer: while let Some(request) = svc.next().await {
        match request {
            IncomingRequest::Lifecycle(mut stream) => {
                if let Some(Ok(LifecycleRequest::Stop { control_handle })) = stream.next().await {
                    stop_control_handle = Some(control_handle);
                    break 'outer;
                }
            }
        }
    }

    drop(svc);
    scope.shutdown();
    scope.wait().await;
    if let Ok(fs) = Arc::try_unwrap(fs) {
        fs.shutdown().await;
    }
    if let Some(control_handle) = stop_control_handle {
        control_handle.shutdown();
    }
}
