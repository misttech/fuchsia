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
use futures::channel::{mpsc, oneshot};
use futures::future::FutureExt as _;
use futures::{StreamExt as _, TryStreamExt as _};
use log::{debug, info, warn};

use crate::bindings::sync::CoreMutex;
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
pub(crate) struct WakeGroups(Arc<CoreMutex<WakeGroupsInner>>);

#[derive(Default)]
struct WakeGroupsInner {
    wake_groups: HashMap<zx::Koid, WakeGroupInputs>,
}

#[derive(Debug)]
struct WakeGroupInputs {
    data_notifier: DataNotifier,
    suspend_events: mpsc::UnboundedSender<SuspendEvent>,
}

enum SuspendEvent {
    /// The system is about to suspend. The group completes the sender once it
    /// has handled the notice.
    Suspending(oneshot::Sender<()>),
    Resumed,
}

impl WakeGroups {
    /// Notifies every wake group that the system is about to suspend. The
    /// returned future completes once every group has acknowledged.
    ///
    /// NOTE: Must be followed by [`WakeGroups::resume`] once the system
    /// resumes.
    pub(crate) fn suspend(&self) -> impl Future<Output = ()> + use<> {
        let Self(inner) = self;
        let acks = inner
            .lock()
            .wake_groups
            .values()
            .map(|WakeGroupInputs { data_notifier: _, suspend_events }| {
                let (sender, receiver) = oneshot::channel();
                // The group's task may have been dropped before it started
                // serving, in which case the dropped sender resolves the ack as
                // canceled.
                let _: Result<(), mpsc::TrySendError<SuspendEvent>> =
                    suspend_events.unbounded_send(SuspendEvent::Suspending(sender));
                receiver
            })
            .collect::<Vec<_>>();
        // A group that shuts down before acknowledging no longer cares about
        // suspension.
        futures::future::join_all(acks).map(|_: Vec<Result<(), oneshot::Canceled>>| ())
    }

    pub(crate) fn resume(&self) {
        let Self(inner) = self;
        for WakeGroupInputs { data_notifier: _, suspend_events } in
            inner.lock().wake_groups.values()
        {
            // The group's task may have been dropped before it started
            // serving, in which case it no longer cares about resumption.
            let _: Result<(), mpsc::TrySendError<SuspendEvent>> =
                suspend_events.unbounded_send(SuspendEvent::Resumed);
        }
    }

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
        let (suspend_events_sender, suspend_events_receiver) = mpsc::unbounded();
        let wake_group =
            WakeGroup::new(debug_name, data_watcher, wake_watcher, suspend_events_receiver);
        let id = wake_group.id.duplicate_for_client();

        assert_matches!(
            inner.lock().wake_groups.insert(
                id.koid,
                WakeGroupInputs { data_notifier, suspend_events: suspend_events_sender }
            ),
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
        let Some(WakeGroupInputs { data_notifier, suspend_events: _ }) =
            wake_groups.get(&wake_group.koid)
        else {
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

/// Asserts [`GROUP_WAKEUP_SIGNAL`] on a wake group's token for as long as it is
/// held.
struct WakeSignalGuard<'a>(&'a zx::Event);

impl<'a> WakeSignalGuard<'a> {
    fn raise(token: &'a zx::Event) -> Self {
        token.signal(zx::Signals::NONE, GROUP_WAKEUP_SIGNAL).expect("signal doesn't fail");
        Self(token)
    }
}

impl Drop for WakeSignalGuard<'_> {
    fn drop(&mut self) {
        let Self(token) = self;
        token.signal(GROUP_WAKEUP_SIGNAL, zx::Signals::NONE).expect("signal doesn't fail");
    }
}

#[derive(Debug)]
struct WakeGroup {
    name: String,
    id: WakeGroupId,
    data_watcher: DataWatcher,
    wake_watcher: zx::EventPair,
    suspend_events: mpsc::UnboundedReceiver<SuspendEvent>,
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
    fn new(
        name: String,
        data_watcher: DataWatcher,
        wake_watcher: zx::EventPair,
        suspend_events: mpsc::UnboundedReceiver<SuspendEvent>,
    ) -> Self {
        Self { name, id: WakeGroupId::new(), data_watcher, wake_watcher, suspend_events }
    }

