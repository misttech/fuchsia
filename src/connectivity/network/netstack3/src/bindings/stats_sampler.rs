// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A long running service in netstack that takes samples from the running
//! system and renders them available in inspect.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use derivative::Derivative;
use fuchsia_async as fasync;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, StreamExt};
use log::error;
use netstack3_core::device::{DeviceCounters, DeviceId, WeakDeviceId};
use windowed_stats::experimental::clock::{Timed, Timestamp};
use windowed_stats::experimental::series::interpolation::{InterpolationKind, LastSample};
use windowed_stats::experimental::series::metadata::{DenseBitSetMap, Metadata};
use windowed_stats::experimental::series::statistic::{
    Diff, FoldError, SerialStatistic, Statistic, Union,
};
use windowed_stats::experimental::series::{
    BitSet, GaugeForceSimple8bRle, SamplingProfile, SerializedBuffer, TimeMatrix,
    TimeMatrixFold as _, TimeMatrixTick,
};

use crate::bindings::devices::DeviceSpecificInfo;
use crate::bindings::{BindingsCtx, Ctx, DeviceIdExt as _};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

// The maximum number of "gone" i.e. removed interfaces to keep.
//
// `StatsSampler` keeps inspect data of `MAX_GONE_INTERFACES` removed interfaces
// for up to `MAX_GONE_INTERFACE_RETENTION` time.
const MAX_GONE_INTERFACES: usize = 5;
const MAX_GONE_INTERFACE_RETENTION: Duration = Duration::from_hours(24);

struct InterfaceStats {
    // Keep the node around so when we drop this stats the node is removed from
    // the parent.
    node: fuchsia_inspect::Node,
    rx: InterfaceTrafficSeries,
    tx: InterfaceTrafficSeries,
    status: Option<InterfaceStatusSampler>,
}

impl InterfaceStats {
    fn new(
        parent: &fuchsia_inspect::Node,
        name: String,
        status: Option<InterfaceStatusSampler>,
    ) -> Self {
        let node = parent.create_child(name);
        let rx = InterfaceTrafficSeries::new(counter_sampling_profile(), Default::default(), ());
        rx.record_node(&node, "rx");
        let tx = InterfaceTrafficSeries::new(counter_sampling_profile(), Default::default(), ());
        tx.record_node(&node, "tx");
        if let Some(status) = status.as_ref() {
            status.inner.record_node(&node, "status");
        }
        Self { node, rx, tx, status }
    }
}

/// A worker that gather sampled statistics from the stack and pushes them to
/// inspect.
pub(crate) struct StatsSampler(StatsSamplerInner<Ctx>);

impl StatsSampler {
    pub(crate) fn new(ctx: Ctx, parent: &fuchsia_inspect::Node) -> Self {
        Self(StatsSamplerInner::new(ctx, parent))
    }

    pub(crate) async fn run(self) {
        let Self(mut inner) = self;
        let mut interval = fasync::Interval::new(SAMPLE_INTERVAL.into());
        while let Some(()) = interval.next().await {
            inner.sample().await;
        }
    }
}

/// The inner implementation of [`StatsSampler`].
///
/// This is extracted out so we can abstract the rest of the stack context
/// easily for tests in this module.
struct StatsSamplerInner<C: StatsSamplerContext> {
    ctx: C,
    interfaces_node: fuchsia_inspect::Node,
    live_interfaces: HashMap<C::WeakDeviceId, InterfaceStats>,
    gone_interfaces: VecDeque<(Timestamp, fuchsia_inspect::Node)>,
}

impl<C: StatsSamplerContext> StatsSamplerInner<C> {
    pub(crate) fn new(ctx: C, parent: &fuchsia_inspect::Node) -> Self {
        let root = parent.create_child("sampled_stats");
        let interfaces_node = root.create_child("interfaces");
        parent.record(root);
        Self {
            ctx,
            interfaces_node,
            live_interfaces: HashMap::new(),
            gone_interfaces: VecDeque::new(),
        }
    }

