// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::accessor::{BatchIterator, BatchRetrievalTimeout};
use crate::diagnostics::AccessorStats;
use crate::error::AccessorError;
use crate::inspect::repository::InspectRepository;
use crate::inspect::{PerformanceConfig, ReaderServer};
use crate::pipeline::StaticHierarchyAllowlist;
use diagnostics_data::{InspectData, InspectHandleName};
use diagnostics_hierarchy::{
    DiagnosticsHierarchy, SelectResult, filter_tree, select_from_hierarchy,
};
use fidl::endpoints::{ControlHandle, DiscoverableProtocolMarker, ServerEnd};
use fidl_fuchsia_component_sandbox as fsandbox;
use fidl_fuchsia_diagnostics::{
    BatchIteratorMarker, BatchIteratorRequestStream, ConfigurationError, DataType, Format,
    RuntimeError, SampleDatum, SampleMarker, SampleParameters, SampleReady, SampleRequest,
    SampleRequestStream, SampleSinkEvent, SampleSinkProxy, SampleSinkResult, SampleStrategy,
    Selector, SelectorArgument, StreamMode, StreamParameters, TreeNames,
};
use fuchsia_async::{Interval, Scope};
use fuchsia_inspect::Node;
use fuchsia_trace as ftrace;
use futures::channel::oneshot;
use futures::{StreamExt, TryStreamExt, select};
use log::{debug, warn};
use moniker::ExtendedMoniker;
use selectors::{FastError, SelectorExt, ValidateExt, parse_selector};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use thiserror::Error;
use zx::MonotonicDuration;

/// This is an unfiltered pipeline.
///
/// `fuchsia.diagnostics.Sample` should NOT be used for data exfiltration that does
/// not have some other privacy control. An example of something acceptable is cobalt.
/// An example of something that would violate privacy policy is feedback.
const UNFILTERED_ALLOWLIST: StaticHierarchyAllowlist = StaticHierarchyAllowlist::new_disabled();

#[derive(Debug, Error)]
enum Error {
    #[error("Data field on SampleParameters must be populated")]
    DataCannotBeEmpty,

    #[error("Selectors field on SampleParameters must be populated")]
    DatumMissingSelectors,

    #[error("Sampling interval must be configured")]
    DatumMissingInterval,