    async fn serve(self, wake_groups: WakeGroups) -> Result<!, WakeGroupShutdownReason> {
        let Self { name, id, data_watcher, wake_watcher, mut suspend_events } = self;
        let WakeGroupId { token, koid: _ } = &id;

        enum WakeGroupState<'a> {
            Awake,
            Asleep,
            Waking(WakeSignalGuard<'a>),
            SystemSuspended,
        }

        // Start by assuming the client is awake. If they're asleep we'll catch
        // it immediately during the loop.
        let mut state = WakeGroupState::Awake;

        let _cleanup = scopeguard::guard((wake_groups, &id), |(wake_groups, id)| {
            // Note that sockets that were attached to this wake group still
            // hold onto their DataNotifiers, but without a data watcher, those
            // notifications are no-ops.
            wake_groups.remove_wake_group(id.koid);
            debug!("removed wake group '{name}' {id:?}");
        });

        let mut next_suspend_event = async || {
            suspend_events.next().await.expect("suspend event channel outlives wake group task")
        };

        loop {
            state = match state {
                WakeGroupState::Awake => {
                    // Other side is (probably) running. There's nothing for us
                    // to do except wait for them to suspend.
                    debug!("wake group '{name}' {id:?} waiting for suspend");
                    futures::select_biased! {
                        res = wait_for_client_state(&wake_watcher, ClientState::Asleep).fuse() => {
                            res?;

                            data_watcher.reset();

                            WakeGroupState::Asleep
                        }
                        event = next_suspend_event().fuse() => {
                            match event {
                                SuspendEvent::Suspending(ack) => {
                                    // NOTE: Data that arrives after this reset
                                    // but before the client falls asleep causes
                                    // a spurious wakeup on resume. The
                                    // alternative is missing data notifications
                                    // on resume if we haven't seen the client
                                    // suspend yet.
                                    data_watcher.reset();
                                    // Completing the sender may fail on
                                    // shutdown because the power worker
                                    // teardown isn't synchronized with us.
                                    let _: Result<(), ()> = ack.send(());

                                    WakeGroupState::SystemSuspended
                                }
                                SuspendEvent::Resumed => WakeGroupState::Awake,
                            }
                        }
                    }
                }
                WakeGroupState::Asleep => {
                    // Bias in favor of client wakeup. In that case, raising the
                    // wake signal is pointless.
                    futures::select_biased! {
                        res = wait_for_client_state(&wake_watcher, ClientState::Awake).fuse() => {
                            res?;

                            // The other side woke up without us. Nothing left to do here.
                            WakeGroupState::Awake
                        }
                        () = data_watcher.wait().fuse() => {
                            // Assert the wake signal to wake up the other side.
                            let signal = WakeSignalGuard::raise(token);
                            debug!("notified wake group '{name}' {id:?} of incoming data");

                            WakeGroupState::Waking(signal)
                        }
                        event = next_suspend_event().fuse() => {
                            if let SuspendEvent::Suspending(ack) = event {
                                let _: Result<(), ()> = ack.send(());
                            }

                            WakeGroupState::Asleep
                        }
                    }
                }
                WakeGroupState::Waking(signal) => {
                    futures::select_biased! {
                        res = wait_for_client_state(&wake_watcher, ClientState::Awake).fuse() => {
                            res?;

                            // TODO(https://fxbug.dev/538164589): Hold delegated wake
                            // leases from netdevice until the client is awake.

                            // Deassert wake signal. When the other side is awake, we
                            // assume they're capable of staying awake until they're
                            // done processing the incoming data.
                            drop(signal);
                            debug!("deasserted wake signal on '{name}' {id:?}");

                            WakeGroupState::Awake
                        }
                        // The client is being woken, and the wake signal is
                        // cleared only once the client is awake.
                        event = next_suspend_event().fuse() => {
                            if let SuspendEvent::Suspending(ack) = event {
                                let _: Result<(), ()> = ack.send(());
                            }

                            WakeGroupState::Waking(signal)
                        }
                    }
                }
                WakeGroupState::SystemSuspended => {
                    match next_suspend_event().await {
                        SuspendEvent::Suspending(ack) => {
                            warn!(
                                "wake group '{name}' {id:?} told to suspend while already suspended"
                            );
                            let _: Result<(), ()> = ack.send(());

                            WakeGroupState::SystemSuspended
                        }
                        // The Asleep state checks for an awake client before
                        // acting on pending data, so an awake client moves
                        // on to Awake without raising the wake signal.
                        SuspendEvent::Resumed => WakeGroupState::Asleep,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testutils {
    use super::*;

    use futures::task::Poll;
    use zx::Peered as _;

    pub(crate) fn assert_wake_signal_clear(token: &zx::Event) {
        assert_matches!(
            token.wait_one(GROUP_WAKEUP_SIGNAL, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(_)
        );
    }

    pub(crate) fn assert_wake_signal_set(token: &zx::Event) {
        let observed =
            token.wait_one(GROUP_WAKEUP_SIGNAL, zx::MonotonicInstant::INFINITE_PAST).unwrap();
        assert!(observed.contains(GROUP_WAKEUP_SIGNAL));
    }

    /// A wake group registered with a [`WakeGroups`], along with everything a
    /// test needs to drive it.
    pub(crate) struct TestWakeGroup {
        /// Stands in for the client, signaling its power state to the group.
        /// `None` once the client has gone away.
        signaller: Option<zx::EventPair>,
        id: WakeGroupId,
        pub(crate) notifier: DataNotifier,
        /// Boxed rather than spawned as an [`fasync::Task`] so the executor
        /// only polls the group when [`Self::run_until_stalled`] or
        /// [`Self::run_until_shutdown`] is called.
        serve_fut: futures::future::LocalBoxFuture<'static, Result<!, WakeGroupShutdownReason>>,
    }

    impl TestWakeGroup {
        pub(crate) fn new(wake_groups: &WakeGroups, name: &str) -> Self {
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
                serve_fut: wake_group.serve(wake_groups.clone()).boxed_local(),
            }
        }

        pub(crate) fn token(&self) -> &zx::Event {
            &self.id.token
        }

        fn signaller(&self) -> &zx::EventPair {
            self.signaller.as_ref().expect("client has gone away")
        }

        pub(crate) fn suspend_client(&self) {
            self.signaller().signal_peer(WAITER_AWAKE_SIGNAL, WAITER_ASLEEP_SIGNAL).unwrap();
        }

        pub(super) fn wake_client(&self) {
            self.signaller().signal_peer(WAITER_ASLEEP_SIGNAL, WAITER_AWAKE_SIGNAL).unwrap();
        }

        /// Closes the client end of the wake watcher, which the group observes
        /// as the client going away for good.
        pub(super) fn close_client(&mut self) {
            drop(self.signaller.take().expect("client has already gone away"));
        }

        /// Violates the protocol by claiming to be awake and asleep at once.
        pub(super) fn raise_conflicting_client_states(&self) {
            self.signaller()
                .signal_peer(zx::Signals::NONE, WAITER_AWAKE_SIGNAL | WAITER_ASLEEP_SIGNAL)
                .unwrap();
        }

        pub(super) fn is_registered(&self, wake_groups: &WakeGroups) -> bool {
            wake_groups.0.lock().wake_groups.contains_key(&self.id.koid)
        }

        /// Lets the group's serving task observe everything that has happened
        /// to it, asserting that it stays alive.
        pub(crate) fn run_until_stalled(&mut self, exec: &mut fasync::TestExecutor) {
            assert_eq!(exec.run_until_stalled(&mut self.serve_fut), Poll::Pending);
        }

        /// Lets the group's serving task observe everything that has happened
        /// to it, asserting that it shuts down for `reason`.
        pub(super) fn run_until_shutdown(
            &mut self,
            exec: &mut fasync::TestExecutor,
            reason: WakeGroupShutdownReason,
        ) {
            assert_eq!(exec.run_until_stalled(&mut self.serve_fut), Poll::Ready(Err(reason)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use test_case::{test_case, test_matrix};

    use crate::bindings::waker::testutils::{
        TestWakeGroup, assert_wake_signal_clear, assert_wake_signal_set,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TriggerShutdownWhen {
        BeforeSuspend,
        DuringSuspend,
        AfterData,
        WhileSystemSuspended,
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
            TriggerShutdownWhen::WhileSystemSuspended,
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

        match shutdown_when {
            TriggerShutdownWhen::BeforeSuspend => {}
            TriggerShutdownWhen::WhileSystemSuspended => {
                // Suspend the system while the client is still awake, so the
                // group stops watching the client until the system resumes.
                let suspend_fut = wake_groups.suspend();
                group.run_until_stalled(&mut exec);
                assert_eq!(suspend_fut.now_or_never(), Some(()));
            }
            TriggerShutdownWhen::DuringSuspend | TriggerShutdownWhen::AfterData => {
                group.suspend_client();
                // State machine is waiting for data to come in or the client to
                // wake.
                group.run_until_stalled(&mut exec);

                if shutdown_when == TriggerShutdownWhen::AfterData {
                    group.notifier.notify();
                    // State machine is waiting for the client to wake up after
                    // we assert the wake signal.
                    group.run_until_stalled(&mut exec);
                    assert_wake_signal_set(group.token());
                }
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

        if shutdown_when == TriggerShutdownWhen::WhileSystemSuspended {
            // Not observed until the system resumes.
            group.run_until_stalled(&mut exec);
            wake_groups.resume();
        }
        // A group that shuts down with a suspend notice outstanding must not
        // keep blocking system suspension.
        let suspend_fut = wake_groups.suspend();

        group.run_until_shutdown(&mut exec, expected_reason);
        assert!(!group.is_registered(&wake_groups));
        assert_wake_signal_clear(group.token());
        assert_eq!(suspend_fut.now_or_never(), Some(()));
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

    #[test_case(false, false, true, false; "client_awake_with_data")]
    #[test_case(false, true, false, false; "client_asleep_without_data")]
    #[test_case(false, true, true, true; "client_asleep_with_data")]
    #[test_case(true, true, false, false; "client_asleep_with_stale_data")]
    fn system_suspend_before_client_observed_asleep(
        notify_before_suspend: bool,
        client_asleep: bool,
        notify_while_suspended: bool,
        expect_wake: bool,
    ) {
        let mut exec = fasync::TestExecutor::new();

        let wake_groups = WakeGroups::default();
        let mut group = TestWakeGroup::new(&wake_groups, "test-group");
        group.run_until_stalled(&mut exec);

        if notify_before_suspend {
            group.notifier.notify();
            group.run_until_stalled(&mut exec);
        }

        let suspend_fut = wake_groups.suspend();
        group.run_until_stalled(&mut exec);
        assert_eq!(suspend_fut.now_or_never(), Some(()));

        if client_asleep {
            group.suspend_client();
        }
        if notify_while_suspended {
            group.notifier.notify();
        }
        group.run_until_stalled(&mut exec);
        // Nothing happens until the group learns that the system resumed.
        assert_wake_signal_clear(group.token());

        wake_groups.resume();
        group.run_until_stalled(&mut exec);
        if expect_wake {
            assert_wake_signal_set(group.token());
        } else {
            assert_wake_signal_clear(group.token());
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SuspendWhile {
        /// The group has observed the client asleep.
        Asleep,
        /// The client is asleep, but the group hasn't run since.
        AsleepUnobserved,
        /// The group has raised the wake signal for the asleep client.
        Waking,
    }

    #[test_matrix(
        [SuspendWhile::Asleep, SuspendWhile::AsleepUnobserved, SuspendWhile::Waking],
        [false, true]
    )]
    fn system_suspend_after_client_observed_asleep(
        suspend_while: SuspendWhile,
        resume_before_data: bool,
    ) {
        let mut exec = fasync::TestExecutor::new();

        let wake_groups = WakeGroups::default();
        let mut group = TestWakeGroup::new(&wake_groups, "test-group");
        group.run_until_stalled(&mut exec);

        group.suspend_client();
        match suspend_while {
            SuspendWhile::Asleep => group.run_until_stalled(&mut exec),
            SuspendWhile::AsleepUnobserved => {}
            SuspendWhile::Waking => {
                group.run_until_stalled(&mut exec);
                group.notifier.notify();
                group.run_until_stalled(&mut exec);
                assert_wake_signal_set(group.token());
            }
        }

        // Only the group's serving task acknowledges the suspend notice.
        let mut suspend_fut = wake_groups.suspend();
        assert_eq!((&mut suspend_fut).now_or_never(), None);
        group.run_until_stalled(&mut exec);
        assert_eq!(suspend_fut.now_or_never(), Some(()));

        if resume_before_data {
            wake_groups.resume();
            group.run_until_stalled(&mut exec);
            match suspend_while {
                SuspendWhile::Asleep | SuspendWhile::AsleepUnobserved => {
                    assert_wake_signal_clear(group.token())
                }
                SuspendWhile::Waking => assert_wake_signal_set(group.token()),
            }
        }

        // Data wakes the client whether or not the system has resumed.
        group.notifier.notify();
        group.run_until_stalled(&mut exec);
        assert_wake_signal_set(group.token());

        if !resume_before_data {
            wake_groups.resume();
            group.run_until_stalled(&mut exec);
            assert_wake_signal_set(group.token());
        }

        group.wake_client();
        group.run_until_stalled(&mut exec);
        assert_wake_signal_clear(group.token());
    }
}
