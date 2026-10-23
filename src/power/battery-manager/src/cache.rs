// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Serves `Battery` and `Charger` to charge governors from a cache of the driver statuses.
//!
//! battery-manager is the drivers' only client, so a governor adds no bus reads. Each governor
//! connection behaves like a driver connection, limited to what battery-manager's own connection
//! delivers:
//! - `Watch` completes when a field of interest changes. A `wake_on` change carries a copy of the
//!   driver's wake lease.
//! - `Watch` also completes with a fresh sample once `max_interval` (at least
//!   [`MIN_SAMPLE_INTERVAL`]) has passed. With `wake_on_max_interval`, the sample wakes the system.
//!   See [`Cache::run_sampler`].
//! - `GetStatus` and `GetSpec` are answered from the cache.
//! - While a driver is disconnected, `GetStatus` and `GetSpec` fail with `IO`, and a pending
//!   `Watch` waits for the next driver.

use anyhow::{Error, anyhow};
use election::Election;
use fidl::endpoints::Proxy;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_time_alarms as falarms;
use fuchsia_async as fasync;
use fuchsia_inspect as inspect;
use futures::channel::mpsc;
use futures::future::{Either, LocalBoxFuture, OptionFuture};
use futures::{FutureExt, StreamExt, TryStreamExt};
use log::{debug, warn};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::future::Future;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

type ConnectionId = u64;

/// Shortest `max_interval` honored. Shorter requests, including non-positive ones, are clamped up
/// to it.
pub(crate) const MIN_SAMPLE_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(1);

/// A sample also serves the connections whose `max_interval` ends within this window after the
/// sample, so that connections with similar intervals share driver reads and wakeups.
const SAMPLE_COALESCE_WINDOW: zx::BootDuration = zx::BootDuration::from_millis(100);

/// ID of the wake alarm that schedules samples. All governors share one alarm.
const SAMPLE_ALARM_ID: &str = "battery_manager_sample";

const INSPECTOR_NODE_GOVERNOR_TELEMETRY: &str = "governor_telemetry";
const INSPECTOR_NODE_BATTERY_SAMPLE_INTERVAL: &str = "battery_sample_interval";
const INSPECTOR_NODE_CHARGER_SAMPLE_INTERVAL: &str = "charger_sample_interval";
const INSPECTOR_PROP_WAKEUP: &str = "wakeup";
const INSPECTOR_PROP_SAMPLE_COUNT: &str = "sample_count";
const WAKEUP_WAKE_ALARMS: &str = "wake_alarms";
const WAKEUP_TIMER: &str = "timer";

/// Per-governor `max_interval` votes, keyed by the governor's child name, in milliseconds.
type IntervalVotes = Election<String, u64>;

/// How [`Cache::run_sampler`] waits for a sample deadline.
pub(crate) enum Wakeup {
    /// A wake alarm, which fires even if the system is suspended.
    WakeAlarms(falarms::WakeAlarmsProxy),
    /// A boot timer, which does not wake the system from suspend.
    Timer,
}

/// Resolves at a sample deadline with the wake lease of the alarm that fired, if any.
type WakeFuture = LocalBoxFuture<'static, Result<Option<zx::EventPair>, Error>>;

impl Wakeup {
    fn at(&self, deadline: fasync::BootInstant) -> WakeFuture {
        match self {
            Wakeup::WakeAlarms(alarms) => {
                // Setting the alarm again with the same ID reschedules it.
                let response = alarms.set_and_wait(
                    deadline.into_zx(),
                    falarms::SetMode::NotifySetupDone(zx::Event::create()),
                    SAMPLE_ALARM_ID,
                );
                async move {
                    match response.await {
                        Ok(Ok(lease)) => Ok(Some(lease)),
                        Ok(Err(e)) => Err(anyhow!("SetAndWait failed: {e:?}")),
                        Err(e) => Err(Error::from(e).context("SetAndWait")),
                    }
                }
                .boxed_local()
            }
            Wakeup::Timer => timer_at(deadline),
        }
    }

    fn cancel(&self) {
        if let Wakeup::WakeAlarms(alarms) = self
            && let Err(e) = alarms.cancel(SAMPLE_ALARM_ID)
            && !e.is_closed()
        {
            warn!("Failed to cancel wake alarm: {e:?}");
        }
    }

    /// Whether wake alarms can no longer be set, because the service closed the channel.
    fn is_unavailable(&self) -> bool {
        match self {
            Wakeup::WakeAlarms(alarms) => alarms.is_closed(),
            Wakeup::Timer => true,
        }
    }
}

/// Waits for the armed wake-up in `armed`, if any, or forever.
fn armed_or_pending(
    armed: &mut Option<(fasync::BootInstant, WakeFuture)>,
) -> impl Future<Output = Result<Option<zx::EventPair>, Error>> + Unpin + '_ {
    match armed {
        Some((_, wake)) => Either::Left(wake),
        None => Either::Right(futures::future::pending()),
    }
}

fn timer_at(deadline: fasync::BootInstant) -> WakeFuture {
    fasync::Timer::new(deadline).map(|()| Ok(None)).boxed_local()
}

fn log_sample_result<S, E: Debug>(
    driver: &str,
    result: Result<Result<S, E>, fidl::Error>,
) -> Option<S> {
    match result {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            warn!("Failed to sample {driver} status: {e:?}");
            None
        }
        Err(e) => {
            warn!("FIDL error sampling {driver} status: {e:?}");
            None
        }
    }
}

/// Field-mask operations on a `Status` table. In a mask, only field presence matters.
pub(crate) trait StatusFields: Clone + Debug + Default + PartialEq {
    /// Returns the fields of `self` that are present in `mask`.
    fn masked(&self, mask: &Self) -> Self;

    /// Returns `self` plus the fields of `other` that `self` lacks.
    fn union(&self, other: &Self) -> Self;

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Implements [`StatusFields`] for a `Status` table. Must list every field of the table: an
/// omitted field is never of interest and never compared.
macro_rules! impl_status_fields {
    ($status:ty { $($field:ident),+ $(,)? }) => {
        impl StatusFields for $status {
            fn masked(&self, mask: &Self) -> Self {
                Self { $($field: mask.$field.and(self.$field),)+ ..Default::default() }
            }

            fn union(&self, other: &Self) -> Self {
                Self { $($field: self.$field.or(other.$field),)+ ..Default::default() }
            }
        }
    };
}

impl_status_fields!(fbattery::Status {
    present,
    voltage_uv,
    current_ua,
    level_percent,
    temp_celsius,
    remaining_capacity_uah,
    full_charge_capacity_uah,
    health,
    cycle_count,
    time_remaining,
});

impl_status_fields!(fcharger::Status {
    online,
    input_voltage_uv,
    input_current_ua,
    operating_mode,
    charge_phase,
    input_current_limit_ua,
    charge_current_limit_ua,
    float_voltage_uv,
    source_type,
    health,
    charge_term_current_ua,
    input_voltage_limit_uv,
    top_off_duration,
});

/// A field mask that may cover every field.
#[derive(Clone, Debug, PartialEq)]
enum Mask<S> {
    /// Every field, including any the driver does not report.
    All,
    /// The fields present in `S`.
    Fields(S),
}

impl<S: StatusFields> Mask<S> {
    fn fields(&self) -> Option<&S> {
        match self {
            Mask::All => None,
            Mask::Fields(fields) => Some(fields),
        }
    }

    fn is_empty(&self) -> bool {
        self.fields().is_some_and(S::is_empty)
    }

    fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Mask::All, mask) | (mask, Mask::All) => mask.clone(),
            (Mask::Fields(a), Mask::Fields(b)) => Mask::Fields(a.masked(b)),
        }
    }

    fn with(&self, fields: &S) -> Self {
        match self {
            Mask::All => Mask::All,
            Mask::Fields(mask) => Mask::Fields(mask.union(fields)),
        }
    }

    fn apply(&self, status: &S) -> S {
        match self {
            Mask::All => status.clone(),
            Mask::Fields(mask) => status.masked(mask),
        }
    }
}

/// `WatchOptions` of either protocol.
#[derive(Default)]
struct Options<S> {
    interest: Option<S>,
    wake_on: Option<S>,
    max_interval: Option<zx::BootDuration>,
    wake_on_max_interval: Option<bool>,
}

impl<S: StatusFields> Options<S> {
    /// Whether any field is requested.
    fn has_fields(&self) -> bool {
        [&self.interest, &self.wake_on].into_iter().flatten().any(|mask| !mask.is_empty())
    }
}

/// Fields whose changes complete a `Watch`, and those whose changes also wake the system for it:
/// battery-manager's own subscription to a driver, or what a governor connection is served of it.
#[derive(Clone, Debug)]
pub(crate) struct Subscription<S> {
    interest: Mask<S>,
    wake_on: S,
}

impl<S: StatusFields> Subscription<S> {
    /// Derives battery-manager's subscription to a driver.
    ///
    /// `supported_interest` is the driver's `Spec.supported_options.interest`, or `None` if
    /// unknown. `interest` and `wake_on` are what the driver's `ConfigureWatch` returned, and are
    /// `None` if the connection was not configured.
    pub(crate) fn from_driver(
        supported_interest: Option<S>,
        interest: Option<S>,
        wake_on: Option<S>,
    ) -> Self {
        let wake_on = wake_on.unwrap_or_default();
        // An absent `interest` means every field the driver supports watching, and `wake_on`
        // fields are implicitly of interest.
        let interest =
            interest.or(supported_interest).map_or(Mask::All, Mask::Fields).with(&wake_on);
        Self { interest, wake_on }
    }

    /// Returns what a governor connection that requested `requested` is served of `self`. An
    /// absent `interest` means every field of `self`, and `wake_on` fields are of interest.
    fn limit(&self, requested: &Options<S>) -> Self {
        let mut interest =
            requested.interest.clone().map_or_else(|| self.interest.clone(), Mask::Fields);
        if let Some(wake_on) = &requested.wake_on {
            interest = interest.with(wake_on);
        }
        Self {
            interest: interest.intersect(&self.interest),
            wake_on: requested
                .wake_on
                .as_ref()
                .map_or_else(S::default, |w| w.masked(&self.wake_on)),
        }
    }

    fn is_empty(&self) -> bool {
        self.interest.is_empty() && self.wake_on.is_empty()
    }

    /// Returns the options `ConfigureWatch` reports for `requested`, limited to `self`. Absent
    /// in, absent out: the governor stays on the default rather than being pinned to the current
    /// subscription.
    fn effective(&self, requested: &Options<S>) -> Options<S> {
        Options {
            interest: requested.interest.as_ref().and_then(|_| self.interest.fields()).cloned(),
            wake_on: requested.wake_on.as_ref().map(|_| self.wake_on.clone()),
            ..Default::default()
        }
    }

    /// Returns the options advertised in `Spec.supported_options`.
    fn supported_options(&self) -> Options<S> {
        Options {
            interest: self.interest.fields().cloned(),
            wake_on: Some(self.wake_on.clone()),
            ..Default::default()
        }
    }

    /// Whether `status` differs from `last_sent` in a field of interest.
    fn is_news(&self, status: &S, last_sent: &S) -> bool {
        self.interest.apply(status) != self.interest.apply(last_sent)
    }

    /// Whether `status` differs from `last_sent` in a `wake_on` field.
    fn is_wake_on_change(&self, status: &S, last_sent: &S) -> bool {
        status.masked(&self.wake_on) != last_sent.masked(&self.wake_on)
    }
}

impl Subscription<fbattery::Status> {
    /// Derives battery-manager's subscription to a battery driver from its optional `spec` and
    /// `ConfigureWatch` response.
    pub(crate) fn from_battery_driver(
        spec: Option<&fbattery::Spec>,
        effective: Option<fbattery::WatchOptions>,
    ) -> Self {
        let effective = effective.unwrap_or_default();
        Self::from_driver(
            spec.and_then(|s| s.supported_options.as_ref()).and_then(|o| o.interest.clone()),
            effective.interest,
            effective.wake_on,
        )
    }
}

