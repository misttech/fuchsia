// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use fidl_connector::Connect as _;
use fidl_fuchsia_pkg_ext as pkg;
use fidl_fuchsia_pkg_http as fpkg_http;
use fuchsia_inspect as finspect;
use fuchsia_inspect::NumericProperty as _;
use fuchsia_sync::Mutex;
use fuchsia_trace as ftrace;
use http_uri_ext::HttpUriExt as _;
use log::warn;
use std::borrow::Cow;
use std::sync::Arc;

mod retry;

const INSPECT_RECENT_FETCH_COUNT: usize = 25;

#[derive(Clone, Copy, Debug, typed_builder::TypedBuilder)]
pub(crate) struct Params {
    header_network_timeout: zx::BootDuration,
    body_network_timeout: zx::BootDuration,
    download_resumption_attempts_limit: u32,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct QueueContext {
    blob_base_url: http::Uri,
    conflict_behavior: ConflictBehavior,
    parent_trace_id: ftrace::Id,
}

/// How the blob fetcher should behave when asked to fetch a blob that is already in blobfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictBehavior {
    /// Ask blobfs if the existing blob should be overwritten, if not skip the fetch.
    AskBlobfs,
    /// Always perform the fetch and overwrite the existing blob.
    Overwrite,
}

impl QueueContext {
    pub(crate) fn new(
        blob_base_url: http::Uri,
        conflict_behavior: ConflictBehavior,
        parent_trace_id: ftrace::Id,
    ) -> Self {
        Self { blob_base_url, conflict_behavior, parent_trace_id }
    }
}

impl work_queue::TryMerge for QueueContext {
    fn try_merge(&mut self, other: Self) -> Result<(), Self> {
        if self.blob_base_url == other.blob_base_url
            && self.conflict_behavior == other.conflict_behavior
        {
            return Ok(());
        }
        Err(other)
    }
}

/// A clonable handle to the blob fetch queue.  When all clones of [`BlobFetcher`] are dropped, the
/// queue will fetch all remaining blobs in the queue and terminate its output stream.
#[derive(Clone)]
pub struct BlobFetcher {
    sender: work_queue::WorkSender<pkg::BlobId, QueueContext, Result<Option<u64>, Arc<FetchError>>>,
}

impl BlobFetcher {
    /// Creates an unbounded queue that will fetch up to `max_concurrency` blobs at once.
    /// Returns:
    ///   1. a Future to be awaited that processes the queue
    ///   2. a Self that enables pushing work onto the queue
    pub(crate) fn new(
        max_concurrency: usize,
        params: Params,
        blobfs_client: blobfs::Client,
        http_client: fidl_connector::ServiceReconnector<fpkg_http::ClientMarker>,
        inspect: finspect::Node,
    ) -> (impl Future<Output = ()>, Self) {
        let inspect = Arc::new(Inspect::new(inspect));
        let (queue, sender) = work_queue::work_queue(
            max_concurrency,
            move |blob_id: pkg::BlobId, context: QueueContext| {
                let http_client = http_client.clone();
                let blobfs_client = blobfs_client.clone();
                let inspect = Arc::clone(&inspect);
                async move {
                    let trace_id = ftrace::Id::new();
                    let guard = ftrace::async_enter!(
                        trace_id,
                        c"app",
                        c"fetch blob",
                        // Async tracing does not support multiple concurrent child durations, so we
                        // create a new top-level duration and attach the parent duration as
                        // metadata.
                        "parent_trace_id" => u64::from(context.parent_trace_id),
                        "hash" => blob_id.to_string().as_str()
                    );
                    let inspect_fetch = inspect.start_fetch(&blob_id, &context);
                    let res = fetch_blob_with_retry(
                        blob_id,
                        context,
                        params,
                        &blobfs_client,
                        &http_client,
                        &inspect_fetch,
                        trace_id,
                    )
                    .await;
                    inspect.end_fetch(inspect_fetch, &res);
                    if let Some(o) = guard {
                        o.end(&[ftrace::ArgValue::of("result", format!("{res:?}").as_str())])
                    }
                    res.map_err(Arc::new)
                }
            },
        );
        (queue.into_future(), BlobFetcher { sender })
    }

    /// Enqueue the given blob to be fetched, or attach to an existing request to fetch the blob.
    /// On success, returns the number of bytes downloaded, which is not necessarily equal to the
    /// uncompressed size of the blob, or None if the blob did not need to be downloaded.
    pub(crate) fn push(
        &self,
        blob_id: pkg::BlobId,
        context: QueueContext,
    ) -> impl Future<Output = Result<Result<Option<u64>, Arc<FetchError>>, work_queue::Closed>>
    {
        self.sender.push(blob_id, context)
    }

