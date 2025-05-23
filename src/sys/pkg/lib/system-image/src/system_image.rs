// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    get_system_image_hash, CachePackages, CachePackagesInitError, StaticPackages,
    StaticPackagesInitError,
};
use anyhow::Context as _;
use fuchsia_hash::Hash;
use package_directory::RootDir;
use std::sync::Arc;

static DISABLE_RESTRICTIONS_FILE_PATH: &str = "data/pkgfs_disable_executability_restrictions";

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ExecutabilityRestrictions {
    Enforce,
    DoNotEnforce,
}

/// System image package.
pub struct SystemImage {
    root_dir: Arc<RootDir<blobfs::Client>>,
}

impl SystemImage {
    pub async fn new(
        blobfs: blobfs::Client,
        boot_args: &fidl_fuchsia_boot::ArgumentsProxy,
    ) -> Result<Self, anyhow::Error> {
        let hash = get_system_image_hash(boot_args).await.context("getting system_image hash")?;
        let root_dir = RootDir::new(blobfs, hash)
            .await
            .with_context(|| format!("creating RootDir for system_image: {hash}"))?;
        Ok(SystemImage { root_dir })
    }

    /// Make a `SystemImage` from a `RootDir` for the `system_image` package.
    pub fn from_root_dir(root_dir: Arc<RootDir<blobfs::Client>>) -> Self {
        Self { root_dir }
    }

    pub fn load_executability_restrictions(&self) -> ExecutabilityRestrictions {
        match self.root_dir.has_file(DISABLE_RESTRICTIONS_FILE_PATH) {
            true => ExecutabilityRestrictions::DoNotEnforce,
            false => ExecutabilityRestrictions::Enforce,
        }
    }

    /// The hash of the `system_image` package.
    pub fn hash(&self) -> &Hash {
        self.root_dir.hash()
    }

    /// Load `data/cache_packages.json`.
    pub async fn cache_packages(&self) -> Result<CachePackages, CachePackagesInitError> {
        self.root_dir
            .read_file("data/cache_packages.json")
            .await
            .map_err(CachePackagesInitError::ReadCachePackagesJson)
            .and_then(|content| CachePackages::from_json(content.as_slice()))
    }

    /// Load `data/static_packages`.
    pub async fn static_packages(&self) -> Result<StaticPackages, StaticPackagesInitError> {
        StaticPackages::deserialize(
            self.root_dir
                .read_file("data/static_packages")
                .await
                .map_err(StaticPackagesInitError::ReadStaticPackages)?
                .as_slice(),
        )
        .map_err(StaticPackagesInitError::ProcessingStaticPackages)
    }

    /// Consume self and return the contained `package_directory::RootDir`.
    pub fn into_root_dir(self) -> Arc<RootDir<blobfs::Client>> {
        self.root_dir
    }

    /// The package path of the system image package.
    pub fn package_path() -> fuchsia_pkg::PackagePath {
        fuchsia_pkg::PackagePath::from_name_and_variant(
            "system_image".parse().expect("valid package name"),
            fuchsia_pkg::PackageVariant::zero(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fuchsia_pkg_testing::SystemImageBuilder;

    struct TestEnv {
        _blobfs: blobfs_ramdisk::BlobfsRamdisk,
    }

    impl TestEnv {
        async fn new(system_image: SystemImageBuilder) -> (Self, SystemImage) {
            let blobfs = blobfs_ramdisk::BlobfsRamdisk::start().await.unwrap();
            let system_image = system_image.build().await;
            system_image.write_to_blobfs(&blobfs).await;
            let root_dir = RootDir::new(blobfs.client(), *system_image.hash()).await.unwrap();
            (Self { _blobfs: blobfs }, SystemImage { root_dir })
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn cache_packages_fails_without_config_files() {
        let (_env, system_image) = TestEnv::new(SystemImageBuilder::new()).await;
        assert_matches!(
            system_image.cache_packages().await,
            Err(CachePackagesInitError::ReadCachePackagesJson(
                package_directory::ReadFileError::NoFileAtPath { .. }
            ))
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn cache_packages_deserialize_valid_line_oriented() {
        let (_env, system_image) = TestEnv::new(
            SystemImageBuilder::new()
                .cache_package("name/variant".parse().unwrap(), [0; 32].into()),
        )
        .await;

        assert_eq!(
            system_image.cache_packages().await.unwrap(),
            CachePackages::from_entries(
                vec!["fuchsia-pkg://fuchsia.com/name/variant?hash=0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap()
                ]
            )
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn static_packages_deserialize_valid_line_oriented() {
        let (_env, system_image) = TestEnv::new(
            SystemImageBuilder::new()
                .static_package("name/variant".parse().unwrap(), [0; 32].into()),
        )
        .await;

        assert_eq!(
            system_image.static_packages().await.unwrap(),
            StaticPackages::from_entries(vec![("name/variant".parse().unwrap(), [0; 32].into())])
        );
    }
}