impl Subscription<fcharger::Status> {
    /// Derives battery-manager's subscription to a charger driver from its optional `spec` and
    /// `ConfigureWatch` response.
    pub(crate) fn from_charger_driver(
        spec: Option<&fcharger::Spec>,
        effective: Option<fcharger::WatchOptions>,
    ) -> Self {
        let effective = effective.unwrap_or_default();
        Self::from_driver(
            spec.and_then(|s| s.supported_options.as_ref()).and_then(|o| o.interest.clone()),
            effective.interest,
            effective.wake_on,
        )
    }
}

/// Glue between [`Topic`] and the FIDL bindings of one cached protocol.
trait Protocol {
    type Status: StatusFields;
    type Spec: Clone;
    type Proxy: Clone;
    type Error: Copy;
    type WatchResponder;

    /// Returned by `GetStatus` and `GetSpec` while no driver is connected.
    const ERROR_UNAVAILABLE: Self::Error;
    /// Returned by `GetSpec` if the driver has no spec, and by `ConfigureWatch` if none of the
    /// requested options can be served.
    const ERROR_NOT_SUPPORTED: Self::Error;
    /// Returned by a `Watch` while another is pending on the same connection.
    const ERROR_ALREADY_WATCHING: Self::Error;

    fn send_watch(
        responder: Self::WatchResponder,
        response: Result<(&Self::Status, Option<zx::EventPair>), Self::Error>,
    ) -> Result<(), fidl::Error>;

    /// Returns `spec` with `supported_options`.
    fn with_supported_options(spec: &Self::Spec, options: Options<Self::Status>) -> Self::Spec;
}

struct Battery;

impl Protocol for Battery {
    type Status = fbattery::Status;
    type Spec = fbattery::Spec;
    type Proxy = fbattery::BatteryProxy;
    type Error = fbattery::Error;
    type WatchResponder = fbattery::BatteryWatchResponder;

    const ERROR_UNAVAILABLE: fbattery::Error = fbattery::Error::Io;
    const ERROR_NOT_SUPPORTED: fbattery::Error = fbattery::Error::NotSupported;
    const ERROR_ALREADY_WATCHING: fbattery::Error = fbattery::Error::AlreadyWatching;

    fn send_watch(
        responder: fbattery::BatteryWatchResponder,
        response: Result<(&fbattery::Status, Option<zx::EventPair>), fbattery::Error>,
    ) -> Result<(), fidl::Error> {
        responder.send(response)
    }

    fn with_supported_options(
        spec: &fbattery::Spec,
        options: Options<fbattery::Status>,
    ) -> fbattery::Spec {
        fbattery::Spec { supported_options: Some(options.into()), ..spec.clone() }
    }
}

impl From<fbattery::WatchOptions> for Options<fbattery::Status> {
    fn from(options: fbattery::WatchOptions) -> Self {
        Self {
            interest: options.interest,
            wake_on: options.wake_on,
            max_interval: options.max_interval.map(zx::BootDuration::from_nanos),
            wake_on_max_interval: options.wake_on_max_interval,
        }
    }
}

impl From<Options<fbattery::Status>> for fbattery::WatchOptions {
    fn from(options: Options<fbattery::Status>) -> Self {
        Self {
            interest: options.interest,
            wake_on: options.wake_on,
            max_interval: options.max_interval.map(|interval| interval.into_nanos()),
            wake_on_max_interval: options.wake_on_max_interval,
            ..Default::default()
        }
    }
}

struct Charger;

impl Protocol for Charger {
    type Status = fcharger::Status;
    type Spec = fcharger::Spec;
    type Proxy = fcharger::ChargerProxy;
    type Error = fcharger::Error;
    type WatchResponder = fcharger::ChargerWatchResponder;

    const ERROR_UNAVAILABLE: fcharger::Error = fcharger::Error::Io;
    const ERROR_NOT_SUPPORTED: fcharger::Error = fcharger::Error::NotSupported;
    const ERROR_ALREADY_WATCHING: fcharger::Error = fcharger::Error::AlreadyWatching;

    fn send_watch(
        responder: fcharger::ChargerWatchResponder,
        response: Result<(&fcharger::Status, Option<zx::EventPair>), fcharger::Error>,
    ) -> Result<(), fidl::Error> {
        responder.send(response)
    }

    fn with_supported_options(
        spec: &fcharger::Spec,
        options: Options<fcharger::Status>,
    ) -> fcharger::Spec {
        fcharger::Spec { supported_options: Some(options.into()), ..spec.clone() }
    }
}

impl From<fcharger::WatchOptions> for Options<fcharger::Status> {
    fn from(options: fcharger::WatchOptions) -> Self {
        Self {
            interest: options.interest,
            wake_on: options.wake_on,
            max_interval: options.max_interval.map(zx::BootDuration::from_nanos),
            wake_on_max_interval: options.wake_on_max_interval,
        }
    }
}

impl From<Options<fcharger::Status>> for fcharger::WatchOptions {
    fn from(options: Options<fcharger::Status>) -> Self {
        Self {
            interest: options.interest,
            wake_on: options.wake_on,
            max_interval: options.max_interval.map(|interval| interval.into_nanos()),
            wake_on_max_interval: options.wake_on_max_interval,
            ..Default::default()
        }
    }
}

/// Duplicates `lease`, if any. Returns `None` if duplication fails.
pub(crate) fn duplicate_lease(lease: Option<&zx::EventPair>) -> Option<zx::EventPair> {
    lease.and_then(|lease| lease.duplicate_handle(zx::Rights::SAME_RIGHTS).ok())
}

/// One governor connection to a cached protocol.
struct Connection<P: Protocol> {
    /// Child name of the governor that opened the connection.
    owner: String,
    /// Options last requested through `ConfigureWatch`.
    requested: Options<P::Status>,
    /// `requested`, limited to battery-manager's current subscription to the driver.
    subscription: Subscription<P::Status>,
    /// `max_interval` as last requested through `ConfigureWatch`, clamped to
    /// [`MIN_SAMPLE_INTERVAL`].
    max_interval: Option<zx::BootDuration>,
    /// Whether samples for `max_interval` wake the system, as last configured and honored.
    wake_on_max_interval: bool,
    /// Start of the current `max_interval`: the last response, or the last sample that could not
    /// be taken for this connection.
    interval_start: Option<fasync::BootInstant>,
    /// Status last sent in a `Watch` response, or `None` before the first response.
    last_sent: Option<P::Status>,
    /// Whether the next `Watch` completes as soon as a status is cached: initially, and once a
    /// field of interest changed since `last_sent` while no `Watch` was pending.
    dirty: bool,
    pending_watch: Option<P::WatchResponder>,
    /// Wake lease of a `wake_on` change or waking sample yet to be sent, returned with the
    /// response that sends it. A waking sample is only due while `Watch` is pending, so its lease
    /// is passed straight from [`Self::offer`] to [`Self::respond`].
    stashed_lease: Option<zx::EventPair>,
}

impl<P: Protocol> Connection<P> {
    /// Whether `status` should complete this connection's `Watch`.
    fn is_news(&self, status: &P::Status) -> bool {
        self.last_sent.as_ref().is_none_or(|last_sent| self.subscription.is_news(status, last_sent))
    }

    /// When a sample must complete this connection's pending `Watch`.
    fn sample_deadline(&self) -> Option<fasync::BootInstant> {
        if self.pending_watch.is_none() {
            return None;
        }
        Some(self.interval_start? + self.max_interval?)
    }

    /// Whether a sample taken at `now` should complete this connection's pending `Watch`.
    ///
    /// When `!include_waking`, a connection with `wake_on_max_interval` is not treated as due, so
    /// that a non-waking timer or driver push cannot complete it without a wake lease.
    fn is_due(&self, now: fasync::BootInstant, include_waking: bool) -> bool {
        (include_waking || !self.wake_on_max_interval)
            && self
                .sample_deadline()
                .is_some_and(|deadline| deadline <= now + SAMPLE_COALESCE_WINDOW)
    }

    /// Whether `status` changes a field in this connection's `wake_on` relative to the status last
    /// sent. The first response is a snapshot, so it never counts as a `wake_on` change.
    fn is_wake_on_change(&self, status: &P::Status) -> bool {
        self.last_sent
            .as_ref()
            .is_some_and(|last_sent| self.subscription.is_wake_on_change(status, last_sent))
    }

    /// Completes the pending `Watch`, if any, with `status`. The response carries the stashed wake
    /// lease if there is one, or else `client_lease`.
    fn respond(&mut self, status: &P::Status, client_lease: Option<zx::EventPair>) {
        let Some(responder) = self.pending_watch.take() else {
            return;
        };
        self.last_sent = Some(status.clone());
        self.dirty = false;
        self.interval_start = Some(fasync::BootInstant::now());
        let wake_lease = self.stashed_lease.take().or(client_lease);
        if let Err(e) = P::send_watch(responder, Ok((status, wake_lease))) {
            debug!("Failed to send status to governor {}: {e:?}", self.owner);
        }
    }

    /// Delivers `status` if it is news to this connection, or if the connection is due for a
    /// sample: now if a `Watch` is pending, or else on the next `Watch`. A duplicate of `lease`
    /// goes with it if `status` changes a field in the connection's `wake_on`, or serves as a
    /// sample that the connection wakes the system for.
    fn offer(
        &mut self,
        status: &P::Status,
        lease: Option<&zx::EventPair>,
        now: fasync::BootInstant,
        include_waking: bool,
    ) {
        let due = self.is_due(now, include_waking);
        // A fresh status that arrives while a sample is due serves as that sample.
        if !(self.is_news(status) || due) {
            return;
        }
        let wakes = self.is_wake_on_change(status) || (self.wake_on_max_interval && due);
        if lease.is_some() && wakes {
            self.stashed_lease = duplicate_lease(lease);
        }
        if self.pending_watch.is_some() {
            self.respond(status, None);
        } else {
            self.dirty = true;
        }
    }
}

/// The driver connected to a [`Topic`].
struct Driver<P: Protocol> {
    /// `None` if the driver has no spec.
    spec: Option<P::Spec>,
    /// For on-demand reads and samples.
    proxy: P::Proxy,
    /// Latest status, or `None` before the driver's first status.
    status: Option<P::Status>,
}

/// When the next samples are due: the earliest deadline of a connection whose samples wake the
/// system, and the earliest deadline of the other connections.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SampleDeadlines {
    wake: Option<fasync::BootInstant>,
    timer: Option<fasync::BootInstant>,
}

impl SampleDeadlines {
    fn earliest(self, other: Self) -> Self {
        let min = |a: Option<fasync::BootInstant>, b| a.into_iter().chain(b).min();
        let wake = min(self.wake, other.wake);
        // A wake alarm that fires before or within the coalesce window after the timer also serves
        // the timer's connection, so the timer is not armed lest it fire first without a lease.
        let timer = min(self.timer, other.timer)
            .filter(|&timer| wake.is_none_or(|wake| wake > timer + SAMPLE_COALESCE_WINDOW));
        Self { wake, timer }
    }
}

/// Cached status and spec from one driver and the governor connections served from it.
struct Topic<P: Protocol> {
    /// `None` while no driver is connected.
    driver: Option<Driver<P>>,
    /// battery-manager's subscription to the connected driver, which bounds the governor
    /// connections' subscriptions. While none is connected, that to the last one (or the initial
    /// one).
    subscription: Subscription<P::Status>,
    connections: BTreeMap<ConnectionId, Connection<P>>,
    next_id: ConnectionId,
    interval_votes: Arc<IntervalVotes>,
    sampling: Arc<SamplingState>,
    /// Notifies the sampler that sample deadlines may have changed.
    rearm: mpsc::Sender<()>,
}

