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
//! - `GetStatus` and `GetSpec` are answered from the cache.
//! - While a driver is disconnected, `GetStatus` and `GetSpec` fail with `IO`, and a pending
//!   `Watch` waits for the next driver.

use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use futures::TryStreamExt;
use log::{debug, warn};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Debug;

type ConnectionId = u64;

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
        }
    }

    /// Returns the options advertised in `Spec.supported_options`.
    fn supported_options(&self) -> Options<S> {
        Options { interest: self.interest.fields().cloned(), wake_on: Some(self.wake_on.clone()) }
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
    /// requested fields can be served.
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
        Self { interest: options.interest, wake_on: options.wake_on }
    }
}

impl From<Options<fbattery::Status>> for fbattery::WatchOptions {
    fn from(options: Options<fbattery::Status>) -> Self {
        Self { interest: options.interest, wake_on: options.wake_on, ..Default::default() }
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
        Self { interest: options.interest, wake_on: options.wake_on }
    }
}

impl From<Options<fcharger::Status>> for fcharger::WatchOptions {
    fn from(options: Options<fcharger::Status>) -> Self {
        Self { interest: options.interest, wake_on: options.wake_on, ..Default::default() }
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
    /// Status last sent in a `Watch` response, or `None` before the first response.
    last_sent: Option<P::Status>,
    /// Whether the next `Watch` completes as soon as a status is cached: initially, and once a
    /// field of interest changed since `last_sent` while no `Watch` was pending.
    dirty: bool,
    pending_watch: Option<P::WatchResponder>,
    /// Wake lease of a `wake_on` change that has not been sent yet, returned with the response
    /// that sends it.
    stashed_lease: Option<zx::EventPair>,
}

impl<P: Protocol> Connection<P> {
    /// Whether `status` should complete this connection's `Watch`.
    fn is_news(&self, status: &P::Status) -> bool {
        self.last_sent.as_ref().is_none_or(|last_sent| self.subscription.is_news(status, last_sent))
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
        let wake_lease = self.stashed_lease.take().or(client_lease);
        if let Err(e) = P::send_watch(responder, Ok((status, wake_lease))) {
            debug!("Failed to send status to governor {}: {e:?}", self.owner);
        }
    }

    /// Delivers `status` if it is news to this connection: now if a `Watch` is pending, or else
    /// on the next `Watch`. A duplicate of `lease` goes with it if `status` changes a field in the
    /// connection's `wake_on`.
    fn offer(&mut self, status: &P::Status, lease: Option<&zx::EventPair>) {
        if !self.is_news(status) {
            return;
        }
        if lease.is_some() && self.is_wake_on_change(status) {
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
    /// For on-demand reads.
    proxy: P::Proxy,
    /// Latest status, or `None` before the driver's first status.
    status: Option<P::Status>,
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
}

impl<P: Protocol> Topic<P> {
    fn new(subscription: Subscription<P::Status>) -> Self {
        Self { driver: None, subscription, connections: BTreeMap::new(), next_id: 0 }
    }

    fn add_connection(&mut self, owner: &str) -> ConnectionId {
        let id = self.next_id;
        self.next_id += 1;
        let requested = Options::default();
        let connection = Connection {
            owner: owner.to_owned(),
            subscription: self.subscription.limit(&requested),
            requested,
            last_sent: None,
            dirty: true,
            pending_watch: None,
            stashed_lease: None,
        };
        self.connections.insert(id, connection);
        id
    }

    fn remove_connection(&mut self, id: ConnectionId) {
        self.connections.remove(&id);
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
    }

    /// Clears the cache. Governor connections stay open and resume with the next driver's status,
    /// which is compared afresh with the status last sent on each.
    fn driver_gone(&mut self) {
        self.driver = None;
        for connection in self.connections.values_mut() {
            connection.dirty = false;
            connection.stashed_lease = None;
        }
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
        for connection in self.connections.values_mut() {
            connection.offer(status, lease);
        }
        driver.status = Some(status.clone());
    }

    /// Returns the connected driver's spec, as served to governors.
    fn spec(&self) -> Result<P::Spec, P::Error> {
        let driver = self.driver.as_ref().ok_or(P::ERROR_UNAVAILABLE)?;
        let spec = driver.spec.as_ref().ok_or(P::ERROR_NOT_SUPPORTED)?;
        Ok(P::with_supported_options(spec, self.subscription.supported_options()))
    }

    /// Applies a `ConfigureWatch` request and returns the effective options: `interest` is limited
    /// to the fields battery-manager is notified of, and `wake_on` to those its connection wakes
    /// on.
    fn configure_watch(
        &mut self,
        id: ConnectionId,
        requested: Options<P::Status>,
    ) -> Result<Options<P::Status>, P::Error> {
        let subscription = self.subscription.limit(&requested);
        // As on a driver: if fields were requested but none can be served, the configuration is
        // left unchanged.
        if requested.has_fields() && subscription.is_empty() {
            return Err(P::ERROR_NOT_SUPPORTED);
        }
        let effective = subscription.effective(&requested);
        let status = self.driver.as_ref().and_then(|driver| driver.status.as_ref());
        let connection = self.connections.get_mut(&id).expect("connection outlives its guard");
        if subscription.wake_on.is_empty() {
            connection.stashed_lease = None;
        }
        connection.requested = requested;
        connection.subscription = subscription;
        // The new subscription applies to a pending `Watch`, compared against the status last
        // sent.
        if let Some(status) = status {
            connection.offer(status, None);
        }
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
        Self {
            battery: RefCell::new(Topic::new(battery)),
            charger: RefCell::new(Topic::new(charger)),
        }
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
    use fuchsia_async as fasync;
    use fuchsia_async::TestExecutor;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::Poll;

    const GOVERNOR: &str = "governor";
    const LEVEL_PERCENT: f32 = 50.0;
    const NEW_LEVEL_PERCENT: f32 = 51.0;
    const VOLTAGE_UV: u32 = 3_800_000;
    const NEW_VOLTAGE_UV: u32 = 3_900_000;
    const DESIGN_CAPACITY_UAH: u32 = 5_000_000;

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
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        let cache = cache.clone();
        fasync::Task::local(async move {
            cache.serve_battery(GOVERNOR, stream).await.expect("serve_battery failed");
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
    /// battery-manager's subscription and returns `NOT_SUPPORTED` when no requested field can be
    /// served.
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
                max_interval: Some(zx::MonotonicDuration::from_seconds(1).into_nanos()),
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
                ..Default::default()
            }
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
}
