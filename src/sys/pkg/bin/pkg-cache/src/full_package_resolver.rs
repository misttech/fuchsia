// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Used to resolve most non-OTA packages on products that support ephemeral resolution.
//! * always uses open package tracking
//! * has a hard-coded priority of authorities (url to hash mappings):
//!   1. base index
//!   2. upgradable packages
//!   3. eager packages
//!   4. the remote TUF repositories managed by pkg-resolver
//!   5. the cache index in case of certain TUF errors

use crate::upgradable_packages::UpgradablePackages;
use anyhow::{Context as _, anyhow};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fidl_fuchsia_pkg_ext as fpkg_ext;
use fuchsia_url::fuchsia_pkg::{AbsolutePackageUrl, PackageUrl};
use futures::stream::TryStreamExt as _;
use log::error;
use std::sync::Arc;

const SLOW_CACHE_FALLBACK_WARN_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(10);
const SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_minutes(10);

/// The package resolver implementation used by the full component resolver.
pub(crate) struct FullResolver {
    base_index: Arc<crate::BaseIndex>,
    upgradable_packages: Option<Arc<UpgradablePackages>>,
    tuf_authority: fpkg::AuthorityProxy,
    cache_index: Arc<crate::CacheIndex>,
    package_fetcher: crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
}

impl FullResolver {
    pub(crate) fn new(
        base_index: Arc<crate::BaseIndex>,
        upgradable_packages: Option<Arc<UpgradablePackages>>,
        tuf_authority: fpkg::AuthorityProxy,
        cache_index: Arc<crate::CacheIndex>,
        package_fetcher: crate::package_fetcher::PackageFetcher,
        authenticator: context_authenticator::ContextAuthenticator,
        open_packages: crate::RootDirCache,
        executability_restrictions: system_image::ExecutabilityRestrictions,
    ) -> Self {
        Self {
            base_index,
            upgradable_packages,
            tuf_authority,
            cache_index,
            package_fetcher,
            authenticator,
            open_packages,
            executability_restrictions,
        }
    }
}

impl crate::component_resolver::PackageResolver for FullResolver {
    type Error = Error;

    async fn resolve_and_serve(
        &self,
        url: &fuchsia_url::fuchsia_pkg::AbsolutePackageUrl,
        dir: fidl::endpoints::ServerEnd<fio::DirectoryMarker>,
        scope: package_directory::ExecutionScope,
    ) -> Result<fpkg::ResolutionContext, Error> {
        resolve_and_serve(
            url,
            dir,
            self.base_index.as_ref(),
            self.upgradable_packages.as_deref(),
            &self.tuf_authority,
            self.cache_index.as_ref(),
            &self.package_fetcher,
            self.authenticator.clone(),
            &self.open_packages,
            self.executability_restrictions,
            scope,
        )
        .await
    }

    async fn resolve_with_context_and_serve(
        &self,
        url: &fuchsia_url::fuchsia_pkg::PackageUrl,
        context: fpkg::ResolutionContext,
        dir: fidl::endpoints::ServerEnd<fio::DirectoryMarker>,
        scope: package_directory::ExecutionScope,
    ) -> Result<fpkg::ResolutionContext, Error> {
        resolve_with_context_and_serve(
            url,
            context,
            dir,
            &self.base_index,
            self.upgradable_packages.as_deref(),
            &self.tuf_authority,
            self.cache_index.as_ref(),
            &self.package_fetcher,
            self.authenticator.clone(),
            &self.open_packages,
            self.executability_restrictions,
            scope,
        )
        .await
    }
}

