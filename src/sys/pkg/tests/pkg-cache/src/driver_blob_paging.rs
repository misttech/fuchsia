// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TestEnv;
use diagnostics_assertions::assert_data_tree;
use fuchsia_pkg_testing::PackageBuilder;

#[fuchsia::test]
async fn driver_blob_paging_enabled() {
    let env = TestEnv::builder().fxblob().use_driver_blob_paging(true).build().await;
    env.block_until_started().await;

    let hierarchy = env.inspect_hierarchy().await;
    assert_data_tree!(
        hierarchy,
        root: contains {
            "driver_blob_paging_status": "active",
            "structured_config": contains {
                "use_driver_blob_paging": true,
                "startup_profiling_seconds": 0u64,
            }
        }
    );

    let content = "hello from driver blob paging".as_bytes();
    let blob_hash = fuchsia_merkle::root_from_slice(content);
    let pkg = PackageBuilder::new("driver-paged-pkg")
        .add_resource_at("data/hello.txt", content)
        .build()
        .await
        .expect("build package failed");

    let dir = crate::get_and_verify_package(
        &env.proxies.package_cache,
        fidl_fuchsia_pkg::GcProtection::OpenPackageTracking,
        &pkg,
    )
    .await;

    // Verify that child VMOs created via BlobReader and via opening a package file share the same
    // underlying parent VMO.
    let reader_vmo = env
        .proxies
        .blob_reader
        .get_vmo(&blob_hash.into())
        .await
        .expect("get_vmo fidl failed")
        .map_err(zx::Status::err_from_raw)
        .expect("get_vmo failed");
    let parent_koid = reader_vmo.info().expect("reader_vmo info failed").parent_koid;
    assert_ne!(parent_koid, zx::Koid::from_raw(zx::sys::ZX_KOID_INVALID));

    let file =
        fuchsia_fs::directory::open_file(&dir, "data/hello.txt", fidl_fuchsia_io::PERM_READABLE)
            .await
            .expect("open_file failed");
    let pkg_dir_vmo = file
        .get_backing_memory(fidl_fuchsia_io::VmoFlags::READ)
        .await
        .expect("get_backing_memory fidl failed")
        .map_err(zx::Status::err_from_raw)
        .expect("get_backing_memory failed");
    // While `file` is open, pkg-cache holds an intermediate child VMO (`VmoBlob`) between the root
    // blob VMO and `pkg_dir_vmo`. Closing `file` drops that intermediate VMO, causing Zircon to
    // re-parent `pkg_dir_vmo` directly to the root blob VMO so both `parent_koid`s match.
    fuchsia_fs::file::close(file).await.expect("close file failed");
    assert_eq!(pkg_dir_vmo.info().expect("pkg_dir_vmo info failed").parent_koid, parent_koid);

    // Also verify that because `pkg_dir_vmo` is a child of `parent_koid`, dropping `reader_vmo`
    // does not trigger ZX_VMO_ZERO_CHILDREN eviction of the shared parent VMO in pkg-cache.
    drop(reader_vmo);
    let reader_vmo_2 = env
        .proxies
        .blob_reader
        .get_vmo(&blob_hash.into())
        .await
        .expect("get_vmo fidl failed")
        .map_err(zx::Status::err_from_raw)
        .expect("get_vmo failed");
    assert_eq!(reader_vmo_2.info().expect("reader_vmo_2 info failed").parent_koid, parent_koid);

    env.stop().await;
}

#[fuchsia::test]
async fn driver_blob_paging_mapper_failure_falls_back_to_blob_reader() {
    let (disconnected_mapper, _) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_storage_block::MapperMarker>();
    let env = TestEnv::builder()
        .fxblob()
        .use_driver_blob_paging(true)
        .mapper(disconnected_mapper)
        .build()
        .await;
    env.block_until_started().await;

    // When the driver's Mapper::OpenSession fails, pkg-cache falls back to the standard Fxfs
    // BlobReader.
    let hierarchy = env.inspect_hierarchy().await;
    assert_data_tree!(
        hierarchy,
        root: contains {
            "driver_blob_paging_status": "fallback",
            "structured_config": contains {
                "use_driver_blob_paging": true,
            }
        }
    );

    let content = "hello world".as_bytes();
    let blob_hash = fuchsia_merkle::root_from_slice(content);
    let () = env.blobfs.add_blob_from(blob_hash, content).await.unwrap();

    // Verify that pkg-cache's exposed BlobReader forwards the channel to Fxfs in fallback mode.
    let reader_vmo = env
        .proxies
        .blob_reader
        .get_vmo(&blob_hash.into())
        .await
        .expect("get_vmo fidl failed")
        .map_err(zx::Status::err_from_raw)
        .expect("get_vmo failed");
    assert_eq!(
        reader_vmo.read_to_vec::<u8>(0, content.len() as u64).expect("read_to_vec failed"),
        content
    );

    env.stop().await;
}

#[fuchsia::test]
async fn blob_reader_forwarding() {
    let env = TestEnv::builder().fxblob().use_driver_blob_paging(false).build().await;
    env.block_until_started().await;

    let content = "hello from blob reader forwarding".as_bytes();
    let blob_hash = fuchsia_merkle::root_from_slice(content);
    let () = env.blobfs.add_blob_from(blob_hash, content).await.unwrap();

    let reader_vmo = env
        .proxies
        .blob_reader
        .get_vmo(&blob_hash.into())
        .await
        .expect("get_vmo fidl failed")
        .map_err(zx::Status::err_from_raw)
        .expect("get_vmo failed");
    assert_eq!(
        reader_vmo.read_to_vec::<u8>(0, content.len() as u64).expect("read_to_vec failed"),
        content
    );

    env.stop().await;
}
