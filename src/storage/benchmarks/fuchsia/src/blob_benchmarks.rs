// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use delivery_blob::CompressionMode;
use delivery_blob::compression::CompressionAlgorithm;
use fidl_fuchsia_io as fio;
use fuchsia_pkg_testing::PackageBuilder;
use fuchsia_storage_benchmarks::filesystems::{
    BlobFilesystem, DeliveryBlob, FxblobInstance, PkgDirInstance,
};
use futures::stream::{self, StreamExt};
use rand::distr::Distribution as _;
use rand::distr::weighted::WeightedIndex;
use rand::seq::SliceRandom as _;
use rand::{RngExt as _, SeedableRng as _};
use rand_xorshift::XorShiftRng;
use std::ops::Range;
use storage_benchmarks::{
    Benchmark, CacheClearableFilesystem as _, OperationDuration, OperationTimer,
};

const RNG_SEED: u64 = 0xda782a0c3ce1819a;

#[derive(Clone)]
pub struct OpenAndGetVmoBenchmark {
    name: &'static str,
    resource_path: String,
    cold: bool,
}

impl OpenAndGetVmoBenchmark {
    pub fn new_content_blob_cold() -> Self {
        Self {
            name: "OpenAndGetVmoContentBlobCold",
            resource_path: "data/foo".to_owned(),
            cold: true,
        }
    }

    pub fn new_meta_file_cold() -> Self {
        Self { name: "OpenAndGetVmoMetaFileCold", resource_path: "meta/bar".to_owned(), cold: true }
    }

    pub fn new_meta_file_warm() -> Self {
        Self {
            name: "OpenAndGetVmoMetaFileWarm",
            resource_path: "meta/bar".to_owned(),
            cold: false,
        }
    }

    pub fn new_content_blob_warm() -> Self {
        Self {
            name: "OpenAndGetVmoContentBlobWarm",
            resource_path: "data/foo".to_owned(),
            cold: false,
        }
    }

    async fn run_test(&self, pkgdir: &fio::DirectoryProxy) -> OperationDuration {
        let timer = OperationTimer::start();
        let file = {
            storage_trace::duration!("benchmark", "open-file");
            fuchsia_fs::directory::open_file(pkgdir, &self.resource_path, fio::PERM_READABLE)
                .await
                .expect("failed to open blob")
        };
        storage_trace::duration!("benchmark", "get-vmo");
        let _ = file.get_backing_memory(fio::VmoFlags::READ).await.unwrap().unwrap();

        timer.stop()
    }
}

#[async_trait]
impl Benchmark<PkgDirInstance> for OpenAndGetVmoBenchmark {
    async fn run(&self, fs: &mut PkgDirInstance) -> Vec<OperationDuration> {
        storage_trace::duration!("benchmark", self.name);
        let package = PackageBuilder::new("pkg")
            .add_resource_at(&self.resource_path, "data".as_bytes())
            .build()
            .await
            .unwrap();
        let (meta, map) = package.contents();

        {
            storage_trace::duration!("benchmark", "write-package");
            fs.write_blob(&DeliveryBlob::new(meta.contents, CompressionMode::Always)).await;
            for (_, content) in map.clone() {
                fs.write_blob(&DeliveryBlob::new(content, CompressionMode::Always)).await;
            }
        }

        fs.clear_cache().await;
        let pkgdir_client_end = fs
            .pkgdir_proxy()
            .open_package_directory(&meta.merkle)
            .await
            .unwrap()
            .expect("Opening pkg dir");
        let mut pkgdir = pkgdir_client_end.into_proxy();

        const SAMPLES: usize = 10;
        let mut durations = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            durations.push(self.run_test(&pkgdir).await);
            if self.cold {
                fs.clear_cache().await;
                let pkgdir_client_end = fs
                    .pkgdir_proxy()
                    .open_package_directory(&meta.merkle)
                    .await
                    .unwrap()
                    .expect("Opening pkg dir");
                pkgdir = pkgdir_client_end.into_proxy();
            }
        }
        durations
    }

    fn name(&self) -> String {
        self.name.into()
    }
}