    /// Enqueue all the given blobs to be fetched, merging them with existing
    /// known tasks if possible, returning an iterator of the futures that will
    /// resolve to the results.
    ///
    /// This method is similar to, but more efficient than, mapping an iterator
    /// to `BlobFetcher::push`.
    pub(crate) fn push_all(
        &self,
        entries: impl Iterator<Item = (pkg::BlobId, QueueContext)>,
    ) -> impl Iterator<
        Item = impl Future<Output = Result<Result<Option<u64>, Arc<FetchError>>, work_queue::Closed>>,
    > {
        self.sender.push_all(entries)
    }
}

/// On success, returns the number of bytes downloaded, which is not necessarily equal to the
/// uncompressed size of the blob, or None if the blob did not need to be downloaded.
async fn fetch_blob_with_retry(
    blob_id: pkg::BlobId,
    QueueContext { blob_base_url, conflict_behavior, parent_trace_id: _ }: QueueContext,
    Params {
        header_network_timeout,
        body_network_timeout,
        download_resumption_attempts_limit,
    }: Params,
    blobfs_client: &blobfs::Client,
    http_client: &fidl_connector::ServiceReconnector<fpkg_http::ClientMarker>,
    inspect: &InspectFetch,
    trace_id: ftrace::Id,
) -> Result<Option<u64>, FetchError> {
    match conflict_behavior {
        ConflictBehavior::AskBlobfs => {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"pre-check presence");
            if blobfs_client.blob_present_and_up_to_date(&blob_id.into()).await {
                return Ok(None);
            }
        }
        ConflictBehavior::Overwrite => (),
    }
    let error_base = blob_base_url.clone();
    let blob_url = &blob_base_url
        .extend_dir_with_path(&blob_id.to_string())
        .map_err(|source| FetchError::BlobUrl { source, base_url: error_base, blob_id })?;
    fuchsia_backoff::retry_or_first_error(retry::blob_fetch(), || async move {
        inspect.attempt();
        let blob = {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"open blob");
            blobfs_client
                .open_blob_for_write(&blob_id.into(), true)
                .await
                .map_err(FetchError::CreateBlob)?
        };
        let guard = ftrace::async_enter!(trace_id, c"app", c"download blob");
        let res = download_blob(
            http_client,
            blob_url,
            blob,
            header_network_timeout,
            body_network_timeout,
            download_resumption_attempts_limit,
        )
        .await;
        if let Some(o) = guard {
            o.end(&[ftrace::ArgValue::of("result", format!("{res:?}").as_str())])
        }
        let download_size = res?;
        {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"post-check presence");
            // Recheck presence to catch the client lying about writing the blob, but accept
            // NeedsOverwrite (unlike in the pre-write check) because the incoming blob may not have
            // been intended by the remote blobstore to pass blobfs' up-to-date requirements. This
            // check can be tightened to UpToDate if we implement blob format negotiation.
            use blobfs::BlobStatus::*;
            match blobfs_client
                .blob_status(&blob_id.into())
                .await
                .map_err(FetchError::PostWriteStatusCheck)?
            {
                UpToDate | NeedsOverwrite => (),
                Absent => {
                    return Err(FetchError::BlobAbsentAfterWrite);
                }
            }
        }
        Ok(Some(download_size))
    })
    .await
}