    #[error(transparent)]
    SelectorParsing(#[from] selectors::Error),

    #[error(transparent)]
    InvalidSelector(#[from] selectors::ValidationError),

    #[error("Sample period {actual:?} is less than the minimum required: {minimum:?}")]
    SamplePeriodTooSmall { minimum: MonotonicDuration, actual: MonotonicDuration },
}

impl From<Error> for ConfigurationError {
    fn from(error: Error) -> ConfigurationError {
        match error {
            Error::DataCannotBeEmpty
            | Error::DatumMissingSelectors
            | Error::DatumMissingInterval => Self::SampleParametersInvalid,
            Error::SelectorParsing(_) | Error::InvalidSelector(_) => Self::InvalidSelectors,
            Error::SamplePeriodTooSmall { .. } => Self::SamplePeriodTooSmall,
        }
    }
}

pub struct SampleServer {
    scope: Scope,
    repo: Arc<InspectRepository>,
    trace_id: ftrace::Id,
    minimum_sample_period: MonotonicDuration,
}

impl SampleServer {
    pub fn new(
        repo: Arc<InspectRepository>,
        minimum_sample_period: MonotonicDuration,
        scope: Scope,
    ) -> Self {
        Self { scope, trace_id: ftrace::Id::new(), repo, minimum_sample_period }
    }

    pub async fn install_and_serve(
        self: &Arc<Self>,
        accessors_dict_id: u64,
        id_gen: &sandbox::CapabilityIdGenerator,
        store: &mut fsandbox::CapabilityStoreProxy,
    ) {
        let (sample_receiver_client, sample_receiver_stream) =
            fidl::endpoints::create_request_stream::<fsandbox::ReceiverMarker>();
        let sample_connector_id = id_gen.next();
        // Unwraps: I think these unwraps are safe in the sense that being unable to install
        // accessors to this store at component startup is actually a big deal, and crashing
        // is appropriate.
        store.connector_create(sample_connector_id, sample_receiver_client).await.unwrap().unwrap();
        debug!("Added fuchsia.diagnostics.Sample to accessors dictionary.");

        // Unwraps: I think these unwraps are safe in the sense that being unable to install
        // accessors to this store at component startup is actually a big deal, and crashing
        // is appropriate.
        store
            .dictionary_insert(
                accessors_dict_id,
                &fsandbox::DictionaryItem {
                    key: SampleMarker::PROTOCOL_NAME.to_string(),
                    value: sample_connector_id,
                },
            )
            .await
            .unwrap()
            .unwrap();

        self.spawn_receiver_request_handler(sample_receiver_stream);
    }

    fn spawn_receiver_request_handler(
        self: &Arc<SampleServer>,
        mut receiver_stream: fsandbox::ReceiverRequestStream,
    ) {
        let this = Arc::clone(self);
        self.scope.spawn(async move {
            while let Some(request) = receiver_stream.try_next().await.unwrap() {
                match request {
                    fsandbox::ReceiverRequest::Receive { channel, control_handle: _ } => {
                        let server_end = ServerEnd::<SampleMarker>::new(channel);
                        let this_for_handler = Arc::clone(&this);
                        this.scope.spawn(
                            this_for_handler.sample_request_handler(server_end.into_stream()),
                        );
                    }
                    fsandbox::ReceiverRequest::_UnknownMethod { method_type, ordinal, .. } => {
                        warn!(method_type:?, ordinal; "Got unknown interaction on Receiver");
                    }
                }
            }
        });
    }

    async fn sample_request_handler(self: Arc<Self>, mut stream: SampleRequestStream) {
        let mut accumulator = SampleParameters { data: Some(vec![]), ..Default::default() };
        let mut accumulation_error = Result::<(), Error>::Ok(());
        while let Some(Ok(request)) = stream.next().await {
            match request {
                SampleRequest::Commit { sink, responder, .. } => {
                    if let Err(error) = accumulation_error {
                        warn!(error:?; "Configuration error in SampleRequest::Set");
                        let _ = responder.send(Err(ConfigurationError::from(error)));
                        accumulation_error = Ok(());
                        continue;
                    }
                    if let Err(error) = self.handle_set_request(accumulator, sink.into_proxy()) {
                        accumulator = SampleParameters { data: Some(vec![]), ..Default::default() };
                        accumulation_error = Ok(());

                        warn!(error:?; "Configuration error in SampleRequest::Commit");
                        let _ = responder.send(Err(ConfigurationError::from(error)));
                        continue;
                    }

                    accumulator = SampleParameters { data: Some(vec![]), ..Default::default() };
                    accumulation_error = Ok(());
                    let _ = responder.send(Ok(()));
                }
                SampleRequest::Set { sample_parameters, .. } => {
                    if accumulation_error.is_err() {
                        // this will return an error when `Commit` is called, so there
                        // is no point continuing to push data
                        continue;
                    }

                    let Some(new_data) = sample_parameters.data else {
                        accumulation_error = Err(Error::DataCannotBeEmpty);
                        continue;
                    };

                    let Some(ref mut acc) = accumulator.data.as_mut() else {
                        continue;
                    };

                    acc.extend(new_data);
                }

                SampleRequest::_UnknownMethod { ordinal, control_handle, method_type, .. } => {
                    warn!(ordinal, method_type:?; "Received unknown request for Sample");
                    control_handle.shutdown();
                }
            }
        }
    }

    fn handle_set_request(
        self: &Arc<Self>,
        sample_parameters: SampleParameters,
        sink: SampleSinkProxy,
    ) -> Result<(), Error> {
        validate_parameters(&sample_parameters)?;
        let data = sample_parameters
            .data
            .unwrap()
            .into_iter()
            .enumerate()
            .map(Datum::try_from)
            .collect::<Result<Vec<Datum>, Error>>()?;

        let sample_period = extract_sample_period(&data);
        if sample_period < self.minimum_sample_period {
            return Err(Error::SamplePeriodTooSmall {
                minimum: self.minimum_sample_period,
                actual: sample_period,
            });
        }

        let this = Arc::clone(self);
        self.scope.spawn(this.batch_processor(data, sample_period, sink));
        Ok(())
    }

    async fn read_inspect(
        self: &Arc<Self>,
        data: Vec<Selector>,
        config: PerformanceConfig,
    ) -> Vec<InspectData> {
        // TODO: b/448187491 - Set up real instrumentation.
        let stats = Arc::new(AccessorStats::new(Node::default()).new_inspect_batch_iterator());
        let data = Some(data);
        std::pin::pin!(ReaderServer::stream(
            self.repo.fetch_inspect_data(&data, UNFILTERED_ALLOWLIST),
            config,
            // `None` here means that all available data from the first parameter
            // gets streamed and collected. It gets trimmed down again based on the
            // cache later.
            None,
            stats,
            self.trace_id,
        ))
        .collect()
        .await
    }

    async fn batch_processor(
        self: Arc<Self>,
        data: Vec<Datum>,
        sample_period: MonotonicDuration,
        sink: SampleSinkProxy,
    ) {
        let mut ticks: i64 = 0;
        // This cache holds the most recently sampled value for data in `data` with
        // a SampleStrategy of OnDiff, keyed per (datum, moniker, tree name).
        let mut on_diff_cache = OnDiffCache::new();
        let mut interval = Interval::new(sample_period).fuse();

        let mut client_events = sink.take_event_stream().fuse();
        let mut last_gasp: Option<(oneshot::Sender<_>, oneshot::Receiver<_>)> = None;

        loop {
            let current_event = sample_period * ticks;
            let batch: Vec<_> = data
                .iter()
                .filter(|datum| {
                    last_gasp.is_some() || (current_event.into_seconds() % datum.interval_secs) == 0
                })
                .collect();

            let (last_gasp_tx, last_gasp_rx) =
                last_gasp.take().map(|(tx, rx)| (Some(tx), Some(rx))).unwrap_or((None, None));

            if let Some(filtered_snapshot) = self.take_sample(batch, &mut on_diff_cache).await {
                let (client_end, request_stream) =
                    fidl::endpoints::create_request_stream::<BatchIteratorMarker>();
                if sink
                    .on_sample_readied(SampleSinkResult::Ready(SampleReady {
                        batch_iter: Some(client_end),
                        seconds_since_start: Some(current_event.into_seconds()),
                        ..Default::default()
                    }))
                    .is_err()
                {
                    // The client is gone, so we are done.
                    return;
                }

                let trace_id = self.trace_id;
                let sink = sink.clone();
                self.scope.spawn(async move {
                    if let Err(error) =
                        serve_iterator(filtered_snapshot, request_stream, trace_id).await
                    {
                        warn!(error:?; "Sample server error");
                        // I believe an error here means the control handle is a dud/shutdown.
                        // That could mean simply that the client doesn't care about hearing
                        // runtime errors, so we don't need to do anything special with the
                        // error from control_handle.
                        let _ = sink.on_sample_readied(SampleSinkResult::Error(
                            RuntimeError::BatchIteratorFailed,
                        ));
                    }

                    last_gasp_tx.map(|tx: oneshot::Sender<()>| tx.send(()));
                });
            }

            // Shutdown the server after a last gasp.
            if let Some(rx) = last_gasp_rx {
                let _ = rx.await;
                return;
            }

            select! {
                _ = interval.next() => {
                    ticks += 1;
                }
                client_event = client_events.next() => match client_event {
                    Some(Ok(SampleSinkEvent::OnNowOrNever { .. })) => {
                        last_gasp = Some(oneshot::channel());
                        continue;
                    }
                    Some(Err(err)) => warn!(
                        err:?;
                        "Received error on SampleSink event stream, ignoring"
                    ),
                    // `None` should imply that the client dropped the sample sink handle,
                    // which means we will shut down on the next iteration.
                    // An unknown interaction we can just ignore and continue servicing
                    // our `SampleSink`.
                    None | Some(Ok(SampleSinkEvent::_UnknownEvent { .. })) => continue,
                }
            }
        }
    }

    async fn take_sample(
        self: &Arc<Self>,
        batch: Vec<&Datum>,
        on_diff_cache: &mut OnDiffCache,
    ) -> Option<Vec<InspectData>> {
        if batch.is_empty() {
            return None;
        }

        let performance_config = PerformanceConfig::new(
            &StreamParameters {
                data_type: Some(DataType::Inspect),
                stream_mode: Some(StreamMode::Snapshot),
                format: Some(Format::Cbor),
                ..Default::default()
            },
            10,
            BatchRetrievalTimeout::from_seconds(10),
        )
        // Safety: this is not taking any input from clients, so it should always
        // be correct if it doesn't error in tests, which it does not.
        .unwrap();

        let selectors_to_read =
            batch.iter().map(|&datum| datum.selector.clone()).collect::<Vec<_>>();
        let snapshot = self.read_inspect(selectors_to_read, performance_config).await;

        let filtered_snapshot = snapshot
            .into_iter()
            .filter_map(|inspect_data| filter_inspect_data(inspect_data, &batch, on_diff_cache))
            .collect::<Vec<_>>();

        if filtered_snapshot.is_empty() { None } else { Some(filtered_snapshot) }
    }
}

/// Trims `inspect_data` down to the data selected by the members of `batch` that apply to it.
///
/// A datum applies to `inspect_data` if its selector matches both the moniker and the tree name,
/// and, for `SampleStrategy::OnDiff`, if the value it selects from this particular tree has
/// changed since the last time it was sampled. Returns `None` if no datum applies.
fn filter_inspect_data(
    mut inspect_data: InspectData,
    batch: &[&Datum],
    on_diff_cache: &mut OnDiffCache,
) -> Option<InspectData> {
    let payload = inspect_data.payload.take()?;

    let active_tree_selectors = batch
        .iter()
        .filter(|datum| datum.matches(&inspect_data))
        .filter(|datum| match datum.strategy {
            SampleStrategy::OnDiff => datum.on_diff_changed(&inspect_data, &payload, on_diff_cache),
            _ => true,
        })
        .filter_map(|datum| datum.selector.tree_selector.as_ref())
        .collect::<Vec<_>>();

    if active_tree_selectors.is_empty() {
        return None;
    }

    let filtered = filter_tree(payload, active_tree_selectors)?;
    inspect_data.payload = Some(filtered);
    Some(inspect_data)
}

async fn serve_iterator(
    hierarchy: Vec<InspectData>,
    stream: BatchIteratorRequestStream,
    trace_id: ftrace::Id,
) -> Result<(), AccessorError> {
    // TODO: b/448187491 - Set up real instrumentation.
    let stats = Arc::new(AccessorStats::new(Node::default()).new_inspect_batch_iterator());
    BatchIterator::new(
        futures::stream::iter(hierarchy),
        stream.peekable(),
        StreamMode::Snapshot,
        stats,
        None,
        trace_id,
        // This has to become JSON if we stabilize this protocol before CBOR is stabilized.
        // For now, Sample is only available at HEAD.
        Format::Cbor,
    )?
    .run()
    .await?;

    Ok(())
}

fn validate_parameters(params: &SampleParameters) -> Result<(), Error> {
    let data = params.data.as_ref().ok_or(Error::DataCannotBeEmpty)?;
    if data.is_empty() {
        return Err(Error::DataCannotBeEmpty);
    }
    for datum in data {
        if datum.selector.is_none() {
            return Err(Error::DatumMissingSelectors);
        }
        if datum.interval_secs.is_none() {
            return Err(Error::DatumMissingInterval);
        }
    }
    Ok(())
}

/// Returns the GCD of the intervals of the data item.
fn extract_sample_period(data: &[Datum]) -> MonotonicDuration {
    MonotonicDuration::from_seconds(
        // use the euclidean algorithm to compute GCD
        data.iter()
            .map(|d| d.interval_secs)
            .reduce(|mut a, mut b| {
                while b != 0 {
                    let tmp = b;
                    b = a % b;
                    a = tmp;
                }
                a
            })
            .expect("the laws of mathematics to hold (at least gcd of 1)"),
    )
}

type DataId = usize;

/// Identifies one sampled value for the `OnDiff` strategy. A single `Datum` whose
/// selector contains wildcards can match many (moniker, tree) pairs; each pair's
/// value must be tracked independently so that one tree's (lack of) change can
/// never suppress another tree's output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OnDiffCacheKey {
    id: DataId,
    moniker: ExtendedMoniker,
    tree_name: InspectHandleName,
}

type OnDiffCache = HashMap<OnDiffCacheKey, SelectResult<'static, String>>;

struct Datum {
    strategy: SampleStrategy,
    selector: Selector,
    interval_secs: i64,
    id: DataId,
}

impl Datum {
    /// Returns true if this datum's selector applies to `inspect_data`'s moniker and tree name.
    fn matches(&self, inspect_data: &InspectData) -> bool {
        // Selectors were validated in `Datum::try_from`, so `Err` is unreachable in practice;
        // treat it as a non-match.
        if !inspect_data.moniker.matches_selector(&self.selector).unwrap_or(false) {
            return false;
        }
        match &inspect_data.metadata.name {
            InspectHandleName::Name(name) => {
                selectors::match_tree_name_against_selector(name.as_str(), &self.selector)
            }
            InspectHandleName::Filename(_) => {
                matches!(self.selector.tree_names.as_ref(), None | Some(TreeNames::All(_)))
            }
        }
    }

