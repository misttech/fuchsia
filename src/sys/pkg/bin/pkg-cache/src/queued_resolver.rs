// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_pkg as fpkg;
use fuchsia_url::fuchsia_pkg::AbsolutePackageUrl;
use std::collections::HashSet;
use std::sync::Arc;

/// Work-queue based package resolver. When all clones of
/// [`QueuedResolver`] are dropped, the queue will resolve all remaining
/// packages and terminate its output stream.
#[derive(Clone, Debug)]
pub struct QueuedResolver {
    sender: work_queue::WorkSender<
        AbsolutePackageUrl,
        QueueContext,
        Result<Arc<crate::RootDir>, Arc<Error>>,
    >,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueContext {
    gc_protection: fpkg::GcProtection,
}

impl work_queue::TryMerge for QueueContext {
    // Do not merge Contexts with differing GC protection. Clients depend on the different GC
    // protection behaviors.
    fn try_merge(&mut self, other: Self) -> Result<(), Self> {
        if self.gc_protection == other.gc_protection { Ok(()) } else { Err(other) }
    }
}

impl QueuedResolver {
    /// Creates an unbounded queue that will resolve up to `max_concurrency` packages at once.
    /// Returns:
    ///   1. a Future to be awaited that processes the queue
    ///   2. a Self that enables pushing work onto the queue
    pub(crate) fn new(
        max_concurrency: usize,
        authority: fpkg::AuthorityProxy,
        package_index: Arc<async_lock::RwLock<crate::index::PackageIndex>>,
        blobfs_client: blobfs::Client,
        blob_fetcher: crate::blob_fetcher::BlobFetcher,
        root_dir_factory: crate::root_dir::RootDirFactory,
    ) -> (impl Future<Output = ()>, Self) {
        let (queue, sender) = work_queue::work_queue(
            max_concurrency,
            move |url: fuchsia_url::FuchsiaPkgAbsolutePackageUrl, context: QueueContext| {
                let authority = authority.clone();
                let package_index = package_index.clone();
                let blobfs_client = blobfs_client.clone();
                let blob_fetcher = blob_fetcher.clone();
                let root_dir_factory = root_dir_factory.clone();
                async move {
                    resolve(
                        url,
                        context.gc_protection,
                        &authority,
                        package_index.as_ref(),
                        &blobfs_client,
                        &blob_fetcher,
                        &root_dir_factory,
                    )
                    .await
                }
            },
        );
        (queue.into_future(), Self { sender })
    }