async fn download_blob(
    http_client: &fidl_connector::ServiceReconnector<fpkg_http::ClientMarker>,
    blob_url: &http::Uri,
    blob: fidl::endpoints::ClientEnd<fidl_fuchsia_fxfs::BlobWriterMarker>,
    header_network_timeout: zx::BootDuration,
    body_network_timeout: zx::BootDuration,
    download_resumption_attempts_limit: u32,
) -> Result<u64, FetchError> {
    let bytes_downloaded = http_client
        .connect()
        .map_err(FetchError::ConnectToHttpClient)?
        .download_blob(
            &blob_url.to_string(),
            blob,
            header_network_timeout.into_nanos(),
            body_network_timeout.into_nanos(),
            download_resumption_attempts_limit,
        )
        .await
        .map_err(FetchError::DownloadBlobFidl)?
        .map_err(FetchError::DownloadBlob)?;
    Ok(bytes_downloaded)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    #[error("could not create blob")]
    CreateBlob(#[source] blobfs::CreateError),

    #[error("creating blob url from base: {base_url}, id: {blob_id}")]
    BlobUrl {
        #[source]
        source: http_uri_ext::Error,
        base_url: http::Uri,
        blob_id: pkg::BlobId,
    },

    #[error("connecting to fuchsia.pkg.http.Client")]
    ConnectToHttpClient(#[source] anyhow::Error),

    #[error("FIDL error while calling fuchsia.pkg.http.Client.DownloadBlob")]
    DownloadBlobFidl(#[source] fidl::Error),

    #[error("fuchsia.pkg.http.Client.DownloadBlob failed: {0:?}")]
    DownloadBlob(fpkg_http::ClientDownloadBlobError),

    #[error("checking blob status after write")]
    PostWriteStatusCheck(#[source] blobfs::BlobfsError),

    #[error("blob was not in blobfs after successful write")]
    BlobAbsentAfterWrite,
}

impl FetchError {
    fn kind(&self) -> FetchErrorKind {
        use FetchError::*;
        match self {
            DownloadBlob(e) => match e {
                fpkg_http::ClientDownloadBlobError::NetworkRateLimit => {
                    FetchErrorKind::NetworkRateLimit
                }
                fpkg_http::ClientDownloadBlobError::Network => FetchErrorKind::Network,
                fpkg_http::ClientDownloadBlobError::NotFound => FetchErrorKind::NotFound,
                fpkg_http::ClientDownloadBlobError::NoSpace
                | fpkg_http::ClientDownloadBlobError::Other => FetchErrorKind::Other,
            },
            CreateBlob { .. }
            | BlobUrl { .. }
            | ConnectToHttpClient(_)
            | DownloadBlobFidl { .. }
            | PostWriteStatusCheck { .. }
            | BlobAbsentAfterWrite => FetchErrorKind::Other,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchErrorKind {
    NetworkRateLimit,
    Network,
    NotFound,
    Other,
}

impl From<&FetchError> for fidl_fuchsia_pkg::ResolveError {
    fn from(err: &FetchError) -> Self {
        use FetchError::*;
        use fidl_fuchsia_pkg::ResolveError as Err;
        match err {
            CreateBlob { .. } => Err::Io,
            BlobUrl { .. } => Err::Internal,
            ConnectToHttpClient(_) => Err::Io,
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
            PostWriteStatusCheck { .. } => Err::Internal,
            BlobAbsentAfterWrite => Err::Internal,
        }
    }
}

struct Inspect {
    active: finspect::Node,
    fetch_count: std::sync::atomic::AtomicU64,
    recent: Mutex<fuchsia_inspect_contrib::nodes::BoundedListNode>,
    _node: finspect::Node,
}

impl Inspect {
    fn new(node: finspect::Node) -> Self {
        Self {
            active: node.create_child("active"),
            fetch_count: std::sync::atomic::AtomicU64::new(0),
            recent: Mutex::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                node.create_child("recent"),
                INSPECT_RECENT_FETCH_COUNT,
            )),
            _node: node,
        }
    }

    fn start_fetch(&self, blob_id: &pkg::BlobId, context: &QueueContext) -> InspectFetch {
        let QueueContext { blob_base_url, conflict_behavior, parent_trace_id: _ } = context;
        let node = self.active.create_child(
            self.fetch_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string(),
        );
        node.record_int("start_boot_ns", zx::BootInstant::get().into_nanos());
        node.record_string("hash", blob_id.to_string());
        node.record_string("base_url", blob_base_url.to_string());
        node.record_string("conflict_behavior", format!("{conflict_behavior:?}"));
        InspectFetch { attempts: node.create_uint("attempts", 0), node }
    }

    fn end_fetch(&self, fetch: InspectFetch, res: &Result<Option<u64>, FetchError>) {
        let InspectFetch { attempts, node } = fetch;
        node.record_int("end_boot_ns", zx::BootInstant::get().into_nanos());
        node.record_string(
            "result",
            match res {
                Ok(Some(download_size)) => {
                    Cow::Owned(format!("success: downloaded {download_size} bytes"))
                }
                Ok(None) => Cow::Borrowed("success: download not necessary"),
                Err(e) => Cow::Owned(format!("error: {}", crate::stringify_error(e))),
            },
        );
        node.record(attempts);
        let () =
            self.recent.lock().adopt_entry(node).map(|_: &finspect::Node| ()).unwrap_or_else(|e| {
                warn!("failed to move inspect node to recent: {:#}", anyhow!(e))
            });
    }
}

struct InspectFetch {
    attempts: finspect::UintProperty,
    node: finspect::Node,
}

impl InspectFetch {
    fn attempt(&self) {
        self.attempts.add(1);
    }
}