    /// Returns true if the value this datum selects from `payload` differs from the value
    /// last observed for the same (datum, moniker, tree name), updating the cache as needed.
    fn on_diff_changed(
        &self,
        inspect_data: &InspectData,
        payload: &DiagnosticsHierarchy,
        on_diff_cache: &mut OnDiffCache,
    ) -> bool {
        let current = match select_from_hierarchy(payload, &self.selector) {
            Ok(current) => current,
            Err(error) => {
                warn!(error:?; "select_from_hierarchy failed for validated selector");
                return true;
            }
        };

        // A selector that matches nothing in this tree carries no information about change.
        // It must neither seed nor be compared against the cache; otherwise a wildcard moniker
        // selector (e.g. `bootstrap/*-drivers:*`) that happens to match other components'
        // trees would cache an empty result and suppress the real tree's data
        // (b/571622551, b/516487002).
        let is_empty = match &current {
            SelectResult::Properties(p) => p.is_empty(),
            SelectResult::Nodes(n) => n.is_empty(),
        };
        if is_empty {
            return false;
        }

        let key = OnDiffCacheKey {
            id: self.id,
            moniker: inspect_data.moniker.clone(),
            tree_name: inspect_data.metadata.name.clone(),
        };
        match on_diff_cache.entry(key) {
            // indicates the value has not updated
            Entry::Occupied(cached) if cached.get() == &current => false,
            // indicates the value has changed
            Entry::Occupied(mut cached) => {
                *cached.get_mut() = current.into_owned();
                true
            }
            Entry::Vacant(vacant) => {
                vacant.insert(current.into_owned());
                true
            }
        }
    }
}

impl TryFrom<(usize, SampleDatum)> for Datum {
    type Error = Error;

