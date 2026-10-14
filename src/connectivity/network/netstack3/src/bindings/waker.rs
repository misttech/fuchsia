// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_matches::assert_matches;
use fidl_fuchsia_net_power as fnet_power;
use fidl_fuchsia_net_resources as fnet_resources;
use fuchsia_async as fasync;
use futures::TryStreamExt as _;
use futures::future::FutureExt as _;
use log::{debug, info, warn};
use netstack3_core::sync::Mutex;

use crate::bindings::util::{DataNotifier, DataWatcher, ResultExt as _};

/// The signal we raise to signal the other side to resume when data is available.
const GROUP_WAKEUP_SIGNAL: zx::Signals =
    zx::Signals::from_bits(fnet_power::GROUP_WAKEUP_SIGNAL).unwrap();

/// The signal the client raises to indicate that it's awake.
const WAITER_AWAKE_SIGNAL: zx::Signals =
    zx::Signals::from_bits(fnet_power::WAITER_AWAKE_SIGNAL).unwrap();

/// The signal the client raises to indicate that it's asleep.
const WAITER_ASLEEP_SIGNAL: zx::Signals =
    zx::Signals::from_bits(fnet_power::WAITER_ASLEEP_SIGNAL).unwrap();

#[derive(Default, Clone)]
pub(crate) struct WakeGroups(Arc<Mutex<WakeGroupsInner>>);

#[derive(Default)]
struct WakeGroupsInner {
    wake_groups: HashMap<zx::Koid, DataNotifier>,
}

impl WakeGroups {
    pub(crate) async fn serve_provider(
        self,
        mut stream: fnet_power::WakeGroupProviderRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.try_next().await? {
            let fnet_power::WakeGroupProviderRequest::CreateWakeGroup {
                options,
                wake_watcher,
                responder,
            } = request;
            let fnet_power::WakeGroupOptions { debug_name, __source_breaking } = options;

            let debug_name = debug_name.unwrap_or_else(|| {
                static COUNTER: AtomicUsize = AtomicUsize::new(0);
                format!("wake-group-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
            });
            let id = self.create_wake_group(debug_name, wake_watcher);

            responder
                .send(fnet_power::CreateWakeGroupResponse {
                    token: Some(fnet_resources::WakeGroupToken { token: id.token }),
                    __source_breaking: fidl::marker::SourceBreaking,
                })
                .unwrap_or_log("failed to respond to CreateWakeGroup");
        }
        Ok(())
    }

    fn register_wake_group(
        &self,
        debug_name: String,
        wake_watcher: zx::EventPair,
    ) -> (WakeGroup, WakeGroupId) {
        let WakeGroups(inner) = self;

        let (data_watcher, data_notifier) = DataWatcher::new();
        let wake_group = WakeGroup::new(debug_name, data_watcher, wake_watcher);
        let id = wake_group.id.duplicate_for_client();

        assert_matches!(
            inner.lock().wake_groups.insert(id.koid, data_notifier),
            None,
            "koid of new wake group should be unique",
        );

        (wake_group, id)
    }

    fn create_wake_group(&self, debug_name: String, wake_watcher: zx::EventPair) -> WakeGroupId {
        let (wake_group, id) = self.register_wake_group(debug_name, wake_watcher);

        info!("creating wake group '{}' {id:?}", wake_group.name);

        let wake_groups = self.clone();
        let _: fasync::JoinHandle<()> = fasync::Scope::current().spawn(async move {
            let name = wake_group.name.clone();
            let Err(reason) = wake_group.serve(wake_groups).await;
            match reason {
                WakeGroupShutdownReason::WakeWatcherClosed => {
                    debug!("wake group '{name}' closing because of client closure");
                }
                WakeGroupShutdownReason::AwakeAndAsleepAsserted => {
                    warn!("closing wake group '{name}' because both AWAKE and ASLEEP signals set");
                }
            }
        });

        id
    }

    fn remove_wake_group(&self, koid: zx::Koid) {
        let WakeGroups(inner) = self;
        assert_matches!(inner.lock().wake_groups.remove(&koid), Some(_));
    }

    pub(crate) fn data_notifier(&self, wake_group: &WakeGroupId) -> Option<DataNotifier> {
        let Self(inner) = self;
        let wake_groups = &inner.lock().wake_groups;
        let Some(data_notifier) = wake_groups.get(&wake_group.koid) else {
            warn!("could not attach socket to nonexistent wake group {wake_group:?}");
            return None;
        };
        Some(data_notifier.clone())
    }
}

pub(crate) struct WakeGroupId {
    token: zx::Event,
    koid: zx::Koid,
}

impl fmt::Debug for WakeGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { token: _, koid } = self;

