// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, anyhow};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_url::fuchsia_pkg::{AbsolutePackageUrl, PackageUrl};
use futures::stream::TryStreamExt as _;
use log::error;
use std::sync::Arc;

// Packages resolved during OTA never need to be executed.
const FLAGS: fio::Flags = fio::PERM_READABLE;

/// Used only by the system-updater to resolve packages during OTA, and so:
/// * assumes the retained index has been initialized with the to-be-resolved package
/// * to attempt to recover from a partially broken system:
///   * always uses the TUF package authority to resolve the package (e.g. does not short-circuit
///     the package resolve if the pinned URL is found in the base index)
///   * queries `fuchsia.fxfs/BlobCreator.NeedsOverwrite` for every blob (e.g. does not
///     short-circuit the blob write if a blob is readable via `fuchsia.fxfs/BlobReader.GetVmo`)
pub(crate) async fn serve_request_stream(
    stream: fpkg::PackageResolverRequestStream,
    queued_tuf_resolver: crate::queued_resolver::QueuedResolver,
    authenticator: context_authenticator::ContextAuthenticator,
    root_dir_factory: crate::root_dir::RootDirFactory,
    scope: package_directory::ExecutionScope,
) -> anyhow::Result<()> {
    stream
        .map_err(anyhow::Error::new)
        .try_for_each_concurrent(None, |req| async {
            match req {
                fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } => {
                    match resolve(
                        &package_url,
                        dir,
                        &queued_tuf_resolver,
                        authenticator.clone(),
                        scope.clone(),
                    )
                    .await
                    {
                        Ok(context) => responder.send(Ok(&context)),
                        Err(e) => {
                            let fidl_error = (&e).into();
                            error!("failed to resolve package {}: {:#}", package_url, anyhow!(e));
                            responder.send(Err(fidl_error))
                        }
                    }
                    .context("sending fuchsia.pkg/PackageResolver-ota.Resolve response")
                }
                fpkg::PackageResolverRequest::ResolveWithContext {
                    package_url,
                    context,
                    dir,
                    responder,
                } => match resolve_with_context(
                    &package_url,
                    context,
                    dir,
                    &queued_tuf_resolver,
                    authenticator.clone(),
                    &root_dir_factory,
                    scope.clone(),
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "failed to resolve with context package {}: {:#}",
                            package_url,
                            anyhow!(e)
                        );
                        responder.send(Err(fidl_error))
                    }
                }
                .context("sending fuchsia.pkg/PackageResolver-ota.ResolveWithContext response"),
                fpkg::PackageResolverRequest::GetHash { package_url, responder } => {
                    error!(
                        "unsupported fuchsia.pkg/PackageResolver-ota.GetHash called with {:?}",
                        package_url
                    );
                    responder
                        .send(Err(zx::Status::NOT_SUPPORTED.into_raw()))
                        .context("sending fuchsia.pkg/PackageResolver.GetHash response")
                }
            }
        })
        .await
}

async fn resolve_with_context(
    package_url: &str,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    queued_tuf_resolver: &crate::queued_resolver::QueuedResolver,
    authenticator: context_authenticator::ContextAuthenticator,
    root_dir_factory: &crate::root_dir::RootDirFactory,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_with_context_impl(
        &PackageUrl::parse(package_url).map_err(Error::InvalidUrl)?,
        context,
        dir,
        queued_tuf_resolver,
        authenticator,
        root_dir_factory,
        scope,
    )
    .await
}

async fn resolve_with_context_impl(
    package_url: &PackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    queued_tuf_resolver: &crate::queued_resolver::QueuedResolver,
    authenticator: context_authenticator::ContextAuthenticator,
    root_dir_factory: &crate::root_dir::RootDirFactory,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    match package_url {
        PackageUrl::Absolute(url) => {
            if !context.bytes.is_empty() {
                return Err(Error::ContextWithAbsoluteUrl);
            }
            resolve_impl(url, dir, queued_tuf_resolver, authenticator, scope).await
        }
        PackageUrl::Relative(url) => {
            resolve_subpackage(url, context, dir, authenticator, root_dir_factory, scope).await
        }
    }
}

