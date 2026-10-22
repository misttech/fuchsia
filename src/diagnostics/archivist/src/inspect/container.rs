// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::diagnostics::{GlobalConnectionStats, TRACE_CATEGORY};
use crate::identity::ComponentIdentity;
use crate::inspect::collector::{self as collector, InspectData};
use crate::pipeline::ComponentAllowlist;
use diagnostics_data::{self as schema, InspectHandleName};
use fidl::endpoints::Proxy;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_inspect as finspect;
use flyweights::FlyStr;
use fuchsia_async as fasync;
use fuchsia_inspect::UintProperty;
use fuchsia_inspect::reader::snapshot::{Snapshot, SnapshotTree};
use fuchsia_trace as ftrace;
use futures::Stream;
use futures::channel::oneshot;
use selectors::SelectorExt;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;
use zx::{self as zx, AsHandleRef};

#[derive(Debug)]
pub enum InspectHandle {
    Tree {
        proxy: finspect::TreeProxy,
        name: Option<FlyStr>,
    },
    Escrow {
        vmo: Arc<zx::Vmo>,
        name: Option<FlyStr>,
        token: finspect::EscrowToken,
        related_koid: zx::Koid,
    },
}

impl InspectHandle {
    fn is_closed(&self) -> bool {
        match self {
            Self::Tree { proxy, .. } => proxy.as_channel().is_closed(),
            Self::Escrow { .. } => false,
        }
    }

    async fn on_closed(&self) -> Result<zx::Signals, zx::Status> {
        match self {
            Self::Tree { proxy, .. } => proxy.on_closed().await,
            Self::Escrow { token, .. } => {
                fasync::OnSignals::new(&token.token, fidl::Signals::OBJECT_PEER_CLOSED).await
            }
        }
    }

    pub fn koid(&self) -> zx::Koid {
        match self {
            Self::Tree { proxy, .. } => {
                proxy.as_channel().as_handle_ref().koid().expect("TreeProxy has koid")
            }
            // We return the related koid to index based on it so that the retrieval is more
            // efficient.
            Self::Escrow { related_koid, .. } => *related_koid,
        }
    }

    pub fn tree<T: Into<FlyStr>>(proxy: finspect::TreeProxy, name: Option<T>) -> Self {
        InspectHandle::Tree { proxy, name: name.map(|n| n.into()) }
    }

    pub fn escrow<T: Into<FlyStr>>(
        vmo: zx::Vmo,
        token: finspect::EscrowToken,
        name: Option<T>,
    ) -> Self {
        let related_koid = token.token.basic_info().unwrap().related_koid;
        InspectHandle::Escrow {
            vmo: Arc::new(vmo),
            name: name.map(|n| n.into()),
            token,
            related_koid,
        }
    }
}

struct StoredInspectHandle {
    handle: Arc<InspectHandle>,
    // Whenever we delete the stored handle from the map, we can leverage dropping this oneshot to
    // trigger completion of the Task listening for related handle peer closed.
    _control: oneshot::Sender<()>,
}

#[derive(Default)]
pub struct InspectArtifactsContainer {
    /// One or more proxies that this container is configured for.
    inspect_handles: HashMap<zx::Koid, StoredInspectHandle>,
}

impl InspectArtifactsContainer {
    /// Remove a handle via its `koid` from the set of proxies managed by `self`.
    pub fn remove_handle(&mut self, koid: zx::Koid) -> (Option<Arc<InspectHandle>>, usize) {
        let stored = self.inspect_handles.remove(&koid);
        (stored.map(|stored| stored.handle), self.inspect_handles.len())
    }

    /// Push a new handle into the container.
    ///
    /// Returns a future that completes, after calling `on_closed`, once the handle's peer is
    /// closed or the handle is removed from the container.
    pub fn push_handle<T: FnOnce(zx::Koid)>(
        &mut self,
        handle: InspectHandle,
        on_closed: T,
    ) -> impl Future<Output = ()> + use<T> {
        let (control_snd, control_rcv) = oneshot::channel();
        let handle = Arc::new(handle);
        let stored = StoredInspectHandle { _control: control_snd, handle: Arc::clone(&handle) };
        let koid = handle.koid();
        self.inspect_handles.insert(koid, stored);
        async move {
            if !handle.is_closed() {
                let closed_fut = handle.on_closed();
                futures::pin_mut!(closed_fut);
                let _ = futures::future::select(closed_fut, control_rcv).await;
            }
            on_closed(koid);
        }
    }