#[derive(Clone)]
enum DataGeneration {
    Compressible,
    Incompressible,
}

#[derive(Clone)]
enum PageIteration {
    Random,
    Sequential,
}

#[derive(Clone)]
pub struct PageInBlobBenchmark {
    name: &'static str,
    page_iter: PageIteration,
    data_gen: DataGeneration,
    blob_size: usize,
}

impl PageInBlobBenchmark {
    pub fn new_sequential_uncompressed(blob_size: usize) -> Self {
        Self {
            name: "PageInBlobSequentialUncompressed",
            page_iter: PageIteration::Sequential,
            data_gen: DataGeneration::Incompressible,
            blob_size,
        }
    }

    pub fn new_sequential_compressed(blob_size: usize) -> Self {
        Self {
            name: "PageInBlobSequentialCompressed",
            page_iter: PageIteration::Sequential,
            data_gen: DataGeneration::Compressible,
            blob_size,
        }
    }

    pub fn new_random_compressed(blob_size: usize) -> Self {
        Self {
            name: "PageInBlobRandomCompressed",
            page_iter: PageIteration::Random,
            data_gen: DataGeneration::Compressible,
            blob_size,
        }
    }
}

#[async_trait]
impl<T: BlobFilesystem> Benchmark<T> for PageInBlobBenchmark {
    async fn run(&self, fs: &mut T) -> Vec<OperationDuration> {
        storage_trace::duration!(
            "benchmark",
            self.name,
            "blob_size" => self.blob_size
        );
        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        let blob = match &self.data_gen {
            DataGeneration::Incompressible => create_incompressible_data(self.blob_size, &mut rng),
            DataGeneration::Compressible => create_compressible_data(self.blob_size, &mut rng),
        };
        let page_iter = match &self.page_iter {
            PageIteration::Sequential => sequential_page_iter(self.blob_size),
            PageIteration::Random => random_page_iter(self.blob_size, &mut rng),
        };
        page_in_blob_benchmark(fs, blob, page_iter).await
    }

    fn name(&self) -> String {
        self.name.into()
    }
}

#[derive(Clone)]
pub struct WriteBlob {
    blob_size: usize,
}

impl WriteBlob {
    pub fn new(blob_size: usize) -> Self {
        Self { blob_size }
    }
}

#[async_trait]
impl<T: BlobFilesystem> Benchmark<T> for WriteBlob {
    async fn run(&self, fs: &mut T) -> Vec<OperationDuration> {
        storage_trace::duration!(
            "benchmark",
            "WriteBlob",
            "blob_size" => self.blob_size
        );
        const SAMPLES: usize = 5;
        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        let mut durations = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let blob = create_compressible_data(self.blob_size, &mut rng);
            let total_duration = OperationTimer::start();
            fs.write_blob(&blob).await;
            durations.push(total_duration.stop());
            fs.remove_blob(&blob.name).await;
        }
        durations
    }

    fn name(&self) -> String {
        format!("WriteBlob/{}", self.blob_size)
    }
}

#[derive(Clone)]
pub struct WriteRealisticBlobs {}

impl WriteRealisticBlobs {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl<T: BlobFilesystem> Benchmark<T> for WriteRealisticBlobs {
    async fn run(&self, fs: &mut T) -> Vec<OperationDuration> {
        storage_trace::duration!("benchmark", "WriteRealisticBlobs");
        // Only write 2 blobs at once to match pkg-cache.
        const CONCURRENT_WRITE_COUNT: usize = 2;

        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        let sizes = vec![
            67 * 1024 * 1024,
            33 * 1024 * 1024,
            2 * 1024 * 1024,
            1024 * 1024,
            131072,
            65536,
            65536,
            32768,
            16384,
            16384,
            4096,
            4096,
            4096,
            4096,
            4096,
            4096,
        ];

        let mut futures = Vec::with_capacity(sizes.len());
        for size in sizes {
            let blob = create_compressible_data(size, &mut rng);
            let fs: &T = fs;
            futures.push(async move {
                fs.write_blob(&blob).await;
            });
        }

        let fut = stream::iter(futures).for_each_concurrent(
            CONCURRENT_WRITE_COUNT,
            |blob_future| async move {
                blob_future.await;
            },
        );

        let timer = OperationTimer::start();
        fut.await;
        vec![timer.stop()]
    }