    async fn sample(&mut self) {
        let Self { ctx, live_interfaces, interfaces_node, gone_interfaces } = self;

        // Gather any new interfaces that may exist.
        ctx.for_each_device(|device| {
            // Only keep stats for devices we're interested in.
            if C::enable_stats(device) {
                let _: &mut InterfaceStats =
                    live_interfaces.entry(C::downgrade(device)).or_insert_with(|| {
                        InterfaceStats::new(
                            interfaces_node,
                            C::display_name(device),
                            C::interface_status_sampler(device),
                        )
                    });
            }
        });

        let mut to_remove = Vec::new();
        for (device, stats) in live_interfaces.iter_mut() {
            let Some(device) = C::upgrade(device) else {
                // Stash this for removal later, we can't remove while
                // iterating.
                //
                // Note that this is a lazy detection of interface removal by
                // failing to upgrade the DeviceId. So we're possibly losing the
                // "last sample" of the interface (up to the sampling interval)
                // and don't really have the delta for its final interval. This
                // is deemed OK in order to maintain the simplicity of observing
                // removal lazily.
                to_remove.push(device.clone());
                continue;
            };

            let InterfaceCounters { rx_bytes: rx, tx_bytes: tx } = ctx.read_counters(&device);
            stats.rx.fold(rx).await;
            stats.tx.fold(tx).await;
        }

        for device in to_remove {
            let InterfaceStats { node, rx, tx, status } =
                live_interfaces.remove(&device).expect("entry must be there");
            // Change to the static serialized version so there's no effort
            // spent in maintaining old interface values.
            let rx = rx.read_buffers().await;
            let tx = tx.read_buffers().await;
            let status = match status {
                Some(status) => Some(status.inner.read_buffers().await),
                None => None,
            };

            node.atomic_update(|node| {
                node.clear_recorded();
                rx.record_with_parent("rx", node);
                tx.record_with_parent("tx", node);
                if let Some(status) = status {
                    status.record_with_parent("status", node);
                }
                node.record_bool("removed", true);
            });

            gone_interfaces.push_back((Timestamp::now(), node));
            if gone_interfaces.len() > MAX_GONE_INTERFACES {
                let _: Option<_> = gone_interfaces.pop_front();
            }
        }

        // Queue of gone interfaces is always ordered, check the front for
        // expired entries.
        let now = Timestamp::now();
        while let Some((added, _stats)) = gone_interfaces.front() {
            if now - *added < MAX_GONE_INTERFACE_RETENTION.into() {
                break;
            }
            let _: Option<_> = gone_interfaces.pop_front();
        }
    }
}

struct InterfaceCounters {
    rx_bytes: u64,
    tx_bytes: u64,
}

trait StatsSamplerContext {
    type DeviceId;
    type WeakDeviceId: Hash + Eq + Clone;

    fn upgrade(w: &Self::WeakDeviceId) -> Option<Self::DeviceId>;
    fn downgrade(d: &Self::DeviceId) -> Self::WeakDeviceId;
    fn display_name(d: &Self::DeviceId) -> String;
    fn interface_status_sampler(d: &Self::DeviceId) -> Option<InterfaceStatusSampler>;
    fn enable_stats(d: &Self::DeviceId) -> bool;
    fn for_each_device<F: FnMut(&Self::DeviceId)>(&self, f: F);
    fn read_counters(&mut self, d: &Self::DeviceId) -> InterfaceCounters;
}

impl StatsSamplerContext for Ctx {
    type DeviceId = DeviceId<BindingsCtx>;
    type WeakDeviceId = WeakDeviceId<BindingsCtx>;

    fn upgrade(w: &Self::WeakDeviceId) -> Option<Self::DeviceId> {
        w.upgrade()
    }

    fn downgrade(d: &Self::DeviceId) -> Self::WeakDeviceId {
        d.downgrade()
    }

    fn display_name(d: &Self::DeviceId) -> String {
        d.bindings_id().to_string()
    }

    fn interface_status_sampler(d: &Self::DeviceId) -> Option<InterfaceStatusSampler> {
        match d.external_state() {
            DeviceSpecificInfo::Loopback(_) | DeviceSpecificInfo::Blackhole(_) => None,
            DeviceSpecificInfo::Ethernet(i) => Some(&i.status_sampler),
            DeviceSpecificInfo::PureIp(i) => Some(&i.status_sampler),
        }
        .cloned()
    }

    fn for_each_device<F: FnMut(&Self::DeviceId)>(&self, f: F) {
        self.bindings_ctx().devices.with_devices(|d| {
            d.for_each(f);
        });
    }