pub(crate) async fn serve_request_stream(
    stream: fpkg::PackageResolverRequestStream,
    base_index: Arc<crate::BaseIndex>,
    upgradable_packages: Option<Arc<UpgradablePackages>>,
    tuf_authority: fpkg::AuthorityProxy,
    cache_index: Arc<crate::CacheIndex>,
    package_fetcher: crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
    scope: package_directory::ExecutionScope,
) -> anyhow::Result<()> {
    stream
        .map_err(anyhow::Error::new)
        .try_for_each_concurrent(None, |req| async {
            match req {
                fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } => {
                    match resolve_unparsed_and_serve(
                        &package_url,
                        dir,
                        base_index.as_ref(),
                        upgradable_packages.as_deref(),
                        &tuf_authority,
                        cache_index.as_ref(),
                        &package_fetcher,
                        authenticator.clone(),
                        &open_packages,
                        executability_restrictions,
                        scope.clone(),
                    )
                    .await
                    {
                        Ok(context) => responder.send(Ok(&context)),
                        Err(e) => {
                            let fidl_error = (&e).into();
                            error!(
                                "full resolver failed to resolve {}: {:#}",
                                package_url,
                                anyhow!(e)
                            );
                            responder.send(Err(fidl_error))
                        }
                    }
                    .context("sending fuchsia.pkg/PackageResolver-full.Resolve response")
                }
                fpkg::PackageResolverRequest::ResolveWithContext {
                    package_url,
                    context,
                    dir,
                    responder,
                } => match resolve_with_context_unparsed_and_serve(
                    &package_url,
                    context,
                    dir,
                    base_index.as_ref(),
                    upgradable_packages.as_deref(),
                    &tuf_authority,
                    cache_index.as_ref(),
                    &package_fetcher,
                    authenticator.clone(),
                    &open_packages,
                    executability_restrictions,
                    scope.clone(),
                )
                .await
                {
                    Ok(context) => responder.send(Ok(&context)),
                    Err(e) => {
                        let fidl_error = (&e).into();
                        error!(
                            "full resolver failed to resolve with context {}: {:#}",
                            package_url,
                            anyhow!(e)
                        );
                        responder.send(Err(fidl_error))
                    }
                }
                .context("sending fuchsia.pkg/PackageResolver-full.ResolveWithContext response"),
                fpkg::PackageResolverRequest::GetHash { package_url, responder } => {
                    match lookup_unparsed(
                        &package_url.url,
                        base_index.as_ref(),
                        upgradable_packages.as_deref(),
                        &tuf_authority,
                        cache_index.as_ref(),
                    )
                    .await
                    {
                        Ok((hash, _)) => {
                            responder.send(Ok(&fpkg::BlobId { merkle_root: hash.into() }))
                        }
                        Err(e) => {
                            let status = zx::Status::from(&e);
                            error!(
                                "full resolver failed to get hash {}: {:#}",
                                package_url.url,
                                anyhow!(e)
                            );
                            responder.send(Err(status.into_raw()))
                        }
                    }
                }
                .context("sending fuchsia.pkg/PackageResolver-full.GetHash response"),
            }
        })
        .await
}

async fn resolve_with_context_unparsed_and_serve(
    package_url: &str,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
    package_fetcher: &crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_with_context_and_serve(
        &PackageUrl::parse(package_url).map_err(Error::InvalidUrl)?,
        context,
        dir,
        base_index,
        upgradable_packages,
        tuf_authority,
        cache_index,
        package_fetcher,
        authenticator,
        open_packages,
        executability_restrictions,
        scope,
    )
    .await
}

async fn resolve_with_context_and_serve(
    package_url: &PackageUrl,
    context: fpkg::ResolutionContext,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
    package_fetcher: &crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let root_dir = match package_url {
        PackageUrl::Absolute(url) => {
            if !context.bytes.is_empty() {
                return Err(Error::ContextWithAbsoluteUrl);
            }
            resolve(
                url,
                base_index,
                upgradable_packages,
                tuf_authority,
                cache_index,
                package_fetcher,
                open_packages,
            )
            .await
        }
        PackageUrl::Relative(url) => {
            resolve_subpackage(url, context, authenticator.clone(), open_packages).await
        }
    }?;
    let hash = *root_dir.hash();
    let flags = executability_status(executability_restrictions, base_index, hash).into();
    vfs::directory::serve_on(root_dir, flags, scope, dir);
    Ok(authenticator.create(&hash))
}

async fn resolve_unparsed_and_serve(
    url: &str,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
    package_fetcher: &crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    resolve_and_serve(
        &url.parse().map_err(Error::InvalidUrl)?,
        dir,
        base_index,
        upgradable_packages,
        tuf_authority,
        cache_index,
        package_fetcher,
        authenticator,
        open_packages,
        executability_restrictions,
        scope,
    )
    .await
}