    pub(crate) async fn resolve(
        &self,
        url: fuchsia_url::FuchsiaPkgAbsolutePackageUrl,
        gc_protection: fpkg::GcProtection,
    ) -> Result<Arc<crate::RootDir>, Arc<Error>> {
        self.sender.push(url, QueueContext { gc_protection }).await.map_err(Error::PushQueue)?
    }
}

async fn resolve(
    url: fuchsia_url::FuchsiaPkgAbsolutePackageUrl,
    gc_protection: fpkg::GcProtection,
    authority: &fpkg::AuthorityProxy,
    package_index: &async_lock::RwLock<crate::index::PackageIndex>,
    blobfs_client: &blobfs::Client,
    blob_fetcher: &crate::blob_fetcher::BlobFetcher,
    root_dir_factory: &crate::root_dir::RootDirFactory,
) -> Result<Arc<crate::RootDir>, Arc<Error>> {
    // TODO(https://fxbug.dev/542381507): Support open package tracking.
    std::assert_matches!(gc_protection, fpkg::GcProtection::Retained);
    let (fpkg::BlobId { merkle_root }, http_blob_dir) = authority
        .lookup(&fpkg::PackageUrl { url: url.as_unpinned().to_string() })
        .await
        .map_err(Error::AuthorityFidl)?
        .map_err(Error::Authority)?;
    // TODO(https://fxbug.dev/519687989): Stop allowing pinned URLs to override authorities.
    let pkg_id = url.hash().unwrap_or_else(|| merkle_root.into());
    let gc_guard = package_index.write().await.start_writing(pkg_id, gc_protection);
    let resolve_ret = resolve_impl(
        pkg_id,
        &http_blob_dir,
        package_index,
        gc_protection,
        blobfs_client,
        blob_fetcher,
        root_dir_factory,
    )
    .await;
    let stop_ret = package_index.write().await.stop_writing(gc_guard);
    match (resolve_ret, stop_ret) {
        (resolve_ret, Ok(())) => resolve_ret,
        (Ok(_), Err(e)) => Err(Error::ClearWritingIndex(e)),
        (Err(resolve_err), Err(stop_err)) => {
            Err(Error::ResolveAndClearFailed { source: Box::new(resolve_err), stop_err })
        }
    }
    .map_err(Arc::new)
}

async fn resolve_impl(
    pkg_id: fuchsia_merkle::Hash,
    http_blob_dir: &str,
    package_index: &async_lock::RwLock<crate::index::PackageIndex>,
    gc_protection: fpkg::GcProtection,
    blobfs_client: &blobfs::Client,
    blob_fetcher: &crate::blob_fetcher::BlobFetcher,
    root_dir_factory: &crate::root_dir::RootDirFactory,
) -> Result<Arc<crate::RootDir>, Error> {
    let mut queue = std::collections::VecDeque::from([pkg_id]);
    let mut queued = HashSet::from([pkg_id]);
    let context = crate::blob_fetcher::QueueContext::new(
        http_blob_dir.parse().map_err(Error::InvalidBlobDirUri)?,
    );
    let mut ret = None;
    while let Some(blob_id) = queue.pop_front() {
        // The blob fetcher performs this check as well, but check here to avoid blocking the
        // resolve of an already cached package on a full blob fetch queue.
        if !blobfs_client.blob_present_and_up_to_date(&blob_id).await {
            let () = blob_fetcher
                .push(blob_id.into(), context.clone())
                .await
                .map_err(Error::BlobPush)?
                .map_err(Error::BlobFetch)?;
        }
        let root_dir = root_dir_factory
            .create(blob_id)
            .await
            .map_err(|source| Error::CreatingRootDir { source, pkg: blob_id })?;
        let subpackages = root_dir
            .subpackages()
            .await
            .map_err(|source| Error::ReadingSubpackages { source, pkg: blob_id })?
            .into_hashes_undeduplicated()
            .collect::<Vec<_>>();
        let () = package_index
            .write()
            .await
            .add_blobs(
                pkg_id,
                HashSet::from_iter(subpackages.as_slice().iter().copied()),
                gc_protection,
            )
            .map_err(Error::ProtectBlobs)?;
        for sub in subpackages {
            if queued.insert(sub) {
                queue.push_back(sub);
            }
        }
        let content = root_dir.external_file_hashes().copied().collect::<HashSet<_>>();
        let () = package_index
            .write()
            .await
            .add_blobs(pkg_id, content.clone(), gc_protection)
            .map_err(Error::ProtectBlobs)?;
        let mut missing_content = vec![];
        for content_id in content.into_iter() {
            if !blobfs_client.blob_present_and_up_to_date(&content_id).await {
                missing_content.push(content_id);
            }
        }
        for fut in
            blob_fetcher.push_all(missing_content.into_iter().map(|h| (h.into(), context.clone())))
        {
            let () = fut.await.map_err(Error::BlobPush)?.map_err(Error::BlobFetch)?;
        }
        ret.get_or_insert(root_dir);
    }
    Ok(Arc::new(ret.expect("queue starts with an entry")))
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("authority call failed")]
    AuthorityFidl(#[source] fidl::Error),

    #[error("authority lookup failed: {0:?}")]
    Authority(fpkg::AuthorityLookupError),

    #[error("invalid blob dir URI")]
    InvalidBlobDirUri(#[source] http::uri::InvalidUri),

    #[error("pushing a blob onto the fetch queue")]
    BlobPush(#[source] work_queue::Closed),

    #[error("fetching blob")]
    BlobFetch(#[source] Arc<crate::blob_fetcher::FetchError>),

    #[error("creating root dir for {pkg}")]
    CreatingRootDir {
        #[source]
        source: package_directory::Error,
        pkg: fuchsia_merkle::Hash,
    },

    #[error("reading subpackages for {pkg}")]
    ReadingSubpackages {
        #[source]
        source: package_directory::SubpackagesError,
        pkg: fuchsia_merkle::Hash,
    },

    #[error("adding blobs to package index")]
    ProtectBlobs(#[source] crate::index::AddBlobsError),

    #[error("clearing the writing index after resolve complete")]
    ClearWritingIndex(#[source] crate::index::StopError),

    #[error("clearing the writing index failed {stop_err:?} after the resolve failed")]
    ResolveAndClearFailed {
        #[source]
        source: Box<Error>,
        stop_err: crate::index::StopError,
    },

    #[error("pushing request into queue")]
    PushQueue(#[source] work_queue::Closed),
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fpkg::ResolveError as Err;
        match err {
            AuthorityFidl(_) => Err::Io,
            Authority(e) => authority_to_resolve_err(e),
            InvalidBlobDirUri(_) => Err::Internal,
            BlobPush(_) => Err::Internal,
            BlobFetch(e) => fetch_to_resolve_err(e),
            CreatingRootDir { .. } => Err::Io,
            ReadingSubpackages { .. } => Err::Io,
            ProtectBlobs(_) => Err::Internal,
            ClearWritingIndex(_) => Err::Internal,
            ResolveAndClearFailed { source, .. } => (&**source).into(),
            PushQueue(_) => Err::Internal,
        }
    }
}

fn authority_to_resolve_err(err: &fpkg::AuthorityLookupError) -> fpkg::ResolveError {
    use fpkg::AuthorityLookupError::*;
    use fpkg::ResolveError as Err;
    match err {
        InvalidUrl => Err::InvalidUrl,
        PinnedUrlNotAllowed => Err::Internal,
        RepositoryNotFound => Err::RepoNotFound,
        PackageNotFound => Err::PackageNotFound,
        Internal => Err::Internal,
    }
}

fn fetch_to_resolve_err(err: &crate::blob_fetcher::FetchError) -> fpkg::ResolveError {
    use crate::blob_fetcher::FetchError::*;
    use fpkg::ResolveError as Err;
    match err {
        CreateBlob { .. } => Err::Io,
        BlobUrl { .. } => Err::Internal,
        DownloadBlobFidl { .. } => Err::Internal,
        DownloadBlob(e) => {
            use fidl_fuchsia_pkg_http::ClientDownloadBlobError::*;
            match e {
                NoSpace => Err::NoSpace,
                Network => Err::UnavailableBlob,
                NotFound => Err::UnavailableBlob,
                NetworkRateLimit => Err::Io,
                Other => Err::Io,
            }
        }
    }
}