    fn enable_stats(d: &Self::DeviceId) -> bool {
        match d {
            DeviceId::Ethernet(_) | DeviceId::Loopback(_) | DeviceId::PureIp(_) => true,
            DeviceId::Blackhole(_) => false,
        }
    }

    fn read_counters(&mut self, d: &Self::DeviceId) -> InterfaceCounters {
        let mut api = self.api().device_any();
        let DeviceCounters { recv_bytes, send_bytes, .. } = api.get_counters(d);
        InterfaceCounters { rx_bytes: recv_bytes.get(), tx_bytes: send_bytes.get() }
    }
}

type InterfaceTrafficSeries = SharedTimeSeries<Diff<u64>, LastSample, ()>;

/// A trait shadowing the [`windowed_stats`] metadata definition, so we can more
/// easily use it here.
trait LocalMetadata<S> {
    fn record(parent: &fuchsia_inspect::Node);
}

impl LocalMetadata<GaugeForceSimple8bRle> for u64 {
    fn record(_parent: &fuchsia_inspect::Node) {}
}

struct SharedTimeSeriesState<S: SerialStatistic<I>, I: InterpolationKind, T> {
    matrix: TimeMatrix<S, I>,
    state: T,
}

/// A generic shared [`TimeMatrix`] kept by netstack.
///
/// This is backed by an async mutex to reduce unnecessary inter-task contention
/// between inspect and sampler, since we should be allowed to continue work
/// while any of those is happening without blocking the thread.
struct SharedTimeSeries<S: SerialStatistic<I>, I: InterpolationKind, T>(
    Arc<AsyncMutex<SharedTimeSeriesState<S, I, T>>>,
);

impl<S: SerialStatistic<I>, I: InterpolationKind, T> Clone for SharedTimeSeries<S, I, T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S, I, T> SharedTimeSeries<S, I, T>
where
    S: SerialStatistic<I> + Send + 'static,
    S::Aggregation: LocalMetadata<S::Semantic>,
    I: InterpolationKind + Send + 'static,
    I::Output<S::Sample>: Send + 'static,
    S::Buffer: Send + 'static,
    T: Send + 'static,
{
    fn new(profile: SamplingProfile, interpolation: I::Output<S::Sample>, state: T) -> Self
    where
        S: Default,
    {
        Self(Arc::new(AsyncMutex::new(SharedTimeSeriesState {
            matrix: TimeMatrix::new(profile, interpolation),
            state: state,
        })))
    }

    async fn fold(&self, value: S::Sample) {
        let Self(series) = self;
        let mut guard = series.lock().await;
        // NB: We may only read the current timestamp value when under lock
        // to prevent losing samples due to concurrent inspect ticking and
        // sample folding.
        guard
            .matrix
            .fold(Timed::now(value))
            .unwrap_or_else(|e| error!("error folding sample: {e:?}"));
    }

    async fn fold_with<F: FnOnce(&mut T) -> S::Sample>(&self, f: F) {
        let Self(series) = self;
        let mut guard = series.lock().await;
        // NB: We may only read the current timestamp value when under lock
        // to prevent losing samples due to concurrent inspect ticking and
        // sample folding.
        let value = f(&mut guard.state);
        guard
            .matrix
            .fold(Timed::now(value))
            .unwrap_or_else(|e| error!("error folding sample: {e:?}"));
    }

    /// Records the shared time series to `parent` with `name`.
    ///
    /// Note that a live time series is always recorded as a lazy inspect node,
    /// which will contend on the shared instance lock to serve inspect. This is
    /// deemed to be the right choice due to how infrequent inspect data needs
    /// to be yielded out compared to the frequency of samples being folded into
    /// the TimeMatrix, as well as the higher cost of producing the serialized
    /// buffer for inspect consumption.
    fn record_node(&self, parent: &fuchsia_inspect::Node, name: &str) {
        let this = self.clone();
        parent.record_lazy_child(name, move || {
            let this = this.clone();
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                let buffers = this.read_buffers().await;
                buffers.record(inspector.root());
                Ok(inspector)
            }
            .boxed()
        });
    }

    async fn read_buffers(&self) -> SharedTimeSeriesBuffers<S> {
        let Self(this) = self;
        let mut guard = this.lock().await;

        // NB: We may only read the current timestamp value when under lock
        // to prevent losing samples due to concurrent inspect ticking and
        // sample folding.
        let buffers = guard.matrix.tick_and_get_buffers(Timestamp::now());
        if let Err(e) = &buffers {
            error!("failed to tick TimeMatrix {e:?}");
        }
        SharedTimeSeriesBuffers { buffers, _marker: PhantomData }
    }
}