    fn name(&self) -> String {
        "WriteRealisticBlobs".to_string()
    }
}

struct MappedBlob {
    addr: usize,
    size: usize,
}

impl MappedBlob {
    fn new(vmo: &zx::Vmo) -> Self {
        let size = vmo.get_size().unwrap() as usize;
        let addr = fuchsia_runtime::vmar_root_self()
            .map(0, vmo, 0, size, zx::VmarFlags::PERM_READ)
            .unwrap();
        Self { addr, size }
    }

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.addr as *const u8, self.size) }
    }
}

impl Drop for MappedBlob {
    fn drop(&mut self) {
        unsafe {
            fuchsia_runtime::vmar_root_self().unmap(self.addr, self.size).expect("unmap");
        }
    }
}

/// Returns completely random bytes that shouldn't be compressible.
fn create_incompressible_bytes(size: usize, rng: &mut XorShiftRng) -> Vec<u8> {
    let mut data = vec![0; size];
    rng.fill(data.as_mut_slice());
    data
}

/// Returns completely random data that shouldn't be compressible.
fn create_incompressible_data(size: usize, rng: &mut XorShiftRng) -> DeliveryBlob {
    DeliveryBlob::new(create_incompressible_bytes(size, rng), CompressionMode::Never)
}

/// Creates runs of the same byte between 2 and 8 bytes long. This should compress to about 40% of
/// the original size which is typical for large executable blobs.
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

fn create_compressible_data(size: usize, rng: &mut XorShiftRng) -> DeliveryBlob {
    DeliveryBlob::new(create_compressible_bytes(size, rng), CompressionMode::Always)
}

/// Returns an iterator to the index of the first byte of every page in sequential order.
fn sequential_page_iter(blob_size: usize) -> Vec<usize> {
    let page_size = zx::system_get_page_size() as usize;
    (0..blob_size).step_by(page_size).collect::<Vec<usize>>()
}

/// Returns an iterator to the index of the first byte of every page. The order of the pages tries
/// to mimic how pages are accessed if the blob was an executable.
fn random_page_iter(blob_size: usize, rng: &mut XorShiftRng) -> Vec<usize> {
    // Executables tend to both randomly jump between pages and go on long runs of sequentially
    // accessing pages.
    const RUN_LENGTHS: [usize; 6] = [1, 3, 15, 40, 60, 80];
    const WEIGHTS: [usize; 6] = [25, 15, 40, 10, 6, 4];
    let distribution = WeightedIndex::new(WEIGHTS).unwrap();
    let page_size = zx::system_get_page_size() as usize;
    let total_pages = (0..blob_size).step_by(page_size).len();
    // Only access 60% of the pages. Not all pages of an executable are typically accessed near the
    // start of a process. Accessing every page would favour filesystems with overly aggressive
    // read-ahead when in practice some of the read-ahead pages won't be used.
    let pages_to_read = if total_pages < 5 { total_pages } else { total_pages / 5 * 3 };

    // Split the pages up into runs.
    let mut taken_pages = 0;
    let mut page_runs = Vec::new();
    while taken_pages < pages_to_read {
        let index = distribution.sample(rng);
        let run_length = std::cmp::min(RUN_LENGTHS[index], pages_to_read - taken_pages);
        let start = taken_pages * page_size;
        let end = (taken_pages + run_length) * page_size;
        taken_pages += run_length;
        page_runs.push((start..end).step_by(page_size));
    }

    page_runs.shuffle(rng);
    page_runs.into_iter().flatten().collect()
}