impl<P: Protocol> Topic<P> {
    fn new(
        subscription: Subscription<P::Status>,
        interval_votes: Arc<IntervalVotes>,
        sampling: Arc<SamplingState>,
        rearm: mpsc::Sender<()>,
    ) -> Self {
        Self {
            driver: None,
            subscription,
            connections: BTreeMap::new(),
            next_id: 0,
            interval_votes,
            sampling,
            rearm,
        }
    }

    fn rearm(&mut self) {
        if let Err(e) = self.rearm.try_send(()) {
            // A full channel already holds a notification the sampler has yet to see; a
            // disconnected channel means no sampler is running.
            if !e.is_full() && !e.is_disconnected() {
                warn!("Failed to notify sampler to rearm: {e}");
            }
        }
    }

    /// Casts `owner`'s vote: the shortest `max_interval` among its connections.
    fn update_vote(&self, owner: &str) {
        let vote = self
            .connections
            .values()
            .filter(|connection| connection.owner == owner)
            .filter_map(|connection| connection.max_interval)
            .min()
            .map(|interval| interval.into_millis() as u64);
        self.interval_votes.set_vote(owner.to_string(), vote);
    }

    fn add_connection(&mut self, owner: &str) -> ConnectionId {
        let id = self.next_id;
        self.next_id += 1;
        let requested = Options::default();
        let connection = Connection {
            owner: owner.to_owned(),
            subscription: self.subscription.limit(&requested),
            requested,
            max_interval: None,
            wake_on_max_interval: false,
            interval_start: None,
            last_sent: None,
            dirty: true,
            pending_watch: None,
            stashed_lease: None,
        };
        self.connections.insert(id, connection);
        id
    }

    fn remove_connection(&mut self, id: ConnectionId) {
        if let Some(connection) = self.connections.remove(&id) {
            self.update_vote(&connection.owner);
            self.rearm();
        }
    }

    fn driver_connected(
        &mut self,
        spec: Option<P::Spec>,
        subscription: Subscription<P::Status>,
        proxy: P::Proxy,
    ) {
        self.driver = Some(Driver { spec, proxy, status: None });
        for connection in self.connections.values_mut() {
            connection.subscription = subscription.limit(&connection.requested);
        }
        self.subscription = subscription;
        self.rearm();
    }

    /// Clears the cache. Governor connections stay open and resume with the next driver's status,
    /// which is compared afresh with the status last sent on each.
    fn driver_gone(&mut self) {
        self.driver = None;
        for connection in self.connections.values_mut() {
            connection.dirty = false;
            connection.stashed_lease = None;
        }
        self.rearm();
    }

    /// Latest status of the connected driver, if any.
    fn status(&self) -> Option<&P::Status> {
        self.driver.as_ref()?.status.as_ref()
    }

    /// Whether publishing `status` would complete the `Watch` of any governor connection, now or
    /// on its next `Watch`.
    fn is_news_to_any(&self, status: &P::Status) -> bool {
        self.connections.values().any(|connection| connection.is_news(status))
    }

    fn publish(&mut self, status: &P::Status, lease: Option<&zx::EventPair>) {
        let Some(driver) = &mut self.driver else {
            warn!("Ignoring status published while no driver is connected");
            return;
        };
        let now = fasync::BootInstant::now();
        let include_waking = lease.is_some() || !self.sampling.can_wake();
        for connection in self.connections.values_mut() {
            connection.offer(status, lease, now, include_waking);
        }
        driver.status = Some(status.clone());
        self.rearm();
    }

    /// Earliest sample deadlines of the connections that wait on one, if a driver is connected.
    fn next_sample_deadlines(&self) -> SampleDeadlines {
        if self.driver.is_none() {
            return SampleDeadlines::default();
        }
        self.connections.values().fold(SampleDeadlines::default(), |deadlines, connection| {
            let deadline = connection.sample_deadline();
            let next = if connection.wake_on_max_interval {
                SampleDeadlines { wake: deadline, timer: None }
            } else {
                SampleDeadlines { wake: None, timer: deadline }
            };
            deadlines.earliest(next)
        })
    }

    /// Returns the driver to read if any connection is due for a sample at `now`.
    fn due_driver(&self, now: fasync::BootInstant, include_waking: bool) -> Option<P::Proxy> {
        let proxy = &self.driver.as_ref()?.proxy;
        self.connections
            .values()
            .any(|connection| connection.is_due(now, include_waking))
            .then(|| proxy.clone())
    }

    /// Delivers the result of a sample: `status`, or `None` if the driver could not be read.
    ///
    /// Completes any governor `Watch` whose `max_interval` is due or whose interest changed, and
    /// returns whether the cached status itself changed (so battery-manager only runs `Polisher`
    /// and notifies `BatteryInfoWatcher` clients when a sampled field actually changed).
    fn sampled(
        &mut self,
        status: Option<&P::Status>,
        lease: Option<&zx::EventPair>,
        include_waking: bool,
    ) -> bool {
        let Some(driver) = &mut self.driver else {
            return false;
        };
        let now = fasync::BootInstant::now();
        let Some(status) = status else {
            // Retry after another interval rather than immediately.
            for connection in
                self.connections.values_mut().filter(|c| c.is_due(now, include_waking))
            {
                connection.interval_start = Some(now);
            }
            return false;
        };
        let changed = driver.status.as_ref() != Some(status);
        for connection in self.connections.values_mut() {
            connection.offer(status, lease, now, include_waking);
        }
        driver.status = Some(status.clone());
        changed
    }

    /// Returns the connected driver's spec, as served to governors.
    fn spec(&self) -> Result<P::Spec, P::Error> {
        let driver = self.driver.as_ref().ok_or(P::ERROR_UNAVAILABLE)?;
        let spec = driver.spec.as_ref().ok_or(P::ERROR_NOT_SUPPORTED)?;
        let supported_options = Options {
            max_interval: Some(MIN_SAMPLE_INTERVAL),
            wake_on_max_interval: self.sampling.can_wake().then_some(true),
            ..self.subscription.supported_options()
        };
        Ok(P::with_supported_options(spec, supported_options))
    }

    /// Applies a `ConfigureWatch` request and returns the effective options: `interest` is limited
    /// to the fields battery-manager is notified of, `wake_on` to those its connection wakes on,
    /// `max_interval` is clamped to [`MIN_SAMPLE_INTERVAL`], and `wake_on_max_interval` is honored
    /// when samples can wake the system and `max_interval` is set.
    fn configure_watch(
        &mut self,
        id: ConnectionId,
        requested: Options<P::Status>,
    ) -> Result<Options<P::Status>, P::Error> {
        let subscription = self.subscription.limit(&requested);
        let max_interval = requested.max_interval.map(|interval| interval.max(MIN_SAMPLE_INTERVAL));
        // Not offered if samples cannot wake the system, and ignored without `max_interval`.
        let wake_on_max_interval = requested
            .wake_on_max_interval
            .filter(|_| self.sampling.can_wake())
            .map(|wake| wake && max_interval.is_some());
        // As on a driver: if options were requested but none can be served, the configuration is
        // left unchanged. Fields that cannot be watched are still served by periodic samples.
        if requested.has_fields() && subscription.is_empty() && max_interval.is_none() {
            return Err(P::ERROR_NOT_SUPPORTED);
        }
        let effective =
            Options { max_interval, wake_on_max_interval, ..subscription.effective(&requested) };
        let status = self.driver.as_ref().and_then(|driver| driver.status.as_ref());
        let connection = self.connections.get_mut(&id).expect("connection outlives its guard");
        if subscription.wake_on.is_empty() {
            connection.stashed_lease = None;
        }
        connection.requested = requested;
        connection.subscription = subscription;
        connection.max_interval = max_interval;
        connection.wake_on_max_interval = wake_on_max_interval.unwrap_or(false);
        // The new subscription applies to a pending `Watch`, compared against the status last
        // sent. An elapsed `max_interval` is handled by `rearm` with a fresh sample rather than
        // cached status.
        if let Some(status) = status
            && connection.is_news(status)
        {
            if connection.pending_watch.is_some() {
                connection.respond(status, None);
            } else {
                connection.dirty = true;
            }
        }
        let owner = connection.owner.clone();
        self.update_vote(&owner);
        self.rearm();
        Ok(effective)
    }

    /// Handles a `Watch` with the governor's `lease`, which is handed back if the `Watch` is
    /// answered immediately and dropped if it is parked, so that a parked `Watch` never blocks
    /// suspend. A concurrent `Watch` fails with `ALREADY_WATCHING`.
    fn watch(
        &mut self,
        id: ConnectionId,
        responder: P::WatchResponder,
        lease: Option<zx::EventPair>,
    ) {
        let status = self.driver.as_ref().and_then(|driver| driver.status.as_ref());
        let connection = self.connections.get_mut(&id).expect("connection outlives its guard");
        if connection.pending_watch.is_some() {
            if let Err(e) = P::send_watch(responder, Err(P::ERROR_ALREADY_WATCHING)) {
                debug!("Failed to reject concurrent Watch from {}: {e:?}", connection.owner);
            }
            return;
        }
        connection.pending_watch = Some(responder);
        if let Some(status) = status
            && connection.dirty
        {
            connection.respond(status, lease);
        }
        // A `Watch` that outlived its `max_interval` is due for a sample now.
        self.rearm();
    }
}

/// Removes a governor connection from its topic when its server ends.
struct ConnectionGuard<'a, P: Protocol> {
    topic: &'a RefCell<Topic<P>>,
    id: ConnectionId,
}

impl<'a, P: Protocol> ConnectionGuard<'a, P> {
    fn new(topic: &'a RefCell<Topic<P>>, owner: &str) -> Self {
        let id = topic.borrow_mut().add_connection(owner);
        Self { topic, id }
    }
}

impl<P: Protocol> Drop for ConnectionGuard<'_, P> {
    fn drop(&mut self) {
        self.topic.borrow_mut().remove_connection(self.id);
    }
}

/// Sampling state shared with Inspect.
struct SamplingState {
    battery_interval_votes: Arc<IntervalVotes>,
    charger_interval_votes: Arc<IntervalVotes>,
    /// Whether samples can wake the system: false once the sampler runs without wake alarms.
    can_wake: AtomicBool,
    sample_count: AtomicU64,
}

impl SamplingState {
    fn can_wake(&self) -> bool {
        self.can_wake.load(Ordering::Relaxed)
    }

    fn record_inspect(&self, node: &inspect::Node) {
        self.battery_interval_votes.record_inspect(node, INSPECTOR_NODE_BATTERY_SAMPLE_INTERVAL);
        self.charger_interval_votes.record_inspect(node, INSPECTOR_NODE_CHARGER_SAMPLE_INTERVAL);
        let wakeup = if self.can_wake() { WAKEUP_WAKE_ALARMS } else { WAKEUP_TIMER };
        node.record_string(INSPECTOR_PROP_WAKEUP, wakeup);
        node.record_uint(INSPECTOR_PROP_SAMPLE_COUNT, self.sample_count.load(Ordering::Relaxed));
    }
}

/// Borrowed view of the cached raw battery and charger statuses and specs.
pub(crate) struct CachedStatus<'a> {
    pub(crate) battery: Option<&'a fbattery::Status>,
    pub(crate) battery_spec: Option<&'a fbattery::Spec>,
    pub(crate) charger: Option<&'a fcharger::Status>,
    pub(crate) charger_spec: Option<&'a fcharger::Spec>,
}

/// Caches battery and charger status and spec and serves `Battery` and `Charger` to governors.
pub(crate) struct Cache {
    battery: RefCell<Topic<Battery>>,
    charger: RefCell<Topic<Charger>>,
    sampling: Arc<SamplingState>,
    /// Notified whenever sample deadlines may have changed, until the sampler takes it.
    rearm_receiver: RefCell<Option<mpsc::Receiver<()>>>,
}