struct SharedTimeSeriesBuffers<S> {
    buffers: Result<SerializedBuffer, FoldError>,
    _marker: PhantomData<S>,
}

impl<S> SharedTimeSeriesBuffers<S>
where
    S: Statistic,
    S::Aggregation: LocalMetadata<S::Semantic>,
{
    fn record(self, node: &fuchsia_inspect::Node) {
        let Self { buffers, _marker } = self;
        SerializedBuffer::write_to_inspect_or_error(buffers, node);
        <S::Aggregation as LocalMetadata<S::Semantic>>::record(node);
    }

    fn record_with_parent(self, name: &str, node: &fuchsia_inspect::Node) {
        node.record_child(name, move |node| self.record(node));
    }
}

bitflags::bitflags! {
    /// The interface status bits reported in history.
    ///
    /// A single bit of state is represented as two bits in the logged state so
    /// we can know for an aggregation window whether an interface was ever in
    /// each of the bit states. This allows tracking of flaps occurring in a
    /// given aggregation period.
    #[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
    struct InterfaceStatusBits: u8 {
        const LINK_UP = 1;
        const LINK_DOWN = 2;
        const ADMIN_UP = 4;
        const ADMIN_DOWN = 8;
    }
}

impl InterfaceStatusBits {
    const fn from_link_up(up: bool) -> Self {
        if up { Self::LINK_UP } else { Self::LINK_DOWN }
    }

    const fn from_admin_up(up: bool) -> Self {
        if up { Self::ADMIN_UP } else { Self::ADMIN_DOWN }
    }

    fn labels() -> impl Iterator<Item = (Self, &'static str)> {
        Self::all().iter().map(|s| {
            let label = match s {
                Self::LINK_UP => "Link Up",
                Self::LINK_DOWN => "Link Down",
                Self::ADMIN_UP => "Admin Up",
                Self::ADMIN_DOWN => "Admin Down",
                _ => unreachable!("not single bit set"),
            };
            (s, label)
        })
    }
}

impl From<InterfaceStatusBits> for u64 {
    fn from(v: InterfaceStatusBits) -> Self {
        v.bits().into()
    }
}

impl LocalMetadata<BitSet> for InterfaceStatusBits {
    fn record(parent: &fuchsia_inspect::Node) {
        let meta = DenseBitSetMap::new(|| Self::labels().map(|(_bits, label)| label));
        Metadata::record_with_parent(&meta, parent);
    }
}

#[derive(Copy, Clone, Default)]
pub(crate) struct InterfaceStatusBufferedState {
    pub(crate) link_up: bool,
    pub(crate) admin_up: bool,
}

impl InterfaceStatusBufferedState {
    fn to_sample(&self) -> InterfaceStatusBits {
        let Self { link_up, admin_up } = self;
        InterfaceStatusBits::from_link_up(*link_up) | InterfaceStatusBits::from_admin_up(*admin_up)
    }
}

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub(crate) struct InterfaceStatusSampler {
    #[derivative(Debug = "ignore")]
    inner: SharedTimeSeries<Union<InterfaceStatusBits>, LastSample, InterfaceStatusBufferedState>,
}

impl InterfaceStatusSampler {
    /// Creates a new [`InterfaceStatusSampler`] with `initial_state`.
    pub(crate) fn new(initial_state: InterfaceStatusBufferedState) -> Self {
        Self {
            inner: SharedTimeSeries::new(
                status_sampling_profile(),
                LastSample::or(initial_state.to_sample()),
                initial_state,
            ),
        }
    }

    pub(crate) async fn report_admin_state(&self, up: bool) {
        self.inner
            .fold_with(|state| {
                state.admin_up = up;
                state.to_sample()
            })
            .await;
    }

