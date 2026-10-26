// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use storage_benchmarks::directory_benchmarks::{
    DirectoryTreeStructure, GitStatus, OpenDeeplyNestedFile, OpenFile, StatPath,
    WalkDirectoryTreeCold, WalkDirectoryTreeWarm,
};
use storage_benchmarks::io_benchmarks::{
    ReadRandomCold, ReadRandomWarm, ReadSequentialCold, ReadSequentialWarm,
};
use storage_benchmarks::{
    Benchmark, BenchmarkSet, BlockDeviceFactory, CacheClearableFilesystem, Filesystem,
    FilesystemConfig, OperationDuration, OperationTimer,
};
use tempfile::TempDir;

const COLD_PAGE_FAULT_SIZES: [usize; 3] = [1024, 4096, 131072];

const LOOP_SET_FD: libc::c_ulong = 0x4C00;
const LOOP_CLR_FD: libc::c_ulong = 0x4C01;
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;

#[derive(Clone)]
pub struct ErofsFilesystem {
    blob_dir: PathBuf,
    subdir: Option<&'static str>,
    name: String,
}

impl ErofsFilesystem {
    pub fn new(blob_dir: PathBuf, subdir: Option<&'static str>, name: String) -> Self {
        Self { blob_dir, subdir, name }
    }
}

#[async_trait]
impl FilesystemConfig for ErofsFilesystem {
    type Filesystem = ErofsFilesystemInstance;