    /// Generate an `UnpopulatedInspectDataContainer` from the proxies managed by `self`.
    ///
    /// Returns `None` if there are no valid proxies.
    pub fn create_unpopulated(
        &self,
        identity: &Arc<ComponentIdentity>,
        allowlist: ComponentAllowlist,
        dynamic_selectors: &Option<impl AsRef<[fdiagnostics::Selector]>>,
    ) -> Option<UnpopulatedInspectDataContainer> {
        if self.inspect_handles.is_empty() {
            return None;
        }

        if allowlist.all_filtered_out() {
            return None;
        }

        // Verify that dynamic selectors contain an entry that applies to this moniker
        let mut dynamic_selectors = dynamic_selectors.as_ref().map(|all_components| {
            all_components
                .as_ref()
                .iter()
                .filter(|s| matches!(identity.moniker.matches_selector(s), Ok(true)))
                .peekable()
        });

        if let Some(None) = dynamic_selectors.as_mut().map(|s| s.peek()) {
            return None;
        }

        Some(UnpopulatedInspectDataContainer {
            identity: Arc::clone(identity),
            component_allowlist: allowlist,
            inspect_handles: self
                .inspect_handles
                .values()
                .filter(|s| {
                    // dynamic_selectors.clone() should just be a clone of the iterator, so as to
                    // always start at the beginning
                    Self::name_filters_satisfied(s.handle.as_ref(), dynamic_selectors.clone())
                })
                .map(|s| Arc::downgrade(&s.handle))
                .collect::<Vec<_>>(),
        })
    }

    fn name_filters_satisfied<'a>(
        handle: &InspectHandle,
        mut dynamic_selectors: Option<impl Iterator<Item = &'a fdiagnostics::Selector>>,
    ) -> bool {
        let Some(ref mut selectors) = dynamic_selectors else {
            return true;
        };
        let (InspectHandle::Tree { name, .. } | InspectHandle::Escrow { name, .. }) = handle;
        selectors.any(|s| {
            let name = match name.as_ref() {
                Some(fly) => fly.as_str(),
                None => finspect::DEFAULT_TREE_NAME,
            };
            selectors::match_tree_name_against_selector(name, s)
        })
    }
}

#[cfg(test)]
impl InspectArtifactsContainer {
    pub(crate) fn handles(&self) -> impl ExactSizeIterator<Item = &InspectHandle> {
        self.inspect_handles.values().map(|stored| stored.handle.as_ref())
    }
}

#[derive(Debug)]
pub enum ReadSnapshot {
    Single(Snapshot),
    Tree(SnapshotTree),
}

/// Packet containing a snapshot and all the metadata needed to
/// populate a diagnostics schema for that snapshot.
#[derive(Debug)]
pub struct SnapshotData {
    /// Optional name of the file or InspectSink proxy that created this snapshot.
    pub name: Option<InspectHandleName>,
    /// Timestamp at which this snapshot resolved or failed.
    pub timestamp: zx::BootInstant,
    /// Errors encountered when processing this snapshot.
    pub errors: Vec<schema::InspectError>,
    /// Optional snapshot of the inspect hierarchy, in case reading fails
    /// and we have errors to share with client.
    pub snapshot: Option<ReadSnapshot>,
    /// Whether or not the data was escrowed.
    pub escrowed: bool,
}

impl SnapshotData {
    async fn new(
        name: Option<InspectHandleName>,
        data: InspectData,
        lazy_child_timeout: Duration,
        identity: &ComponentIdentity,
        parent_trace_id: ftrace::Id,
        timeout_counter: &UintProperty,
    ) -> SnapshotData {
        let trace_id = ftrace::Id::new();
        let _trace_guard = ftrace::async_enter!(
            trace_id,
            TRACE_CATEGORY,
            c"SnapshotData::new",
            // An async duration cannot have multiple concurrent child async durations
            // so we include the nonce as metadata to manually determine relationship.
            "parent_trace_id" => u64::from(parent_trace_id),
            "trace_id" => u64::from(trace_id),
            "moniker" => identity.to_string().as_ref(),
            "filename" => name
                    .as_ref()
                    .and_then(InspectHandleName::as_filename)
                    .unwrap_or(""),
            "name" => name
                    .as_ref()
                    .and_then(InspectHandleName::as_name)
                    .unwrap_or("")
        );
        match data {
            InspectData::Tree(tree) => {
                match SnapshotTree::try_from_with_timeout(
                    &tree,
                    lazy_child_timeout,
                    timeout_counter,
                )
                .await
                {
                    Ok(snapshot_tree) => {
                        SnapshotData::successful(ReadSnapshot::Tree(snapshot_tree), name, false)
                    }
                    Err(e) => SnapshotData::failed(
                        schema::InspectError { message: format!("{e:?}") },
                        name,
                        false,
                    ),
                }
            }
            InspectData::Vmo { data: vmo, escrowed } => match Snapshot::try_from(vmo.as_ref()) {
                Ok(snapshot) => {
                    SnapshotData::successful(ReadSnapshot::Single(snapshot), name, escrowed)
                }
                Err(e) => SnapshotData::failed(
                    schema::InspectError { message: format!("{e:?}") },
                    name,
                    escrowed,
                ),
            },
        }
    }