async fn page_in_blob_benchmark(
    fs: &mut impl BlobFilesystem,
    blob: DeliveryBlob,
    page_iter: Vec<usize>,
) -> Vec<OperationDuration> {
    {
        storage_trace::duration!("benchmark", "write-blob");
        fs.write_blob(&blob).await;
    };

    // The first request that uses decompression to the driver has a one-time set up cost, which we
    // don't want to capture in the benchmark, so issue one request now.
    {
        let vmo = fs.get_vmo(&blob.name).await;
        let mapped_blob = MappedBlob::new(&vmo);
        std::hint::black_box(mapped_blob.data()[0]);
    }

    fs.clear_cache().await;

    let vmo = fs.get_vmo(&blob.name).await;
    let mapped_blob = MappedBlob::new(&vmo);
    let data = mapped_blob.data();
    let mut durations = Vec::new();
    for i in page_iter {
        storage_trace::duration!("benchmark", "page_in", "offset" => i);
        let timer = OperationTimer::start();
        std::hint::black_box(data[i]);
        durations.push(timer.stop());
    }
    durations
}

#[derive(Clone)]
enum BlobDataSource {
    Compressed { resource_path: &'static str, algo: CompressionAlgorithm },
    Uncompressed { size: usize },
}

#[derive(Clone)]
pub struct ColdPageFaultBenchmark {
    name: String,
    data_source: BlobDataSource,
}

impl ColdPageFaultBenchmark {
    pub fn new_lz4_40() -> Self {
        Self {
            name: "ColdPageFault/LZ4/40p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/lz4_40.bin",
                algo: CompressionAlgorithm::Lz4,
            },
        }
    }
    pub fn new_lz4_55() -> Self {
        Self {
            name: "ColdPageFault/LZ4/55p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/lz4_55.bin",
                algo: CompressionAlgorithm::Lz4,
            },
        }
    }
    pub fn new_lz4_70() -> Self {
        Self {
            name: "ColdPageFault/LZ4/70p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/lz4_70.bin",
                algo: CompressionAlgorithm::Lz4,
            },
        }
    }
    pub fn new_zstd_40() -> Self {
        Self {
            name: "ColdPageFault/ZSTD/40p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/zstd_40.bin",
                algo: CompressionAlgorithm::Zstd,
            },
        }
    }
    pub fn new_zstd_55() -> Self {
        Self {
            name: "ColdPageFault/ZSTD/55p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/zstd_55.bin",
                algo: CompressionAlgorithm::Zstd,
            },
        }
    }
    pub fn new_zstd_70() -> Self {
        Self {
            name: "ColdPageFault/ZSTD/70p".to_string(),
            data_source: BlobDataSource::Compressed {
                resource_path: "/pkg/data/blobs/zstd_70.bin",
                algo: CompressionAlgorithm::Zstd,
            },
        }
    }

    pub fn new_uncompressed(size: usize) -> Self {
        Self {
            name: format!("ColdPageFault/Uncompressed/{}", size),
            data_source: BlobDataSource::Uncompressed { size },
        }
    }
}