    async fn start_filesystem(
        &self,
        _block_device_factory: &dyn BlockDeviceFactory,
    ) -> ErofsFilesystemInstance {
        let bundle_dir = TempDir::new_in("/tmp").expect("failed to create bundle mount tempdir");
        let mount_dir = TempDir::new_in("/tmp").expect("failed to create erofs mount tempdir");
        let benchmark_dir = match self.subdir {
            Some(sub) => mount_dir.path().join(sub),
            None => mount_dir.path().to_path_buf(),
        };
        let mut instance = ErofsFilesystemInstance {
            blob_dir: self.blob_dir.clone(),
            bundle_dir,
            mount_dir,
            benchmark_dir,
            loop_file: None,
        };
        instance.mount();
        instance
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

pub struct ErofsFilesystemInstance {
    blob_dir: PathBuf,
    bundle_dir: TempDir,
    mount_dir: TempDir,
    benchmark_dir: PathBuf,
    loop_file: Option<File>,
}

impl ErofsFilesystemInstance {
    fn mount(&mut self) {
        assert!(self.loop_file.is_none(), "EROFS already mounted");
        let bundle_source_cstr = CString::new(self.blob_dir.as_os_str().as_bytes()).unwrap();
        let bundle_target_cstr =
            CString::new(self.bundle_dir.path().as_os_str().as_bytes()).unwrap();
        let bundle_fstype_cstr = CString::new("remote_bundle").unwrap();
        let ret = unsafe {
            libc::mount(
                bundle_source_cstr.as_ptr(),
                bundle_target_cstr.as_ptr(),
                bundle_fstype_cstr.as_ptr(),
                libc::MS_RDONLY,
                std::ptr::null(),
            )
        };
        assert_eq!(ret, 0, "mount remote_bundle failed: {:?}", std::io::Error::last_os_error());

        let image_path = self.bundle_dir.path().join("image.erofs");
        let image_file = File::open(&image_path)
            .unwrap_or_else(|e| panic!("failed to open '{}': {e:?}", image_path.display()));

        let loop_ctl = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/loop-control")
            .expect("failed to open /dev/loop-control");
        let loop_num = unsafe { libc::ioctl(loop_ctl.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        assert!(loop_num >= 0, "LOOP_CTL_GET_FREE failed: {:?}", std::io::Error::last_os_error());

        let loop_path = format!("/dev/loop{loop_num}");
        let loop_file = File::open(&loop_path)
            .unwrap_or_else(|e| panic!("failed to open '{loop_path}': {e:?}"));
        let ret =
            unsafe { libc::ioctl(loop_file.as_raw_fd(), LOOP_SET_FD as _, image_file.as_raw_fd()) };
        assert_eq!(ret, 0, "LOOP_SET_FD failed: {:?}", std::io::Error::last_os_error());

        let source_cstr = CString::new(loop_path).unwrap();
        let target_cstr = CString::new(self.mount_dir.path().as_os_str().as_bytes()).unwrap();
        let fstype_cstr = CString::new("erofs").unwrap();
        let data_cstr = CString::new("fsoffset=4096").unwrap();
        let ret = unsafe {
            libc::mount(
                source_cstr.as_ptr(),
                target_cstr.as_ptr(),
                fstype_cstr.as_ptr(),
                libc::MS_RDONLY,
                data_cstr.as_ptr() as *const libc::c_void,
            )
        };
        assert_eq!(ret, 0, "mount erofs failed: {:?}", std::io::Error::last_os_error());
        self.loop_file = Some(loop_file);
    }

    fn unmount(&mut self) {
        if let Some(loop_file) = self.loop_file.take() {
            let target_cstr = CString::new(self.mount_dir.path().as_os_str().as_bytes()).unwrap();
            let ret = unsafe { libc::umount2(target_cstr.as_ptr(), 0) };
            assert_eq!(ret, 0, "umount2 erofs failed: {:?}", std::io::Error::last_os_error());
            let ret = unsafe { libc::ioctl(loop_file.as_raw_fd(), LOOP_CLR_FD as _, 0) };
            assert_eq!(ret, 0, "LOOP_CLR_FD failed: {:?}", std::io::Error::last_os_error());
            drop(loop_file);

            let bundle_target_cstr =
                CString::new(self.bundle_dir.path().as_os_str().as_bytes()).unwrap();
            let ret = unsafe { libc::umount2(bundle_target_cstr.as_ptr(), 0) };
            assert_eq!(
                ret,
                0,
                "umount2 remote_bundle failed: {:?}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[async_trait]
impl Filesystem for ErofsFilesystemInstance {
    async fn shutdown(mut self) {
        self.unmount();
    }

    fn benchmark_dir(&self) -> &Path {
        &self.benchmark_dir
    }
}

#[async_trait]
impl CacheClearableFilesystem for ErofsFilesystemInstance {
    async fn clear_cache(&mut self) {
        self.unmount();
        self.mount();
    }
}

impl Drop for ErofsFilesystemInstance {
    fn drop(&mut self) {
        self.unmount();
    }
}

struct MappedFile {
    addr: *mut libc::c_void,
    size: usize,
}

impl MappedFile {
    fn new(file: &File, size: usize) -> Self {
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED, "mmap failed: {:?}", std::io::Error::last_os_error());
        Self { addr, size }
    }

    fn as_ptr(&self) -> *const u8 {
        self.addr as *const u8
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        unsafe {
            assert_eq!(libc::munmap(self.addr, self.size), 0, "munmap failed");
        }
    }
}

#[derive(Clone)]
pub struct ColdPageFaultBenchmark {
    size: usize,
}

impl ColdPageFaultBenchmark {
    pub fn new(size: usize) -> Self {
        Self { size }
    }
}

#[async_trait]
impl Benchmark<ErofsFilesystemInstance> for ColdPageFaultBenchmark {
    async fn run(&self, fs: &mut ErofsFilesystemInstance) -> Vec<OperationDuration> {
        storage_trace::duration!("benchmark", "ColdPageFault", "size" => self.size);
        const SAMPLES: usize = 20;
        let file_name = format!("file_{}", self.size);
        let mut durations = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            fs.clear_cache().await;
            let file = File::open(fs.benchmark_dir().join(&file_name)).expect("open file");
            let mapped = MappedFile::new(&file, self.size);
            let ptr = mapped.as_ptr();
            let duration = {
                storage_trace::duration!("benchmark", "page_in", "iteration" => i);
                let timer = OperationTimer::start();
                let val = unsafe { std::ptr::read_volatile(ptr) };
                std::hint::black_box(val);
                timer.stop()
            };
            durations.push(duration);
        }
        durations
    }

    fn name(&self) -> String {
        format!("ColdPageFault/{}", self.size)
    }
}

pub fn build_erofs_benchmark_set(erofs_blob_dir: PathBuf, filesystem_name: String) -> BenchmarkSet {
    const OP_SIZE: usize = 8 * 1024;
    const OP_COUNT: usize = 1024;

    let bundle_dir = erofs_blob_dir.join("bundle");

    let mut benchmark_set = BenchmarkSet::new();

    // 1. Read-only I/O benchmarks (8 MiB file in `io/file`).
    let io_fs = ErofsFilesystem::new(bundle_dir.clone(), Some("io"), filesystem_name.clone());
    benchmark_set
        .add_benchmark(ReadSequentialCold::new_read_only(OP_SIZE, OP_COUNT), io_fs.clone());
    benchmark_set
        .add_benchmark(ReadSequentialWarm::new_read_only(OP_SIZE, OP_COUNT), io_fs.clone());
    benchmark_set.add_benchmark(ReadRandomCold::new_read_only(OP_SIZE, OP_COUNT), io_fs.clone());
    benchmark_set.add_benchmark(ReadRandomWarm::new_read_only(OP_SIZE, OP_COUNT), io_fs);

    // 2. Read-only directory and metadata benchmarks.
    let stat_open_fs =
        ErofsFilesystem::new(bundle_dir.clone(), Some("stat_open"), filesystem_name.clone());
    benchmark_set.add_benchmark(StatPath::new_read_only(), stat_open_fs.clone());
    benchmark_set.add_benchmark(OpenFile::new_read_only(), stat_open_fs);

    let nested_fs =
        ErofsFilesystem::new(bundle_dir.clone(), Some("nested"), filesystem_name.clone());
    benchmark_set.add_benchmark(OpenDeeplyNestedFile::new_read_only(), nested_fs);

    let dts = DirectoryTreeStructure {
        files_per_directory: 3,
        directories_per_directory: 2,
        max_depth: 5,
    };
    let walk_fs =
        ErofsFilesystem::new(bundle_dir.clone(), Some("walk_tree"), filesystem_name.clone());
    benchmark_set.add_benchmark(WalkDirectoryTreeCold::new_read_only(dts, 20), walk_fs.clone());
    benchmark_set.add_benchmark(WalkDirectoryTreeWarm::new_read_only(dts, 20), walk_fs);

    let git_fs =
        ErofsFilesystem::new(bundle_dir.clone(), Some("git_tree"), filesystem_name.clone());
    benchmark_set.add_benchmark(GitStatus::new_read_only(), git_fs);

    // 3. Cold page fault benchmarks across representative file sizes (1 KiB FlatInline,
    //    4 KiB single-block FlatPlain, and 128 KiB multi-block FlatPlain).
    let cold_page_fault_fs =
        ErofsFilesystem::new(bundle_dir, Some("cold_page_fault"), filesystem_name);
    for size in COLD_PAGE_FAULT_SIZES {
        benchmark_set.add_benchmark(ColdPageFaultBenchmark::new(size), cold_page_fault_fs.clone());
    }

    benchmark_set
}
