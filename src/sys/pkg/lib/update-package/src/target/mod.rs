// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod board;
mod epoch;
mod hash;
mod image;
mod name;

pub use board::VerifyBoardError;
pub use epoch::ParseEpochError;
pub use hash::HashError;
pub use image::OpenImageError;
pub use name::VerifyNameError;

use fidl_fuchsia_io as fio;
use fidl_fuchsia_mem as fmem;
use fuchsia_hash::Hash;
use fuchsia_url::fuchsia_pkg::PinnedAbsolutePackageUrl;
use std::str::FromStr;
use zx_status::Status;

use crate::images::ImagesMetadata;
use crate::update_mode::UpdateMode;
use crate::version::SystemVersion;

/// An error encountered while resolving images.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ResolveImagesError {
    #[error("while listing files in the update package")]
    ListCandidates(#[source] fuchsia_fs::directory::EnumerateError),
}

/// An error encountered while reading the version.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ReadVersionError {
    #[error("while opening the file")]
    OpenFile(#[source] fuchsia_fs::node::OpenError),

    #[error("while reading the file")]
    ReadFile(#[source] fuchsia_fs::file::ReadError),
}

/// An error encountered while parsing the update-mode file.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ParseUpdateModeError {
    #[error("while opening the file")]
    OpenFile(#[source] fuchsia_fs::node::OpenError),

    #[error("while reading the file")]
    ReadFile(#[source] fuchsia_fs::file::ReadError),

    #[error("while deserializing: '{0:?}'")]
    Deserialize(String, #[source] serde_json::Error),

    #[error("update mode not supported: '{0}'")]
    UpdateModeNotSupported(String),
}

/// ParsePackageError represents any error which might occur while reading
/// `packages.json` from an update package.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ParsePackageError {
    #[error("could not open `packages.json`")]
    FailedToOpen(#[source] fuchsia_fs::node::OpenError),

    #[error("could not parse url from line: {0:?}")]
    URLParseError(String, #[source] fuchsia_url::errors::ParseError),

    #[error("error reading file `packages.json`")]
    ReadError(#[source] fuchsia_fs::file::ReadError),

    #[error("json parsing error while reading `packages.json`")]
    JsonError(#[source] serde_json::error::Error),
}

impl From<crate::packages::ParsePackageError> for ParsePackageError {
    fn from(e: crate::packages::ParsePackageError) -> Self {
        match e {
            crate::packages::ParsePackageError::URLParseError(s, err) => {
                Self::URLParseError(s, err)
            }
            crate::packages::ParsePackageError::JsonError(err) => Self::JsonError(err),
        }
    }
}

/// An error encountered while loading the images.json manifest.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ImagePackagesError {
    #[error("`images.json` not present in update package")]
    NotFound,

    #[error("while opening `images.json`")]
    Open(#[source] fuchsia_fs::node::OpenError),

    #[error("while reading `images.json`")]
    Read(#[source] fuchsia_fs::file::ReadError),

    #[error("while parsing `images.json`")]
    Parse(#[source] serde_json::error::Error),
}

impl From<crate::images::ImagePackagesError> for ImagePackagesError {
    fn from(e: crate::images::ImagePackagesError) -> Self {
        match e {
            crate::images::ImagePackagesError::NotFound => Self::NotFound,
            crate::images::ImagePackagesError::Parse(err) => Self::Parse(err),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "version", content = "content", deny_unknown_fields)]
enum UpdateModeFile {
    #[serde(rename = "1")]
    Version1 {
        #[serde(rename = "mode")]
        update_mode: String,
    },
}

/// An open handle to an image package.
pub struct UpdateImagePackage {
    proxy: fio::DirectoryProxy,
}

impl UpdateImagePackage {
    /// Creates a new [`UpdateImagePackage`] with a given proxy.
    pub fn new(proxy: fio::DirectoryProxy) -> Self {
        Self { proxy }
    }

    /// Opens the image at given `path` as a VMO buffer.
    ///
    /// The returned VMO is not resizable. If a resizable VMO is needed (e.g. it will be used with
    /// some of the `fuchsia.paver/*` protocols), create a child VMO of the returned VMO with
    /// options `SNAPSHOT_AT_LEAST_ON_WRITE | RESIZABLE`.
    pub async fn open_image(&self, path: &str) -> Result<fmem::Buffer, OpenImageError> {
        image::open_from_path(&self.proxy, path).await
    }
}

/// An open handle to an "update" package.
#[derive(Debug)]
pub struct UpdatePackage {
    proxy: fio::DirectoryProxy,
}

impl UpdatePackage {
    /// Creates a new [`UpdatePackage`] with the given proxy.
    pub fn new(proxy: fio::DirectoryProxy) -> Self {
        Self { proxy }
    }

    /// Verifies that the package's name/variant is "update/0".
    pub async fn verify_name(&self) -> Result<(), VerifyNameError> {
        name::verify(&self.proxy).await
    }

    /// Loads the image packages manifest, or determines that it is not present.
    pub async fn images_metadata(&self) -> Result<ImagesMetadata, ImagePackagesError> {
        let file =
            match fuchsia_fs::directory::open_file(&self.proxy, "images.json", fio::PERM_READABLE)
                .await
            {
                Ok(file) => file,
                Err(fuchsia_fs::node::OpenError::OpenError(Status::NOT_FOUND)) => {
                    return Err(ImagePackagesError::NotFound);
                }
                Err(e) => return Err(ImagePackagesError::Open(e)),
            };
        let contents = fuchsia_fs::file::read(&file).await.map_err(ImagePackagesError::Read)?;
        crate::images::parse_image_packages_json(&contents).map_err(Into::into).map(Into::into)
    }

    /// Verifies the board file has the given `contents`.
    pub async fn verify_board(&self, contents: &str) -> Result<(), VerifyBoardError> {
        board::verify_board(&self.proxy, contents).await
    }

    /// Parses the update-mode file to obtain update mode. Returns `Ok(None)` if the update-mode
    /// file is not present in the update package.
    pub async fn update_mode(&self) -> Result<Option<UpdateMode>, ParseUpdateModeError> {
        let fopen_res =
            fuchsia_fs::directory::open_file(&self.proxy, "update-mode", fio::PERM_READABLE).await;
        if let Err(fuchsia_fs::node::OpenError::OpenError(Status::NOT_FOUND)) = fopen_res {
            return Ok(None);
        }
        let file = fopen_res.map_err(ParseUpdateModeError::OpenFile)?;
        let contents = fuchsia_fs::file::read_to_string(&file)
            .await
            .map_err(ParseUpdateModeError::ReadFile)?;
        let UpdateModeFile::Version1 { update_mode: mode_str } = serde_json::from_str(&contents)
            .map_err(|e| ParseUpdateModeError::Deserialize(contents, e))?;
        match mode_str.as_str() {
            "normal" => Ok(Some(UpdateMode::Normal)),
            "force-recovery" => Ok(Some(UpdateMode::ForceRecovery)),
            other => Err(ParseUpdateModeError::UpdateModeNotSupported(other.to_string())),
        }
    }

    /// Returns the list of package urls that go in the universe of this update package.
    pub async fn packages(&self) -> Result<Vec<PinnedAbsolutePackageUrl>, ParsePackageError> {
        let file =
            fuchsia_fs::directory::open_file(&self.proxy, "packages.json", fio::PERM_READABLE)
                .await
                .map_err(ParsePackageError::FailedToOpen)?;
        let contents = fuchsia_fs::file::read(&file).await.map_err(ParsePackageError::ReadError)?;
        crate::packages::parse_packages_json(&contents).map_err(Into::into)
    }

    /// Returns the package hash of this update package.
    pub async fn hash(&self) -> Result<Hash, HashError> {
        hash::hash(&self.proxy).await
    }

    /// Returns the version of this update package.
    pub async fn version(&self) -> Result<SystemVersion, ReadVersionError> {
        let file = fuchsia_fs::directory::open_file(&self.proxy, "version", fio::PERM_READABLE)
            .await
            .map_err(ReadVersionError::OpenFile)?;
        let contents =
            fuchsia_fs::file::read_to_string(&file).await.map_err(ReadVersionError::ReadFile)?;
        SystemVersion::from_str(&contents).map_err(|e| match e {})
    }

    /// Parses the epoch.json file to obtain the epoch. Returns `Ok(None)` if the epoch.json file
    /// is not present in the update package.
    pub async fn epoch(&self) -> Result<Option<u64>, ParseEpochError> {
        epoch::epoch(&self.proxy).await
    }
}

#[cfg(test)]
pub(crate) struct TestUpdatePackage {
    update_pkg: UpdatePackage,
    temp_dir: tempfile::TempDir,
}

#[cfg(test)]
impl TestUpdatePackage {
    pub(crate) fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("/tmp to exist");
        let update_pkg_proxy = fuchsia_fs::directory::open_in_namespace(
            temp_dir.path().to_str().unwrap(),
            fio::PERM_READABLE,
        )
        .expect("temp dir to open");
        Self { temp_dir, update_pkg: UpdatePackage::new(update_pkg_proxy) }
    }

    pub(crate) async fn add_file(
        self,
        path: impl AsRef<std::path::Path>,
        contents: impl AsRef<[u8]>,
    ) -> Self {
        let path = path.as_ref();
        match path.parent() {
            Some(empty) if empty == std::path::Path::new("") => {}
            None => {}
            Some(parent) => std::fs::create_dir_all(self.temp_dir.path().join(parent)).unwrap(),
        }
        fuchsia_fs::file::write_in_namespace(
            self.temp_dir.path().join(path).to_str().unwrap(),
            contents,
        )
        .await
        .expect("create test update package file");
        self
    }
}

#[cfg(test)]
impl std::ops::Deref for TestUpdatePackage {
    type Target = UpdatePackage;

    fn deref(&self) -> &Self::Target {
        &self.update_pkg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use omaha_client::version::Version as SemanticVersion;
    use serde_json::json;

    fn pkg_urls<'a>(v: impl IntoIterator<Item = &'a str>) -> Vec<PinnedAbsolutePackageUrl> {
        v.into_iter().map(|s| s.parse().unwrap()).collect()
    }

    #[fuchsia::test]
    async fn lifecycle() {
        let (proxy, _server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        UpdatePackage::new(proxy);
    }

    #[fuchsia::test]
    async fn test_packages_json_success() {
        let pkg_list = [
            "fuchsia-pkg://fuchsia.com/ls/0?hash=71bad1a35b87a073f72f582065f6b6efec7b6a4a129868f37f6131f02107f1ea",
            "fuchsia-pkg://fuchsia.com/pkg-resolver/0?hash=26d43a3fc32eaa65e6981791874b6ab80fae31fbfca1ce8c31ab64275fd4e8c0",
        ];
        let packages = json!({
            "version": "1",
            "content": pkg_list,
        })
        .to_string();
        let update_pkg = TestUpdatePackage::new().add_file("packages.json", packages).await;
        assert_eq!(update_pkg.packages().await.unwrap(), pkg_urls(pkg_list));
    }

    #[fuchsia::test]
    async fn test_packages_json_corrupt() {
        let update_pkg = TestUpdatePackage::new().add_file("packages.json", "{}").await;
        assert_matches!(update_pkg.packages().await, Err(ParsePackageError::JsonError(_)));
    }

    #[fuchsia::test]
    async fn test_packages_json_version_not_supported() {
        let pkg_list = vec![
            "fuchsia-pkg://fuchsia.com/ls/0?hash=71bad1a35b87a073f72f582065f6b6efec7b6a4a129868f37f6131f02107f1ea",
        ];
        let packages = json!({
            "version": "2",
            "content": pkg_list,
        })
        .to_string();
        let update_pkg = TestUpdatePackage::new().add_file("packages.json", packages).await;
        assert_matches!(
            update_pkg.packages().await,
            Err(ParsePackageError::JsonError(e))
                if e.to_string().contains("unknown variant `2`, expected `1`")
        );
    }

    #[fuchsia::test]
    async fn test_packages_json_failure() {
        let update_pkg = TestUpdatePackage::new();
        assert_matches!(update_pkg.packages().await, Err(ParsePackageError::FailedToOpen(_)));
    }

    proptest::proptest! {
        #[test]
        fn test_json_serialize_roundtrip(s in ".+") {
            let starting_json_value = json!({
                "version": "1",
                "content": {
                    "mode": &s,
                },
            });
            let deserialized_object: UpdateModeFile =
                serde_json::from_value(starting_json_value.clone())
                    .expect("json to deserialize");
            assert_eq!(deserialized_object, UpdateModeFile::Version1 { update_mode: s });

            let final_json_value =
                serde_json::to_value(&deserialized_object)
                    .expect("serialize to value");
            assert_eq!(final_json_value, starting_json_value);
        }
    }

    #[fuchsia::test]
    async fn test_update_mode_normal() {
        let p = TestUpdatePackage::new()
            .add_file(
                "update-mode",
                serde_json::to_vec(&UpdateModeFile::Version1 { update_mode: "normal".to_string() })
                    .unwrap(),
            )
            .await;
        assert_matches!(p.update_mode().await, Ok(Some(UpdateMode::Normal)));
    }

    #[fuchsia::test]
    async fn test_update_mode_force_recovery() {
        let p = TestUpdatePackage::new()
            .add_file(
                "update-mode",
                serde_json::to_vec(&UpdateModeFile::Version1 {
                    update_mode: "force-recovery".to_string(),
                })
                .unwrap(),
            )
            .await;
        assert_matches!(p.update_mode().await, Ok(Some(UpdateMode::ForceRecovery)));
    }

    #[fuchsia::test]
    async fn test_update_mode_fail_unsupported_mode() {
        let p = TestUpdatePackage::new()
            .add_file(
                "update-mode",
                serde_json::to_vec(&UpdateModeFile::Version1 { update_mode: "potato".to_string() })
                    .unwrap(),
            )
            .await;
        assert_matches!(
            p.update_mode().await,
            Err(ParseUpdateModeError::UpdateModeNotSupported(mode)) if mode == "potato"
        );
    }

    #[fuchsia::test]
    async fn test_update_mode_fail_deserialize() {
        let p = TestUpdatePackage::new().add_file("update-mode", "oh no! this isn't json.").await;
        assert_matches!(
            p.update_mode().await,
            Err(ParseUpdateModeError::Deserialize(s, _)) if s == "oh no! this isn't json."
        );
    }

    #[fuchsia::test]
    async fn test_update_mode_missing() {
        let p = TestUpdatePackage::new();
        assert_matches!(p.update_mode().await, Ok(None));
    }

    #[fuchsia::test]
    async fn test_version_success_semantic() {
        let p = TestUpdatePackage::new().add_file("version", "123").await;
        assert_eq!(
            p.version().await.unwrap(),
            SystemVersion::Semantic(SemanticVersion::from([123]))
        );
    }

    #[fuchsia::test]
    async fn test_version_success_opaque() {
        let p = TestUpdatePackage::new().add_file("version", "2020-09-08T10:17:00+10:00").await;
        assert_eq!(
            p.version().await.unwrap(),
            SystemVersion::Opaque("2020-09-08T10:17:00+10:00".to_owned())
        );
    }

    #[fuchsia::test]
    async fn test_version_trims_trailing_whitespace() {
        let p = TestUpdatePackage::new().add_file("version", "2020-09-08T10:17:00+10:00\n").await;
        assert_eq!(
            p.version().await.unwrap(),
            SystemVersion::Opaque("2020-09-08T10:17:00+10:00".to_owned())
        );
    }

    #[fuchsia::test]
    async fn test_version_missing() {
        let p = TestUpdatePackage::new();
        assert_matches!(p.version().await, Err(ReadVersionError::OpenFile(_)));
    }

    #[fuchsia::test]
    async fn test_images_metadata_missing() {
        let p = TestUpdatePackage::new();
        assert_matches!(p.images_metadata().await, Err(ImagePackagesError::NotFound));
    }
}