#[cfg(test)]
impl Default for Cache {
    /// A cache whose drivers' subscriptions are unknown.
    fn default() -> Self {
        Self::new(
            Subscription::from_driver(None, None, None),
            Subscription::from_driver(None, None, None),
        )
    }
}

impl Cache {
    /// Creates a cache that serves no status until a driver connects. Until then,
    /// `ConfigureWatch` is answered against `battery` and `charger`.
    pub(crate) fn new(
        battery: Subscription<fbattery::Status>,
        charger: Subscription<fcharger::Status>,
    ) -> Self {
        // Without buffering, each topic holds at most one pending notification.
        let (rearm, rearm_receiver) = mpsc::channel(0);
        let sampling = Arc::new(SamplingState {
            battery_interval_votes: Arc::new(IntervalVotes::min()),
            charger_interval_votes: Arc::new(IntervalVotes::min()),
            can_wake: AtomicBool::new(true),
            sample_count: AtomicU64::new(0),
        });
        Self {
            battery: RefCell::new(Topic::new(
                battery,
                sampling.battery_interval_votes.clone(),
                sampling.clone(),
                rearm.clone(),
            )),
            charger: RefCell::new(Topic::new(
                charger,
                sampling.charger_interval_votes.clone(),
                sampling.clone(),
                rearm,
            )),
            sampling,
            rearm_receiver: RefCell::new(Some(rearm_receiver)),
        }
    }

    /// Records the governors' `max_interval` votes and sampler state under
    /// `parent/governor_telemetry`.
    pub(crate) fn record_inspect(&self, parent: &inspect::Node) {
        let sampling = Arc::downgrade(&self.sampling);
        parent.record_lazy_child(INSPECTOR_NODE_GOVERNOR_TELEMETRY, move || {
            let sampling = sampling.clone();
            async move {
                let inspector = inspect::Inspector::default();
                if let Some(sampling) = sampling.upgrade() {
                    sampling.record_inspect(inspector.root());
                }
                Ok(inspector)
            }
            .boxed()
        });
    }

    pub(crate) fn battery_connected(
        &self,
        spec: Option<fbattery::Spec>,
        subscription: Subscription<fbattery::Status>,
        proxy: fbattery::BatteryProxy,
    ) {
        self.battery.borrow_mut().driver_connected(spec, subscription, proxy);
    }

    pub(crate) fn publish_battery(&self, status: &fbattery::Status, lease: Option<&zx::EventPair>) {
        self.battery.borrow_mut().publish(status, lease);
    }

    /// Delivers a battery sample: `status`, or `None` if the driver could not be read.
    ///
    /// Completes any due governor `Watch` and returns whether the cached battery status changed.
    #[cfg(test)]
    pub(crate) fn battery_sampled(
        &self,
        status: Option<&fbattery::Status>,
        lease: Option<&zx::EventPair>,
    ) -> bool {
        self.battery.borrow_mut().sampled(status, lease, true)
    }

    pub(crate) fn battery_gone(&self) {
        self.battery.borrow_mut().driver_gone();
    }

    /// Proxy to the connected battery driver, if any.
    pub(crate) fn battery_proxy(&self) -> Option<fbattery::BatteryProxy> {
        self.battery.borrow().driver.as_ref().map(|driver| driver.proxy.clone())
    }

    pub(crate) fn charger_connected(
        &self,
        spec: Option<fcharger::Spec>,
        subscription: Subscription<fcharger::Status>,
        proxy: fcharger::ChargerProxy,
    ) {
        self.charger.borrow_mut().driver_connected(spec, subscription, proxy);
    }

    pub(crate) fn publish_charger(&self, status: &fcharger::Status, lease: Option<&zx::EventPair>) {
        self.charger.borrow_mut().publish(status, lease);
    }

    pub(crate) fn charger_gone(&self) {
        self.charger.borrow_mut().driver_gone();
    }

    /// Whether publishing charger `status` would complete a governor's `Watch`, now or on its next
    /// `Watch`.
    pub(crate) fn charger_update_is_news(&self, status: &fcharger::Status) -> bool {
        self.charger.borrow().is_news_to_any(status)
    }

    /// Calls `f` with a borrowed view of the cached battery and charger statuses and specs.
    pub(crate) fn with_cached<R>(&self, f: impl FnOnce(CachedStatus<'_>) -> R) -> R {
        let battery = self.battery.borrow();
        let charger = self.charger.borrow();
        f(CachedStatus {
            battery: battery.status(),
            battery_spec: battery.driver.as_ref().and_then(|driver| driver.spec.as_ref()),
            charger: charger.status(),
            charger_spec: charger.driver.as_ref().and_then(|driver| driver.spec.as_ref()),
        })
    }

    /// When the next samples are due, if any connection waits on one.
    fn next_sample_deadlines(&self) -> SampleDeadlines {
        let battery = self.battery.borrow().next_sample_deadlines();
        let charger = self.charger.borrow().next_sample_deadlines();
        battery.earliest(charger)
    }

    /// Records whether samples can wake the system.
    fn set_can_wake(&self, can_wake: bool) {
        self.sampling.can_wake.store(can_wake, Ordering::Relaxed);
    }

    /// Samples drivers for connections as their deadlines come, delivering sampled statuses to the
    /// cache and invoking `on_sampled` whenever a sample changes a cached status.
    ///
    /// Each driver is read once for every connection due within [`SAMPLE_COALESCE_WINDOW`] of the
    /// earliest deadline, and a failed read is retried after another interval. Intervals are on
    /// the boot timeline, so time suspended counts towards them. Each governor's vote, the
    /// shortest `max_interval` of its connections, is recorded in Inspect.
    ///
    /// Samples for connections that wake the system wait with `wakeup`, and the others with boot
    /// timers. If a wake alarm fails, the sample it was set for is scheduled with a timer instead,
    /// and the next one with a wake alarm again, unless the wake alarm service closed the channel.
    pub(crate) async fn run_sampler(
        &self,
        mut wakeup: Wakeup,
        on_sampled: impl Fn(Option<zx::EventPair>),
    ) {
        let Some(mut rearm) = self.rearm_receiver.borrow_mut().take() else {
            warn!("Sampler is already running");
            return;
        };
        self.set_can_wake(matches!(wakeup, Wakeup::WakeAlarms(_)));
        let mut armed_wake: Option<(fasync::BootInstant, WakeFuture)> = None;
        let mut alarm_armed = false;
        let mut armed_timer: Option<(fasync::BootInstant, WakeFuture)> = None;
        loop {
            let deadlines = self.next_sample_deadlines();
            // Rescheduling only when the deadline changes keeps the alarm set across notifications.
            if armed_wake.as_ref().map(|&(armed_deadline, _)| armed_deadline) != deadlines.wake {
                if deadlines.wake.is_none() && mem::take(&mut alarm_armed) {
                    wakeup.cancel();
                }
                armed_wake = deadlines.wake.map(|deadline| (deadline, wakeup.at(deadline)));
                alarm_armed = armed_wake.is_some();
            }
            if armed_timer.as_ref().map(|&(armed_deadline, _)| armed_deadline) != deadlines.timer {
                armed_timer = deadlines.timer.map(|deadline| (deadline, timer_at(deadline)));
            }

            let wakes = futures::future::select(
                armed_or_pending(&mut armed_wake),
                armed_or_pending(&mut armed_timer),
            );
            let (woken, by_alarm) = match futures::future::select(wakes, rearm.next()).await {
                Either::Left((Either::Left((woken, _)), _)) => (woken, true),
                Either::Left((Either::Right((woken, _)), _)) => (woken, false),
                Either::Right((Some(()), _)) => continue,
                Either::Right((None, _)) => return,
            };
            if !by_alarm {
                armed_timer = None;
                if let Ok(wake_lease) = woken {
                    self.sample(wake_lease, false, &on_sampled).await;
                }
                continue;
            }
            armed_wake = None;
            alarm_armed = false;
            match woken {
                Ok(wake_lease) => {
                    self.sample(wake_lease, true, &on_sampled).await;
                }
                Err(e) if wakeup.is_unavailable() => {
                    warn!("Wake alarms unavailable, sampling with timers: {e:?}");
                    wakeup = Wakeup::Timer;
                    self.set_can_wake(false);
                }
                Err(e) => {
                    warn!("Wake alarm failed, sampling with a timer once: {e:?}");
                    armed_wake = deadlines.wake.map(|deadline| (deadline, timer_at(deadline)));
                }
            }
        }
    }

    /// Reads each driver that has a connection due, delivers the sampled statuses to the cache,
    /// and calls `on_sampled` if any cached status changed.
    async fn sample(
        &self,
        wake_lease: Option<zx::EventPair>,
        include_waking: bool,
        on_sampled: &impl Fn(Option<zx::EventPair>),
    ) {
        let now = fasync::BootInstant::now();
        let battery_proxy = self.battery.borrow().due_driver(now, include_waking);
        let charger_proxy = self.charger.borrow().due_driver(now, include_waking);
        if battery_proxy.is_none() && charger_proxy.is_none() {
            return;
        }
        let battery =
            OptionFuture::from(battery_proxy.map(|proxy| async move {
                log_sample_result("battery", proxy.get_status().await)
            }));
        let charger =
            OptionFuture::from(charger_proxy.map(|proxy| async move {
                log_sample_result("charger", proxy.get_status().await)
            }));
        let (battery, charger) = futures::join!(battery, charger);
        self.sampling.sample_count.fetch_add(1, Ordering::Relaxed);
        let mut changed = false;
        if let Some(status) = battery {
            changed |= self.battery.borrow_mut().sampled(
                status.as_ref(),
                wake_lease.as_ref(),
                include_waking,
            );
        }
        if let Some(status) = charger {
            changed |= self.charger.borrow_mut().sampled(
                status.as_ref(),
                wake_lease.as_ref(),
                include_waking,
            );
        }
        if changed {
            on_sampled(wake_lease);
        }
    }

    /// Serves one `Battery` connection opened by governor `owner` until it closes.
    pub(crate) async fn serve_battery(
        &self,
        owner: &str,
        mut stream: fbattery::BatteryRequestStream,
    ) -> Result<(), fidl::Error> {
        let topic = &self.battery;
        let connection = ConnectionGuard::new(topic, owner);
        while let Some(request) = stream.try_next().await? {
            match request {
                fbattery::BatteryRequest::GetSpec { responder } => {
                    responder.send(topic.borrow().spec().as_ref().map_err(|&e| e))?
                }
                fbattery::BatteryRequest::GetStatus { responder } => {
                    responder.send(topic.borrow().status().ok_or(Battery::ERROR_UNAVAILABLE))?
                }
                fbattery::BatteryRequest::ConfigureWatch { options, responder } => {
                    let effective =
                        topic.borrow_mut().configure_watch(connection.id, options.into());
                    responder.send(
                        effective.map(fbattery::WatchOptions::from).as_ref().map_err(|&e| e),
                    )?
                }
                fbattery::BatteryRequest::Watch { lease, responder } => {
                    topic.borrow_mut().watch(connection.id, responder, lease)
                }
                fbattery::BatteryRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("Unknown Battery method from governor {owner}: ordinal {ordinal}")
                }
            }
        }
        Ok(())
    }