async fn resolve_and_serve(
    url: &AbsolutePackageUrl,
    dir: ServerEnd<fio::DirectoryMarker>,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
    package_fetcher: &crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
    executability_restrictions: system_image::ExecutabilityRestrictions,
    scope: package_directory::ExecutionScope,
) -> Result<fpkg::ResolutionContext, Error> {
    let root_dir = resolve(
        url,
        base_index,
        upgradable_packages,
        tuf_authority,
        cache_index,
        package_fetcher,
        open_packages,
    )
    .await?;
    let hash = *root_dir.hash();
    let flags = executability_status(executability_restrictions, base_index, hash).into();
    vfs::directory::serve_on(root_dir, flags, scope, dir);
    Ok(authenticator.create(&hash))
}

async fn resolve(
    url: &AbsolutePackageUrl,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
    package_fetcher: &crate::package_fetcher::PackageFetcher,
    open_packages: &crate::RootDirCache,
) -> Result<Arc<crate::root_dir::RootDir>, Error> {
    let (pkg_id, blob_source) =
        lookup(url, base_index, upgradable_packages, tuf_authority, cache_index).await?;
    if let Some(root_dir) = open_packages.get(&pkg_id) {
        return Ok(root_dir);
    }
    if let Some(blob_source) = blob_source {
        return package_fetcher
            .fetch(pkg_id, blob_source, fpkg::GcProtection::OpenPackageTracking)
            .await
            .map_err(Error::PackageFetcher);
    }
    open_packages
        .get_or_insert(pkg_id, None)
        .await
        .map_err(|source| Error::CreatingRootDir { source, pkg_id })
}

async fn lookup_unparsed(
    url: &str,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
) -> Result<(fuchsia_hash::Hash, Option<http::Uri>), Error> {
    lookup(
        &url.parse().map_err(Error::InvalidUrl)?,
        base_index,
        upgradable_packages,
        tuf_authority,
        cache_index,
    )
    .await
}

// Returns the hash of the package and an optional http blob dir that contains the package's blobs.
// If the http blob dir is present, the package blobs may not all be present in local storage,
// otherwise the blobs are guaranteed to be present.
async fn lookup(
    url: &AbsolutePackageUrl,
    base_index: &crate::BaseIndex,
    upgradable_packages: Option<&UpgradablePackages>,
    tuf_authority: &fpkg::AuthorityProxy,
    cache_index: &crate::CacheIndex,
) -> Result<(fuchsia_hash::Hash, Option<http::Uri>), Error> {
    // Use monotonic timeline to warn on slow cache fallback to avoid warning on suspension.
    let start_mono = zx::MonotonicInstant::get();
    let () = match crate::base_package_resolver::lookup(url, base_index) {
        Ok(pkg_id) => return Ok((pkg_id, None)),
        Err(crate::base_package_resolver::Error::PackageNotInIndex) => (),
        Err(e) => return Err(Error::BaseResolver(e)),
    };

    if let Some(upgradable_packages) = upgradable_packages
        && let Some(hash) = upgradable_packages.get_hash(url.as_unpinned()).await
    {
        if url.hash().is_some() {
            return Err(Error::PinnedUpgradablePackage);
        }
        return Ok((hash, None));
    }

    // TODO(https://fxbug.dev/542690903): Add eager package support.

    let (tuf_err, deprecated_fallback) = match tuf_authority
        .lookup(&fpkg::PackageUrl { url: url.as_unpinned().to_string() })
        .await
        .map_err(Error::AuthorityFidl)?
    {
        Ok((fpkg::BlobId { merkle_root }, http_blob_dir)) => {
            // TODO(https://fxbug.dev/519687989): Forbid pinned URL authority override.
            let pkg_id = url.hash().unwrap_or_else(|| merkle_root.into());
            return Ok((pkg_id, Some(http_blob_dir.parse().map_err(Error::InvalidBlobDirUri)?)));
        }
        // TODO(https://fxbug.dev/42127880): Remove package not found cache fallback.
        Err(e @ fpkg::AuthorityLookupError::PackageNotFound) => (e, true),
        Err(e @ fpkg::AuthorityLookupError::RepositoryNotFound) => (e, false),
        Err(e @ fpkg::AuthorityLookupError::UpstreamConnection) => (e, false),
        Err(e) => return Err(Error::Authority(e)),
    };

    let Some(pkg_id) = lookup_cache_fallback(url, cache_index) else {
        return Err(Error::Authority(tuf_err));
    };
    if deprecated_fallback {
        log::warn!(
            "Did not find {url} in a TUF repo, but did find a matching package name in the \
             built-in cache packages set, so falling back to it. Your package repository may not \
             be configured to serve the package correctly, or may be overriding the domain for the \
             repository which would normally serve this package. This will be an error in a future \
             version of Fuchsia, see https://fxbug.dev/42127862."
        );
    }
    let () = log_slow_cache_fallback(start_mono, url);
    Ok((pkg_id, None))
}