async fn resolve(
    url: &str,
    dir: ServerEnd<fio::DirectoryMarker>,
    queued_tuf_resolver: &crate::queued_resolver::QueuedResolver,
    authenticator: context_authenticator::ContextAuthenticator,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_impl(
        &url.parse().map_err(Error::InvalidUrl)?,
        dir,
        queued_tuf_resolver,
        authenticator,
        scope,
    )
    .await
}

pub(crate) async fn resolve_impl(
    url: &AbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    queued_tuf_resolver: &crate::queued_resolver::QueuedResolver,
    authenticator: context_authenticator::ContextAuthenticator,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let root_dir = queued_tuf_resolver
        .resolve(url.clone(), fpkg::GcProtection::Retained)
        .await
        .map_err(Error::QueuedResolve)?;
    let hash = *root_dir.hash();
    vfs::directory::serve_on(root_dir, FLAGS, scope, dir);
    Ok(authenticator.create(&hash))
}

async fn resolve_subpackage(
    url: &fuchsia_url::RelativePackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    authenticator: context_authenticator::ContextAuthenticator,
    root_dir_factory: &crate::root_dir::RootDirFactory,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let super_hash =
        authenticator.clone().authenticate(context).map_err(Error::ContextAuthenticator)?;
    let super_package = root_dir_factory.create(super_hash).await.map_err(|source| {
        Error::CreatingSuperpackageRootDir { source, superpackage: super_hash }
    })?;
    let sub_hash = *super_package
        .subpackages()
        .await
        .map_err(Error::ReadingSubpackages)?
        .subpackages()
        .get(url)
        .ok_or_else(|| Error::SubpackageNotFound {
            subpackage: url.clone(),
            superpackage: super_hash,
        })?;
    let sub_package = root_dir_factory
        .create(sub_hash)
        .await
        .map_err(|source| Error::CreatingSubpackageRootDir { source, subpackage: sub_hash })?;
    vfs::directory::serve_on(Arc::new(sub_package), FLAGS, scope, dir);
    Ok(authenticator.create(&sub_hash))
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid url")]
    InvalidUrl(#[source] fuchsia_url::ParseError),

    #[error("absolute package URLs must have an empty context")]
    ContextWithAbsoluteUrl,

    #[error("forwarding to the queued resolver")]
    QueuedResolve(#[source] Arc<crate::queued_resolver::Error>),

    #[error("authenticating context")]
    ContextAuthenticator(#[source] context_authenticator::ContextAuthenticatorError),

    #[error("creating superpackage root dir")]
    CreatingSuperpackageRootDir {
        #[source]
        source: package_directory::Error,
        superpackage: fuchsia_merkle::Hash,
    },

    #[error("reading subpackage manifest")]
    ReadingSubpackages(#[source] package_directory::SubpackagesError),

    #[error("subpackage {subpackage} of {superpackage} not found")]
    SubpackageNotFound {
        subpackage: fuchsia_url::RelativePackageUrl,
        superpackage: fuchsia_merkle::Hash,
    },

    #[error("creating subpackage root dir")]
    CreatingSubpackageRootDir {
        #[source]
        source: package_directory::Error,
        subpackage: fuchsia_merkle::Hash,
    },
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fpkg::ResolveError as Err;
        match err {
            InvalidUrl(_) => Err::InvalidUrl,
            ContextWithAbsoluteUrl => Err::InvalidContext,
            QueuedResolve(source) => source.as_ref().into(),
            ContextAuthenticator(_) => Err::InvalidContext,
            CreatingSuperpackageRootDir { .. } => Err::Io,
            ReadingSubpackages(_) => Err::Io,
            SubpackageNotFound { .. } => Err::PackageNotFound,
            CreatingSubpackageRootDir { .. } => Err::Io,
        }
    }
}