    fn try_from((id, data): (usize, SampleDatum)) -> Result<Datum, Error> {
        let selector = match data.selector {
            Some(SelectorArgument::StructuredSelector(s)) => s,
            Some(SelectorArgument::RawSelector(s)) => parse_selector::<FastError>(&s)?,
            None => unreachable!("selector validated before"),
            _ => unimplemented!("unknown selector variants not supported"),
        };

        selector.validate()?;

        Ok(Datum {
            interval_secs: data.interval_secs.expect("already validated"),
            strategy: data.strategy.unwrap_or(SampleStrategy::Always),
            selector,
            id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::*;
    use crate::inspect::container::InspectHandle;
    use crate::pipeline::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::*;
    use diagnostics_data::*;
    use diagnostics_reader::*;
    use fidl::endpoints::*;
    use fidl_fuchsia_diagnostics::*;
    use fidl_fuchsia_inspect::*;
    use fuchsia_async::*;
    use fuchsia_inspect::{Property, *};
    use futures::*;
    use inspect_runtime::service::*;
    use inspect_runtime::*;
    use selectors::*;
    use std::sync::LazyLock;
    use std::task::Poll;
    use zx;

    impl SampleServer {
        fn spawn(self: &Arc<Self>, stream: SampleRequestStream) {
            let this = Arc::clone(self);
            self.scope.spawn(async move {
                this.sample_request_handler(stream).await;
            });
        }
    }

    const TEST_URL: &str = "fuchsia-pkg://test";
    static MONIKER: LazyLock<ExtendedMoniker> =
        LazyLock::new(|| ExtendedMoniker::parse_str("./a/b/foo").unwrap());

    #[track_caller]
    fn permissive_pipeline(scope: &ScopeHandle) -> (Arc<Pipeline>, Arc<InspectRepository>) {
        let pipeline = Arc::new(Pipeline::for_test(Some(vec![
            selectors::parse_verbose("**:[...]*:*").unwrap(),
        ])));
        let inspect_repo =
            Arc::new(InspectRepository::new(vec![Arc::downgrade(&pipeline)], scope.new_child()));

        (pipeline, inspect_repo)
    }

    fn spawn_test_inspector(scope: &ScopeHandle) -> (TreeProxy, Inspector) {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<TreeMarker>();
        let inspector = Inspector::default();
        inspector.root().record_int("hello", 0);
        spawn_tree_server_with_stream(
            inspector.clone(),
            TreeServerSendPreference::default(),
            stream,
            scope,
        );
        (proxy, inspector)
    }

    // recall that serialization happens here, so positive integers will flip to uint
    // values in the below tests
    async fn drain_batch(batch_iter: ClientEnd<BatchIteratorMarker>) -> Vec<InspectData> {
        drain_batch_iterator::<InspectData>(Arc::new(batch_iter.into_proxy()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    async fn extract(
        sample_sink_server: &mut SampleSinkRequestStream,
    ) -> (ClientEnd<BatchIteratorMarker>, i64) {
        let Some(Ok(SampleSinkRequest::OnSampleReadied {
            event:
                SampleSinkResult::Ready(SampleReady {
                    batch_iter: Some(batch_iter),
                    seconds_since_start: Some(seconds_since_start),
                    ..
                }),
            ..
        })) = sample_sink_server.next().await
        else {
            panic!("Malformed SampleSinkResult")
        };

        (batch_iter, seconds_since_start)
    }

    /// Polls `fut` until the executor stalls, panicking if it did not complete.
    #[track_caller]
    fn run_ready<F: Future>(exec: &mut TestExecutor, fut: F) -> F::Output {
        match exec.run_until_stalled(&mut std::pin::pin!(fut)) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("future did not complete before the executor stalled"),
        }
    }

    /// Advances fake time by `secs` and wakes any timers that expired.
    fn advance(exec: &mut TestExecutor, secs: i64) {
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(secs)));
        exec.wake_expired_timers();
    }

    /// Adds an inspector for `moniker` publishing a tree called `name` (or the default
    /// tree when `None`).
    fn add_test_inspector(
        scope: &ScopeHandle,
        inspect_repo: &Arc<InspectRepository>,
        moniker: &str,
        name: Option<&str>,
    ) -> Inspector {
        let moniker = ExtendedMoniker::parse_str(moniker).unwrap();
        let identity = Arc::new(ComponentIdentity::new(moniker, TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(identity, InspectHandle::tree(proxy, name));
        inspector
    }

    fn on_diff_datum(selector: &str, interval_secs: i64) -> SampleDatum {
        SampleDatum {
            selector: Some(SelectorArgument::RawSelector(selector.to_string())),
            strategy: Some(SampleStrategy::OnDiff),
            interval_secs: Some(interval_secs),
            ..Default::default()
        }
    }

    /// Commits `data` to a new `SampleServer` and returns the server along with the
    /// stream of sample events.
    fn start_sampling(
        exec: &mut TestExecutor,
        inspect_repo: &Arc<InspectRepository>,
        data: Vec<SampleDatum>,
    ) -> (Arc<SampleServer>, SampleSinkRequestStream) {
        let sampler = Arc::new(SampleServer::new(
            Arc::clone(inspect_repo),
            MonotonicDuration::from_seconds(60),
            exec.global_scope().new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();
        sample_proxy.set(&SampleParameters { data: Some(data), ..Default::default() }).unwrap();
        run_ready(exec, sample_proxy.commit(sample_sink_client)).unwrap().unwrap();

        (sampler, sample_sink_server.into_stream())
    }

    /// Waits for the next sample, asserts it was taken at `expected_runtime`, and returns its
    /// contents sorted by moniker and tree name.
    #[track_caller]
    fn next_sample(
        exec: &mut TestExecutor,
        sample_sink_server: &mut SampleSinkRequestStream,
        expected_runtime: i64,
    ) -> Vec<InspectData> {
        let (batch_iter, runtime) = run_ready(exec, extract(sample_sink_server));
        assert_eq!(runtime, expected_runtime);
        let mut actual = run_ready(exec, drain_batch(batch_iter));
        actual.sort_by_key(|data| (data.moniker.to_string(), data.name().to_string()));
        actual
    }

    /// Asserts that no sample is delivered before the executor stalls.
    #[track_caller]
    fn assert_no_sample(exec: &mut TestExecutor, sample_sink_server: &mut SampleSinkRequestStream) {
        // Dropping a pending `next()` future does not lose stream items.
        assert!(exec.run_until_stalled(&mut sample_sink_server.next()).is_pending());
    }

    #[fuchsia::test]
    async fn sample_server_bad_configuration() {
        let scope = Scope::new();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, _) = spawn_test_inspector(&scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, _) = fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![SampleDatum {
                    // missing selector
                    strategy: Some(SampleStrategy::OnDiff),
                    interval_secs: Some(300),
                    ..Default::default()
                }]),
                ..Default::default()
            })
            .unwrap();
        assert_matches!(
            sample_proxy.commit(sample_sink_client).await,
            Ok(Err(ConfigurationError::SampleParametersInvalid))
        );

        let (sample_sink_client, _) = fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![SampleDatum {
                    selector: Some(SelectorArgument::RawSelector("::::".to_string())),
                    strategy: Some(SampleStrategy::OnDiff),
                    interval_secs: Some(300),
                    ..Default::default()
                }]),
                ..Default::default()
            })
            .unwrap();
        assert_matches!(
            sample_proxy.commit(sample_sink_client).await,
            Ok(Err(ConfigurationError::InvalidSelectors))
        );

        let (sample_sink_client, _) = fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![
                    SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("*:*:root".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(60),
                        ..Default::default()
                    },
                    SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("*:*:root".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(61),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            })
            .unwrap();
        assert_matches!(
            sample_proxy.commit(sample_sink_client).await,
            Ok(Err(ConfigurationError::SamplePeriodTooSmall))
        );
    }

