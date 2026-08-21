// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::upgradable_packages::UpgradablePackages;
use anyhow::Context as _;
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_url::fuchsia_pkg::{AbsolutePackageUrl, PackageUrl, UnpinnedAbsolutePackageUrl};
use futures::stream::TryStreamExt as _;
use log::error;
use std::sync::Arc;

const FLAGS: fio::Flags = fio::PERM_READABLE.union(fio::PERM_EXECUTABLE);

pub(crate) async fn serve_request_stream(
    mut stream: fpkg::PackageResolverRequestStream,
    base_index: Arc<crate::BaseIndex>,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: Option<Arc<UpgradablePackages>>,
) -> anyhow::Result<()> {
    while let Some(request) =
        stream.try_next().await.context("failed to read request from FIDL stream")?
    {
        match request {
            fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } => {
                match resolve(
                    &package_url,
                    dir,
                    &base_index,
                    authenticator.clone(),
                    &open_packages,
                    scope.clone(),
                    &upgradable_packages,
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "failed to resolve package {}: {:#}",
                            package_url,
                            anyhow::anyhow!(e)
                        );
                        responder.send(Err(fidl_error))
                    }
                }
                .context("sending fuchsia.pkg/PackageResolver.Resolve response")?;
            }
            fpkg::PackageResolverRequest::ResolveWithContext {
                package_url,
                context,
                dir,
                responder,
            } => {
                match resolve_with_context(
                    &package_url,
                    context,
                    dir,
                    &base_index,
                    authenticator.clone(),
                    &open_packages,
                    scope.clone(),
                    &upgradable_packages,
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "failed to resolve with context package {}: {:#}",
                            package_url,
                            anyhow::anyhow!(e)
                        );
                        responder.send(Err(fidl_error))
                    }
                }
                .context("sending fuchsia.pkg/PackageResolver.ResolveWithContext response")?;
            }
            fpkg::PackageResolverRequest::GetHash { package_url, responder } => {
                error!(
                    "unsupported fuchsia.pkg/PackageResolver.GetHash called with {:?}",
                    package_url
                );
                let () = responder
                    .send(Err(zx::Status::NOT_SUPPORTED.into_raw()))
                    .context("sending fuchsia.pkg/PackageResolver.GetHash response")?;
            }
        }
    }
    Ok(())
}

async fn resolve_with_context(
    package_url: &str,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_with_context_impl(
        &PackageUrl::parse(package_url)?,
        context,
        dir,
        base_index,
        authenticator,
        open_packages,
        scope,
        upgradable_packages,
    )
    .await
}