    /// Serves one `Charger` connection opened by governor `owner` until it closes.
    pub(crate) async fn serve_charger(
        &self,
        owner: &str,
        mut stream: fcharger::ChargerRequestStream,
    ) -> Result<(), fidl::Error> {
        let topic = &self.charger;
        let connection = ConnectionGuard::new(topic, owner);
        while let Some(request) = stream.try_next().await? {
            match request {
                fcharger::ChargerRequest::GetSpec { responder } => {
                    responder.send(topic.borrow().spec().as_ref().map_err(|&e| e))?
                }
                fcharger::ChargerRequest::GetStatus { responder } => {
                    responder.send(topic.borrow().status().ok_or(Charger::ERROR_UNAVAILABLE))?
                }
                fcharger::ChargerRequest::ConfigureWatch { options, responder } => {
                    let effective =
                        topic.borrow_mut().configure_watch(connection.id, options.into());
                    responder.send(
                        effective.map(fcharger::WatchOptions::from).as_ref().map_err(|&e| e),
                    )?
                }
                fcharger::ChargerRequest::Watch { lease, responder } => {
                    topic.borrow_mut().watch(connection.id, responder, lease)
                }
                fcharger::ChargerRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("Unknown Charger method from governor {owner}: ordinal {ordinal}")
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async::TestExecutor;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::Poll;

    const GOVERNOR: &str = "governor";
    const LEVEL_PERCENT: f32 = 50.0;
    const NEW_LEVEL_PERCENT: f32 = 51.0;
    const VOLTAGE_UV: u32 = 3_800_000;
    const NEW_VOLTAGE_UV: u32 = 3_900_000;
    const DESIGN_CAPACITY_UAH: u32 = 5_000_000;
    const MAX_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(5);
    const TOO_SHORT_INTERVAL: zx::BootDuration = zx::BootDuration::from_millis(10);

    /// A battery status mask with only `level_percent`.
    fn level_only() -> fbattery::Status {
        fbattery::Status { level_percent: Some(0.0), ..Default::default() }
    }

    /// battery-manager's subscription to a battery driver: notified of `level_percent` and
    /// `health` changes, and woken by `level_percent` changes.
    fn battery_subscription() -> Subscription<fbattery::Status> {
        Subscription::from_driver(
            Some(fbattery::Status {
                level_percent: Some(0.0),
                health: Some(fbattery::HealthStatus::Good),
                ..Default::default()
            }),
            None,
            Some(level_only()),
        )
    }

    fn charger_subscription() -> Subscription<fcharger::Status> {
        let interest = fcharger::Status { online: Some(true), ..Default::default() };
        Subscription::from_driver(Some(interest.clone()), Some(interest), None)
    }

    fn battery_status(level_percent: f32, voltage_uv: u32) -> fbattery::Status {
        fbattery::Status {
            level_percent: Some(level_percent),
            voltage_uv: Some(voltage_uv),
            ..Default::default()
        }
    }

    fn battery_spec() -> fbattery::Spec {
        fbattery::Spec { design_capacity_uah: Some(DESIGN_CAPACITY_UAH), ..Default::default() }
    }

    /// A proxy to a battery driver that is never read.
    fn battery_driver() -> fbattery::BatteryProxy {
        fidl::endpoints::create_proxy::<fbattery::BatteryMarker>().0
    }

    /// A proxy to a charger driver that is never read.
    fn charger_driver() -> fcharger::ChargerProxy {
        fidl::endpoints::create_proxy::<fcharger::ChargerMarker>().0
    }

    /// A cache whose battery driver is connected but has not reported a status yet.
    fn connected_cache() -> Rc<Cache> {
        let cache = Rc::new(Cache::new(battery_subscription(), charger_subscription()));
        cache.battery_connected(Some(battery_spec()), battery_subscription(), battery_driver());
        cache
    }

    fn connect_battery(cache: &Rc<Cache>) -> fbattery::BatteryProxy {
        connect_battery_as(cache, GOVERNOR)
    }

    fn connect_battery_as(cache: &Rc<Cache>, owner: &'static str) -> fbattery::BatteryProxy {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        let cache = cache.clone();
        fasync::Task::local(async move {
            cache.serve_battery(owner, stream).await.expect("serve_battery failed");
        })
        .detach();
        proxy
    }

    fn connect_charger(cache: &Rc<Cache>) -> fcharger::ChargerProxy {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        let cache = cache.clone();
        fasync::Task::local(async move {
            cache.serve_charger(GOVERNOR, stream).await.expect("serve_charger failed");
        })
        .detach();
        proxy
    }

    /// Configures `battery` with `options` and returns the effective options.
    async fn configure(
        battery: &fbattery::BatteryProxy,
        options: fbattery::WatchOptions,
    ) -> fbattery::WatchOptions {
        battery.configure_watch(&options).await.unwrap().unwrap()
    }

    async fn configure_max_interval(
        battery: &fbattery::BatteryProxy,
        max_interval: Option<zx::BootDuration>,
    ) {
        let _ = configure_sampling(battery, max_interval, None).await;
    }

    /// Configures `max_interval` and `wake_on_max_interval` on `battery` and returns the effective
    /// options.
    async fn configure_sampling(
        battery: &fbattery::BatteryProxy,
        max_interval: Option<zx::BootDuration>,
        wake_on_max_interval: Option<bool>,
    ) -> fbattery::WatchOptions {
        let options = fbattery::WatchOptions {
            max_interval: max_interval.map(|interval| interval.into_nanos()),
            wake_on_max_interval,
            ..Default::default()
        };
        configure(battery, options).await
    }

    /// Configures `charger` with `options` and returns the effective options.
    async fn configure_charger(
        charger: &fcharger::ChargerProxy,
        options: fcharger::WatchOptions,
    ) -> fcharger::WatchOptions {
        charger.configure_watch(&options).await.unwrap().unwrap()
    }

    /// Verifies that the first `Watch` on a governor connection blocks until the driver publishes
    /// a status, and that a connection opened after the first status completes immediately.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_first_watch_completes_once_driver_data_is_available() {
        let cache = Rc::new(Cache::new(battery_subscription(), charger_subscription()));
        let battery = connect_battery(&cache);

        let mut watch = battery.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);

        cache.battery_connected(Some(battery_spec()), battery_subscription(), battery_driver());
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let (status, lease) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert!(lease.is_none());

        // A connection opened after the first status is answered immediately.
        let late = connect_battery(&cache);
        let (status, _) = late.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
    }

    /// Verifies that subsequent `Watch` calls only complete when a field in the connection's
    /// interest changes relative to the last status sent on that connection.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_watch_completes_only_on_change_in_interest() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        // `voltage_uv` is not in the default interest (the driver's supported interest).
        let mut watch = battery.watch(None);
        cache.publish_battery(&battery_status(LEVEL_PERCENT, NEW_VOLTAGE_UV), None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);

        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, NEW_VOLTAGE_UV), None);
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, NEW_VOLTAGE_UV));
    }

    /// Verifies that if an interested field changes while no `Watch` is pending, the connection
    /// is marked dirty so the next `Watch` completes immediately.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_change_while_no_watch_pending_completes_next_watch() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        // The level changes and changes back while the governor is not watching.
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let (status, _) = battery.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
    }

    /// Verifies that each connection completes `Watch` against its own interest and the status
    /// last sent on it.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_connections_are_independent() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let level = connect_battery(&cache);
        let health = connect_battery(&cache);
        let health_only =
            fbattery::Status { health: Some(fbattery::HealthStatus::Good), ..Default::default() };
        let _ = configure(
            &health,
            fbattery::WatchOptions { interest: Some(health_only), ..Default::default() },
        )
        .await;
        let _ = level.watch(None).await.unwrap().unwrap();
        let _ = health.watch(None).await.unwrap().unwrap();

        // A level change is news only to the connection interested in it.
        let mut health_watch = health.watch(None);
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        let (status, _) = level.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
        assert_matches!(TestExecutor::poll_until_stalled(&mut health_watch).await, Poll::Pending);

        let cold = fbattery::Status {
            health: Some(fbattery::HealthStatus::Cold),
            ..battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV)
        };
        cache.publish_battery(&cold, None);
        let (status, _) = health_watch.await.unwrap().unwrap();
        assert_eq!(status, cold);
    }

    /// Whether the peer of `lease_peer` has been closed, i.e. every handle to the lease is gone.
    fn is_released(lease_peer: &zx::EventPair) -> bool {
        matches!(
            lease_peer
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(_)
        )
    }

    /// Configures `battery` to wake on `level_percent` changes.
    async fn wake_on_level(battery: &fbattery::BatteryProxy) {
        let options = fbattery::WatchOptions { wake_on: Some(level_only()), ..Default::default() };
        let _ = configure(battery, options).await;
    }

    /// Verifies that a parked `Watch` is completed with a duplicate of the driver's wake lease
    /// when a field in the connection's `wake_on` changes, and that nothing else holds a copy.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_wake_on_change_returns_wake_lease() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        wake_on_level(&battery).await;
        let _ = battery.watch(None).await.unwrap().unwrap();

        let mut watch = battery.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        let (_, wake_lease) = watch.await.unwrap().unwrap();
        assert!(!is_released(&lease_peer));
        drop(wake_lease.expect("a wake_on change carries a wake lease"));
        assert!(is_released(&lease_peer));
    }

    /// Verifies that a change only in fields of interest, not `wake_on`, carries no wake lease even
    /// if the driver update that delivered it did.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_interest_only_change_carries_no_wake_lease() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        let watch = battery.watch(None);
        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        let (_, wake_lease) = watch.await.unwrap().unwrap();
        assert!(wake_lease.is_none());
        assert!(is_released(&lease_peer));
    }

    /// Verifies that the wake lease of a `wake_on` change that arrives while no `Watch` is pending
    /// is returned by the next `Watch`, and dropped if the connection closes first.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_wake_lease_is_stashed_until_next_watch() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        wake_on_level(&battery).await;
        let _ = battery.watch(None).await.unwrap().unwrap();

        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        assert!(!is_released(&lease_peer));
        let (_, wake_lease) = battery.watch(None).await.unwrap().unwrap();
        drop(wake_lease.expect("the stashed wake lease is returned"));
        assert!(is_released(&lease_peer));

        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        drop(battery);
        // Let the server observe the close.
        let _ = TestExecutor::poll_until_stalled(&mut std::future::pending::<()>()).await;
        assert!(is_released(&lease_peer));
    }

    /// Verifies that a stashed wake lease is dropped once `ConfigureWatch` removes `wake_on`,
    /// while the change it came with is still delivered.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_configure_watch_without_wake_on_drops_stashed_lease() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        wake_on_level(&battery).await;
        let _ = battery.watch(None).await.unwrap().unwrap();

        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        assert!(!is_released(&lease_peer));
        let _ = configure(&battery, fbattery::WatchOptions::default()).await;
        assert!(is_released(&lease_peer));

        let (status, wake_lease) = battery.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
        assert!(wake_lease.is_none());
    }

    /// Verifies that a driver disconnection drops a stashed wake lease, since the change it came
    /// with can no longer be sent.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_driver_gone_drops_stashed_lease() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        wake_on_level(&battery).await;
        let _ = battery.watch(None).await.unwrap().unwrap();

        let (lease, lease_peer) = zx::EventPair::create();
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), Some(&lease));
        drop(lease);
        assert!(!is_released(&lease_peer));
        cache.battery_gone();
        assert!(is_released(&lease_peer));
    }

    /// Verifies that the governor's lease is handed back by a `Watch` answered immediately, and
    /// dropped by a `Watch` that is parked.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_governor_lease_handed_back_or_dropped() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);

        // The first `Watch` is answered immediately with a snapshot.
        let (lease, lease_peer) = zx::EventPair::create();
        let (_, wake_lease) = battery.watch(Some(lease)).await.unwrap().unwrap();
        assert!(!is_released(&lease_peer));
        drop(wake_lease.expect("the governor's lease is handed back"));
        assert!(is_released(&lease_peer));

        // A `Watch` with nothing new to report is parked.
        let (lease, lease_peer) = zx::EventPair::create();
        let mut watch = battery.watch(Some(lease));
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert!(is_released(&lease_peer));
    }

    /// Verifies that a concurrent `Watch` on the same connection is rejected with
    /// `ALREADY_WATCHING` while leaving the original `Watch` pending and functional.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_concurrent_watch_is_rejected() {
        let cache = connected_cache();
        let battery = connect_battery(&cache);

        let mut first = battery.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut first).await, Poll::Pending);
        assert_matches!(battery.watch(None).await, Ok(Err(fbattery::Error::AlreadyWatching)));

        // The first `Watch` stays pending and the connection stays usable.
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        assert_matches!(first.await, Ok(Ok(_)));
    }

    /// Verifies that `GetStatus` and `GetSpec` are served from the cache, returning `IO` while
    /// disconnected and `NOT_SUPPORTED` if the connected driver has no spec.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_get_status_and_spec_are_served_from_cache() {
        let cache = Rc::new(Cache::new(battery_subscription(), charger_subscription()));
        let battery = connect_battery(&cache);
        assert_matches!(battery.get_status().await, Ok(Err(fbattery::Error::Io)));
        assert_matches!(battery.get_spec().await, Ok(Err(fbattery::Error::Io)));

        cache.battery_connected(Some(battery_spec()), battery_subscription(), battery_driver());
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        assert_eq!(
            battery.get_status().await.unwrap().unwrap(),
            battery_status(LEVEL_PERCENT, VOLTAGE_UV)
        );
        let spec = battery.get_spec().await.unwrap().unwrap();
        assert_eq!(spec.design_capacity_uah, Some(DESIGN_CAPACITY_UAH));
        // `supported_options` describes what this connection is served, not the driver.
        assert_eq!(
            spec.supported_options,
            Some(fbattery::WatchOptions {
                interest: Some(fbattery::Status {
                    level_percent: Some(0.0),
                    health: Some(fbattery::HealthStatus::Good),
                    ..Default::default()
                }),
                wake_on: Some(level_only()),
                max_interval: Some(MIN_SAMPLE_INTERVAL.into_nanos()),
                wake_on_max_interval: Some(true),
                ..Default::default()
            })
        );

        cache.battery_connected(None, battery_subscription(), battery_driver());
        assert_matches!(battery.get_spec().await, Ok(Err(fbattery::Error::NotSupported)));
    }

    /// Verifies that a driver disconnection clears the cache without closing governor connections,
    /// and that pending `Watch` calls resume once the driver reconnects and publishes a new status.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_driver_reconnect_keeps_governor_connections() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        let mut watch = battery.watch(None);
        cache.battery_gone();
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert_matches!(battery.get_status().await, Ok(Err(fbattery::Error::Io)));

        cache.battery_connected(Some(battery_spec()), battery_subscription(), battery_driver());
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
    }

    /// Verifies that `ConfigureWatch` intersects requested `interest` and `wake_on` fields with
    /// battery-manager's subscription, clamps `max_interval` to `MIN_SAMPLE_INTERVAL`, and
    /// returns `NOT_SUPPORTED` when no requested option can be served.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_configure_watch_returns_effective_options() {
        let cache = connected_cache();
        let battery = connect_battery(&cache);

        // `interest` is limited to the fields battery-manager is notified of, and `wake_on` to
        // the fields it wakes on. `wake_on` fields are implicitly of interest.
        let effective = configure(
            &battery,
            fbattery::WatchOptions {
                interest: Some(fbattery::Status { voltage_uv: Some(0), ..Default::default() }),
                wake_on: Some(fbattery::Status {
                    level_percent: Some(0.0),
                    health: Some(fbattery::HealthStatus::Good),
                    ..Default::default()
                }),
                max_interval: Some(MAX_INTERVAL.into_nanos()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            effective,
            fbattery::WatchOptions {
                interest: Some(fbattery::Status {
                    level_percent: Some(0.0),
                    health: Some(fbattery::HealthStatus::Good),
                    ..Default::default()
                }),
                wake_on: Some(level_only()),
                max_interval: Some(MAX_INTERVAL.into_nanos()),
                ..Default::default()
            }
        );

        // `max_interval` is clamped, and serves fields that cannot be watched.
        let effective = configure(
            &battery,
            fbattery::WatchOptions {
                interest: Some(fbattery::Status { voltage_uv: Some(0), ..Default::default() }),
                max_interval: Some(TOO_SHORT_INTERVAL.into_nanos()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            effective,
            fbattery::WatchOptions {
                interest: Some(fbattery::Status::default()),
                max_interval: Some(MIN_SAMPLE_INTERVAL.into_nanos()),
                ..Default::default()
            }
        );

        // `wake_on_max_interval` is honored with `max_interval`, and ignored without it.
        assert_eq!(
            configure_sampling(&battery, Some(MAX_INTERVAL), Some(true)).await,
            fbattery::WatchOptions {
                max_interval: Some(MAX_INTERVAL.into_nanos()),
                wake_on_max_interval: Some(true),
                ..Default::default()
            }
        );
        assert_eq!(
            configure_sampling(&battery, None, Some(true)).await,
            fbattery::WatchOptions { wake_on_max_interval: Some(false), ..Default::default() }
        );

        // Absent in, absent out.
        assert_eq!(
            configure(&battery, fbattery::WatchOptions::default()).await,
            fbattery::WatchOptions::default()
        );

        // None of the requested fields can be served.
        assert_matches!(
            battery
                .configure_watch(&fbattery::WatchOptions {
                    interest: Some(fbattery::Status { voltage_uv: Some(0), ..Default::default() }),
                    ..Default::default()
                })
                .await,
            Ok(Err(fbattery::Error::NotSupported))
        );
    }

    /// Verifies that `ConfigureWatch` with an absent `interest` succeeds while battery-manager's
    /// subscription covers every field, even if none of the requested `wake_on` fields can be
    /// served.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_configure_watch_with_interest_in_every_field_succeeds() {
        let subscription = Subscription::from_driver(None, None, None);
        let cache = Rc::new(Cache::new(subscription.clone(), charger_subscription()));
        cache.battery_connected(Some(battery_spec()), subscription, battery_driver());
        let battery = connect_battery(&cache);

        let effective = configure(
            &battery,
            fbattery::WatchOptions { wake_on: Some(level_only()), ..Default::default() },
        )
        .await;
        assert_eq!(
            effective,
            fbattery::WatchOptions {
                wake_on: Some(fbattery::Status::default()),
                ..Default::default()
            }
        );
    }

    /// Verifies that `ConfigureWatch` re-evaluates a pending `Watch` against the status last sent
    /// on that connection and completes it immediately if a newly interested field differs.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_configure_watch_applies_to_pending_watch() {
        let cache = connected_cache();
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        // Interest in `health` only: a level change does not complete the `Watch`.
        let health_only =
            fbattery::Status { health: Some(fbattery::HealthStatus::Good), ..Default::default() };
        let _ = configure(
            &battery,
            fbattery::WatchOptions { interest: Some(health_only), ..Default::default() },
        )
        .await;
        let mut watch = battery.watch(None);
        cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);

        // Widening the interest to `level_percent` completes it, against the status last sent.
        let _ = configure(&battery, fbattery::WatchOptions::default()).await;
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
    }

    /// Verifies that `ConfigureWatch` called before any driver connects is answered against the
    /// initial subscription passed to `Cache::new`.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_configure_watch_before_driver_connects() {
        // Before any driver connects, options are bounded by the subscription the cache was
        // created with: here, every field of interest and no wake fields.
        let cache = Rc::new(Cache::new(
            Subscription::from_driver(None, None, None),
            charger_subscription(),
        ));
        let battery = connect_battery(&cache);
        let interest = fbattery::Status { voltage_uv: Some(0), ..Default::default() };
        let effective = configure(
            &battery,
            fbattery::WatchOptions {
                interest: Some(interest.clone()),
                wake_on: Some(level_only()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            effective,
            fbattery::WatchOptions {
                interest: Some(fbattery::Status { level_percent: Some(0.0), ..interest }),
                wake_on: Some(fbattery::Status::default()),
                ..Default::default()
            }
        );
    }

    /// Verifies that `Subscription::from_battery_driver` treats a `Spec` with `supported_options:
    /// None` as supporting all fields (`Mask::All`) rather than an empty field mask when
    /// `ConfigureWatch` also returns `interest: None`.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_subscription_from_driver_without_supported_options_watches_all_fields() {
        let spec = battery_spec();
        let subscription = Subscription::from_battery_driver(
            Some(&spec),
            Some(fbattery::WatchOptions {
                interest: None,
                wake_on: Some(level_only()),
                ..Default::default()
            }),
        );
        let cache = Rc::new(Cache::new(subscription.clone(), charger_subscription()));
        cache.battery_connected(Some(spec), subscription, battery_driver());
        cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);

        let battery = connect_battery(&cache);
        let _ = battery.watch(None).await.unwrap().unwrap();

        // Even `voltage_uv` (not in `wake_on`) triggers a `Watch` completion because `interest`
        // defaults to all fields when neither `ConfigureWatch` nor `Spec.supported_options`
        // restricts it.
        cache.publish_battery(&battery_status(LEVEL_PERCENT, NEW_VOLTAGE_UV), None);
        let (status, _) = battery.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, NEW_VOLTAGE_UV));
    }

    /// Verifies that `serve_charger` serves `Watch`, `GetStatus`, `GetSpec`, and `ConfigureWatch`
    /// over cached charger status, and rejects concurrent `Watch` calls with `ALREADY_WATCHING`.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_charger_watch_status_spec_and_configure_watch() {
        let cache = Rc::new(Cache::new(battery_subscription(), charger_subscription()));
        let charger = connect_charger(&cache);
        assert_matches!(charger.get_status().await, Ok(Err(fcharger::Error::Io)));
        assert_matches!(charger.get_spec().await, Ok(Err(fcharger::Error::Io)));

        let online = fcharger::Status { online: Some(true), ..Default::default() };
        let mut watch = pin!(charger.watch(None));
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert_matches!(charger.watch(None).await, Ok(Err(fcharger::Error::AlreadyWatching)));

        let spec = fcharger::Spec { max_charge_current_ua: Some(2_000_000), ..Default::default() };
        cache.charger_connected(Some(spec), charger_subscription(), charger_driver());
        cache.publish_charger(&online, None);
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, online);
        assert_eq!(charger.get_status().await.unwrap().unwrap(), online);

        let returned_spec = charger.get_spec().await.unwrap().unwrap();
        assert_eq!(returned_spec.max_charge_current_ua, Some(2_000_000));
        assert_eq!(
            returned_spec.supported_options,
            Some(fcharger::WatchOptions {
                interest: Some(fcharger::Status { online: Some(true), ..Default::default() }),
                wake_on: Some(fcharger::Status::default()),
                max_interval: Some(MIN_SAMPLE_INTERVAL.into_nanos()),
                wake_on_max_interval: Some(true),
                ..Default::default()
            })
        );

        let effective = configure_charger(
            &charger,
            fcharger::WatchOptions {
                interest: Some(fcharger::Status {
                    online: Some(true),
                    input_voltage_uv: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            effective.interest,
            Some(fcharger::Status { online: Some(true), ..Default::default() })
        );

        cache.charger_gone();
        assert_matches!(charger.get_status().await, Ok(Err(fcharger::Error::Io)));
    }

    /// Verifies that a parked charger `Watch` is completed with a duplicate of the driver's wake
    /// lease when a field in the connection's `wake_on` changes.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_charger_wake_on_change_returns_wake_lease() {
        let online = fcharger::Status { online: Some(true), ..Default::default() };
        let subscription =
            Subscription::from_driver(None, Some(online.clone()), Some(online.clone()));
        let cache = Rc::new(Cache::new(battery_subscription(), subscription.clone()));
        cache.charger_connected(None, subscription, charger_driver());
        cache.publish_charger(&online, None);
        let charger = connect_charger(&cache);
        let _ = configure_charger(
            &charger,
            fcharger::WatchOptions { wake_on: Some(online.clone()), ..Default::default() },
        )
        .await;
        let _ = charger.watch(None).await.unwrap().unwrap();

        let mut watch = charger.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        let (lease, lease_peer) = zx::EventPair::create();
        let offline = fcharger::Status { online: Some(false), ..Default::default() };
        cache.publish_charger(&offline, Some(&lease));
        drop(lease);
        let (status, wake_lease) = watch.await.unwrap().unwrap();
        assert_eq!(status, offline);
        assert!(!is_released(&lease_peer));
        drop(wake_lease.expect("a wake_on change carries a wake lease"));
        assert!(is_released(&lease_peer));
    }

    /// Verifies that each governor votes for the shortest `max_interval` among its connections,
    /// and that closing or reconfiguring a connection updates its vote and Inspect.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_max_interval_votes_shortest_interval_per_governor() {
        const OTHER_GOVERNOR: &str = "other_governor";
        const SHORT_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(2);
        const OTHER_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(3);
        let cache = connected_cache();
        let short = connect_battery(&cache);
        let long = connect_battery(&cache);
        let other = connect_battery_as(&cache, OTHER_GOVERNOR);
        configure_max_interval(&short, Some(SHORT_INTERVAL)).await;
        configure_max_interval(&long, Some(MAX_INTERVAL)).await;
        configure_max_interval(&other, Some(OTHER_INTERVAL)).await;
        let votes = &cache.sampling.battery_interval_votes;
        let short_millis = SHORT_INTERVAL.into_millis() as u64;
        let max_millis = MAX_INTERVAL.into_millis() as u64;
        let other_millis = OTHER_INTERVAL.into_millis() as u64;
        assert_eq!(votes.vote(GOVERNOR), Some(short_millis));
        assert_eq!(votes.vote(OTHER_GOVERNOR), Some(other_millis));
        assert_eq!(votes.result(), Some(short_millis));

        // Closing a connection withdraws its interval from its governor's vote.
        drop(short);
        let _: Poll<()> = TestExecutor::poll_until_stalled(std::future::pending()).await;
        assert_eq!(votes.vote(GOVERNOR), Some(max_millis));
        assert_eq!(votes.result(), Some(other_millis));

        // So does configuring the last one without `max_interval`.
        configure_max_interval(&long, None).await;
        assert_eq!(votes.vote(GOVERNOR), None);
        assert_eq!(votes.result(), Some(other_millis));

        let inspector = inspect::Inspector::default();
        cache.sampling.record_inspect(inspector.root());
        assert_data_tree!(inspector, root: {
            battery_sample_interval: {
                result: other_millis,
                overridden: false,
                votes: { other_governor: other_millis },
            },
            charger_sample_interval: { overridden: false, votes: {} },
            wakeup: "wake_alarms",
            sample_count: 0u64,
        });
    }

    const SAMPLE_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);
    const HALF_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(500);
    const LONG_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(3);
    /// Longer than the coalescing window, so that a deadline this far away is not yet due.
    const BEFORE_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(200);

    /// Serves a battery driver that returns `driver_status` on `GetStatus`, counts calls in
    /// `reads`, and fails them while `failing` is set.
    fn fake_battery_driver(
        driver_status: Rc<RefCell<fbattery::Status>>,
        reads: Rc<Cell<u32>>,
        failing: Rc<Cell<bool>>,
    ) -> fbattery::BatteryProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        fasync::Task::local(async move {
            while let Some(Ok(request)) = stream.next().await {
                let responder = match request {
                    fbattery::BatteryRequest::GetStatus { responder } => responder,
                    request => panic!("Unexpected battery driver request: {request:?}"),
                };
                reads.set(reads.get() + 1);
                let status = driver_status.borrow().clone();
                let result = if failing.get() { Err(fbattery::Error::Io) } else { Ok(&status) };
                responder.send(result).expect("Failed to send battery status");
            }
        })
        .detach();
        proxy
    }

    /// Serves a charger driver that returns `driver_status` on `GetStatus` and counts calls in
    /// `reads`.
    fn fake_charger_driver(
        driver_status: Rc<RefCell<fcharger::Status>>,
        reads: Rc<Cell<u32>>,
    ) -> fcharger::ChargerProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        fasync::Task::local(async move {
            while let Some(Ok(request)) = stream.next().await {
                let responder = match request {
                    fcharger::ChargerRequest::GetStatus { responder } => responder,
                    request => panic!("Unexpected charger driver request: {request:?}"),
                };
                reads.set(reads.get() + 1);
                let status = driver_status.borrow().clone();
                responder.send(Ok(&status)).expect("Failed to send charger status");
            }
        })
        .detach();
        proxy
    }

    /// Serves wake alarms that fire at their deadlines on the fake boot timeline, recording the
    /// peer of each keep-alive lease handed out in `fired` and the number of `Cancel` calls in
    /// `canceled`. If `failing`, `SetAndWait` fails.
    fn fake_wake_alarms(
        failing: bool,
        fired: Rc<RefCell<Vec<zx::EventPair>>>,
        canceled_count: Rc<Cell<u32>>,
    ) -> falarms::WakeAlarmsProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<falarms::WakeAlarmsMarker>();
        fasync::Task::local(async move {
            // Dropping an alarm's sender cancels it.
            let mut alarms: HashMap<String, futures::channel::oneshot::Sender<()>> = HashMap::new();
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    falarms::WakeAlarmsRequest::SetAndWait {
                        deadline,
                        alarm_id,
                        responder,
                        ..
                    } => {
                        if failing {
                            let _ = responder.send(Err(falarms::WakeAlarmsError::Internal));
                            continue;
                        }
                        let (cancel, canceled) = futures::channel::oneshot::channel();
                        // Setting an alarm again with the same ID reschedules it.
                        let _ = alarms.insert(alarm_id, cancel);
                        let fired = fired.clone();
                        fasync::Task::local(async move {
                            let timer = pin!(fasync::Timer::new(deadline));
                            match futures::future::select(timer, canceled).await {
                                Either::Left(_) => {
                                    let (keep_alive, peer) = zx::EventPair::create();
                                    fired.borrow_mut().push(peer);
                                    let _ = responder.send(Ok(keep_alive));
                                }
                                Either::Right(_) => {
                                    let _ = responder.send(Err(falarms::WakeAlarmsError::Dropped));
                                }
                            }
                        })
                        .detach();
                    }
                    falarms::WakeAlarmsRequest::Cancel { alarm_id, .. } => {
                        canceled_count.set(canceled_count.get() + 1);
                        let _ = alarms.remove(&alarm_id);
                    }
                    request => panic!("Unexpected WakeAlarms request: {request:?}"),
                }
            }
        })
        .detach();
        proxy
    }

    /// The wake alarm service the sampler uses.
    enum Alarms {
        /// Fires alarms at their deadlines.
        Working,
        /// Fails `SetAndWait` but keeps the channel open.
        Failing,
        /// Has closed the channel.
        Closed,
    }

    /// A cache with a connected battery driver, sampled by a running sampler.
    struct Harness {
        cache: Rc<Cache>,
        /// Status returned by the fake battery driver on `GetStatus`.
        driver_status: Rc<RefCell<fbattery::Status>>,
        /// Number of battery driver `GetStatus` calls.
        reads: Rc<Cell<u32>>,
        /// Number of times `on_sampled` was called because the cached status changed.
        changed_notifications: Rc<Cell<u32>>,
        /// Fails battery driver `GetStatus` calls while set.
        failing_reads: Rc<Cell<bool>>,
        /// Peers of the keep-alive leases of the wake alarms that fired.
        fired_alarms: Rc<RefCell<Vec<zx::EventPair>>>,
        /// Number of `WakeAlarms.Cancel` requests received.
        canceled_alarms: Rc<Cell<u32>>,
        _sampler: fasync::Task<()>,
    }

    impl Harness {
        fn new(alarms: Alarms) -> Self {
            let cache = Rc::new(Cache::new(battery_subscription(), charger_subscription()));
            let driver_status = Rc::new(RefCell::new(battery_status(LEVEL_PERCENT, VOLTAGE_UV)));
            let reads = Rc::new(Cell::new(0));
            let changed_notifications = Rc::new(Cell::new(0));
            let failing_reads = Rc::new(Cell::new(false));
            cache.battery_connected(
                None,
                battery_subscription(),
                fake_battery_driver(driver_status.clone(), reads.clone(), failing_reads.clone()),
            );
            cache.publish_battery(&battery_status(LEVEL_PERCENT, VOLTAGE_UV), None);
            let fired_alarms = Rc::new(RefCell::new(Vec::new()));
            let canceled_alarms = Rc::new(Cell::new(0));
            let wakeup = Wakeup::WakeAlarms(match alarms {
                Alarms::Working => {
                    fake_wake_alarms(false, fired_alarms.clone(), canceled_alarms.clone())
                }
                Alarms::Failing => {
                    fake_wake_alarms(true, fired_alarms.clone(), canceled_alarms.clone())
                }
                Alarms::Closed => {
                    fidl::endpoints::create_proxy_and_stream::<falarms::WakeAlarmsMarker>().0
                }
            });
            let changed_for_sampler = changed_notifications.clone();
            let cache_for_sampler = cache.clone();
            let sampler = fasync::Task::local(async move {
                cache_for_sampler
                    .run_sampler(wakeup, move |_| {
                        changed_for_sampler.set(changed_for_sampler.get() + 1);
                    })
                    .await;
            });
            Self {
                cache,
                driver_status,
                reads,
                changed_notifications,
                failing_reads,
                fired_alarms,
                canceled_alarms,
                _sampler: sampler,
            }
        }

        /// Connects a governor that configures `max_interval`, with samples that wake the system,
        /// and completes its first `Watch`, which starts its interval.
        async fn connect(&self, max_interval: zx::MonotonicDuration) -> fbattery::BatteryProxy {
            self.connect_with_wake(max_interval, true).await
        }

        /// Like [`Self::connect`], with `wake_on_max_interval` set to `wake`.
        async fn connect_with_wake(
            &self,
            max_interval: zx::MonotonicDuration,
            wake: bool,
        ) -> fbattery::BatteryProxy {
            let battery = connect_battery(&self.cache);
            let _ = configure_sampling(
                &battery,
                Some(zx::BootDuration::from_nanos(max_interval.into_nanos())),
                Some(wake),
            )
            .await;
            let _ = battery.watch(None).await.unwrap().unwrap();
            battery
        }
    }

    async fn advance(duration: zx::MonotonicDuration) {
        TestExecutor::advance_to(fasync::MonotonicInstant::now() + duration).await;
    }

    /// Verifies that a sample completes a pending `Watch` once `max_interval` has elapsed, and
    /// returns the wake alarm's lease to a connection that wakes the system for samples.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_sample_completes_watch_after_max_interval() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect(SAMPLE_INTERVAL).await;

        let mut watch = battery.watch(None);
        advance(SAMPLE_INTERVAL - BEFORE_DEADLINE).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert_eq!(harness.reads.get(), 0);

        advance(BEFORE_DEADLINE).await;
        let (status, wake_lease) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert_eq!(harness.reads.get(), 1);
        // The sampled status was identical to the cached status, so `on_sampled` was not called.
        assert_eq!(harness.changed_notifications.get(), 0);
        // The governor holds the wake alarm's lease; battery-manager keeps no copy.
        let lease_peer = harness.fired_alarms.borrow_mut().pop().expect("alarm should have fired");
        assert!(!is_released(&lease_peer));
        drop(wake_lease.expect("a waking sample carries the wake alarm's lease"));
        assert!(is_released(&lease_peer));
        let mut watch = battery.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);

        // When the next sample returns a changed status, `on_sampled` is called.
        *harness.driver_status.borrow_mut() = battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV);
        advance(SAMPLE_INTERVAL).await;
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
        assert_eq!(harness.reads.get(), 2);
        assert_eq!(harness.changed_notifications.get(), 1);
    }

    /// Verifies that samples for a connection that does not wake the system wait with a timer, and
    /// are returned without a wake lease.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_samples_without_wake_use_timers() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect_with_wake(SAMPLE_INTERVAL, false).await;

        let mut watch = battery.watch(None);
        advance(SAMPLE_INTERVAL - BEFORE_DEADLINE).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        advance(BEFORE_DEADLINE).await;
        let (status, wake_lease) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert!(wake_lease.is_none());
        assert_eq!(harness.reads.get(), 1);
        assert!(harness.fired_alarms.borrow().is_empty());
    }

    /// Verifies that connections due within the coalesce window share a single driver read and
    /// wake alarm, and that a non-waking timer due within the coalesce window before a wake alarm
    /// is coalesced onto the wake alarm so the waking connection still receives a wake lease.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_one_read_serves_connections_due_together() {
        const WITHIN_COALESCE_WINDOW: zx::MonotonicDuration =
            zx::MonotonicDuration::from_millis(50);

        let harness = Harness::new(Alarms::Working);
        let first = harness.connect(SAMPLE_INTERVAL).await;
        advance(WITHIN_COALESCE_WINDOW).await;
        let second = harness.connect(SAMPLE_INTERVAL).await;
        let slow = harness.connect(LONG_INTERVAL).await;

        let (mut first_watch, mut second_watch, mut slow_watch) =
            (first.watch(None), second.watch(None), slow.watch(None));
        advance(SAMPLE_INTERVAL - WITHIN_COALESCE_WINDOW).await;
        assert_matches!(
            TestExecutor::poll_until_stalled(&mut first_watch).await,
            Poll::Ready(Ok(Ok((_, Some(_)))))
        );
        assert_matches!(
            TestExecutor::poll_until_stalled(&mut second_watch).await,
            Poll::Ready(Ok(Ok((_, Some(_)))))
        );
        assert_matches!(TestExecutor::poll_until_stalled(&mut slow_watch).await, Poll::Pending);
        assert_eq!(harness.reads.get(), 1);
        assert_eq!(harness.fired_alarms.borrow().len(), 1);

        // A non-waking connection due 50ms before a waking connection coalesces onto the wake
        // alarm rather than firing a timer first without a wake lease.
        let no_wake = harness.connect_with_wake(SAMPLE_INTERVAL, false).await;
        advance(WITHIN_COALESCE_WINDOW).await;
        let waking = harness.connect(SAMPLE_INTERVAL).await;
        let (mut no_wake_watch, mut waking_watch) = (no_wake.watch(None), waking.watch(None));
        advance(SAMPLE_INTERVAL - WITHIN_COALESCE_WINDOW).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut no_wake_watch).await, Poll::Pending);
        advance(WITHIN_COALESCE_WINDOW).await;
        assert_matches!(
            TestExecutor::poll_until_stalled(&mut no_wake_watch).await,
            Poll::Ready(Ok(Ok((_, None))))
        );
        assert_matches!(
            TestExecutor::poll_until_stalled(&mut waking_watch).await,
            Poll::Ready(Ok(Ok((_, Some(_)))))
        );
        assert_eq!(harness.reads.get(), 2);
        assert_eq!(harness.fired_alarms.borrow().len(), 2);
        assert_eq!(harness.canceled_alarms.get(), 0);
    }

    /// Verifies that a pushed driver update resets the connection's `max_interval` deadline.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_pushed_update_restarts_interval() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect(SAMPLE_INTERVAL).await;

        let watch = battery.watch(None);
        advance(HALF_INTERVAL).await;
        harness.cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));

        // The next sample is due an interval after the pushed response.
        let mut watch = battery.watch(None);
        advance(HALF_INTERVAL).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        advance(HALF_INTERVAL).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Ready(_));
        assert_eq!(harness.reads.get(), 1);
    }

    /// Verifies that reconfiguring a connection with a shorter `max_interval` reschedules its
    /// pending sample deadline earlier, and samples immediately if the new interval has already
    /// elapsed rather than returning stale cached status.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_shorter_max_interval_reschedules_sample() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect(LONG_INTERVAL).await;

        let mut watch = battery.watch(None);
        advance(HALF_INTERVAL).await;
        configure_max_interval(
            &battery,
            Some(zx::BootDuration::from_nanos(SAMPLE_INTERVAL.into_nanos())),
        )
        .await;
        advance(HALF_INTERVAL).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Ready(_));
        assert_eq!(harness.reads.get(), 1);

        // Shortening `max_interval` to an already-elapsed duration triggers a fresh sample.
        configure_max_interval(
            &battery,
            Some(zx::BootDuration::from_nanos(LONG_INTERVAL.into_nanos())),
        )
        .await;
        let watch = battery.watch(None);
        *harness.driver_status.borrow_mut() = battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV);
        advance(SAMPLE_INTERVAL).await;
        configure_max_interval(
            &battery,
            Some(zx::BootDuration::from_nanos(SAMPLE_INTERVAL.into_nanos())),
        )
        .await;
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV));
        assert_eq!(harness.reads.get(), 2);
    }

    /// Verifies that clearing `max_interval` while a `Watch` is pending cancels the armed wake
    /// alarm, and that calling `Watch` after `max_interval` has already elapsed triggers a sample
    /// immediately.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_cancel_alarm_on_cleared_interval_and_late_watch_samples_immediately() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect(SAMPLE_INTERVAL).await;

        // Arm the alarm with a pending `Watch`, then clear `max_interval`.
        let mut watch = battery.watch(None);
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        configure_max_interval(&battery, None).await;
        let _: Poll<()> = TestExecutor::poll_until_stalled(std::future::pending()).await;
        assert_eq!(harness.canceled_alarms.get(), 1);

        // Complete the pending `Watch` via a pushed update, reconfigure `max_interval`, and let
        // `SAMPLE_INTERVAL` elapse before calling `Watch` again.
        harness.cache.publish_battery(&battery_status(NEW_LEVEL_PERCENT, VOLTAGE_UV), None);
        let _ = watch.await.unwrap().unwrap();
        configure_max_interval(
            &battery,
            Some(zx::BootDuration::from_nanos(SAMPLE_INTERVAL.into_nanos())),
        )
        .await;
        advance(SAMPLE_INTERVAL).await;
        assert_eq!(harness.reads.get(), 0);

        let (status, _) = battery.watch(None).await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert_eq!(harness.reads.get(), 1);
    }

    /// Verifies that periodic charger sampling polls the charger driver, completes due `Watch`
    /// calls, and records `charger_sample_interval` in Inspect.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_charger_periodic_sampling() {
        let harness = Harness::new(Alarms::Working);
        let charger_status =
            Rc::new(RefCell::new(fcharger::Status { online: Some(true), ..Default::default() }));
        let charger_reads = Rc::new(Cell::new(0));
        harness.cache.charger_connected(
            None,
            charger_subscription(),
            fake_charger_driver(charger_status.clone(), charger_reads.clone()),
        );
        harness.cache.publish_charger(&charger_status.borrow(), None);

        let charger = connect_charger(&harness.cache);
        let _ = configure_charger(
            &charger,
            fcharger::WatchOptions {
                max_interval: Some(SAMPLE_INTERVAL.into_nanos()),
                ..Default::default()
            },
        )
        .await;
        let _ = charger.watch(None).await.unwrap().unwrap();

        let watch = charger.watch(None);
        *charger_status.borrow_mut() =
            fcharger::Status { online: Some(false), ..Default::default() };
        advance(SAMPLE_INTERVAL).await;
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status, fcharger::Status { online: Some(false), ..Default::default() });
        assert_eq!(charger_reads.get(), 1);
        assert_eq!(harness.changed_notifications.get(), 1);
    }

    /// Verifies that if a driver read fails during a sample, the connection is retried after
    /// another `max_interval` rather than spinning immediately.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_failed_read_retries_after_interval() {
        let harness = Harness::new(Alarms::Working);
        let battery = harness.connect(SAMPLE_INTERVAL).await;
        harness.failing_reads.set(true);

        let mut watch = battery.watch(None);
        advance(SAMPLE_INTERVAL).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert_eq!(harness.reads.get(), 1);

        harness.failing_reads.set(false);
        advance(SAMPLE_INTERVAL - BEFORE_DEADLINE).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        assert_eq!(harness.reads.get(), 1);
        advance(BEFORE_DEADLINE).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Ready(_));
        assert_eq!(harness.reads.get(), 2);
    }

    /// Asserts that Inspect reports `wakeup` once the sampler has settled.
    async fn assert_wakeup(cache: &Cache, wakeup: &str, sample_count: u64) {
        let _: Poll<()> = TestExecutor::poll_until_stalled(std::future::pending()).await;
        let inspector = inspect::Inspector::default();
        cache.record_inspect(inspector.root());
        let hierarchy = inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(hierarchy, root: {
            governor_telemetry: contains {
                wakeup: wakeup.to_string(),
                sample_count: sample_count,
            },
        });
    }

    /// Verifies that a transient `WakeAlarms` error falls back to a timer for that sample and
    /// retries `WakeAlarms` on the next sample.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_failed_wake_alarm_falls_back_to_timer_once() {
        let harness = Harness::new(Alarms::Failing);
        let battery = harness.connect(SAMPLE_INTERVAL).await;

        for samples in 1..=2 {
            let mut watch = battery.watch(None);
            advance(SAMPLE_INTERVAL - BEFORE_DEADLINE).await;
            assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
            advance(BEFORE_DEADLINE).await;
            let (status, lease) = watch.await.unwrap().unwrap();
            assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
            assert!(lease.is_none());
            assert_eq!(harness.reads.get(), samples);
            // The next sample is scheduled with a wake alarm again.
            assert_wakeup(&harness.cache, "wake_alarms", samples.into()).await;
        }
    }

    /// Verifies that if the `WakeAlarms` channel is closed, the sampler permanently falls back to
    /// boot timers and records `wakeup = "timer"` in Inspect.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_falls_back_to_timers_if_wake_alarms_closed() {
        let harness = Harness::new(Alarms::Closed);
        let battery = harness.connect(SAMPLE_INTERVAL).await;

        let mut watch = battery.watch(None);
        advance(SAMPLE_INTERVAL - BEFORE_DEADLINE).await;
        assert_matches!(TestExecutor::poll_until_stalled(&mut watch).await, Poll::Pending);
        advance(BEFORE_DEADLINE).await;
        let (status, lease) = watch.await.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert!(lease.is_none());
        assert_eq!(harness.reads.get(), 1);
        assert_wakeup(&harness.cache, "timer", 1).await;

        // Samples can no longer wake the system, so `wake_on_max_interval` is not offered.
        let effective = configure_sampling(
            &battery,
            Some(zx::BootDuration::from_nanos(SAMPLE_INTERVAL.into_nanos())),
            Some(true),
        )
        .await;
        assert_eq!(effective.wake_on_max_interval, None);
    }

    /// Verifies that time spent suspended counts towards `max_interval`: when the boot clock
    /// passes the deadline while the monotonic clock is stopped, the wake alarm takes the sample.
    #[fuchsia::test]
    fn test_suspended_time_counts_towards_max_interval() {
        let mut executor = TestExecutor::new_with_fake_time();
        let harness = Harness::new(Alarms::Working);
        let mut connect = pin!(harness.connect(SAMPLE_INTERVAL));
        let Poll::Ready(battery) = executor.run_until_stalled(&mut connect) else {
            panic!("connect should complete without time passing");
        };

        let mut watch = battery.watch(None);
        assert_matches!(executor.run_until_stalled(&mut watch), Poll::Pending);
        executor.set_fake_boot_to_mono_offset(zx::BootDuration::from_nanos(
            SAMPLE_INTERVAL.into_nanos(),
        ));
        assert!(executor.wake_expired_timers());
        let Poll::Ready(response) = executor.run_until_stalled(&mut watch) else {
            panic!("the wake alarm should have completed the Watch");
        };
        let (status, lease) = response.unwrap().unwrap();
        assert_eq!(status, battery_status(LEVEL_PERCENT, VOLTAGE_UV));
        assert!(lease.is_some());
        assert_eq!(harness.reads.get(), 1);
        assert_eq!(harness.fired_alarms.borrow().len(), 1);
    }
}