        write!(f, "WakeGroupId(koid={koid:?})")
    }
}

impl WakeGroupId {
    fn new() -> Self {
        let token = zx::Event::create();
        let koid = token.koid().expect("get koid of wake group token");
        Self { token, koid }
    }

    fn duplicate_for_client(&self) -> Self {
        let Self { token, koid } = self;
        let token = token
            .duplicate_handle(zx::Rights::TRANSFER | zx::Rights::DUPLICATE | zx::Rights::WAIT)
            .expect("must be able to duplicate wake group token");

        Self { token, koid: *koid }
    }
}

impl From<fnet_resources::WakeGroupToken> for WakeGroupId {
    fn from(token: fnet_resources::WakeGroupToken) -> Self {
        let fnet_resources::WakeGroupToken { token } = token;
        let koid = token.koid().expect("get koid of wake group token");
        Self { token, koid }
    }
}

#[derive(Debug)]
struct WakeGroup {
    name: String,
    id: WakeGroupId,
    data_watcher: DataWatcher,
    wake_watcher: zx::EventPair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Awake,
    Asleep,
}

impl ClientState {
    fn target_signal(self) -> zx::Signals {
        match self {
            ClientState::Awake => WAITER_AWAKE_SIGNAL,
            ClientState::Asleep => WAITER_ASLEEP_SIGNAL,
        }
    }