pub(super) async fn resolve_with_context_impl(
    package_url: &PackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Result<fpkg::ResolutionContext, Error> {
    match package_url {
        PackageUrl::Absolute(url) => {
            if !context.bytes.is_empty() {
                return Err(Error::ContextWithAbsoluteUrl);
            }
            resolve_impl(
                url,
                dir,
                base_index,
                authenticator,
                open_packages,
                scope,
                upgradable_packages,
            )
            .await
        }
        PackageUrl::Relative(url) => {
            resolve_subpackage(url, context, dir, authenticator, open_packages, scope).await
        }
    }
}

async fn resolve(
    url: &str,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_impl(
        &url.parse()?,
        dir,
        base_index,
        authenticator,
        open_packages,
        scope,
        upgradable_packages,
    )
    .await
}

pub(super) async fn resolve_impl(
    url: &AbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Result<fpkg::ResolutionContext, Error> {
    let url = match url {
        AbsolutePackageUrl::Pinned(pinned) => {
            // Resolution of pinned packages is used by CM to save memory by recreating component
            // declarations on demand (by re-resolving them) instead of caching them.
            // We specifically only allow resolution of pinned base packages (i.e. do not allow
            // resolution of pinned upgradeable packages) because upgradeable packages could be
            // upgraded at any time at which point the blobs may no longer be available.
            // TODO(https://fxbug.dev/452379656) Implement handle-based contexts for package
            // resolution, migrate CM to using said contexts to re-resolve packages instead of
            // making pinned resolves, and then re-forbid pinned resolves here.
            match base_index.url_to_hash(pinned.as_unpinned()) {
                Some(base_hash) if base_hash == &pinned.hash() => url,
                Some(base_hash) => {
                    return Err(Error::MismatchedPin {
                        pinned_hash: pinned.hash(),
                        base_hash: *base_hash,
                    });
                }
                None => return Err(Error::PackageHashNotSupported),
            }
        }
        AbsolutePackageUrl::Unpinned(url) => url,
    };
    let hash =
        resolve_package(url, dir, base_index, open_packages, scope, upgradable_packages).await?;
    Ok(authenticator.create(&hash))
}

pub(crate) async fn resolve_package(
    url: &UnpinnedAbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Result<fuchsia_hash::Hash, Error> {
    // TODO(https://fxbug.dev/335388895) Remove zero-variant fallback once variant concept is gone.
    // Base packages must have a variant of zero, and the variant is cleared before adding the URL
    // to the base_packages map. Clients are allowed to specify or omit the variant (clients
    // generally omit so we minimize the number of allocations in that case).
    let url = match url.variant() {
        Some(variant) if variant.is_zero() => &{
            let mut url = url.clone();
            url.clear_variant();
            url
        },
        _ => url,
    };
    let hash = get_package_hash(url, base_index, upgradable_packages)
        .await
        .ok_or_else(|| Error::PackageNotInIndex)?;
    let root =
        open_packages.get_or_insert(hash, None).await.map_err(Error::CreatePackageDirectory)?;
    vfs::directory::serve_on(root, FLAGS, scope, dir);
    Ok(hash)
}

async fn get_package_hash(
    url: &UnpinnedAbsolutePackageUrl,
    base_index: &crate::BaseIndex,
    upgradable_packages: &Option<Arc<UpgradablePackages>>,
) -> Option<fuchsia_hash::Hash> {
    if let Some(hash) = base_index.url_to_hash(url) {
        return Some(*hash);
    }
    if let Some(upgradable_packages) = upgradable_packages
        && let Some(hash) = upgradable_packages.get_hash(url).await
    {
        return Some(hash);
    }
    None
}

async fn resolve_subpackage(
    package_url: &fuchsia_url::RelativePackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let super_hash = authenticator.clone().authenticate(context)?;
    let super_package = open_packages.get(&super_hash).ok_or_else(|| {
        Error::SuperpackageNotOpen { superpackage: super_hash, subpackage: package_url.clone() }
    })?;
    let subpackage = *super_package
        .subpackages()
        .await?
        .subpackages()
        .get(package_url)
        .ok_or_else(|| Error::SubpackageNotFound)?;
    let root = open_packages
        .get_or_insert(subpackage, None)
        .await
        .map_err(Error::CreatePackageDirectory)?;
    vfs::directory::serve_on(root, FLAGS, scope, dir);
    Ok(authenticator.create(&subpackage))
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid URL")]
    InvalidUrl(#[from] fuchsia_url::errors::ParseError),

    #[error("resolution of pinned URLs only supported for base packages")]
    PackageHashNotSupported,

    #[error("hash in URL does not match hash in index, url: {pinned_hash}, index: {base_hash}")]
    MismatchedPin { pinned_hash: fuchsia_hash::Hash, base_hash: fuchsia_hash::Hash },

    #[error("create package directory")]
    CreatePackageDirectory(#[source] package_directory::Error),

    #[error("context must be empty when resolving absolute URL")]
    ContextWithAbsoluteUrl,

    #[error("subpackage name was not found in the package's subpackage list")]
    SubpackageNotFound,

    #[error("the package URL was not found in the index")]
    PackageNotInIndex,

    #[error("failed to read the superpackage's subpackage manifest")]
    ReadingSubpackageManifest(#[from] package_directory::SubpackagesError),

    #[error("invalid context")]
    InvalidContext(#[from] context_authenticator::ContextAuthenticatorError),

    #[error(
        "package directory for {superpackage} was not open when resolving subpackage {subpackage}"
    )]
    SuperpackageNotOpen {
        superpackage: fuchsia_hash::Hash,
        subpackage: fuchsia_url::RelativePackageUrl,
    },
}

impl From<&Error> for fidl_fuchsia_component_resolution::ResolverError {
    fn from(err: &Error) -> fidl_fuchsia_component_resolution::ResolverError {
        use Error::*;
        use fidl_fuchsia_component_resolution::ResolverError as ferror;
        match err {
            InvalidUrl(_)
            | PackageHashNotSupported
            | MismatchedPin { .. }
            | InvalidContext(_)
            | ContextWithAbsoluteUrl => ferror::InvalidArgs,
            CreatePackageDirectory(_) | ReadingSubpackageManifest(_) => ferror::Io,
            SuperpackageNotOpen { .. } => ferror::Internal,
            SubpackageNotFound | PackageNotInIndex => ferror::PackageNotFound,
        }
    }
}

impl From<Error> for fidl_fuchsia_component_resolution::ResolverError {
    fn from(err: Error) -> fidl_fuchsia_component_resolution::ResolverError {
        (&err).into()
    }
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> fpkg::ResolveError {
        use Error::*;
        use fpkg::ResolveError as ferror;
        match err {
            InvalidUrl(_) | PackageHashNotSupported | MismatchedPin { .. } => ferror::InvalidUrl,
            SuperpackageNotOpen { .. } => ferror::Internal,
            CreatePackageDirectory(_) | ReadingSubpackageManifest(_) => ferror::Io,
            PackageNotInIndex | SubpackageNotFound => ferror::PackageNotFound,
            ContextWithAbsoluteUrl | InvalidContext(_) => ferror::InvalidContext,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use std::collections::HashSet;

    #[fuchsia::test]
    async fn resolve_rejects_pinned_url_that_does_not_match_base_package_hash() {
        assert_matches!(
            resolve(
                "fuchsia-pkg://fuchsia.test/name?\
                    hash=1111111111111111111111111111111111111111111111111111111111111111",
                fidl::endpoints::create_endpoints().1,
                &crate::BaseIndex::new_test_only(HashSet::new(), [(
                    "fuchsia-pkg://fuchsia.test/name".parse().unwrap(),
                    [0; 32].into()
                )]),
                context_authenticator::ContextAuthenticator::new(),
                &crate::root_dir::new_test(blobfs::Client::new_test().0).await.1,
                vfs::execution_scope::ExecutionScope::new(),
                &None,
            )
            .await,
            Err(Error::MismatchedPin{pinned_hash, base_hash})
                if pinned_hash == [17; 32].into() && base_hash == [0; 32].into()
        )
    }

    #[fuchsia::test]
    async fn resolve_clears_zero_variant() {
        let pkg = fuchsia_pkg_testing::PackageBuilder::new("name").build().await.unwrap();
        let blobfs = blobfs_ramdisk::BlobfsRamdisk::start().await.unwrap();
        pkg.write_to_blobfs(&blobfs).await;
        let open_packages = crate::root_dir::new_test(blobfs.client()).await.1;
        let (proxy, server) = fidl::endpoints::create_proxy();

        let _: fpkg::ResolutionContext = resolve(
            "fuchsia-pkg://fuchsia.test/name/0",
            server,
            &crate::BaseIndex::new_test_only(
                HashSet::new(),
                [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), *pkg.hash())],
            ),
            context_authenticator::ContextAuthenticator::new(),
            &open_packages,
            vfs::execution_scope::ExecutionScope::new(),
            &None,
        )
        .await
        .unwrap();

        assert_eq!(
            fuchsia_pkg::PackageDirectory::from_proxy(proxy).merkle_root().await.unwrap(),
            *pkg.hash()
        );
    }

    #[fuchsia::test]
    async fn resolve_allows_pinned_url_that_matches_base_package_hash() {
        let pkg = fuchsia_pkg_testing::PackageBuilder::new("name").build().await.unwrap();
        let blobfs = blobfs_ramdisk::BlobfsRamdisk::start().await.unwrap();
        pkg.write_to_blobfs(&blobfs).await;
        let open_packages = crate::root_dir::new_test(blobfs.client()).await.1;
        let (proxy, server) = fidl::endpoints::create_proxy();

        let _: fpkg::ResolutionContext = resolve(
            &format!("fuchsia-pkg://fuchsia.test/name?hash={}", pkg.hash()),
            server,
            &crate::BaseIndex::new_test_only(
                HashSet::new(),
                [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), *pkg.hash())],
            ),
            context_authenticator::ContextAuthenticator::new(),
            &open_packages,
            vfs::execution_scope::ExecutionScope::new(),
            &None,
        )
        .await
        .unwrap();

        assert_eq!(
            fuchsia_pkg::PackageDirectory::from_proxy(proxy).merkle_root().await.unwrap(),
            *pkg.hash()
        );
    }

    #[fuchsia::test]
    async fn resolve_does_not_clear_non_zero_variant() {
        assert_matches!(
            resolve(
                "fuchsia-pkg://fuchsia.test/name/1",
                fidl::endpoints::create_proxy().1,
                &crate::BaseIndex::new_test_only(
                    HashSet::new(),
                    [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), [0u8; 32].into())]
                ),
                context_authenticator::ContextAuthenticator::new(),
                &crate::root_dir::new_test(blobfs::Client::new_test().0).await.1,
                vfs::execution_scope::ExecutionScope::new(),
                &None,
            )
            .await,
            Err(Error::PackageNotInIndex)
        );
    }
}