    pub(crate) async fn report_link_state(&self, up: bool) {
        self.inner
            .fold_with(|state| {
                state.link_up = up;
                state.to_sample()
            })
            .await;
    }
}

/// The sampling profile used by counter aggregations in this module.
fn counter_sampling_profile() -> SamplingProfile {
    SamplingProfile::balanced()
}

/// The sampling profile used by status bitset aggregations in this module.
fn status_sampling_profile() -> SamplingProfile {
    SamplingProfile::highly_granular()
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use diagnostics_assertions::{AnyProperty, TreeAssertion, assert_data_tree, tree_assertion};
    use netstack3_core::sync::{PrimaryRc, StrongRc, WeakRc};
    use test_case::test_case;
    use windowed_stats::experimental::series::decode::{DataPoint, DecodeError, Decoder};

    use super::*;

    struct FakeDeviceId {
        name: String,
        enabled: AtomicBool,
        status_sampler: Option<InterfaceStatusSampler>,
    }

    impl FakeDeviceId {
        fn new(name: impl Into<String>) -> Self {
            Self::new_with_status_sampler(name, true)
        }

        fn new_with_status_sampler(name: impl Into<String>, status_sampler: bool) -> Self {
            Self {
                name: name.into(),
                enabled: AtomicBool::new(true),
                status_sampler: status_sampler
                    .then(|| InterfaceStatusSampler::new(Default::default())),
            }
        }

        fn disable(&self) {
            self.enabled.store(false, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct FakeCtx {
        devices: Vec<PrimaryRc<FakeDeviceId>>,
    }

    impl FakeCtx {
        fn new_device(&mut self, name: impl Into<String>) -> StrongRc<FakeDeviceId> {
            self.insert_device(FakeDeviceId::new(name))
        }

        fn insert_device(&mut self, device: FakeDeviceId) -> StrongRc<FakeDeviceId> {
            let device = PrimaryRc::new(device);
            let strong = PrimaryRc::clone_strong(&device);
            self.devices.push(device);
            strong
        }
    }

    impl StatsSamplerContext for FakeCtx {
        type DeviceId = StrongRc<FakeDeviceId>;
        type WeakDeviceId = WeakRc<FakeDeviceId>;

        fn upgrade(w: &Self::WeakDeviceId) -> Option<Self::DeviceId> {
            w.upgrade()
        }

        fn downgrade(d: &Self::DeviceId) -> Self::WeakDeviceId {
            StrongRc::downgrade(d)
        }

        fn display_name(d: &Self::DeviceId) -> String {
            d.name.clone()
        }

        fn interface_status_sampler(d: &Self::DeviceId) -> Option<InterfaceStatusSampler> {
            d.status_sampler.clone()
        }

        fn for_each_device<F: FnMut(&Self::DeviceId)>(&self, mut f: F) {
            for d in &self.devices {
                f(&PrimaryRc::clone_strong(d));
            }
        }

        fn enable_stats(d: &Self::DeviceId) -> bool {
            d.enabled.load(Ordering::Relaxed)
        }

        fn read_counters(&mut self, _d: &Self::DeviceId) -> InterfaceCounters {
            // NB: There's no good way of reading the data back from the
            // serialized buffers today back into a series that we can assert
            // on, so there's no point generating any sample values.
            InterfaceCounters { rx_bytes: 0, tx_bytes: 0 }
        }
    }

    fn time_series_assertion(name: &str) -> TreeAssertion {
        tree_assertion!(
            var name: {
                "type": AnyProperty,
                "data": AnyProperty,
            }
        )
    }

    fn time_series_assertion_with_metadata(name: &str) -> TreeAssertion {
        tree_assertion!(
            var name: {
                "type": AnyProperty,
                "data": AnyProperty,
                "metadata": contains {},
            }
        )
    }

    #[derive(Copy, Clone)]
    struct InterfaceInspectAssertion<'a> {
        name: &'a str,
        status: bool,
        removed: bool,
    }

    impl<'a> Default for InterfaceInspectAssertion<'a> {
        fn default() -> Self {
            Self { name: "", status: true, removed: false }
        }
    }

    impl<'a> InterfaceInspectAssertion<'a> {
        fn assertion(self) -> TreeAssertion {
            let Self { name, status, removed } = self;
            let mut assertion = tree_assertion!(
                var name: {
                    time_series_assertion("rx"),
                    time_series_assertion("tx"),
                }
            );
            if removed {
                assertion.add_property_assertion("removed", Arc::new(true));
            }
            if status {
                assertion.add_child_assertion(time_series_assertion_with_metadata("status"));
            }
            assertion
        }
    }

    impl<S: SerialStatistic<I>, I: InterpolationKind, T> SharedTimeSeries<S, I, T> {
        #[track_caller]
        fn unwrap(self) -> TimeMatrix<S, I> {
            let Self(arc) = self;
            Arc::try_unwrap(arc)
                .unwrap_or_else(|a| {
                    panic!("failed to unwrap, {} references exist", Arc::strong_count(&a))
                })
                .into_inner()
                .matrix
        }
    }

    #[fuchsia::test]
    async fn empty_sampler() {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        sampler.sample().await;
        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {}
            }
        });
    }

    #[test_case(true; "with status")]
    #[test_case(false; "without status")]
    #[fuchsia::test]
    async fn sample_creates_interfaces(with_status: bool) {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        let name1 = "test1";
        let _ =
            sampler.ctx.insert_device(FakeDeviceId::new_with_status_sampler(name1, with_status));
        sampler.sample().await;

        let base_assertion =
            InterfaceInspectAssertion { status: with_status, ..Default::default() };

        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    InterfaceInspectAssertion {
                        name: name1,
                        ..base_assertion
                    }.assertion(),
                }
            }
        });

        // New interface adds new data to inspect tree.
        // The new interface uses the default value for with_status.
        let name2 = "test2";
        let _ = sampler.ctx.new_device(name2);
        sampler.sample().await;

        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    InterfaceInspectAssertion {
                        name: name1,
                        ..base_assertion
                    }.assertion(),
                    InterfaceInspectAssertion {
                        name: name2,
                        ..Default::default()
                    }.assertion(),
                }
            }
        });
    }

    #[fuchsia::test]
    async fn skips_disabled_interfaces() {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        let dev1 = sampler.ctx.new_device("test1");
        let name2 = "test2";
        let _dev2 = sampler.ctx.new_device(name2);
        dev1.disable();
        sampler.sample().await;
        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    InterfaceInspectAssertion {
                        name: name2,
                        ..Default::default()
                    }.assertion(),
                }
            }
        });
    }

    #[test_case(true; "with_status")]
    #[test_case(false; "without status")]
    #[fuchsia::test]
    fn drops_removed_interface(with_status: bool) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let fut = async {
            let inspector = fuchsia_inspect::Inspector::default();
            let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
            let name = "test";
            let dev =
                sampler.ctx.insert_device(FakeDeviceId::new_with_status_sampler(name, with_status));
            sampler.sample().await;

            let base_assertion =
                InterfaceInspectAssertion { name, status: with_status, ..Default::default() };

            let InterfaceStats { node: _, rx, tx, status } =
                sampler.live_interfaces.get(&StrongRc::downgrade(&dev)).unwrap();
            let rx = rx.clone();
            let tx = tx.clone();
            let status = status.clone();

            assert_data_tree!(inspector, "root" : {
                "sampled_stats": {
                    "interfaces": {
                        base_assertion.assertion(),
                    }
                }
            });

            drop(dev);
            sampler.ctx.devices.clear();
            sampler.sample().await;

            assert_data_tree!(inspector, "root" : {
                "sampled_stats": {
                    "interfaces": {
                        InterfaceInspectAssertion {
                            removed: true,
                            ..base_assertion
                        }.assertion(),
                    }
                }
            });

            // Once we're removed, we should be using static nodes which means
            // that there should be no longer any references on the shared time
            // series.
            let _ = rx.unwrap();
            let _ = tx.unwrap();
            if let Some(InterfaceStatusSampler { inner }) = status {
                let _ = inner.unwrap();
            }

            // Advance time enough that the interface is eventually removed.
            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::after(
                MAX_GONE_INTERFACE_RETENTION.into(),
            ))
            .await;

            sampler.sample().await;

            assert_data_tree!(inspector, "root" : {
                "sampled_stats": {
                    "interfaces": {}
                }
            });
        };
        let mut fut = pin!(fut);
        assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    async fn limits_old_interface_count() {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        let mut full_assertion = TreeAssertion::new("interfaces", true);
        let mut removed_assertion = TreeAssertion::new("interfaces", true);

        const CREATE_IFACE_COUNT: usize = MAX_GONE_INTERFACES + 3;
        for i in 0..CREATE_IFACE_COUNT {
            let name = format!("test{i}");
            full_assertion.add_child_assertion(
                InterfaceInspectAssertion { name: &name, ..Default::default() }.assertion(),
            );
            if i < MAX_GONE_INTERFACES {
                removed_assertion.add_child_assertion(
                    InterfaceInspectAssertion { name: &name, removed: true, ..Default::default() }
                        .assertion(),
                );
            }
            let _ = sampler.ctx.new_device(name);
        }

        sampler.sample().await;
        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                full_assertion,
            }
        });

        // Remove one interface at a time so we have some guaranteed ordering.
        while let Some(dev) = sampler.ctx.devices.pop() {
            drop(dev);
            sampler.sample().await;
        }
        assert_eq!(sampler.gone_interfaces.len(), MAX_GONE_INTERFACES);

        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                removed_assertion,
            }
        });
    }

    impl DataPoint for InterfaceStatusBits {
        fn from_u64(v: u64) -> Result<Self, DecodeError> {
            u8::try_from(v)
                .ok()
                .and_then(|v| InterfaceStatusBits::from_bits(v))
                .ok_or_else(|| DecodeError::Other(anyhow::anyhow!("invalid data point {v:X}")))
        }
    }

    #[test]
    fn interface_status_sampler() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
        let sampler = InterfaceStatusSampler::new(Default::default());
        let wait = fasync::MonotonicDuration::from_nanos(
            status_sampling_profile().granularity().into_nanos(),
        );

        let fut = async move {
            let mut expect = Vec::new();
            expect.push(InterfaceStatusBits::LINK_DOWN | InterfaceStatusBits::ADMIN_DOWN);

            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + wait).await;
            sampler.report_admin_state(true).await;
            expect.push(InterfaceStatusBits::LINK_DOWN | InterfaceStatusBits::ADMIN_UP);

            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + wait).await;
            sampler.report_link_state(true).await;
            expect.push(InterfaceStatusBits::LINK_UP | InterfaceStatusBits::ADMIN_UP);

            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + wait).await;
            sampler.report_admin_state(false).await;
            expect.push(InterfaceStatusBits::LINK_UP | InterfaceStatusBits::ADMIN_DOWN);

            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + wait).await;
            sampler.report_admin_state(true).await;
            sampler.report_admin_state(false).await;
            // Records all seen states.
            expect.push(
                InterfaceStatusBits::LINK_UP
                    | InterfaceStatusBits::ADMIN_DOWN
                    | InterfaceStatusBits::ADMIN_UP,
            );

            // Interpolates with last state.
            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + wait + wait).await;
            expect.push(InterfaceStatusBits::LINK_UP | InterfaceStatusBits::ADMIN_DOWN);

            let SharedTimeSeriesBuffers { buffers, _marker } = sampler.inner.read_buffers().await;
            let buffers = buffers.expect("serialized");
            let decoder = Decoder::from_serialized_buffer(&buffers).expect("decode");
            let series = decoder
                .iter_series()
                .next()
                .expect("series")
                .expect("first series")
                .data_points::<InterfaceStatusBits>()
                .collect::<Result<Vec<_>, _>>()
                .expect("data points");
            assert_eq!(series, expect);
        };
        let mut fut = pin!(fut);
        assert_eq!(executor.run_until_stalled(&mut fut), Poll::Ready(()));
    }

    /// Given we use Flags::all() from bitflags to generate our bit labels,
    /// ensure that that method actually iterates over the flags in bit order.
    #[test]
    fn interface_status_bits_labels() {
        let flags = InterfaceStatusBits::labels().collect::<Vec<_>>();
        let mut sorted = flags.clone();
        sorted.sort_by_key(|(bits, _label)| bits.bits().trailing_zeros());
        assert_eq!(flags, sorted);
        // Finally assert that all the values have exactly one bit set.
        for (bits, _label) in sorted {
            assert_eq!(bits.bits().count_ones(), 1, "{bits:?}");
        }
    }
}