    // Constructs packet that timestamps and packages inspect snapshot for exfiltration.
    fn successful(
        snapshot: ReadSnapshot,
        name: Option<InspectHandleName>,
        escrowed: bool,
    ) -> SnapshotData {
        SnapshotData {
            name,
            timestamp: zx::BootInstant::get(),
            errors: Vec::new(),
            snapshot: Some(snapshot),
            escrowed,
        }
    }

    // Constructs packet that timestamps and packages inspect snapshot failure for exfiltration.
    fn failed(
        error: schema::InspectError,
        name: Option<InspectHandleName>,
        escrowed: bool,
    ) -> SnapshotData {
        SnapshotData {
            name,
            timestamp: zx::BootInstant::get(),
            errors: vec![error],
            snapshot: None,
            escrowed,
        }
    }
}

/// PopulatedInspectDataContainer is the container that
/// holds the actual Inspect data for a given component,
/// along with all information needed to transform that data
/// to be returned to the client.
pub struct PopulatedInspectDataContainer {
    pub identity: Arc<ComponentIdentity>,
    /// Vector of all the snapshots of inspect hierarchies under
    /// the diagnostics directory of the component identified by
    /// moniker, along with the metadata needed to populate
    /// this snapshot's diagnostics schema.
    pub snapshot: SnapshotData,
    pub component_allowlist: ComponentAllowlist,
}

enum Status {
    Begin,
    Pending(collector::InspectHandleDeque),
}

struct State {
    status: Status,
    unpopulated: Arc<UnpopulatedInspectDataContainer>,
    batch_timeout: zx::MonotonicDuration,
    elapsed_time: zx::MonotonicDuration,
    global_stats: Arc<GlobalConnectionStats>,
    trace_guard: Arc<Option<ftrace::AsyncScope>>,
    trace_id: ftrace::Id,
}

impl State {
    fn into_pending(
        self,
        pending: collector::InspectHandleDeque,
        start_time: zx::MonotonicInstant,
    ) -> Self {
        Self {
            unpopulated: self.unpopulated,
            status: Status::Pending(pending),
            batch_timeout: self.batch_timeout,
            global_stats: self.global_stats,
            elapsed_time: self.elapsed_time + (zx::MonotonicInstant::get() - start_time),
            trace_guard: self.trace_guard,
            trace_id: self.trace_id,
        }
    }

    fn add_elapsed_time(&mut self, start_time: zx::MonotonicInstant) {
        self.elapsed_time += zx::MonotonicInstant::get() - start_time
    }

    async fn iterate(
        mut self,
        start_time: zx::MonotonicInstant,
    ) -> Option<(PopulatedInspectDataContainer, State)> {
        let timeout = Duration::from_nanos(self.batch_timeout.into_nanos() as u64);
        loop {
            match &mut self.status {
                Status::Begin => {
                    let data_map = collector::populate_data_map(&self.unpopulated.inspect_handles);
                    self = self.into_pending(data_map, start_time);
                }
                Status::Pending(pending) => match pending.pop_front() {
                    None => {
                        self.global_stats.record_component_duration(
                            self.unpopulated.identity.moniker.as_ref(),
                            self.elapsed_time + (zx::MonotonicInstant::get() - start_time),
                        );
                        return None;
                    }
                    Some((name, data)) => {
                        let snapshot = SnapshotData::new(
                            name,
                            data,
                            timeout,
                            &self.unpopulated.identity,
                            self.trace_id,
                            self.global_stats.timeout_counter(),
                        )
                        .await;
                        let result = PopulatedInspectDataContainer {
                            identity: Arc::clone(&self.unpopulated.identity),
                            snapshot,
                            component_allowlist: self.unpopulated.component_allowlist.clone(),
                        };
                        self.add_elapsed_time(start_time);
                        return Some((result, self));
                    }
                },
            }
        }
    }
}