fn lookup_cache_fallback(
    url: &AbsolutePackageUrl,
    cache_index: &crate::CacheIndex,
) -> Option<fuchsia_hash::Hash> {
    // TODO(https://fxbug.dev/335388895): Remove variant concept.
    // The URLs in the cache index do not have a variant, but we still want to accept incoming URLs
    // with a variant of "0".
    let mut no_variant;
    let url = match url.variant() {
        None => url,
        Some(variant) if !variant.is_zero() => {
            return None;
        }
        Some(_) => {
            no_variant = url.clone();
            no_variant.clear_variant();
            &no_variant
        }
    };
    match (cache_index.url_to_hash(url), url.hash()) {
        (None, _) => None,
        (Some(index_hash), None) => Some(*index_hash),
        (Some(index_hash), Some(url_hash)) if *index_hash == url_hash => Some(*index_hash),
        _ => None,
    }
}

fn log_slow_cache_fallback(start_ts: zx::MonotonicInstant, url: &AbsolutePackageUrl) {
    static LAST_LOG_TIME: std::sync::LazyLock<fuchsia_sync::Mutex<zx::MonotonicInstant>> =
        std::sync::LazyLock::new(|| fuchsia_sync::Mutex::new(zx::MonotonicInstant::INFINITE_PAST));

    let now = zx::MonotonicInstant::get();
    let resolve_duration = now - start_ts;
    if resolve_duration < SLOW_CACHE_FALLBACK_WARN_DURATION {
        return;
    }
    {
        let mut last = LAST_LOG_TIME.lock();
        if now - *last < SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION {
            return;
        } else {
            *last = now;
        }
    }
    log::warn!(
        "Resolve of {} via cache fallback took {} seconds. This could be slowing down your system, \
         and may be due to trouble connecting to a remote repository. This log will only print \
         every {} minutes, so the issue may be occuring more often. See inspect for more \
         information.",
        url,
        resolve_duration.into_seconds(),
        SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION.into_minutes(),
    );
}