    fn conflicting_signal(self) -> zx::Signals {
        match self {
            ClientState::Awake => WAITER_ASLEEP_SIGNAL,
            ClientState::Asleep => WAITER_AWAKE_SIGNAL,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WakeGroupShutdownReason {
    WakeWatcherClosed,
    AwakeAndAsleepAsserted,
}

async fn wait_for_client_state(
    wake_watcher: &zx::EventPair,
    state: ClientState,
) -> Result<(), WakeGroupShutdownReason> {
    let signals = fasync::OnSignals::new(
        wake_watcher,
        state.target_signal() | zx::Signals::EVENTPAIR_PEER_CLOSED,
    )
    .await
    .expect("OnSignals doesn't fail");

    if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED) {
        Err(WakeGroupShutdownReason::WakeWatcherClosed)
    } else if signals.contains(state.conflicting_signal()) {
        Err(WakeGroupShutdownReason::AwakeAndAsleepAsserted)
    } else {
        Ok(())
    }
}

impl WakeGroup {
    fn new(name: String, data_watcher: DataWatcher, wake_watcher: zx::EventPair) -> Self {
        Self { name, id: WakeGroupId::new(), data_watcher, wake_watcher }
    }

    async fn serve(self, wake_groups: WakeGroups) -> Result<!, WakeGroupShutdownReason> {
        let Self { name, id, mut data_watcher, wake_watcher } = self;
        let WakeGroupId { token, koid: _ } = &id;

        let _cleanup = scopeguard::guard((wake_groups, &id), |(wake_groups, id)| {
            let _ = id.token.signal(GROUP_WAKEUP_SIGNAL, zx::Signals::NONE);
            // Note that sockets that were attached to this wake group still
            // hold onto their DataNotifiers, but without a data watcher, those
            // notifications are no-ops.
            wake_groups.remove_wake_group(id.koid);
            debug!("removed wake group '{name}' {id:?}");
        });

        loop {
            // Other side is (probably) running. There's nothing for us to do
            // except wait for them to suspend.
            debug!("wake group '{name}' {id:?} waiting for suspend");
            wait_for_client_state(&wake_watcher, ClientState::Asleep).await?;

            // Bias in favor of client wakeup. In that case, raising the
            // wake signal is pointless.
            futures::select_biased! {
                res = wait_for_client_state(&wake_watcher, ClientState::Awake).fuse() => {
                    res?;
                    // The other side woke up without us. Nothing left to do here.
                    continue;
                }
                () = data_watcher.reset_and_wait().fuse() => (),
            }

            // Assert the wake signal to wake up the other side.
            token.signal(zx::Signals::NONE, GROUP_WAKEUP_SIGNAL).expect("signal doesn't fail");
            debug!("notified wake group '{name}' {id:?} of incoming data");

            wait_for_client_state(&wake_watcher, ClientState::Awake).await?;

            // TODO(https://fxbug.dev/538164589): Hold delegated wake leases
            // from netdevice until the client is awake.

            // Deassert wake signal. When the other side is awake, we assume
            // they're capable of staying awake until they're done processing
            // the incoming data.
            token.signal(GROUP_WAKEUP_SIGNAL, zx::Signals::NONE).expect("signal doesn't fail");
            debug!("deasserted wake signal on '{name}' {id:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::task::Poll;
    use test_case::{test_case, test_matrix};
    use zx::Peered as _;

    fn assert_wake_signal_clear(token: &zx::Event) {
        assert_matches!(
            token.wait_one(GROUP_WAKEUP_SIGNAL, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(_)
        );
    }

    fn assert_wake_signal_set(token: &zx::Event) {
        let observed =
            token.wait_one(GROUP_WAKEUP_SIGNAL, zx::MonotonicInstant::INFINITE_PAST).unwrap();
        assert!(observed.contains(GROUP_WAKEUP_SIGNAL));
    }

    /// A wake group registered with a [`WakeGroups`], along with everything a
    /// test needs to drive it.
    struct TestWakeGroup {
        /// Stands in for the client, signaling its power state to the group.
        /// `None` once the client has gone away.
        signaller: Option<zx::EventPair>,
        id: WakeGroupId,
        notifier: DataNotifier,
        serve_fut: fasync::Task<Result<!, WakeGroupShutdownReason>>,
    }

    impl TestWakeGroup {
        fn new(wake_groups: &WakeGroups, name: &str) -> Self {
            let (wake_watcher_observer, signaller) = zx::EventPair::create();
            // Clients start out awake.
            signaller.signal_peer(zx::Signals::NONE, WAITER_AWAKE_SIGNAL).unwrap();

            let (wake_group, id) =
                wake_groups.register_wake_group(name.to_string(), wake_watcher_observer);
            let notifier = wake_groups.data_notifier(&id).expect("newly registered wake group");

            Self {
                signaller: Some(signaller),
                id,
                notifier,
                serve_fut: fasync::Task::local(wake_group.serve(wake_groups.clone())),
            }
        }

        fn token(&self) -> &zx::Event {
            &self.id.token
        }

        fn signaller(&self) -> &zx::EventPair {
            self.signaller.as_ref().expect("client has gone away")
        }

        fn suspend_client(&self) {
            self.signaller().signal_peer(WAITER_AWAKE_SIGNAL, WAITER_ASLEEP_SIGNAL).unwrap();
        }

        fn wake_client(&self) {
            self.signaller().signal_peer(WAITER_ASLEEP_SIGNAL, WAITER_AWAKE_SIGNAL).unwrap();
        }

        /// Closes the client end of the wake watcher, which the group observes
        /// as the client going away for good.
        fn close_client(&mut self) {
            drop(self.signaller.take().expect("client has already gone away"));
        }

        /// Violates the protocol by claiming to be awake and asleep at once.
        fn raise_conflicting_client_states(&self) {
            self.signaller()
                .signal_peer(zx::Signals::NONE, WAITER_AWAKE_SIGNAL | WAITER_ASLEEP_SIGNAL)
                .unwrap();
        }

        fn is_registered(&self, wake_groups: &WakeGroups) -> bool {
            wake_groups.0.lock().wake_groups.contains_key(&self.id.koid)
        }

        /// Lets the group's serving task observe everything that has happened
        /// to it, asserting that it stays alive.
        fn run_until_stalled(&mut self, exec: &mut fasync::TestExecutor) {
            assert_eq!(exec.run_until_stalled(&mut self.serve_fut), Poll::Pending);
        }

        /// Lets the group's serving task observe everything that has happened
        /// to it, asserting that it shuts down for `reason`.
        fn run_until_shutdown(
            &mut self,
            exec: &mut fasync::TestExecutor,
            reason: WakeGroupShutdownReason,
        ) {
            assert_eq!(exec.run_until_stalled(&mut self.serve_fut), Poll::Ready(Err(reason)));
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TriggerShutdownWhen {
        BeforeSuspend,
        DuringSuspend,
        AfterData,
    }

    enum ShutdownTrigger {
        DropWaker,
        RaiseInvalidSignals,
    }

    #[test_matrix(
        [
            TriggerShutdownWhen::BeforeSuspend,
            TriggerShutdownWhen::DuringSuspend,
            TriggerShutdownWhen::AfterData,
        ],
        [
            ShutdownTrigger::DropWaker,
            ShutdownTrigger::RaiseInvalidSignals,
        ]
    )]
    fn wake_group_shutdown_triggers(
        shutdown_when: TriggerShutdownWhen,
        shutdown_trigger: ShutdownTrigger,
    ) {
        let mut exec = fasync::TestExecutor::new();

        let wake_groups = WakeGroups::default();
        let mut group = TestWakeGroup::new(&wake_groups, "test-group");

        // State machine is waiting for the client to signal it's asleep.
        group.run_until_stalled(&mut exec);

        if shutdown_when != TriggerShutdownWhen::BeforeSuspend {
            group.suspend_client();
            // State machine is waiting for data to come in or the client to
            // wake.
            group.run_until_stalled(&mut exec);

            if shutdown_when != TriggerShutdownWhen::DuringSuspend {
                group.notifier.notify();
                // State machine is waiting for the client to wake up after we
                // assert the wake signal.
                group.run_until_stalled(&mut exec);
                assert_wake_signal_set(group.token());
            }
        }

        let expected_reason = match shutdown_trigger {
            ShutdownTrigger::DropWaker => {
                group.close_client();
                WakeGroupShutdownReason::WakeWatcherClosed
            }
            ShutdownTrigger::RaiseInvalidSignals => {
                group.raise_conflicting_client_states();
                WakeGroupShutdownReason::AwakeAndAsleepAsserted
            }
        };

        group.run_until_shutdown(&mut exec, expected_reason);
        assert!(!group.is_registered(&wake_groups));
        assert_wake_signal_clear(group.token());
    }

    #[test_case(false; "without_pre_suspend_notification")]
    #[test_case(true; "with_pre_suspend_notification")]
    fn wake_group_happy_path_signals(notify_before_suspend: bool) {
        let mut exec = fasync::TestExecutor::new();

        let wake_groups = WakeGroups::default();
        let mut group = TestWakeGroup::new(&wake_groups, "test-group");

        group.run_until_stalled(&mut exec);
        assert_wake_signal_clear(group.token());

        // If a notification comes in before the client suspends, the netstack
        // shouldn't assert the wake signal, either immediately or after the
        // client does suspend.
        if notify_before_suspend {
            group.notifier.notify();
            group.run_until_stalled(&mut exec);
            assert_wake_signal_clear(group.token());
        }

        group.suspend_client();
        group.run_until_stalled(&mut exec);
        assert_wake_signal_clear(group.token());

        group.notifier.notify();
        group.run_until_stalled(&mut exec);
        assert_wake_signal_set(group.token());

        group.wake_client();
        group.run_until_stalled(&mut exec);
        assert_wake_signal_clear(group.token());
    }
}
