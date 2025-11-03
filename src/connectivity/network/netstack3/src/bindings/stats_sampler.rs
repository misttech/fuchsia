// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A long running service in netstack that takes samples from the running
//! system and renders them available in inspect.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use fuchsia_async as fasync;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, StreamExt};
use log::error;
use netstack3_core::device::{DeviceCounters, DeviceId, WeakDeviceId};
use windowed_stats::experimental::clock::{Timed, Timestamp};
use windowed_stats::experimental::series::interpolation::{InterpolationKind, LastSample};
use windowed_stats::experimental::series::statistic::{Diff, FoldError, SerialStatistic};
use windowed_stats::experimental::series::{
    SamplingProfile, SerializedBuffer, TimeMatrix, TimeMatrixFold as _, TimeMatrixTick,
};

use crate::bindings::{BindingsCtx, Ctx};

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
}

impl InterfaceStats {
    fn new(parent: &fuchsia_inspect::Node, name: String) -> Self {
        let node = parent.create_child(name);
        let rx = InterfaceTrafficSeries::new();
        rx.record_node(&node, "rx");
        let tx = InterfaceTrafficSeries::new();
        tx.record_node(&node, "tx");
        Self { node, rx, tx }
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
                        InterfaceStats::new(interfaces_node, C::display_name(device))
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
            let InterfaceStats { node, rx, tx } =
                live_interfaces.remove(&device).expect("entry must be there");
            // Change to the static serialized version so there's no effort
            // spent in maintaining old interface values.
            let rx = rx.read_buffers().await;
            let tx = tx.read_buffers().await;
            node.atomic_update(|node| {
                node.clear_recorded();
                node.record_child("rx", |node| {
                    SerializedBuffer::write_to_inspect_or_error(rx, node)
                });
                node.record_child("tx", |node| {
                    SerializedBuffer::write_to_inspect_or_error(tx, node)
                });
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

type InterfaceTrafficSeries = SharedTimeSeries<Diff<u64>, LastSample>;

/// A generic shared [`TimeMatrix`] kept by netstack.
///
/// This is backed by an async mutex to reduce unnecessary inter-task contention
/// between inspect and sampler, since we should be allowed to continue work
/// while any of those is happening without blocking the thread.
struct SharedTimeSeries<S: SerialStatistic<I>, I: InterpolationKind>(
    Arc<AsyncMutex<TimeMatrix<S, I>>>,
);

impl<S: SerialStatistic<I>, I: InterpolationKind> Clone for SharedTimeSeries<S, I> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S, I> SharedTimeSeries<S, I>
where
    S: SerialStatistic<I> + Send + 'static,
    I: InterpolationKind + Send + 'static,
    I::Output<S::Sample>: Send + 'static,
    S::Buffer: Send + 'static,
{
    fn new() -> Self
    where
        S: Default,
        I::Output<S::Sample>: Default,
    {
        Self(Arc::new(AsyncMutex::new(TimeMatrix::new(sampling_profile(), Default::default()))))
    }

    async fn fold(&self, value: S::Sample) {
        let Self(series) = self;
        let mut guard = series.lock().await;
        // NB: We may only read the current timestamp value when under lock
        // to prevent losing samples due to concurrent inspect ticking and
        // sample folding.
        guard.fold(Timed::now(value)).unwrap_or_else(|e| error!("error folding sample: {e:?}"));
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
                SerializedBuffer::write_to_inspect_or_error(buffers, inspector.root());
                Ok(inspector)
            }
            .boxed()
        });
    }

    async fn read_buffers(&self) -> Result<SerializedBuffer, FoldError> {
        let Self(this) = self;
        let mut guard = this.lock().await;

        // NB: We may only read the current timestamp value when under lock
        // to prevent losing samples due to concurrent inspect ticking and
        // sample folding.
        let buffers = guard.tick_and_get_buffers(Timestamp::now());
        if let Err(e) = &buffers {
            error!("failed to tick TimeMatrix {e:?}");
        }
        buffers
    }
}

/// The sampling profile used by all aggregations in this module.
fn sampling_profile() -> SamplingProfile {
    SamplingProfile::balanced()
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use diagnostics_assertions::{AnyProperty, TreeAssertion, assert_data_tree, tree_assertion};
    use netstack3_core::sync::{PrimaryRc, StrongRc, WeakRc};

    use super::*;

    struct FakeDeviceId {
        name: String,
        enabled: AtomicBool,
    }

    impl FakeDeviceId {
        fn new(name: impl Into<String>) -> Self {
            Self { name: name.into(), enabled: AtomicBool::new(true) }
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
            let device = PrimaryRc::new(FakeDeviceId::new(name));
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

    impl<S: SerialStatistic<I>, I: InterpolationKind> SharedTimeSeries<S, I> {
        #[track_caller]
        fn unwrap(self) -> TimeMatrix<S, I> {
            let Self(arc) = self;
            Arc::try_unwrap(arc)
                .unwrap_or_else(|a| {
                    panic!("failed to unwrap, {} references exist", Arc::strong_count(&a))
                })
                .into_inner()
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

    #[fuchsia::test]
    async fn sample_creates_interfaces() {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        let _ = sampler.ctx.new_device("test1");
        sampler.sample().await;

        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    "test1": {
                        time_series_assertion("rx"),
                        time_series_assertion("tx"),
                    }
                }
            }
        });

        // New interface adds new data to inspect tree.
        let _ = sampler.ctx.new_device("test2");
        sampler.sample().await;

        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    "test1": {
                        time_series_assertion("rx"),
                        time_series_assertion("tx"),
                    },
                    "test2": {
                        time_series_assertion("rx"),
                        time_series_assertion("tx"),
                    },
                }
            }
        });
    }

    #[fuchsia::test]
    async fn skips_disabled_interfaces() {
        let inspector = fuchsia_inspect::Inspector::default();
        let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
        let dev1 = sampler.ctx.new_device("test1");
        let _dev2 = sampler.ctx.new_device("test2");
        dev1.disable();
        sampler.sample().await;
        assert_data_tree!(inspector, "root" : {
            "sampled_stats": {
                "interfaces": {
                    "test2": {
                        time_series_assertion("rx"),
                        time_series_assertion("tx"),
                    },
                }
            }
        });
    }

    #[fuchsia::test]
    fn drops_removed_interface() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let fut = async {
            let inspector = fuchsia_inspect::Inspector::default();
            let mut sampler = StatsSamplerInner::new(FakeCtx::default(), inspector.root());
            let dev = sampler.ctx.new_device("test");
            sampler.sample().await;

            let InterfaceStats { node: _, rx, tx } =
                sampler.live_interfaces.get(&StrongRc::downgrade(&dev)).unwrap();
            let rx = rx.clone();
            let tx = tx.clone();

            assert_data_tree!(inspector, "root" : {
                "sampled_stats": {
                    "interfaces": {
                        "test": {
                            time_series_assertion("rx"),
                            time_series_assertion("tx"),
                        },
                    }
                }
            });

            drop(dev);
            sampler.ctx.devices.clear();
            sampler.sample().await;

            assert_data_tree!(inspector, "root" : {
                "sampled_stats": {
                    "interfaces": {
                        "test": {
                            time_series_assertion("rx"),
                            time_series_assertion("tx"),
                            "removed": true,
                        },
                    }
                }
            });

            // Once we're removed, we should be using static nodes which means
            // that there should be no longer any references on the shared time
            // series.
            let _ = rx.unwrap();
            let _ = tx.unwrap();

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
            let name_str = name.as_str();
            full_assertion.add_child_assertion(tree_assertion!(
                var name_str: {
                    time_series_assertion("rx"),
                    time_series_assertion("tx"),
                }
            ));
            if i < MAX_GONE_INTERFACES {
                removed_assertion.add_child_assertion(tree_assertion!(
                    var name_str: {
                        time_series_assertion("rx"),
                        time_series_assertion("tx"),
                        removed: true,
                    }
                ));
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
}