async fn resolve_subpackage(
    url: &fuchsia_url::RelativePackageUrl,
    context: fpkg::ResolutionContext,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: &crate::RootDirCache,
) -> Result<Arc<crate::root_dir::RootDir>, Error> {
    let super_hash = authenticator.authenticate(context).map_err(Error::ContextAuthenticator)?;
    let super_package = open_packages.get(&super_hash).ok_or_else(|| {
        Error::SuperpackageNotOpen { superpackage: super_hash, subpackage: url.clone() }
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
    open_packages
        .get_or_insert(sub_hash, None)
        .await
        .map_err(|source| Error::CreatingSubpackageRootDir { source, subpackage: sub_hash })
}

enum ExecutabilityStatus {
    Allowed,
    Forbidden,
}

fn executability_status(
    executability_restrictions: system_image::ExecutabilityRestrictions,
    base_packages: &crate::BaseIndex,
    package: fuchsia_hash::Hash,
) -> ExecutabilityStatus {
    use ExecutabilityStatus::*;
    use system_image::ExecutabilityRestrictions::*;
    let is_base = base_packages.is_package(package);
    match (is_base, executability_restrictions) {
        (true, _) => Allowed,
        (false, Enforce) => Forbidden,
        (false, DoNotEnforce) => Allowed,
    }
}

impl From<ExecutabilityStatus> for fio::Flags {
    fn from(status: ExecutabilityStatus) -> Self {
        match status {
            ExecutabilityStatus::Allowed => fio::PERM_READABLE | fio::PERM_EXECUTABLE,
            ExecutabilityStatus::Forbidden => fio::PERM_READABLE,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid url")]
    InvalidUrl(#[source] fuchsia_url::ParseError),

    #[error("absolute package URLs must have an empty context")]
    ContextWithAbsoluteUrl,

    #[error("forwarding to base resolver")]
    BaseResolver(#[source] crate::base_package_resolver::Error),

    #[error("upgradable packages must not be pinned")]
    PinnedUpgradablePackage,

    #[error("authority call failed")]
    AuthorityFidl(#[source] fidl::Error),

    #[error("authority lookup failed: {0:?}")]
    Authority(fpkg::AuthorityLookupError),

    #[error("invalid blob dir URI")]
    InvalidBlobDirUri(#[source] http::uri::InvalidUri),

    #[error("forwarding to package fetcher")]
    PackageFetcher(#[source] Arc<crate::package_fetcher::Error>),

    #[error("creating root dir")]
    CreatingRootDir {
        #[source]
        source: package_directory::Error,
        pkg_id: fuchsia_merkle::Hash,
    },

    #[error("authenticating context")]
    ContextAuthenticator(#[source] context_authenticator::ContextAuthenticatorError),

    #[error(
        "package directory for {superpackage} was not open when resolving subpackage {subpackage}"
    )]
    SuperpackageNotOpen {
        superpackage: fuchsia_merkle::Hash,
        subpackage: fuchsia_url::RelativePackageUrl,
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

impl From<&Error> for fidl_fuchsia_component_resolution::ResolverError {
    fn from(err: &Error) -> fidl_fuchsia_component_resolution::ResolverError {
        use Error::*;
        use fidl_fuchsia_component_resolution::ResolverError as Err;
        match err {
            InvalidUrl(_) => Err::InvalidArgs,
            ContextWithAbsoluteUrl => Err::InvalidArgs,
            BaseResolver(e) => e.into(),
            PinnedUpgradablePackage => Err::InvalidArgs,
            AuthorityFidl(_) => Err::Io,
            Authority(e) => authority_to_component_resolve_err(e),
            InvalidBlobDirUri(_) => Err::Internal,
            CreatingRootDir { .. } => Err::Io,
            PackageFetcher(source) => source.as_ref().into(),
            ContextAuthenticator(_) => Err::InvalidArgs,
            SuperpackageNotOpen { .. } => Err::Internal,
            ReadingSubpackages(_) => Err::Io,
            SubpackageNotFound { .. } => Err::PackageNotFound,
            CreatingSubpackageRootDir { .. } => Err::Io,
        }
    }
}

fn authority_to_component_resolve_err(
    e: &fpkg::AuthorityLookupError,
) -> fidl_fuchsia_component_resolution::ResolverError {
    use fidl_fuchsia_component_resolution::ResolverError as Err;
    use fpkg::AuthorityLookupError::*;
    match e {
        InvalidUrl => Err::InvalidArgs,
        PinnedUrlNotAllowed => Err::Internal,
        RepositoryNotFound => Err::ResourceUnavailable,
        PackageNotFound => Err::PackageNotFound,
        UpstreamConnection => Err::Io,
        Internal => Err::Internal,
    }
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fpkg::ResolveError as Err;
        match err {
            InvalidUrl(_) => Err::InvalidUrl,
            ContextWithAbsoluteUrl => Err::InvalidContext,
            BaseResolver(e) => e.into(),
            PinnedUpgradablePackage => Err::InvalidUrl,
            AuthorityFidl(_) => Err::Io,
            Authority(e) => fpkg_ext::errors::authority_to_resolve_err(e),
            InvalidBlobDirUri(_) => Err::Internal,
            CreatingRootDir { .. } => Err::Io,
            PackageFetcher(source) => source.as_ref().into(),
            ContextAuthenticator(_) => Err::InvalidContext,
            SuperpackageNotOpen { .. } => Err::Internal,
            ReadingSubpackages(_) => Err::Io,
            SubpackageNotFound { .. } => Err::PackageNotFound,
            CreatingSubpackageRootDir { .. } => Err::Io,
        }
    }
}

impl From<&Error> for zx::Status {
    fn from(err: &Error) -> Self {
        let fidl_err: fpkg::ResolveError = err.into();
        use fpkg::ResolveError::*;
        match fidl_err {
            Internal => zx::Status::INTERNAL,
            AccessDenied => zx::Status::ACCESS_DENIED,
            Io => zx::Status::IO,
            BlobNotFound => zx::Status::INTERNAL,
            PackageNotFound => zx::Status::NOT_FOUND,
            RepoNotFound => zx::Status::NOT_FOUND,
            NoSpace => zx::Status::NO_SPACE,
            UnavailableBlob => zx::Status::UNAVAILABLE,
            UnavailableRepoMetadata => zx::Status::UNAVAILABLE,
            InvalidUrl => zx::Status::INVALID_ARGS,
            InvalidContext => zx::Status::INVALID_ARGS,
        }
    }
}