#[async_trait]
impl Benchmark<FxblobInstance> for ColdPageFaultBenchmark {
    async fn run(&self, fs: &mut FxblobInstance) -> Vec<OperationDuration> {
        storage_trace::duration!("benchmark", "ColdPageFault", "name" => self.name.as_str());

        let delivery_blob = match &self.data_source {
            BlobDataSource::Compressed { resource_path, algo } => {
                let raw_data = std::fs::read(resource_path).unwrap_or_else(|e| {
                    panic!("failed to read resource {}: {:?}", resource_path, e)
                });
                match algo {
                    CompressionAlgorithm::Zstd => {
                        DeliveryBlob::new(raw_data, delivery_blob::CompressionMode::Always)
                    }
                    CompressionAlgorithm::Lz4 => DeliveryBlob::new_with_type(
                        delivery_blob::DeliveryBlobType::Type3,
                        raw_data,
                        delivery_blob::CompressionMode::Always,
                    ),
                }
            }
            BlobDataSource::Uncompressed { size } => {
                let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
                create_incompressible_data(*size, &mut rng)
            }
        };
        let blob_hash = delivery_blob.name;

        fs.write_blob(&delivery_blob).await;

        // Warm up the decompression threads (to avoid measuring startup latency).
        {
            let vmo = fs.get_vmo(&blob_hash).await;
            let mut warm_buf = [0u8; 1];
            vmo.read(&mut warm_buf, 0).expect("warm up read failed");
        }

        let debug_proxy = fs.connect_to_debug();

        let mut durations = Vec::new();
        for _ in 0..20 {
            // Call ClearCaches to purge Fxfs's internal caches.
            debug_proxy
                .clear_caches()
                .await
                .expect("ClearCaches transport error")
                .expect("ClearCaches failed");
            fs.clear_unreferenced_blobs();

            let vmo = fs.get_vmo(&blob_hash).await;
            let mapped = MappedBlob::new(&vmo);

            let duration = {
                storage_trace::duration!("benchmark", "page_in", "offset" => 0usize);
                let timer = OperationTimer::start();
                // Read the first byte of mapped VMO to trigger page fault
                let ptr = mapped.data().as_ptr();
                let val = unsafe { std::ptr::read_volatile(ptr) };
                std::hint::black_box(val);
                timer.stop()
            };
            durations.push(duration);
        }

        fs.remove_blob(&blob_hash).await;

        durations
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

const EROFS_FILE_OFFSET: u64 = 4096;

#[derive(Clone)]
pub struct PageInErofsBenchmark {
    name: &'static str,
    page_iter: PageIteration,
    data_gen: DataGeneration,
    file_size: usize,
}

impl PageInErofsBenchmark {
    pub fn new_sequential_uncompressed(file_size: usize) -> Self {
        Self {
            name: "PageInErofsSequentialUncompressed",
            page_iter: PageIteration::Sequential,
            data_gen: DataGeneration::Incompressible,
            file_size,
        }
    }

    pub fn new_sequential_compressed(file_size: usize) -> Self {
        Self {
            name: "PageInErofsSequentialCompressed",
            page_iter: PageIteration::Sequential,
            data_gen: DataGeneration::Compressible,
            file_size,
        }
    }

    pub fn new_random_compressed(file_size: usize) -> Self {
        Self {
            name: "PageInErofsRandomCompressed",
            page_iter: PageIteration::Random,
            data_gen: DataGeneration::Compressible,
            file_size,
        }
    }
}

#[async_trait]
impl Benchmark<FxblobInstance> for PageInErofsBenchmark {
    async fn run(&self, fs: &mut FxblobInstance) -> Vec<OperationDuration> {
        storage_trace::duration!(
            "benchmark",
            self.name,
            "file_size" => self.file_size
        );
        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        let (file_bytes, mode) = match &self.data_gen {
            DataGeneration::Incompressible => {
                (create_incompressible_bytes(self.file_size, &mut rng), CompressionMode::Never)
            }
            DataGeneration::Compressible => {
                (create_compressible_bytes(self.file_size, &mut rng), CompressionMode::Always)
            }
        };
        let page_iter = match &self.page_iter {
            PageIteration::Sequential => sequential_page_iter(self.file_size),
            PageIteration::Random => random_page_iter(self.file_size, &mut rng),
        };

        let erofs_image = erofs_serializer::serialize(&[erofs_serializer::SerializerNode::File {
            name: "large_file".to_string(),
            data: file_bytes,
        }]);
        let mut blob_bytes = vec![0xABu8; EROFS_FILE_OFFSET as usize];
        blob_bytes.extend_from_slice(&erofs_image);
        let blob = DeliveryBlob::new(blob_bytes, mode);

        fs.write_blob(&blob).await;

        // Warm up decompression.
        {
            let vmo = fs.get_vmo(&blob.name).await;
            let mapped = MappedBlob::new(&vmo);
            std::hint::black_box(mapped.data()[0]);
        }

        fs.clear_cache().await;

        let erofs = fs.mount_erofs(&blob.name, EROFS_FILE_OFFSET).await;
        let file =
            fuchsia_fs::directory::open_file(erofs.root_dir(), "large_file", fio::PERM_READABLE)
                .await
                .expect("failed to open large_file in EROFS");
        let vmo = file
            .get_backing_memory(fio::VmoFlags::READ)
            .await
            .expect("get_backing_memory FIDL error")
            .expect("get_backing_memory failed");
        let mapped = MappedBlob::new(&vmo);
        let data = mapped.data();
        let mut durations = Vec::with_capacity(page_iter.len());
        for i in page_iter {
            storage_trace::duration!("benchmark", "page_in", "offset" => i);
            let timer = OperationTimer::start();
            let val = unsafe { std::ptr::read_volatile(data.as_ptr().add(i)) };
            std::hint::black_box(val);
            durations.push(timer.stop());
        }
        drop(mapped);
        drop(vmo);
        drop(file);
        drop(erofs);
        fs.remove_blob(&blob.name).await;
        durations
    }

    fn name(&self) -> String {
        self.name.into()
    }
}

#[derive(Clone)]
pub struct PageInErofsSmallFilesBenchmark {
    name: &'static str,
    data_gen: DataGeneration,
}

impl PageInErofsSmallFilesBenchmark {
    pub fn new_uncompressed() -> Self {
        Self { name: "PageInErofsSmallFilesUncompressed", data_gen: DataGeneration::Incompressible }
    }

    pub fn new_compressed() -> Self {
        Self { name: "PageInErofsSmallFilesCompressed", data_gen: DataGeneration::Compressible }
    }
}

#[async_trait]
impl Benchmark<FxblobInstance> for PageInErofsSmallFilesBenchmark {
    async fn run(&self, fs: &mut FxblobInstance) -> Vec<OperationDuration> {
        storage_trace::duration!("benchmark", self.name);
        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        let mode = match &self.data_gen {
            DataGeneration::Incompressible => CompressionMode::Never,
            DataGeneration::Compressible => CompressionMode::Always,
        };

        // Construct a representative EROFS image with 64 small files across three common layouts:
        // - 32 tail-only FlatInline files (512 B each, packed ~7 per 4 KiB metadata block)
        // - 16 single-block FlatPlain files (4,096 B each, 8 per 32 KiB parent chunk)
        // - 16 multi-extent FlatInline files (4,608 B = 1 full 4 KiB block + 512 B inline tail)
        let mut nodes = Vec::with_capacity(64);
        let mut file_names = Vec::with_capacity(64);
        for i in 0..64 {
            let size = if i < 32 {
                512
            } else if i < 48 {
                4096
            } else {
                4608
            };
            let name = format!("small_file_{i:02}");
            let data = match &self.data_gen {
                DataGeneration::Incompressible => create_incompressible_bytes(size, &mut rng),
                DataGeneration::Compressible => create_compressible_bytes(size, &mut rng),
            };
            file_names.push(name.clone());
            nodes.push(erofs_serializer::SerializerNode::File { name, data });
        }

        let erofs_image = erofs_serializer::serialize(&nodes);
        let mut blob_bytes = vec![0xABu8; EROFS_FILE_OFFSET as usize];
        blob_bytes.extend_from_slice(&erofs_image);
        let blob = DeliveryBlob::new(blob_bytes, mode);

        fs.write_blob(&blob).await;

        // Warm up decompression.
        {
            let vmo = fs.get_vmo(&blob.name).await;
            let mapped = MappedBlob::new(&vmo);
            std::hint::black_box(mapped.data()[0]);
        }

        fs.clear_cache().await;

        let erofs = fs.mount_erofs(&blob.name, EROFS_FILE_OFFSET).await;
        let page_size = zx::system_get_page_size() as usize;
        let mut durations = Vec::with_capacity(80);

        for name in &file_names {
            let file = fuchsia_fs::directory::open_file(erofs.root_dir(), name, fio::PERM_READABLE)
                .await
                .expect("failed to open small EROFS file");
            let vmo = file
                .get_backing_memory(fio::VmoFlags::READ)
                .await
                .expect("get_backing_memory FIDL error")
                .expect("get_backing_memory failed");
            let mapped = MappedBlob::new(&vmo);
            let data = mapped.data();
            for offset in (0..data.len()).step_by(page_size) {
                storage_trace::duration!("benchmark", "page_in", "offset" => offset);
                let timer = OperationTimer::start();
                let val = unsafe { std::ptr::read_volatile(data.as_ptr().add(offset)) };
                std::hint::black_box(val);
                durations.push(timer.stop());
            }
        }

        drop(erofs);
        fs.remove_blob(&blob.name).await;
        durations
    }

    fn name(&self) -> String {
        self.name.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_storage_benchmarks::filesystems::{Blobfs, Fxblob, PkgDirTest};
    use fuchsia_storage_benchmarks::testing::RamdiskFactory;
    use storage_benchmarks::FilesystemConfig;
    use test_util::assert_lt;
    const PAGE_COUNT: usize = 32;

    fn page_size() -> usize {
        zx::system_get_page_size() as usize
    }

    async fn check_benchmark<T, U, V>(benchmark: T, filesystem_config: V, op_count: usize)
    where
        T: Benchmark<U>,
        U: BlobFilesystem,
        V: FilesystemConfig<Filesystem = U>,
    {
        let blocks = 200 * 1024 * 1024 / page_size() as u64; // 200MiB
        let ramdisk_factory = RamdiskFactory::new(page_size() as u64, blocks);
        let mut filesystem = filesystem_config.start_filesystem(&ramdisk_factory).await;
        let results = benchmark.run(&mut filesystem).await;
        assert_eq!(results.len(), op_count);
        filesystem.shutdown().await;
    }

    #[fuchsia::test]
    async fn page_in_blob_sequential_compressed_blobfs_test() {
        check_benchmark(
            PageInBlobBenchmark::new_sequential_compressed(PAGE_COUNT * page_size()),
            Blobfs,
            PAGE_COUNT,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_blob_sequential_compressed_fxblob_test() {
        check_benchmark(
            PageInBlobBenchmark::new_sequential_compressed(PAGE_COUNT * page_size()),
            Fxblob::new(),
            PAGE_COUNT,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_blob_sequential_uncompressed_test() {
        check_benchmark(
            PageInBlobBenchmark::new_sequential_uncompressed(PAGE_COUNT * page_size()),
            Fxblob::new(),
            PAGE_COUNT,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_blob_random_compressed_test() {
        check_benchmark(
            PageInBlobBenchmark::new_random_compressed(PAGE_COUNT * page_size()),
            Fxblob::new(),
            PAGE_COUNT / 5 * 3,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_erofs_sequential_uncompressed_test() {
        check_benchmark(
            PageInErofsBenchmark::new_sequential_uncompressed(PAGE_COUNT * page_size()),
            Fxblob::new(),
            PAGE_COUNT,
        )
        .await;
        check_benchmark(
            PageInErofsBenchmark::new_sequential_uncompressed(PAGE_COUNT * page_size()),
            Fxblob::with_driver_paging(),
            PAGE_COUNT,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_erofs_sequential_compressed_test() {
        check_benchmark(
            PageInErofsBenchmark::new_sequential_compressed(PAGE_COUNT * page_size()),
            Fxblob::new(),
            PAGE_COUNT,
        )
        .await;
        check_benchmark(
            PageInErofsBenchmark::new_sequential_compressed(PAGE_COUNT * page_size()),
            Fxblob::with_driver_paging(),
            PAGE_COUNT,
        )
        .await;
    }

    #[fuchsia::test]
    async fn page_in_erofs_small_files_test() {
        check_benchmark(PageInErofsSmallFilesBenchmark::new_uncompressed(), Fxblob::new(), 80)
            .await;
        check_benchmark(
            PageInErofsSmallFilesBenchmark::new_uncompressed(),
            Fxblob::with_driver_paging(),
            80,
        )
        .await;
        check_benchmark(PageInErofsSmallFilesBenchmark::new_compressed(), Fxblob::new(), 80).await;
        check_benchmark(
            PageInErofsSmallFilesBenchmark::new_compressed(),
            Fxblob::with_driver_paging(),
            80,
        )
        .await;
    }

    #[fuchsia::test]
    async fn write_blob_blobfs_test() {
        check_benchmark(WriteBlob::new(PAGE_COUNT * page_size()), Blobfs, 5).await;
    }

    #[fuchsia::test]
    async fn write_blob_fxblob_test() {
        check_benchmark(WriteBlob::new(PAGE_COUNT * page_size()), Fxblob::new(), 5).await;
    }

    #[fuchsia::test]
    async fn write_realistic_blobs_blobfs_test() {
        check_benchmark(WriteRealisticBlobs::new(), Blobfs, 1).await;
    }

    #[fuchsia::test]
    async fn write_realistic_blobs_fxblob_test() {
        check_benchmark(WriteRealisticBlobs::new(), Fxblob::new(), 1).await;
    }

    #[fuchsia::test]
    async fn open_and_get_vmo_blobfs_test() {
        check_benchmark(
            OpenAndGetVmoBenchmark::new_content_blob_cold(),
            PkgDirTest::new_blobfs(),
            10,
        )
        .await;
        check_benchmark(
            OpenAndGetVmoBenchmark::new_content_blob_warm(),
            PkgDirTest::new_blobfs(),
            10,
        )
        .await;
        check_benchmark(OpenAndGetVmoBenchmark::new_meta_file_cold(), PkgDirTest::new_blobfs(), 10)
            .await;
        check_benchmark(OpenAndGetVmoBenchmark::new_meta_file_warm(), PkgDirTest::new_blobfs(), 10)
            .await;
    }

    #[fuchsia::test]
    async fn open_and_get_vmo_fxblob_test() {
        check_benchmark(
            OpenAndGetVmoBenchmark::new_content_blob_cold(),
            PkgDirTest::new_fxblob(),
            10,
        )
        .await;
        check_benchmark(
            OpenAndGetVmoBenchmark::new_content_blob_warm(),
            PkgDirTest::new_fxblob(),
            10,
        )
        .await;
        check_benchmark(OpenAndGetVmoBenchmark::new_meta_file_cold(), PkgDirTest::new_fxblob(), 10)
            .await;
        check_benchmark(OpenAndGetVmoBenchmark::new_meta_file_warm(), PkgDirTest::new_fxblob(), 10)
            .await;
    }

    #[fuchsia::test]
    fn sequential_page_iter_test() {
        assert_eq!(sequential_page_iter(0).into_iter().max(), None);
        assert_eq!(sequential_page_iter(1).into_iter().max(), Some(0));
        assert_eq!(sequential_page_iter(page_size() - 1).into_iter().max(), Some(0));
        assert_eq!(sequential_page_iter(page_size()).into_iter().max(), Some(0));
        assert_eq!(sequential_page_iter(page_size() + 1).into_iter().max(), Some(page_size()));

        let offsets = sequential_page_iter(page_size() * 4);
        assert_eq!(&offsets, &[0, page_size(), page_size() * 2, page_size() * 3]);
    }

    #[fuchsia::test]
    fn random_page_iter_test() {
        let mut rng = XorShiftRng::seed_from_u64(RNG_SEED);
        assert_eq!(random_page_iter(0, &mut rng).into_iter().max(), None);
        assert_eq!(random_page_iter(1, &mut rng).into_iter().max(), Some(0));
        assert_eq!(random_page_iter(page_size() - 1, &mut rng).into_iter().max(), Some(0));
        assert_eq!(random_page_iter(page_size(), &mut rng).into_iter().max(), Some(0));
        assert_eq!(
            random_page_iter(page_size() + 1, &mut rng).into_iter().max(),
            Some(page_size())
        );
        assert_eq!(random_page_iter(page_size() * 4, &mut rng).len(), 4);
        assert_eq!(random_page_iter(page_size() * 5, &mut rng).len(), 3);
        assert_eq!(random_page_iter(page_size() * 9, &mut rng).len(), 3);
        assert_eq!(random_page_iter(page_size() * 10, &mut rng).len(), 6);

        let blob_size = page_size() * 500;
        let mut offsets = random_page_iter(blob_size, &mut rng);

        // Make sure that the offsets aren't sorted.
        let mut is_sorted = true;
        for i in 1..offsets.len() {
            if offsets[i - 1] >= offsets[i] {
                is_sorted = false;
                break;
            }
        }
        assert!(!is_sorted);

        offsets.sort();
        // Make sure that there are no duplicates.
        for i in 1..offsets.len() {
            assert_ne!(offsets[i - 1], offsets[i]);
        }
        // Make sure that the largest page offset is part of the blob.
        assert_lt!(offsets.last().unwrap(), &blob_size);
    }
}
