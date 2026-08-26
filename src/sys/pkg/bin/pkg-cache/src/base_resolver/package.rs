// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_url::fuchsia_pkg::{AbsolutePackageUrl, PackageUrl};
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
) -> anyhow::Result<()> {
    while let Some(request) =
        stream.try_next().await.context("failed to read request from FIDL stream")?
    {
        match request {
            fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } => {
                match resolve_unparsed(
                    &package_url,
                    dir,
                    &base_index,
                    authenticator.clone(),
                    &open_packages,
                    scope.clone(),
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "base resolver failed to resolve {}: {:#}",
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
                match resolve_with_context_unparsed(
                    &package_url,
                    context,
                    dir,
                    &base_index,
                    authenticator.clone(),
                    &open_packages,
                    scope.clone(),
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "base resolver failed to resolve with context {}: {:#}",
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

async fn resolve_with_context_unparsed(
    package_url: &str,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_with_context(
        &PackageUrl::parse(package_url)?,
        context,
        dir,
        base_index,
        authenticator,
        open_packages,
        scope,
    )
    .await
}

pub(super) async fn resolve_with_context(
    package_url: &PackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let root_dir = match package_url {
        PackageUrl::Absolute(url) => {
            if !context.bytes.is_empty() {
                return Err(Error::ContextWithAbsoluteUrl);
            }
            resolve(url, base_index, open_packages).await?
        }
        PackageUrl::Relative(url) => {
            resolve_subpackage(url, context, authenticator.clone(), open_packages).await?
        }
    };
    let hash = *root_dir.hash();
    vfs::directory::serve_on(root_dir, FLAGS, scope, dir);
    Ok(authenticator.create(&hash))
}

async fn resolve_unparsed(
    url: &str,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_and_serve(&url.parse()?, dir, base_index, authenticator, open_packages, scope).await
}

pub(super) async fn resolve_and_serve(
    url: &AbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let root_dir = resolve(url, base_index, open_packages).await?;
    let hash = *root_dir.hash();
    vfs::directory::serve_on(root_dir, FLAGS, scope, dir);
    Ok(authenticator.create(&hash))
}

pub(crate) async fn resolve(
    url: &AbsolutePackageUrl,
    base_index: &crate::BaseIndex,
    open_packages: &crate::RootDirCache,
) -> Result<Arc<crate::root_dir::RootDir>, Error> {
    let pkg_id = lookup(url, base_index)?;
    open_packages.get_or_insert(pkg_id, None).await.map_err(Error::CreatePackageDirectory)
}

pub(crate) fn lookup(
    url: &AbsolutePackageUrl,
    base_index: &crate::BaseIndex,
) -> Result<fuchsia_hash::Hash, Error> {
    // TODO(https://fxbug.dev/335388895): Remove zero-variant fallback once variant concept is gone.
    // Base packages must have a variant of zero, and the variant is cleared before adding the URL
    // to the base_packages map. Clients are allowed to specify or omit the variant (clients
    // generally omit so we minimize the number of allocations in that case).
    let mut url_storage;
    let url = match url.variant() {
        Some(variant) if variant.is_zero() => {
            url_storage = url.clone();
            url_storage.clear_variant();
            &url_storage
        }
        _ => url,
    };
    match url {
        AbsolutePackageUrl::Pinned(pinned) => {
            // Resolution of pinned packages is used by CM to save memory by recreating component
            // declarations on demand (by re-resolving them) instead of caching them.
            // TODO(https://fxbug.dev/452379656): Implement handle-based contexts for package
            // resolution, migrate CM to using said contexts to re-resolve packages instead of
            // making pinned resolves, and then re-forbid pinned resolves here.
            match base_index.url_to_hash(pinned.as_unpinned()) {
                Some(index_hash) if index_hash == &pinned.hash() => Ok(*index_hash),
                Some(index_hash) => Err(Error::MismatchedPin {
                    pinned_hash: pinned.hash(),
                    index_hash: *index_hash,
                }),
                None => Err(Error::PackageNotInIndex),
            }
        }
        AbsolutePackageUrl::Unpinned(url) => match base_index.url_to_hash(url) {
            Some(index_hash) => Ok(*index_hash),
            None => Err(Error::PackageNotInIndex),
        },
    }
}

async fn resolve_subpackage(
    package_url: &fuchsia_url::RelativePackageUrl,
    context: fpkg::ResolutionContext,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
) -> Result<Arc<crate::root_dir::RootDir>, Error> {
    let super_hash = authenticator.authenticate(context)?;
    let super_package = open_packages.get(&super_hash).ok_or_else(|| {
        Error::SuperpackageNotOpen { superpackage: super_hash, subpackage: package_url.clone() }
    })?;
    let subpackage = *super_package
        .subpackages()
        .await?
        .subpackages()
        .get(package_url)
        .ok_or_else(|| Error::SubpackageNotFound)?;
    open_packages.get_or_insert(subpackage, None).await.map_err(Error::CreatePackageDirectory)
}

pub(crate) async fn resolve_and_serve_no_context(
    url: &AbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    open_packages: &crate::RootDirCache,
    scope: package_directory::ExecutionScope,
) -> Result<(), Error> {
    let root_dir = resolve(url, base_index, open_packages).await?;
    let () = vfs::directory::serve_on(root_dir, FLAGS, scope, dir);
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid URL")]
    InvalidUrl(#[from] fuchsia_url::errors::ParseError),

    #[error("hash in URL does not match hash in index, url: {pinned_hash}, index: {index_hash}")]
    MismatchedPin { pinned_hash: fuchsia_hash::Hash, index_hash: fuchsia_hash::Hash },

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
            InvalidUrl(_) | MismatchedPin { .. } | InvalidContext(_) | ContextWithAbsoluteUrl => {
                ferror::InvalidArgs
            }
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
            InvalidUrl(_) | MismatchedPin { .. } => ferror::InvalidUrl,
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
            resolve_unparsed(
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
            )
            .await,
            Err(Error::MismatchedPin{pinned_hash, index_hash})
                if pinned_hash == [17; 32].into() && index_hash == [0; 32].into()
        )
    }

    #[fuchsia::test]
    async fn resolve_clears_zero_variant() {
        let pkg = fuchsia_pkg_testing::PackageBuilder::new("name").build().await.unwrap();
        let blobfs = blobfs_ramdisk::BlobfsRamdisk::start().await.unwrap();
        pkg.write_to_blobfs(&blobfs).await;
        let open_packages = crate::root_dir::new_test(blobfs.client()).await.1;
        let (proxy, server) = fidl::endpoints::create_proxy();

        let _: fpkg::ResolutionContext = resolve_unparsed(
            "fuchsia-pkg://fuchsia.test/name/0",
            server,
            &crate::BaseIndex::new_test_only(
                HashSet::new(),
                [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), *pkg.hash())],
            ),
            context_authenticator::ContextAuthenticator::new(),
            &open_packages,
            vfs::execution_scope::ExecutionScope::new(),
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

        let _: fpkg::ResolutionContext = resolve_unparsed(
            &format!("fuchsia-pkg://fuchsia.test/name?hash={}", pkg.hash()),
            server,
            &crate::BaseIndex::new_test_only(
                HashSet::new(),
                [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), *pkg.hash())],
            ),
            context_authenticator::ContextAuthenticator::new(),
            &open_packages,
            vfs::execution_scope::ExecutionScope::new(),
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
            resolve_unparsed(
                "fuchsia-pkg://fuchsia.test/name/1",
                fidl::endpoints::create_proxy().1,
                &crate::BaseIndex::new_test_only(
                    HashSet::new(),
                    [("fuchsia-pkg://fuchsia.test/name".parse().unwrap(), [0u8; 32].into())]
                ),
                context_authenticator::ContextAuthenticator::new(),
                &crate::root_dir::new_test(blobfs::Client::new_test().0).await.1,
                vfs::execution_scope::ExecutionScope::new(),
            )
            .await,
            Err(Error::PackageNotInIndex)
        );
    }
}