    #[fuchsia::test]
    fn sample_server_on_diff_with_changes_node_selector() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let prop = inspector.root().create_int("foo", 0);

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                hello: 0u64,
                foo: 0u64,
            });

            prop.set(5);

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 300);

            let actual = drain_batch(batch_iter).await;

            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                hello: 0u64,
                foo: 5u64,
            });
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Ready(()), exec.run_until_stalled(&mut fut));
    }

    #[fuchsia::test]
    fn sample_server_on_diff_without_changes_node_selector() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, _) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { hello: 0u64 }})
                        .build()
                ]
            );

            // this should never complete
            let _ = sample_sink_server.next().await;
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(310)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Pending, exec.run_until_stalled(&mut fut));
    }

    #[fuchsia::test]
    fn sample_server_mixed_strategies() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let foo = inspector.root().create_int("foo", 0);
        let bar = inspector.root().create_string("bar", "baz");

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![
                        SampleDatum {
                            selector: Some(SelectorArgument::RawSelector(
                                "**:root:foo".to_string(),
                            )),
                            strategy: Some(SampleStrategy::Always),
                            interval_secs: Some(300),
                            ..Default::default()
                        },
                        SampleDatum {
                            selector: Some(SelectorArgument::RawSelector(
                                "**:root:bar".to_string(),
                            )),
                            strategy: Some(SampleStrategy::OnDiff),
                            interval_secs: Some(300),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                foo: 0u64,
                bar: "baz",
            });

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 300);

            let actual = drain_batch(batch_iter).await;
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                foo: 0u64,
            });

            foo.set(10);

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 600);

            let actual = drain_batch(batch_iter).await;
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                foo: 10u64,
            });

            bar.set("foobarbaz");

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 900);

            let actual = drain_batch(batch_iter).await;
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].moniker, *MONIKER);
            assert_eq!(actual[0].metadata.component_url, TEST_URL);
            assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: {
                foo: 10u64,
                bar: "foobarbaz",
            });
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Ready(()), exec.run_until_stalled(&mut fut));
    }

    #[fuchsia::test]
    fn sample_server_on_diff_with_changes() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let prop = inspector.root().create_int("foo", 0);

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:foo".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { foo: 0u64 }})
                        .build()
                ]
            );

            prop.set(5);

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 300);

            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { foo: 5u64 }})
                        .build()
                ]
            );
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Ready(()), exec.run_until_stalled(&mut fut));
    }

    #[fuchsia::test]
    fn sample_server_on_diff_without_changes() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, _) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:hello".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();
            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { hello: 0u64 }})
                        .build()
                ]
            );

            // this should never complete
            let _ = sample_sink_server.next().await;
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(310)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Pending, exec.run_until_stalled(&mut fut));
    }

    // A regression test for b/503719785.
    //
    // Ensure that inspect data with the wrong moniker doesn't influence the
    // on_diff cache.
    #[fuchsia::test]
    fn sample_server_on_diff_with_results_from_multiple_monikers() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);

        // Create 3 inspectors. The last inspector is the one we'll actually
        // select against.
        struct InspectorState {
            inspector: Inspector,
            moniker: ExtendedMoniker,
            url: String,
        }
        let inspectors = (0..3)
            .map(|i| {
                let moniker = ExtendedMoniker::parse_str(format!("./a/b/foo{i}").as_str()).unwrap();
                let url = format!("fuchsia-package://test-url{i}");
                let identity = Arc::new(ComponentIdentity::new(moniker.clone(), url.as_str()));
                let (proxy, inspector) = spawn_test_inspector(scope);
                inspect_repo.add_inspect_handle(
                    Arc::clone(&identity),
                    InspectHandle::tree(proxy, Option::<String>::None),
                );
                InspectorState { inspector, moniker, url }
            })
            .collect::<Vec<_>>();

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        // Only set the expected property on the third inspector.
        let last_inspector = inspectors.last().unwrap();
        let prop = last_inspector.inspector.root().create_int("foo", 0);

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector(format!(
                            "{}:root:foo",
                            last_inspector.moniker
                        ))),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();
            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(
                        last_inspector.moniker.clone(),
                        last_inspector.url.as_str(),
                        actual_ts
                    )
                    .with_hierarchy(hierarchy! { root: { foo: 0u64 }})
                    .build()
                ]
            );

            prop.set(5);

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 300);

            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(
                        last_inspector.moniker.clone(),
                        last_inspector.url.as_str(),
                        actual_ts
                    )
                    .with_hierarchy(hierarchy! { root: { foo: 5u64 }})
                    .build()
                ]
            );
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Ready(()), exec.run_until_stalled(&mut fut));
    }

    // A regression test for b/571622551 and b/516487002.
    //
    // A wildcard moniker selector matches several components, only one of which
    // has the selected property. The components without the property must not
    // seed or poison the on_diff cache for the component that has it.
    #[fuchsia::test]
    fn sample_server_on_diff_wildcard_moniker_not_suppressed_by_non_matching_trees() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let inspectors = (0..4)
            .map(|i| add_test_inspector(&scope, &inspect_repo, &format!("./a/b/foo{i}"), None))
            .collect::<Vec<_>>();
        // Use an inspector in the middle so that trees without the property are
        // processed both before and after it.
        let prop = inspectors[2].root().create_int("foo", 0);
        let expected_moniker = ExtendedMoniker::parse_str("./a/b/foo2").unwrap();

        let (_sampler, mut sample_sink_server) =
            start_sampling(&mut exec, &inspect_repo, vec![on_diff_datum("a/b/foo*:root:foo", 300)]);

        // recall that serialization happens here, so positive integers will flip to uint
        // values in the below tests
        let actual = next_sample(&mut exec, &mut sample_sink_server, 0);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, expected_moniker);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        prop.set(5);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 300);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, expected_moniker);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 5u64 } }));

        // No change, so nothing is delivered.
        advance(&mut exec, 300);
        assert_no_sample(&mut exec, &mut sample_sink_server);
    }

    // When a wildcard moniker selector matches several components that each have
    // the selected property, each component's value is tracked independently.
    #[fuchsia::test]
    fn sample_server_on_diff_wildcard_moniker_tracks_each_moniker_independently() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let inspector0 = add_test_inspector(&scope, &inspect_repo, "./a/b/foo0", None);
        let inspector1 = add_test_inspector(&scope, &inspect_repo, "./a/b/foo1", None);
        let moniker0 = ExtendedMoniker::parse_str("./a/b/foo0").unwrap();
        let moniker1 = ExtendedMoniker::parse_str("./a/b/foo1").unwrap();
        // Both start with the same value, which must not cause the second to be
        // treated as "unchanged" relative to the first.
        let prop0 = inspector0.root().create_int("foo", 0);
        let prop1 = inspector1.root().create_int("foo", 0);

        let (_sampler, mut sample_sink_server) =
            start_sampling(&mut exec, &inspect_repo, vec![on_diff_datum("a/b/foo*:root:foo", 300)]);

        let actual = next_sample(&mut exec, &mut sample_sink_server, 0);
        assert_eq!(actual.len(), 2);
        assert_eq!(actual[0].moniker, moniker0);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));
        assert_eq!(actual[1].moniker, moniker1);
        assert_eq!(actual[1].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        prop1.set(1);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 300);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, moniker1);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 1u64 } }));

        prop0.set(2);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 600);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, moniker0);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 2u64 } }));
    }

    // A selector that names a specific tree must only sample, and only track
    // changes in, that tree.
    #[fuchsia::test]
    fn sample_server_on_diff_respects_tree_name_filter() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let alpha = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", Some("alpha"));
        let beta = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", Some("beta"));
        let alpha_prop = alpha.root().create_int("foo", 0);
        let beta_prop = beta.root().create_int("foo", 0);

        let (_sampler, mut sample_sink_server) = start_sampling(
            &mut exec,
            &inspect_repo,
            vec![on_diff_datum("a/b/foo:[name=alpha]root:foo", 300)],
        );

        let actual = next_sample(&mut exec, &mut sample_sink_server, 0);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].metadata.name, InspectHandleName::name("alpha"));
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        // A change in a tree that isn't selected is not delivered.
        beta_prop.set(5);
        advance(&mut exec, 300);
        assert_no_sample(&mut exec, &mut sample_sink_server);

        alpha_prop.set(7);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 600);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].metadata.name, InspectHandleName::name("alpha"));
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 7u64 } }));
    }

    // When a selector matches multiple trees on the same component (`[...]` tree name
    // wildcard), each tree's value is tracked independently in the OnDiff cache.
    #[fuchsia::test]
    fn sample_server_on_diff_wildcard_tree_names_tracks_each_tree_independently() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let alpha = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", Some("alpha"));
        let beta = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", Some("beta"));
        // Both trees start with the same value on the same moniker; the second tree
        // must not be suppressed as "unchanged" relative to the first.
        let _alpha_prop = alpha.root().create_int("foo", 0);
        let beta_prop = beta.root().create_int("foo", 0);

        let (_sampler, mut sample_sink_server) = start_sampling(
            &mut exec,
            &inspect_repo,
            vec![on_diff_datum("a/b/foo:[...]root:foo", 300)],
        );

        let actual = next_sample(&mut exec, &mut sample_sink_server, 0);
        assert_eq!(actual.len(), 2);
        assert_eq!(actual[0].metadata.name, InspectHandleName::name("alpha"));
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));
        assert_eq!(actual[1].metadata.name, InspectHandleName::name("beta"));
        assert_eq!(actual[1].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        beta_prop.set(5);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 300);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].metadata.name, InspectHandleName::name("beta"));
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 5u64 } }));
    }

    // Two data with an identical selector but different intervals each track their
    // own last-seen value; one being unchanged must not suppress the other.
    #[fuchsia::test]
    fn sample_server_on_diff_same_tree_selector_different_datums_do_not_suppress_each_other() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let inspector = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", None);
        let prop = inspector.root().create_int("foo", 0);

        let (_sampler, mut sample_sink_server) = start_sampling(
            &mut exec,
            &inspect_repo,
            vec![on_diff_datum("**:root:foo", 300), on_diff_datum("**:root:foo", 600)],
        );

        let actual = next_sample(&mut exec, &mut sample_sink_server, 0);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        // Only the 300s datum runs here, and it sees the change.
        prop.set(1);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 300);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 1u64 } }));

        // Both data run here. The 300s datum saw `1` last time and is unchanged, but
        // the 600s datum last saw `0`, so the value must be delivered.
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 600);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 1u64 } }));
    }

    // A selector that matches nothing must not seed the cache, so the first real
    // value is delivered and subsequently deduplicated.
    #[fuchsia::test]
    fn sample_server_on_diff_empty_match_does_not_seed_cache() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope().clone();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let inspector = add_test_inspector(&scope, &inspect_repo, "./a/b/foo", None);

        let (_sampler, mut sample_sink_server) =
            start_sampling(&mut exec, &inspect_repo, vec![on_diff_datum("**:root:foo", 300)]);

        // `foo` doesn't exist yet.
        assert_no_sample(&mut exec, &mut sample_sink_server);

        let _prop = inspector.root().create_int("foo", 0);
        advance(&mut exec, 300);
        let actual = next_sample(&mut exec, &mut sample_sink_server, 300);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].payload, Some(hierarchy! { root: { foo: 0u64 } }));

        advance(&mut exec, 300);
        assert_no_sample(&mut exec, &mut sample_sink_server);
    }

    #[fuchsia::test]
    async fn on_now_or_never() {
        let scope = Scope::new();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(&scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            // minimum duration should be ignored for OnNowOrNever
            MonotonicDuration::from_seconds(120),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![
                    SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:a".to_string())),
                        strategy: Some(SampleStrategy::Always),
                        interval_secs: Some(300),
                        ..Default::default()
                    },
                    SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:b".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    },
                    SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:c".to_string())),
                        strategy: Some(SampleStrategy::OnDiff),
                        interval_secs: Some(300),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            })
            .unwrap();

        sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

        // will never change, Always strategy
        inspector.root().record_int("a", 0);
        // will never change, OnDiff. Only observed once.
        inspector.root().record_int("b", 5);
        // will change, on diff strategy
        let c = inspector.root().create_int("c", 100);

        let mut sample_sink_server = sample_sink_server.into_stream();

        let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
        assert_eq!(runtime, 0);

        // recall that serialization happens here, so positive integers will flip to uint
        // values in the below tests
        let actual = drain_batch(batch_iter).await;

        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].metadata.component_url, TEST_URL);
        assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: contains {
            a: 0u64,
            b: 5u64,
            c: 100u64,
        });

        // shouldn't be picked up for 300 more seconds, except by OnNowOrNever
        c.set(10);

        sample_sink_server.control_handle().send_on_now_or_never().unwrap();

        let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
        assert_eq!(runtime, 0);

        let actual = drain_batch(batch_iter).await;

        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].metadata.component_url, TEST_URL);
        assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: contains {
            a: 0u64,
            c: 10u64,
        });
    }

    #[fuchsia::test]
    async fn sample_server_multiple_set() {
        let scope = Scope::new();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, inspector) = spawn_test_inspector(&scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![SampleDatum {
                    selector: Some(SelectorArgument::RawSelector("**:root:a".to_string())),
                    strategy: Some(SampleStrategy::Always),
                    interval_secs: Some(300),
                    ..Default::default()
                }]),
                ..Default::default()
            })
            .unwrap();

        sample_proxy
            .set(&SampleParameters {
                data: Some(vec![SampleDatum {
                    selector: Some(SelectorArgument::RawSelector("**:root:b".to_string())),
                    strategy: Some(SampleStrategy::Always),
                    interval_secs: Some(300),
                    ..Default::default()
                }]),
                ..Default::default()
            })
            .unwrap();

        sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

        inspector.root().record_int("a", 0);
        inspector.root().record_int("b", 1);

        let mut sample_sink_server = sample_sink_server.into_stream();

        let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
        assert_eq!(runtime, 0);

        // recall that serialization happens here, so positive integers will flip to uint
        // values in the below tests
        let actual = drain_batch(batch_iter).await;

        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].moniker, *MONIKER);
        assert_eq!(actual[0].metadata.component_url, TEST_URL);
        assert_data_tree!(actual[0].payload.as_ref().unwrap(), root: contains {
            a: 0u64,
            b: 1u64,
        });
    }

    #[fuchsia::test]
    fn sample_server_simple_always() {
        let mut exec = TestExecutor::new_with_fake_time();
        let scope = exec.global_scope();
        let (_pipeline, inspect_repo) = permissive_pipeline(scope);
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        let (proxy, _) = spawn_test_inspector(scope);
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));
        let (sample_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SampleMarker>();
        sampler.spawn(stream);

        let (sample_sink_client, sample_sink_server) =
            fidl::endpoints::create_endpoints::<SampleSinkMarker>();

        let mut fut = async move {
            sample_proxy
                .set(&SampleParameters {
                    data: Some(vec![SampleDatum {
                        selector: Some(SelectorArgument::RawSelector("**:root:hello".to_string())),
                        strategy: Some(SampleStrategy::Always),
                        interval_secs: Some(300),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .unwrap();

            sample_proxy.commit(sample_sink_client).await.unwrap().unwrap();

            let mut sample_sink_server = sample_sink_server.into_stream();

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 0);

            // recall that serialization happens here, so positive integers will flip to uint
            // values in the below tests
            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { hello: 0u64 }})
                        .build()
                ]
            );

            let (batch_iter, runtime) = extract(&mut sample_sink_server).await;
            assert_eq!(runtime, 300);

            let actual = drain_batch(batch_iter).await;

            let actual_ts = actual[0].metadata.timestamp;
            assert_eq!(
                actual,
                vec![
                    InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                        .with_hierarchy(hierarchy! { root: { hello: 0u64 }})
                        .build()
                ]
            );
        }
        .boxed();

        let _ = exec.run_until_stalled(&mut fut);
        exec.set_fake_time(MonotonicInstant::after(zx::MonotonicDuration::from_seconds(300)));
        exec.wake_expired_timers();
        assert_eq!(Poll::Ready(()), exec.run_until_stalled(&mut fut));
    }

    #[fuchsia::test]
    async fn read_inspect_test() {
        let scope = Scope::new();
        let (_pipeline, inspect_repo) = permissive_pipeline(&scope);

        let sampler = Arc::new(SampleServer::new(
            Arc::clone(&inspect_repo),
            MonotonicDuration::from_seconds(60),
            scope.new_child(),
        ));

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<TreeMarker>();
        let identity = Arc::new(ComponentIdentity::new(MONIKER.clone(), TEST_URL));
        inspect_repo.add_inspect_handle(
            Arc::clone(&identity),
            InspectHandle::tree(proxy, Option::<String>::None),
        );

        let inspector = Inspector::default();
        inspector.root().record_int("hello", 0);
        spawn_tree_server_with_stream(
            inspector,
            TreeServerSendPreference::default(),
            stream,
            &scope,
        );

        let config = PerformanceConfig {
            aggregated_content_limit_bytes: None,
            batch_timeout_sec: 100,
            maximum_concurrent_snapshots_per_reader: 100,
        };

        let actual = sampler
            .read_inspect(vec![selectors::parse_verbose("**:[...]*:*").unwrap()], config)
            .await;
        let actual_ts = actual[0].metadata.timestamp;
        assert_eq!(
            actual,
            vec![
                InspectDataBuilder::new(MONIKER.clone(), TEST_URL, actual_ts)
                    .with_hierarchy(hierarchy! { root: { hello: 0 }})
                    .build()
            ]
        );
    }

    #[fuchsia::test]
    fn test_sample_period() {
        let data = &[
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 300,
                id: 0,
            },
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 300,
                id: 1,
            },
        ];

        assert_eq!(extract_sample_period(data), MonotonicDuration::from_seconds(300));

        let data = &[
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 300,
                id: 0,
            },
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 50,
                id: 1,
            },
        ];

        assert_eq!(extract_sample_period(data), MonotonicDuration::from_seconds(50));

        let data = &[
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 300,
                id: 0,
            },
            Datum {
                strategy: SampleStrategy::OnDiff,
                selector: parse_verbose("**:*:*").unwrap(),
                interval_secs: 350,
                id: 1,
            },
        ];

        assert_eq!(extract_sample_period(data), MonotonicDuration::from_seconds(50));
    }
}