/// UnpopulatedInspectDataContainer is the container that holds
/// all information needed to retrieve Inspect data
/// for a given component, when requested.
#[derive(Debug)]
pub struct UnpopulatedInspectDataContainer {
    pub identity: Arc<ComponentIdentity>,
    /// Proxies configured for container.
    pub inspect_handles: Vec<Weak<InspectHandle>>,
    /// Optional hierarchy matcher. If unset, the reader is running
    /// in all-access mode, meaning no matching or filtering is required.
    pub component_allowlist: ComponentAllowlist,
}

impl UnpopulatedInspectDataContainer {
    /// Populates this data container with a timeout. On the timeout firing returns a
    /// container suitable to return to clients, but with timeout error information recorded.
    pub fn populate(
        self,
        timeout: i64,
        global_stats: Arc<GlobalConnectionStats>,
        parent_trace_id: ftrace::Id,
    ) -> impl Stream<Item = PopulatedInspectDataContainer> {
        let trace_id = ftrace::Id::new();
        let trace_guard = ftrace::async_enter!(
            trace_id,
            TRACE_CATEGORY,
            c"ReaderServer::stream.populate",
            // An async duration cannot have multiple concurrent child async durations
            // so we include the nonce as metadata to manually determine relationship.
            "parent_trace_id" => u64::from(parent_trace_id),
            "trace_id" => u64::from(trace_id),
            "moniker" => self.identity.to_string().as_ref()
        );
        let this = Arc::new(self);
        let state = State {
            status: Status::Begin,
            unpopulated: this,
            batch_timeout: zx::MonotonicDuration::from_seconds(timeout),
            global_stats,
            elapsed_time: zx::MonotonicDuration::ZERO,
            trace_guard: Arc::new(trace_guard),
            trace_id,
        };

        futures::stream::unfold(state, |state| {
            let start_time = zx::MonotonicInstant::get();

            state.iterate(start_time)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::LazyLock;

    static O_K_IDENTITY: LazyLock<Arc<ComponentIdentity>> =
        LazyLock::new(|| Arc::new(ComponentIdentity::from(vec!["o", "k"])));

    fn inspect_artifacts_container_with_n_trees<'a>(
        tree_names: impl IntoIterator<Item = &'a str>,
    ) -> InspectArtifactsContainer {
        let mut container = InspectArtifactsContainer::default();
        for name in tree_names {
            let (proxy, _) = fidl::endpoints::create_proxy::<finspect::TreeMarker>();
            let handle = InspectHandle::tree(proxy, Some(name));
            let _on_closed = container.push_handle(handle, |_| {});
        }
        container
    }

    #[fuchsia::test]
    fn name_filter_with_no_matches() {
        let _executor = fuchsia_async::LocalExecutor::default();

        let b_only = fdiagnostics::Selector {
            tree_names: Some(fdiagnostics::TreeNames::Some(vec!["b".to_string()])),
            component_selector: Some(fdiagnostics::ComponentSelector {
                moniker_segments: Some(vec![
                    fdiagnostics::StringSelector::ExactMatch("o".to_string()),
                    fdiagnostics::StringSelector::ExactMatch("k".to_string()),
                ]),
                ..Default::default()
            }),
            tree_selector: Some(fdiagnostics::TreeSelector::SubtreeSelector(
                fdiagnostics::SubtreeSelector {
                    node_path: vec![fdiagnostics::StringSelector::ExactMatch("root".to_string())],
                },
            )),
            ..Default::default()
        };
        let selectors = Some(vec![b_only]);

        let iac = inspect_artifacts_container_with_n_trees(["a"]);
        let container = iac
            .create_unpopulated(&O_K_IDENTITY, ComponentAllowlist::new_disabled(), &selectors)
            .unwrap();
        assert_eq!(0, container.inspect_handles.len());
    }

    #[fuchsia::test]
    fn all_name_filter_matches_everything() {
        let _executor = fuchsia_async::LocalExecutor::default();

        let iac = inspect_artifacts_container_with_n_trees(["a", "b"]);
        let container = iac
            .create_unpopulated(
                &O_K_IDENTITY,
                ComponentAllowlist::new_disabled(),
                &None::<&[fdiagnostics::Selector]>,
            )
            .unwrap();
        assert_eq!(2, container.inspect_handles.len());

        let all_selector = fdiagnostics::Selector {
            tree_names: Some(fdiagnostics::TreeNames::All(fdiagnostics::All {})),
            component_selector: Some(fdiagnostics::ComponentSelector {
                moniker_segments: Some(vec![
                    fdiagnostics::StringSelector::ExactMatch("o".to_string()),
                    fdiagnostics::StringSelector::ExactMatch("k".to_string()),
                ]),
                ..Default::default()
            }),
            tree_selector: Some(fdiagnostics::TreeSelector::SubtreeSelector(
                fdiagnostics::SubtreeSelector {
                    node_path: vec![fdiagnostics::StringSelector::ExactMatch("root".to_string())],
                },
            )),
            ..Default::default()
        };
        let selectors = Some(vec![all_selector]);

        let iac = inspect_artifacts_container_with_n_trees(["a", "b"]);
        let container = iac
            .create_unpopulated(&O_K_IDENTITY, ComponentAllowlist::new_disabled(), &selectors)
            .unwrap();
        assert_eq!(2, container.inspect_handles.len());
    }

    #[fuchsia::test]
    fn none_name_filter_matches_root_only() {
        let _executor = fuchsia_async::LocalExecutor::default();

        let lists_each_name = fdiagnostics::Selector {
            tree_names: None,
            component_selector: Some(fdiagnostics::ComponentSelector {
                moniker_segments: Some(vec![
                    fdiagnostics::StringSelector::ExactMatch("o".to_string()),
                    fdiagnostics::StringSelector::ExactMatch("k".to_string()),
                ]),
                ..Default::default()
            }),
            tree_selector: Some(fdiagnostics::TreeSelector::SubtreeSelector(
                fdiagnostics::SubtreeSelector {
                    node_path: vec![fdiagnostics::StringSelector::ExactMatch("root".to_string())],
                },
            )),
            ..Default::default()
        };
        let selectors = Some(vec![lists_each_name]);

        let iac = inspect_artifacts_container_with_n_trees(["a", "b", "root"]);
        let container = iac
            .create_unpopulated(&O_K_IDENTITY, ComponentAllowlist::new_disabled(), &selectors)
            .unwrap();
        assert_eq!(1, container.inspect_handles.len());
    }

    #[fuchsia::test]
    fn select_subset_of_names() {
        let _executor = fuchsia_async::LocalExecutor::default();

        let a_only = fdiagnostics::Selector {
            tree_names: Some(fdiagnostics::TreeNames::Some(vec!["a".to_string()])),
            component_selector: Some(fdiagnostics::ComponentSelector {
                moniker_segments: Some(vec![
                    fdiagnostics::StringSelector::ExactMatch("o".to_string()),
                    fdiagnostics::StringSelector::ExactMatch("k".to_string()),
                ]),
                ..Default::default()
            }),
            tree_selector: Some(fdiagnostics::TreeSelector::SubtreeSelector(
                fdiagnostics::SubtreeSelector {
                    node_path: vec![fdiagnostics::StringSelector::ExactMatch("root".to_string())],
                },
            )),
            ..Default::default()
        };
        let selectors = Some(vec![a_only]);

        let iac = inspect_artifacts_container_with_n_trees(["a", "b"]);
        let container = iac
            .create_unpopulated(&O_K_IDENTITY, ComponentAllowlist::new_disabled(), &selectors)
            .unwrap();
        assert_eq!(1, container.inspect_handles.len());

        match container.inspect_handles[0].upgrade().unwrap().as_ref() {
            InspectHandle::Tree { name: Some(n), .. } => assert_eq!(n.as_str(), "a"),
            bad_handle => {
                panic!("filtering failed, got {bad_handle:?}");
            }
        };
    }

    #[fuchsia::test]
    fn names_are_case_sensitive() {
        let _executor = fuchsia_async::LocalExecutor::default();

        let a_but_wrong_case = fdiagnostics::Selector {
            tree_names: Some(fdiagnostics::TreeNames::Some(vec!["A".to_string()])),
            component_selector: Some(fdiagnostics::ComponentSelector {
                moniker_segments: Some(vec![
                    fdiagnostics::StringSelector::ExactMatch("o".to_string()),
                    fdiagnostics::StringSelector::ExactMatch("k".to_string()),
                ]),
                ..Default::default()
            }),
            tree_selector: Some(fdiagnostics::TreeSelector::SubtreeSelector(
                fdiagnostics::SubtreeSelector {
                    node_path: vec![fdiagnostics::StringSelector::ExactMatch("root".to_string())],
                },
            )),
            ..Default::default()
        };
        let selectors = Some(vec![a_but_wrong_case]);

        let iac = inspect_artifacts_container_with_n_trees(["a", "b"]);
        let container = iac
            .create_unpopulated(&O_K_IDENTITY, ComponentAllowlist::new_disabled(), &selectors)
            .unwrap();
        assert_eq!(0, container.inspect_handles.len());
    }
}
