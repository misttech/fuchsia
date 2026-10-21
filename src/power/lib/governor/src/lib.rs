// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reusable charge governor runtime framework for Fuchsia.
//!
//! Provides the [`Governor`] trait and [`run_governor`], which bridges the battery and
//! charger telemetry served by battery-manager to voted [`fcharger::ControllerMarker`] settings.

use anyhow::{Context as _, Result, anyhow};
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use futures::FutureExt;
use log::{debug, info, warn};
use std::convert::Infallible;

const RECONNECT_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

/// Combined hardware telemetry snapshot passed to [`Governor::evaluate`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Telemetry {
    pub battery: fbattery::Status,
    pub charger: Option<fcharger::Status>,
}

/// Desired vote and optional periodic sampling, returned by [`Governor::evaluate`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Evaluation {
    /// Complete desired vote for this governor session. A `None` field withdraws this governor's
    /// vote on it, so that field relaxes to the other governors' votes or the charger default.
    pub vote: fcharger::ControlOptions,
    /// If `Some(interval)`, battery-manager is asked to deliver a freshly sampled battery status at
    /// least this often, so the governor is re-evaluated even when no watched field changes.
    /// battery-manager may clamp the interval. If `None`, the governor is re-evaluated only when
    /// the telemetry changes.
    pub sample_interval: Option<zx::BootDuration>,
    /// Whether a sample that falls due while the system is suspended wakes the system, rather than
    /// waiting for it to resume. Waking the system costs power, so a governor sets this only while
    /// its samples must not wait, e.g. while charging. Ignored without `sample_interval`.
    pub wake_for_samples: bool,
}

/// Battery and charger fields whose changes trigger an evaluation, and those whose changes also
/// wake the system from suspend to do so. Each is a `Status` mask: only field presence matters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchInterest {
    /// Sent as the battery `WatchOptions.interest`. `None` keeps battery-manager's default, which
    /// is every field it can watch. Changes to other fields are seen at the next evaluation, e.g.
    /// the next sample requested through [`Evaluation::sample_interval`].
    pub battery: Option<fbattery::Status>,
    /// Sent as the charger `WatchOptions.interest`, like `battery`.
    pub charger: Option<fcharger::Status>,
    /// Sent as the battery `WatchOptions.wake_on`. `None` wakes the system on no field.
    pub battery_wake_on: Option<fbattery::Status>,
    /// Sent as the charger `WatchOptions.wake_on`, like `battery_wake_on`.
    pub charger_wake_on: Option<fcharger::Status>,
}

/// Trait implemented by specific charging algorithms (e.g. MSC, thermal, battery defender).
pub trait Governor: Send + 'static {
    /// Evaluates the latest telemetry snapshot and returns the desired session vote and optional
    /// periodic sampling.
    ///
    /// May be called more than once per battery-manager update, usually with the same snapshot, so
    /// a call is not necessarily a new sample.
    fn evaluate(&mut self, telemetry: &Telemetry) -> Evaluation;

    /// Returns the fields whose changes trigger an evaluation or wake the system. Queried once per
    /// session.
    fn watch_interest(&self) -> WatchInterest {
        WatchInterest::default()
    }
}

/// Invokes `$m!(field)` for every `ControlOptions` field a governor can vote on. Merging, clearing,
/// and diffing votes all use this list. A field missing from it is never sent to the charger (see
/// [`unforwarded_fields`]), so add each new `ControlOptions` field here.
macro_rules! for_each_control_field {
    ($m:ident) => {
        $m!(operating_mode);
        $m!(input_current_limit_ua);
        $m!(charge_current_limit_ua);
        $m!(float_voltage_uv);
        $m!(charge_term_current_ua);
        $m!(input_voltage_limit_uv);
        $m!(top_off_duration);
    };
}

/// Returns the fields of `vote` that `for_each_control_field!` doesn't list, which are never sent.
fn unforwarded_fields(vote: &fcharger::ControlOptions) -> fcharger::ControlOptions {
    let mut rest = vote.clone();
    macro_rules! strip {
        ($f:ident) => {
            rest.$f = None;
        };
    }
    for_each_control_field!(strip);
    rest
}

/// Copies the `Some` fields of `patch` into `vote`, keeping `vote`'s other fields.
pub fn merge_vote(vote: &mut fcharger::ControlOptions, patch: &fcharger::ControlOptions) {
    macro_rules! merge {
        ($f:ident) => {
            vote.$f = patch.$f.or(vote.$f);
        };
    }
    for_each_control_field!(merge);
}

/// Clears the fields selected by `mask` from `vote`.
pub fn clear_vote_fields(vote: &mut fcharger::ControlOptions, mask: &fcharger::ControlOptions) {
    macro_rules! clear {
        ($f:ident) => {
            if mask.$f.is_some() {
                vote.$f = None;
            }
        };
    }
    for_each_control_field!(clear);
}

/// Computes `(to_set, to_clear)` delta masks between `applied` and `desired`.
pub fn diff_controls(
    applied: &fcharger::ControlOptions,
    desired: &fcharger::ControlOptions,
) -> (fcharger::ControlOptions, fcharger::ControlOptions) {
    let mut to_set = fcharger::ControlOptions::default();
    let mut to_clear = fcharger::ControlOptions::default();
    // `ClearControl` ignores values, so `to_clear` carries the withdrawn values for logging.
    macro_rules! diff {
        ($f:ident) => {
            to_set.$f = desired.$f.filter(|&v| Some(v) != applied.$f);
            to_clear.$f = applied.$f.filter(|_| desired.$f.is_none());
        };
    }
    for_each_control_field!(diff);
    (to_set, to_clear)
}

/// Evaluates `telemetry` and synchronizes the resulting vote with `controller`.
///
/// Fields to set are sent before fields to clear, so that the vote never relaxes before it
/// restricts. If `SetControl` is rejected, `ClearControl` is skipped so that no field is relaxed
/// while a new restriction could not be applied. Rejected changes are logged and retried on the
/// next evaluation, while the session and the votes `controller` holds are kept. `last_accepted`
/// tracks the vote `controller` accepted. The evaluation is returned even if a change was
/// rejected, so its sampling still takes effect. Returns `Err` only on FIDL transport failure.
async fn process_telemetry_event<T: Governor>(
    governor: &mut T,
    telemetry: &Telemetry,
    last_accepted: &mut fcharger::ControlOptions,
    controller: &fcharger::ControllerProxy,
) -> Result<Evaluation> {
    let evaluation = governor.evaluate(telemetry);
    let unforwarded = unforwarded_fields(&evaluation.vote);
    if unforwarded != fcharger::ControlOptions::default() {
        static WARN_UNFORWARDED: std::sync::Once = std::sync::Once::new();
        WARN_UNFORWARDED
            .call_once(|| warn!("Ignoring vote fields the harness can't send: {unforwarded:?}"));
    }
    let (to_set, to_clear) = diff_controls(last_accepted, &evaluation.vote);

    if to_set != fcharger::ControlOptions::default() {
        match controller.set_control(&to_set).await.context("Controller.SetControl")? {
            Ok(()) => merge_vote(last_accepted, &to_set),
            Err(err) => {
                warn!("Charger controller rejected SetControl ({to_set:?}): {err:?}");
                return Ok(evaluation);
            }
        }
    }

    if to_clear != fcharger::ControlOptions::default() {
        match controller.clear_control(&to_clear).await.context("Controller.ClearControl")? {
            Ok(()) => clear_vote_fields(last_accepted, &to_clear),
            Err(err) => {
                warn!("Charger controller rejected ClearControl ({to_clear:?}): {err:?}");
            }
        }
    }

    Ok(evaluation)
}

/// The periodic battery sampling a governor votes for.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sampling {
    /// Longest time between battery statuses, or `None` for no periodic samples.
    interval: Option<zx::BootDuration>,
    /// Whether samples wake the system from suspend. Only meaningful with an `interval`.
    wake: bool,
}

impl From<&Evaluation> for Sampling {
    fn from(evaluation: &Evaluation) -> Self {
        let interval = evaluation.sample_interval;
        Self { interval, wake: interval.is_some() && evaluation.wake_for_samples }
    }
}

/// Configures `Battery.Watch` with the battery fields of `interest`, and votes for battery-manager
/// to sample the battery as `sampling` asks, or withdraws the vote if `sampling.interval` is
/// `None`.
///
/// `ConfigureWatch` replaces all options, so `interest` is sent with every change in `sampling`.
/// Rejections are logged rather than retried. Returns `Err` only on FIDL transport failure.
async fn configure_battery_watch(
    battery: &fbattery::BatteryProxy,
    interest: &WatchInterest,
    sampling: Sampling,
) -> Result<()> {
    let options = fbattery::WatchOptions {
        interest: interest.battery.clone(),
        wake_on: interest.battery_wake_on.clone(),
        max_interval: sampling.interval.map(zx::BootDuration::into_nanos),
        wake_on_max_interval: sampling.wake.then_some(true),
        ..Default::default()
    };
    match battery.configure_watch(&options).await.context("Battery.ConfigureWatch")? {
        Ok(effective) if effective == options => debug!("Battery watch options set to {options:?}"),
        Ok(effective) => info!("Battery honors watch options {effective:?} instead of {options:?}"),
        Err(e) => warn!("Battery.ConfigureWatch rejected {options:?}: {e:?}"),
    }
    Ok(())
}

/// Configures `Charger.Watch` with the charger fields of `interest`.
///
/// Rejections are logged rather than retried. Returns `Err` only on FIDL transport failure.
async fn configure_charger_watch(
    charger: &fcharger::ChargerProxy,
    interest: &WatchInterest,
) -> Result<()> {
    let options = fcharger::WatchOptions {
        interest: interest.charger.clone(),
        wake_on: interest.charger_wake_on.clone(),
        ..Default::default()
    };
    match charger.configure_watch(&options).await.context("Charger.ConfigureWatch")? {
        Ok(effective) if effective == options => debug!("Charger watch options set to {options:?}"),
        Ok(effective) => info!("Charger honors watch options {effective:?} instead of {options:?}"),
        Err(e) => warn!("Charger.ConfigureWatch rejected {options:?}: {e:?}"),
    }
    Ok(())
}

/// Connections to battery-manager for one governor session.
struct Session {
    battery: fbattery::BatteryProxy,
    charger: fcharger::ChargerProxy,
    controller: fcharger::ControllerProxy,
}

impl Session {
    /// Connects to the protocols battery-manager provides in the incoming namespace.
    fn connect() -> Result<Self> {
        Ok(Self {
            battery: connect_to_protocol::<fbattery::BatteryMarker>()?,
            charger: connect_to_protocol::<fcharger::ChargerMarker>()?,
            controller: connect_to_protocol::<fcharger::ControllerMarker>()?,
        })
    }
}

/// Records the status of a `Battery.Watch` response in `latest` and returns its wake lease, which
/// the caller holds until it passes it to the next `Watch`.
fn battery_response(
    result: Result<fbattery::BatteryWatchResult, fidl::Error>,
    latest: &mut Option<fbattery::Status>,
) -> Result<Option<zx::EventPair>> {
    let (status, wake_lease) =
        result.context("Battery.Watch")?.map_err(|e| anyhow!("Battery.Watch failed: {e:?}"))?;
    *latest = Some(status);
    Ok(wake_lease)
}

/// Records the status of a `Charger.Watch` response in `latest` and returns its wake lease, which
/// the caller holds until it passes it to the next `Watch`.
fn charger_response(
    result: Result<fcharger::ChargerWatchResult, fidl::Error>,
    latest: &mut Option<fcharger::Status>,
) -> Result<Option<zx::EventPair>> {
    let (status, wake_lease) =
        result.context("Charger.Watch")?.map_err(|e| anyhow!("Charger.Watch failed: {e:?}"))?;
    *latest = Some(status);
    Ok(wake_lease)
}

/// Runs the governor event loop over one `session` until a connection fails.
///
/// Configures [`Governor::watch_interest`] on both connections before the first `Watch`. Then, on
/// each update once a battery status is known:
///
/// - **Snapshot**: the statuses arrive on separate channels, so a `Watch` response is only a
///   trigger. If the other response isn't ready, the other status is read with `GetStatus`, which
///   battery-manager updates before completing either `Watch`. So the governor never evaluates a
///   new status with a stale one, e.g. when it wakes between battery-manager's responses to one
///   charger event, and a narrowed charger interest still sees other charger fields, such as the
///   charge phase, at the next battery sample. The later response re-evaluates, usually the same
///   snapshot, so [`Governor::evaluate`] may run more than once per update.
/// - **No charger**: once `Charger.GetStatus` fails with no charger status known, as on a board
///   without a charger driver, the charger is not read again until `Charger.Watch` reports it.
/// - **Sampling**: `Evaluation::sample_interval` and `Evaluation::wake_for_samples` are voted
///   through `Battery.ConfigureWatch` whenever they change. Only the battery is sampled: charger
///   changes in the governor's interest are notified as they happen, whereas battery fields such
///   as temperature and voltage are only observed by sampling.
/// - **Wake leases**: a response's lease is held while the update is handled, then passed to the
///   next `Watch` on that connection, which hands it back if it completes immediately and drops it
///   if it is parked.
async fn run_session<T: Governor>(governor: &mut T, session: &Session) -> Result<Infallible> {
    let Session { battery, charger, controller } = session;
    let mut last_accepted = fcharger::ControlOptions::default();
    // battery-manager starts every connection without a `max_interval` vote.
    let mut sampling = Sampling::default();
    let mut latest_battery = None;
    let mut latest_charger = None;
    // Whether `Charger.GetStatus` failed before any charger status was known.
    let mut charger_unavailable = false;

    let interest = governor.watch_interest();
    if interest.battery.is_some() || interest.battery_wake_on.is_some() {
        configure_battery_watch(battery, &interest, sampling).await?;
    }
    if interest.charger.is_some() || interest.charger_wake_on.is_some() {
        configure_charger_watch(charger, &interest).await?;
    }

    let mut battery_watch = battery.watch(None).fuse();
    let mut charger_watch = charger.watch(None).fuse();

    loop {
        let mut battery_updated = false;
        let mut charger_updated = false;
        let mut battery_lease = None;
        let mut charger_lease = None;
        futures::select_biased! {
            result = battery_watch => {
                battery_lease = battery_response(result, &mut latest_battery)?;
                battery_updated = true;
                if let Some(result) = (&mut charger_watch).now_or_never() {
                    charger_lease = charger_response(result, &mut latest_charger)?;
                    charger_updated = true;
                }
            }
            result = charger_watch => {
                charger_lease = charger_response(result, &mut latest_charger)?;
                charger_updated = true;
                if let Some(result) = (&mut battery_watch).now_or_never() {
                    battery_lease = battery_response(result, &mut latest_battery)?;
                    battery_updated = true;
                }
            }
        }
        if charger_updated {
            charger_unavailable = false;
        }

        // Read the status that did not trigger this update, so that it is evaluated as of the same
        // battery-manager update. This also picks up fields outside the governor's interest, such
        // as the battery current on plug-in, which change without completing `Watch`.
        // battery-manager re-reads the battery before it publishes a charger update, so the cached
        // battery status is current. It fails while no battery driver is connected.
        if !battery_updated {
            match battery.get_status().await.context("Battery.GetStatus")? {
                Ok(status) => latest_battery = Some(status),
                Err(e) => warn!("Battery.GetStatus failed ({e:?}); using the last battery status"),
            }
        }
        if !charger_updated && !charger_unavailable {
            match charger.get_status().await.context("Charger.GetStatus")? {
                Ok(status) => latest_charger = Some(status),
                Err(e) if latest_charger.is_none() => {
                    debug!("No charger status ({e:?}); not reading it until Charger.Watch reports");
                    charger_unavailable = true;
                }
                Err(e) => warn!("Charger.GetStatus failed ({e:?}); using the last charger status"),
            }
        }

        if let Some(battery_status) = &latest_battery {
            let telemetry =
                Telemetry { battery: battery_status.clone(), charger: latest_charger.clone() };
            let evaluation =
                process_telemetry_event(governor, &telemetry, &mut last_accepted, controller)
                    .await?;
            let requested = Sampling::from(&evaluation);
            if requested != sampling {
                configure_battery_watch(battery, &interest, requested).await?;
                sampling = requested;
            }
        }

        // Re-arm the hanging gets only after the battery refresh, the vote, and the sampling, so
        // that the wake leases of the responses keep the system awake across actuation.
        if battery_updated {
            battery_watch = battery.watch(battery_lease).fuse();
        }
        if charger_updated {
            charger_watch = charger.watch(charger_lease).fuse();
        }
    }
}

/// Runs sessions obtained from `connect` forever, reconnecting [`RECONNECT_DELAY`] after each
/// failure.
async fn supervise<T: Governor>(
    governor: &mut T,
    mut connect: impl FnMut() -> Result<Session>,
) -> Infallible {
    loop {
        match connect() {
            Ok(session) => {
                let Err(e) = run_session(governor, &session).await;
                warn!("Governor session ended ({e:?}); reconnecting in {RECONNECT_DELAY:?}");
            }
            Err(e) => warn!("Failed to connect to battery-manager ({e:?}); retrying"),
        }
        fasync::Timer::new(fasync::MonotonicInstant::after(RECONNECT_DELAY)).await;
    }
}

/// Main execution loop for an autonomous charge governor component. Never returns.
///
/// Publishes the component's singleton [`fuchsia_inspect::Inspector`] and connects to
/// `fuchsia.hardware.power.battery.Battery`, `fuchsia.hardware.power.charger.Charger` and
/// `fuchsia.hardware.power.charger.Controller`, all provided by battery-manager, reconnecting
/// whenever any of them fails.
pub async fn run_governor<T: Governor>(mut governor: T) -> Result<()> {
    let _inspect_server_task = inspect_runtime::publish(
        fuchsia_inspect::component::inspector(),
        inspect_runtime::PublishOptions::default(),
    );
    match supervise(&mut governor, Session::connect).await {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use futures::future::LocalBoxFuture;
    use futures::{Stream, StreamExt, TryStreamExt};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::task::Poll;

    const TEST_FCC_HIGH_UA: u32 = 1_500_000;
    const TEST_FCC_LOW_UA: u32 = 800_000;
    const TEST_FV_UV: u32 = 4_200_000;
    const TEST_ICL_UA: u32 = 2_000_000;
    const TEST_ITERM_UA: u32 = 100_000;
    const TEST_VINDPM_UV: u32 = 4_600_000;
    const TEST_TOP_OFF_DURATION_NANOS: i64 = 30_000_000_000;
    const TEST_VBAT_UV: u32 = 3_900_000;
    const TEST_VBAT_UPDATED_UV: u32 = 3_950_000;
    const TEST_SAMPLE_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(5);
    const TEST_LONGER_SAMPLE_INTERVAL: zx::BootDuration = zx::BootDuration::from_seconds(10);
    const TEST_LONG_IDLE: zx::MonotonicDuration = zx::MonotonicDuration::from_hours(1);
    const ONE_NANO: zx::MonotonicDuration = zx::MonotonicDuration::from_nanos(1);

    #[derive(Clone, Debug, PartialEq)]
    enum ControllerCall {
        Set(fcharger::ControlOptions),
        Clear(fcharger::ControlOptions),
    }

    struct SequenceGovernor {
        votes: Vec<Evaluation>,
        idx: usize,
        interest: WatchInterest,
        /// Every telemetry snapshot passed to `evaluate`, in order.
        seen: Arc<Mutex<Vec<Telemetry>>>,
    }

    impl SequenceGovernor {
        /// Returns `votes` in order, then repeats the last one forever.
        fn new(votes: Vec<Evaluation>) -> Self {
            Self::with_interest(votes, WatchInterest::default())
        }

        fn with_interest(votes: Vec<Evaluation>, interest: WatchInterest) -> Self {
            Self { votes, idx: 0, interest, seen: Arc::default() }
        }
    }

    impl Governor for SequenceGovernor {
        fn evaluate(&mut self, telemetry: &Telemetry) -> Evaluation {
            self.seen.lock().unwrap().push(telemetry.clone());
            let eval = self.votes[self.idx.min(self.votes.len() - 1)].clone();
            self.idx += 1;
            eval
        }

        fn watch_interest(&self) -> WatchInterest {
            self.interest.clone()
        }
    }

    fn battery_status(voltage_uv: u32) -> fbattery::Status {
        fbattery::Status { voltage_uv: Some(voltage_uv), ..Default::default() }
    }

    fn fcc_vote(fcc_ua: u32) -> fcharger::ControlOptions {
        fcharger::ControlOptions { charge_current_limit_ua: Some(fcc_ua), ..Default::default() }
    }

    /// A vote with every `ControlOptions` field populated.
    fn full_vote() -> fcharger::ControlOptions {
        fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Passthrough),
            input_current_limit_ua: Some(TEST_ICL_UA),
            charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
            float_voltage_uv: Some(TEST_FV_UV),
            charge_term_current_ua: Some(TEST_ITERM_UA),
            input_voltage_limit_uv: Some(TEST_VINDPM_UV),
            top_off_duration: Some(TEST_TOP_OFF_DURATION_NANOS),
            ..Default::default()
        }
    }

    /// Serves `stream`, recording every call and replying with `set_result` / `clear_result`.
    fn serve_controller(
        mut stream: fcharger::ControllerRequestStream,
        set_result: Result<(), fcharger::Error>,
        clear_result: Result<(), fcharger::Error>,
    ) -> (fasync::Task<()>, Arc<Mutex<Vec<ControllerCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let task = fasync::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    fcharger::ControllerRequest::SetControl { options, responder } => {
                        calls_clone.lock().unwrap().push(ControllerCall::Set(options));
                        let _ = responder.send(set_result);
                    }
                    fcharger::ControllerRequest::ClearControl { options, responder } => {
                        calls_clone.lock().unwrap().push(ControllerCall::Clear(options));
                        let _ = responder.send(clear_result);
                    }
                    fcharger::ControllerRequest::_UnknownMethod { .. } => {}
                }
            }
        });
        (task, calls)
    }

    /// Verifies that every field a governor votes on is listed in `for_each_control_field!`, so the
    /// harness sends all of them.
    #[fuchsia::test]
    fn test_unforwarded_fields_is_empty_for_every_field() {
        assert_eq!(unforwarded_fields(&full_vote()), fcharger::ControlOptions::default());
    }

    /// Verifies that `merge_vote` overwrites only the fields present in `patch` and leaves `None`
    /// fields in `patch` untouched.
    #[fuchsia::test]
    fn test_merge_vote_table() {
        struct Case {
            name: &'static str,
            vote: fcharger::ControlOptions,
            patch: fcharger::ControlOptions,
            expected: fcharger::ControlOptions,
        }
        let cases = [
            Case {
                name: "empty patch leaves vote unchanged",
                vote: full_vote(),
                patch: fcharger::ControlOptions::default(),
                expected: full_vote(),
            },
            Case {
                name: "patch onto empty vote copies patch",
                vote: fcharger::ControlOptions::default(),
                patch: full_vote(),
                expected: full_vote(),
            },
            Case {
                name: "partial patch overrides only its fields",
                vote: full_vote(),
                patch: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                },
                expected: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..full_vote()
                },
            },
            Case {
                name: "patch adds new field alongside existing",
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                patch: fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
                expected: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
            },
        ];
        for Case { name, mut vote, patch, expected } in cases {
            merge_vote(&mut vote, &patch);
            assert_eq!(vote, expected, "case: {name}");
        }
    }

    /// Verifies that `clear_vote_fields` clears only the fields present in `mask`, regardless of
    /// the field values inside `mask`.
    #[fuchsia::test]
    fn test_clear_vote_fields_table() {
        struct Case {
            name: &'static str,
            vote: fcharger::ControlOptions,
            mask: fcharger::ControlOptions,
            expected: fcharger::ControlOptions,
        }
        let cases = [
            Case {
                name: "empty mask leaves vote unchanged",
                vote: full_vote(),
                mask: fcharger::ControlOptions::default(),
                expected: full_vote(),
            },
            Case {
                name: "full mask clears every field",
                vote: full_vote(),
                mask: full_vote(),
                expected: fcharger::ControlOptions::default(),
            },
            Case {
                name: "mask value is ignored, only presence matters",
                vote: full_vote(),
                mask: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Otg),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                },
                expected: fcharger::ControlOptions {
                    operating_mode: None,
                    charge_current_limit_ua: None,
                    ..full_vote()
                },
            },
            Case {
                name: "clearing already-absent field is a no-op",
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                mask: fcharger::ControlOptions { float_voltage_uv: Some(0), ..Default::default() },
                expected: fcc_vote(TEST_FCC_HIGH_UA),
            },
        ];
        for Case { name, mut vote, mask, expected } in cases {
            clear_vote_fields(&mut vote, &mask);
            assert_eq!(vote, expected, "case: {name}");
        }
    }

    /// Verifies that `diff_controls` partitions changes between `applied` and `desired` into
    /// `to_set` (added or modified fields) and `to_clear` (withdrawn fields).
    #[fuchsia::test]
    fn test_diff_controls_table() {
        struct Case {
            name: &'static str,
            applied: fcharger::ControlOptions,
            desired: fcharger::ControlOptions,
            to_set: fcharger::ControlOptions,
            to_clear: fcharger::ControlOptions,
        }
        let cases = [
            Case {
                name: "both empty",
                applied: fcharger::ControlOptions::default(),
                desired: fcharger::ControlOptions::default(),
                to_set: fcharger::ControlOptions::default(),
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "identical full vote",
                applied: full_vote(),
                desired: full_vote(),
                to_set: fcharger::ControlOptions::default(),
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "all fields newly set",
                applied: fcharger::ControlOptions::default(),
                desired: full_vote(),
                to_set: full_vote(),
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "all fields withdrawn",
                applied: full_vote(),
                desired: fcharger::ControlOptions::default(),
                to_set: fcharger::ControlOptions::default(),
                to_clear: full_vote(),
            },
            Case {
                name: "single field changed",
                applied: full_vote(),
                desired: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..full_vote()
                },
                to_set: fcc_vote(TEST_FCC_LOW_UA),
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "operating mode switch is a set",
                applied: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    ..Default::default()
                },
                desired: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    ..Default::default()
                },
                to_set: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    ..Default::default()
                },
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "changing a value to zero is a set, not a clear",
                applied: fcc_vote(TEST_FCC_HIGH_UA),
                desired: fcc_vote(0),
                to_set: fcc_vote(0),
                to_clear: fcharger::ControlOptions::default(),
            },
            Case {
                name: "mixed set and clear",
                applied: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
                desired: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    input_current_limit_ua: Some(TEST_ICL_UA),
                    ..Default::default()
                },
                to_set: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    input_current_limit_ua: Some(TEST_ICL_UA),
                    ..Default::default()
                },
                to_clear: fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
            },
        ];
        for Case { name, applied, desired, to_set, to_clear } in cases {
            assert_eq!(diff_controls(&applied, &desired), (to_set, to_clear), "case: {name}");
        }
    }

    /// Verifies that `process_telemetry_event` sends only changed fields, deduplicates identical
    /// consecutive votes, and orders `SetControl` before `ClearControl`.
    #[fuchsia::test]
    async fn test_process_telemetry_event_sets_and_clears_diffs() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let (_server, calls) = serve_controller(controller_stream, Ok(()), Ok(()));

        let mut gov = SequenceGovernor::new(vec![
            // 1. Initial vote: FCC + FV + Passthrough
            Evaluation {
                vote: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                ..Default::default()
            },
            // 2. Identical vote -> no FIDL calls
            Evaluation {
                vote: fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                ..Default::default()
            },
            // 3. Lower FCC and clear operating_mode -> SetControl(FCC) + ClearControl(mode)
            Evaluation {
                vote: fcharger::ControlOptions {
                    operating_mode: None,
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                },
                sample_interval: None,
                ..Default::default()
            },
        ]);

        let telemetry = Telemetry::default();
        let mut last_accepted = fcharger::ControlOptions::default();

        // Step 1: full initial vote
        let eval1 =
            process_telemetry_event(&mut gov, &telemetry, &mut last_accepted, &controller_proxy)
                .await
                .unwrap();
        assert_eq!(eval1.sample_interval, Some(TEST_SAMPLE_INTERVAL));
        assert_eq!(
            std::mem::take(&mut *calls.lock().unwrap()),
            vec![ControllerCall::Set(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Passthrough),
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_UV),
                ..Default::default()
            })]
        );

        // Step 2: deduplicated -> empty calls
        let _ =
            process_telemetry_event(&mut gov, &telemetry, &mut last_accepted, &controller_proxy)
                .await
                .unwrap();
        assert!(calls.lock().unwrap().is_empty());

        // Step 3: SetControl(FCC) + ClearControl(operating_mode)
        let eval3 =
            process_telemetry_event(&mut gov, &telemetry, &mut last_accepted, &controller_proxy)
                .await
                .unwrap();
        assert_eq!(eval3.sample_interval, None);
        assert_eq!(
            std::mem::take(&mut *calls.lock().unwrap()),
            vec![
                ControllerCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                }),
                ControllerCall::Clear(fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    ..Default::default()
                }),
            ]
        );
    }

    /// Verifies that a domain error from `SetControl` is logged rather than treated as a fatal
    /// session error, leaves `last_accepted` unchanged, and skips `ClearControl`.
    #[fuchsia::test]
    async fn test_process_telemetry_event_rejection_is_not_an_error() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let (_server, calls) =
            serve_controller(controller_stream, Err(fcharger::Error::InvalidArgs), Ok(()));
        let mut gov = SequenceGovernor::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_LOW_UA),
            sample_interval: None,
            ..Default::default()
        }]);

        let initial_accepted = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
            float_voltage_uv: Some(TEST_FV_UV),
            ..Default::default()
        };
        let mut last_accepted = initial_accepted.clone();
        let res = process_telemetry_event(
            &mut gov,
            &Telemetry::default(),
            &mut last_accepted,
            &controller_proxy,
        )
        .await;
        assert!(res.is_ok(), "rejection is not an error: {res:?}");
        // ClearControl is skipped when SetControl fails, and last_accepted is unchanged.
        assert_eq!(
            std::mem::take(&mut *calls.lock().unwrap()),
            vec![ControllerCall::Set(fcc_vote(TEST_FCC_LOW_UA))]
        );
        assert_eq!(last_accepted, initial_accepted);
    }

    /// Verifies that an initial empty vote sends no `SetControl` or `ClearControl` calls.
    #[fuchsia::test]
    async fn test_process_telemetry_event_empty_vote_initializes_without_calls() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let (_server, calls) = serve_controller(controller_stream, Ok(()), Ok(()));
        let mut gov = SequenceGovernor::new(vec![Evaluation::default()]);

        let mut last_accepted = fcharger::ControlOptions::default();
        process_telemetry_event(
            &mut gov,
            &Telemetry::default(),
            &mut last_accepted,
            &controller_proxy,
        )
        .await
        .unwrap();

        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(last_accepted, fcharger::ControlOptions::default());
    }

    /// Verifies that `last_accepted` stays in sync with the desired vote across a sequence of
    /// arbitrary set, modify, and clear transitions when all `Controller` calls succeed.
    #[fuchsia::test]
    async fn test_process_telemetry_event_tracks_desired_vote_across_sequence() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let (_server, _calls) = serve_controller(controller_stream, Ok(()), Ok(()));
        let votes = vec![
            full_vote(),
            fcharger::ControlOptions::default(),
            fcc_vote(TEST_FCC_LOW_UA),
            full_vote(),
            fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                float_voltage_uv: Some(TEST_FV_UV),
                ..Default::default()
            },
            fcharger::ControlOptions::default(),
        ];
        let mut gov = SequenceGovernor::new(
            votes
                .iter()
                .cloned()
                .map(|vote| Evaluation { vote, sample_interval: None, ..Default::default() })
                .collect(),
        );

        let mut last_accepted = fcharger::ControlOptions::default();
        for (step, vote) in votes.into_iter().enumerate() {
            process_telemetry_event(
                &mut gov,
                &Telemetry::default(),
                &mut last_accepted,
                &controller_proxy,
            )
            .await
            .unwrap();
            assert_eq!(last_accepted, vote, "step {step}");
        }
    }

    /// Verifies that if `SetControl` succeeds and `ClearControl` is rejected in the same turn,
    /// `last_accepted` records the accepted `SetControl` while retaining the un-cleared field.
    #[fuchsia::test]
    async fn test_process_telemetry_event_clear_rejection_keeps_accepted_set() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let (_server, calls) =
            serve_controller(controller_stream, Ok(()), Err(fcharger::Error::NotSupported));
        let mut gov = SequenceGovernor::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_LOW_UA),
            sample_interval: None,
            ..Default::default()
        }]);

        let mut last_accepted = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
            float_voltage_uv: Some(TEST_FV_UV),
            ..Default::default()
        };
        let res = process_telemetry_event(
            &mut gov,
            &Telemetry::default(),
            &mut last_accepted,
            &controller_proxy,
        )
        .await;

        assert!(res.is_ok(), "rejection is not an error: {res:?}");
        assert_eq!(
            std::mem::take(&mut *calls.lock().unwrap()),
            vec![
                ControllerCall::Set(fcc_vote(TEST_FCC_LOW_UA)),
                ControllerCall::Clear(fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_UV),
                    ..Default::default()
                }),
            ]
        );
        // The accepted SetControl is recorded; the rejected clear leaves FV in place.
        assert_eq!(
            last_accepted,
            fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                float_voltage_uv: Some(TEST_FV_UV),
                ..Default::default()
            }
        );
    }

    /// Verifies that a FIDL transport error on `Controller` is propagated as `Err`.
    #[fuchsia::test]
    async fn test_process_telemetry_event_propagates_transport_error() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        drop(controller_stream);
        let mut gov = SequenceGovernor::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);

        let mut last_accepted = fcharger::ControlOptions::default();
        let res = process_telemetry_event(
            &mut gov,
            &Telemetry::default(),
            &mut last_accepted,
            &controller_proxy,
        )
        .await;

        assert!(res.is_err());
        assert_eq!(last_accepted, fcharger::ControlOptions::default());
    }

    /// Verifies that a FIDL transport error on `Controller.ClearControl` is propagated as `Err`.
    #[fuchsia::test]
    async fn test_process_telemetry_event_clear_propagates_transport_error() {
        let (controller_proxy, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        drop(controller_stream);
        let mut gov = SequenceGovernor::new(vec![Evaluation::default()]);

        let mut last_accepted = fcc_vote(TEST_FCC_HIGH_UA);
        let res = process_telemetry_event(
            &mut gov,
            &Telemetry::default(),
            &mut last_accepted,
            &controller_proxy,
        )
        .await;

        assert!(res.is_err());
        assert_eq!(last_accepted, fcc_vote(TEST_FCC_HIGH_UA));
    }

    /// Polls `stream` once, returning its next item if one is ready (`None` if pending or closed).
    fn poll_next<S: Stream + Unpin>(
        executor: &mut fasync::TestExecutor,
        stream: &mut S,
    ) -> Option<S::Item> {
        match executor.run_until_stalled(&mut stream.next()) {
            Poll::Ready(item) => item,
            Poll::Pending => None,
        }
    }

    /// Server ends of one [`Session`].
    struct ServerEnds {
        battery: fbattery::BatteryRequestStream,
        charger: fcharger::ChargerRequestStream,
        controller: fcharger::ControllerRequestStream,
    }

    fn new_session() -> (Session, ServerEnds) {
        let (battery, battery_stream) = create_proxy_and_stream::<fbattery::BatteryMarker>();
        let (charger, charger_stream) = create_proxy_and_stream::<fcharger::ChargerMarker>();
        let (controller, controller_stream) =
            create_proxy_and_stream::<fcharger::ControllerMarker>();
        let server = ServerEnds {
            battery: battery_stream,
            charger: charger_stream,
            controller: controller_stream,
        };
        (Session { battery, charger, controller }, server)
    }

    /// Drives [`supervise`] on a fake-time executor against in-process fake Battery, Charger, and
    /// Controller servers whose requests the test answers by hand.
    ///
    /// Closing a server requires setting its stream to `None` and dropping any outstanding
    /// responders, since they keep the channel alive.
    struct LoopHarness {
        run: LocalBoxFuture<'static, Infallible>,
        battery: Option<fbattery::BatteryRequestStream>,
        charger: Option<fcharger::ChargerRequestStream>,
        controller: Option<fcharger::ControllerRequestStream>,
        /// Server ends of the sessions the governor connected that the test has yet to take.
        connected: Rc<RefCell<VecDeque<ServerEnds>>>,
        seen: Arc<Mutex<Vec<Telemetry>>>,
        // Declared last so it outlives the channels above when the harness is dropped.
        executor: fasync::TestExecutor,
    }

    impl LoopHarness {
        /// Starts the governor with the default watch interest and takes its first session.
        fn new(votes: Vec<Evaluation>) -> Self {
            Self::with_interest(votes, WatchInterest::default())
        }

        /// Starts the governor with `interest` and takes its first session.
        fn with_interest(votes: Vec<Evaluation>, interest: WatchInterest) -> Self {
            let executor = fasync::TestExecutor::new_with_fake_time();
            let connected: Rc<RefCell<VecDeque<ServerEnds>>> = Rc::default();
            let mut governor = SequenceGovernor::with_interest(votes, interest);
            let seen = governor.seen.clone();
            let sessions = connected.clone();
            let run = Box::pin(async move {
                let connect = || {
                    let (session, server) = new_session();
                    sessions.borrow_mut().push_back(server);
                    Ok(session)
                };
                supervise(&mut governor, connect).await
            });
            let mut h = Self {
                run,
                battery: None,
                charger: None,
                controller: None,
                connected,
                seen,
                executor,
            };
            assert!(h.take_session(), "governor did not connect");
            h
        }

        /// Serves the next session the governor connected, returning whether there was one.
        fn take_session(&mut self) -> bool {
            self.pump();
            let Some(server) = self.connected.borrow_mut().pop_front() else {
                return false;
            };
            self.battery = Some(server.battery);
            self.charger = Some(server.charger);
            self.controller = Some(server.controller);
            true
        }

        /// Runs the governor until it stalls.
        fn pump(&mut self) {
            match self.executor.run_until_stalled(&mut self.run) {
                Poll::Pending => {}
                Poll::Ready(never) => match never {},
            }
        }

        /// Advances fake time by `duration` and fires any expired timers.
        fn advance(&mut self, duration: zx::MonotonicDuration) {
            self.executor.set_fake_time(self.executor.now() + duration);
            let _ = self.executor.wake_expired_timers();
        }

        /// Expects the current session to have ended, and takes the session the governor
        /// reconnects exactly [`RECONNECT_DELAY`] later.
        fn expect_reconnect(&mut self) {
            self.pump();
            self.advance(RECONNECT_DELAY - ONE_NANO);
            assert!(!self.take_session(), "governor reconnected early");
            self.advance(ONE_NANO);
            assert!(self.take_session(), "governor did not reconnect");
        }

        fn next_battery_request(&mut self) -> Option<fbattery::BatteryRequest> {
            self.pump();
            let battery = self.battery.as_mut().expect("battery closed");
            poll_next(&mut self.executor, battery).map(|r| r.expect("battery request"))
        }

        fn next_charger_request(&mut self) -> Option<fcharger::ChargerRequest> {
            self.pump();
            let charger = self.charger.as_mut().expect("charger closed");
            poll_next(&mut self.executor, charger).map(|r| r.expect("charger request"))
        }

        fn next_controller_request(&mut self) -> Option<fcharger::ControllerRequest> {
            self.pump();
            let controller = self.controller.as_mut().expect("controller closed");
            poll_next(&mut self.executor, controller).map(|r| r.expect("controller request"))
        }

        /// Expects `Battery.Watch` without a lease.
        fn expect_battery_watch(&mut self) -> fbattery::BatteryWatchResponder {
            let (lease, responder) = self.expect_battery_watch_with_lease();
            assert!(lease.is_none());
            responder
        }

        fn expect_battery_watch_with_lease(
            &mut self,
        ) -> (Option<zx::EventPair>, fbattery::BatteryWatchResponder) {
            match self.next_battery_request() {
                Some(fbattery::BatteryRequest::Watch { lease, responder }) => (lease, responder),
                other => panic!("expected Battery.Watch, got {other:?}"),
            }
        }

        fn expect_battery_configure_watch(
            &mut self,
        ) -> (fbattery::WatchOptions, fbattery::BatteryConfigureWatchResponder) {
            match self.next_battery_request() {
                Some(fbattery::BatteryRequest::ConfigureWatch { options, responder }) => {
                    (options, responder)
                }
                other => panic!("expected Battery.ConfigureWatch, got {other:?}"),
            }
        }

        /// Expects `Charger.ConfigureWatch`, and accepts it.
        fn accept_charger_configure_watch(&mut self) -> fcharger::WatchOptions {
            match self.next_charger_request() {
                Some(fcharger::ChargerRequest::ConfigureWatch { options, responder }) => {
                    responder.send(Ok(&options)).unwrap();
                    options
                }
                other => panic!("expected Charger.ConfigureWatch, got {other:?}"),
            }
        }

        fn expect_battery_get_status(&mut self) -> fbattery::BatteryGetStatusResponder {
            match self.next_battery_request() {
                Some(fbattery::BatteryRequest::GetStatus { responder }) => responder,
                other => panic!("expected Battery.GetStatus, got {other:?}"),
            }
        }

        fn expect_charger_get_status(&mut self) -> fcharger::ChargerGetStatusResponder {
            match self.next_charger_request() {
                Some(fcharger::ChargerRequest::GetStatus { responder }) => responder,
                other => panic!("expected Charger.GetStatus, got {other:?}"),
            }
        }

        /// Expects `Charger.Watch` without a lease.
        fn expect_charger_watch(&mut self) -> fcharger::ChargerWatchResponder {
            let (lease, responder) = self.expect_charger_watch_with_lease();
            assert!(lease.is_none());
            responder
        }

        fn expect_charger_watch_with_lease(
            &mut self,
        ) -> (Option<zx::EventPair>, fcharger::ChargerWatchResponder) {
            match self.next_charger_request() {
                Some(fcharger::ChargerRequest::Watch { lease, responder }) => (lease, responder),
                other => panic!("expected Charger.Watch, got {other:?}"),
            }
        }

        /// Expects `Battery.ConfigureWatch` with `max_interval` set to `interval`, and accepts it.
        fn accept_sample_interval(&mut self, interval: Option<zx::BootDuration>) {
            let max_interval = interval.map(zx::BootDuration::into_nanos);
            let (options, responder) = self.expect_battery_configure_watch();
            assert_eq!(options, fbattery::WatchOptions { max_interval, ..Default::default() });
            responder.send(Ok(&options)).unwrap();
        }

        /// Expects a `SetControl` request, returning its options after accepting it.
        fn accept_set_control(&mut self) -> fcharger::ControlOptions {
            let (options, responder) = self.expect_set_control();
            responder.send(Ok(())).unwrap();
            options
        }

        fn expect_set_control(
            &mut self,
        ) -> (fcharger::ControlOptions, fcharger::ControllerSetControlResponder) {
            match self.next_controller_request() {
                Some(fcharger::ControllerRequest::SetControl { options, responder }) => {
                    (options, responder)
                }
                other => panic!("expected Controller.SetControl, got {other:?}"),
            }
        }

        fn expect_clear_control(
            &mut self,
        ) -> (fcharger::ControlOptions, fcharger::ControllerClearControlResponder) {
            match self.next_controller_request() {
                Some(fcharger::ControllerRequest::ClearControl { options, responder }) => {
                    (options, responder)
                }
                other => panic!("expected Controller.ClearControl, got {other:?}"),
            }
        }

        fn assert_no_battery_request(&mut self) {
            if let Some(req) = self.next_battery_request() {
                panic!("unexpected battery request: {req:?}");
            }
        }

        fn assert_no_charger_request(&mut self) {
            if let Some(req) = self.next_charger_request() {
                panic!("unexpected charger request: {req:?}");
            }
        }

        fn assert_no_controller_request(&mut self) {
            if let Some(req) = self.next_controller_request() {
                panic!("unexpected controller request: {req:?}");
            }
        }

        fn evaluation_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn last_telemetry(&self) -> Telemetry {
            self.seen.lock().unwrap().last().cloned().expect("governor never evaluated")
        }

        /// Answers `Charger.GetStatus` as battery-manager does while no charger status is known.
        fn answer_no_charger(&mut self) {
            self.expect_charger_get_status().send(Err(fcharger::Error::Io)).unwrap();
        }

        /// Answers the initial `Battery.Watch` with `voltage_uv` and the resulting
        /// `Charger.GetStatus` as if there were no charger, accepts the initial `SetControl`, and
        /// returns the re-armed (still pending) `Battery.Watch` responder.
        ///
        /// The governor must not request a sample interval on its first evaluation.
        fn complete_initial_battery_update(
            &mut self,
            voltage_uv: u32,
        ) -> fbattery::BatteryWatchResponder {
            let watch = self.expect_battery_watch();
            watch.send(Ok((&battery_status(voltage_uv), None))).unwrap();
            self.answer_no_charger();
            let _ = self.accept_set_control();
            self.expect_battery_watch()
        }
    }

    /// Verifies that `Battery.Watch` completions drive governor evaluations and send the resulting
    /// `SetControl` / `ClearControl` diffs before re-arming `Battery.Watch`.
    #[fuchsia::test]
    fn test_loop_battery_watch_drives_votes() {
        let mut h = LoopHarness::new(vec![
            Evaluation { vote: full_vote(), sample_interval: None, ..Default::default() },
            // Same vote -> no controller traffic.
            Evaluation { vote: full_vote(), sample_interval: None, ..Default::default() },
            // Lower FCC and withdraw FV.
            Evaluation {
                vote: fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    float_voltage_uv: None,
                    ..full_vote()
                },
                sample_interval: None,
                ..Default::default()
            },
        ]);

        let _pending_charger_watch = h.expect_charger_watch();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        assert_eq!(h.accept_set_control(), full_vote());
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UV), charger: None }
        );

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 2);
        assert_eq!(h.last_telemetry().battery, battery_status(TEST_VBAT_UPDATED_UV));

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        assert_eq!(h.accept_set_control(), fcc_vote(TEST_FCC_LOW_UA));
        let (clear, responder) = h.expect_clear_control();
        assert_eq!(
            clear,
            fcharger::ControlOptions { float_voltage_uv: Some(TEST_FV_UV), ..Default::default() }
        );
        responder.send(Ok(())).unwrap();

        let _pending_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 3);
    }

    /// Whether every handle to the lease whose peer is `lease_peer` has been closed.
    fn is_released(lease_peer: &zx::EventPair) -> bool {
        matches!(
            lease_peer
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(_)
        )
    }

    /// Verifies that the governor holds the wake lease of a `Watch` response while it actuates
    /// the vote, and then passes that same lease to the next `Watch` on the connection.
    #[fuchsia::test]
    fn test_loop_passes_response_lease_to_next_watch() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();

        let (lease, lease_peer) = zx::EventPair::create();
        let lease_koid = lease.koid().unwrap();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), Some(lease)))).unwrap();
        h.answer_no_charger();

        let (_, set_responder) = h.expect_set_control();
        assert!(!is_released(&lease_peer));
        set_responder.send(Ok(())).unwrap();

        let (setup_lease, _pending_watch) = h.expect_battery_watch_with_lease();
        let setup_lease = setup_lease.expect("the response's lease is passed to the next Watch");
        assert_eq!(setup_lease.koid().unwrap(), lease_koid);
        drop(setup_lease);
        assert!(is_released(&lease_peer));
    }

    /// Verifies that `Evaluation::wake_for_samples` is voted as `wake_on_max_interval` with the
    /// sample interval, and withdrawn when the governor stops asking for it.
    #[fuchsia::test]
    fn test_loop_votes_wake_for_samples() {
        let mut h = LoopHarness::new(vec![
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                wake_for_samples: true,
            },
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                wake_for_samples: false,
            },
        ]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let _ = h.accept_set_control();
        let (options, responder) = h.expect_battery_configure_watch();
        assert_eq!(
            options,
            fbattery::WatchOptions {
                max_interval: Some(TEST_SAMPLE_INTERVAL.into_nanos()),
                wake_on_max_interval: Some(true),
                ..Default::default()
            }
        );
        responder.send(Ok(&options)).unwrap();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.accept_sample_interval(Some(TEST_SAMPLE_INTERVAL));
        let _pending_watch = h.expect_battery_watch();
    }

    /// Verifies that `WatchInterest::charger_wake_on` is configured as the charger connection's
    /// `wake_on` before the first `Watch`.
    #[fuchsia::test]
    fn test_loop_configures_charger_wake_on() {
        let wake_on = fcharger::Status { online: Some(true), ..Default::default() };
        let mut h = LoopHarness::with_interest(
            vec![Evaluation { vote: fcc_vote(TEST_FCC_HIGH_UA), ..Default::default() }],
            WatchInterest { charger_wake_on: Some(wake_on.clone()), ..Default::default() },
        );
        assert_eq!(
            h.accept_charger_configure_watch(),
            fcharger::WatchOptions { wake_on: Some(wake_on), ..Default::default() }
        );
        let _pending_charger_watch = h.expect_charger_watch();
    }

    /// Verifies that `Battery.Watch` is re-armed only after `Controller` and
    /// `Battery.ConfigureWatch` calls complete, so battery-manager's wake lease remains held
    /// across actuation.
    #[fuchsia::test]
    fn test_loop_rearms_watch_only_after_actuation() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: Some(TEST_SAMPLE_INTERVAL),
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();

        // While the vote and the interval are in flight, Battery.Watch is not re-armed yet.
        let (_, set_responder) = h.expect_set_control();
        h.assert_no_battery_request();
        set_responder.send(Ok(())).unwrap();
        let (options, battery_responder) = h.expect_battery_configure_watch();
        h.assert_no_battery_request();
        battery_responder.send(Ok(&options)).unwrap();

        let _pending_watch = h.expect_battery_watch();
    }

    /// Verifies that `Battery.ConfigureWatch` is called only when `Evaluation::sample_interval`
    /// changes, and that withdrawing the interval sends `max_interval: None`.
    #[fuchsia::test]
    fn test_loop_configures_sample_interval_on_change() {
        let mut h = LoopHarness::new(vec![
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                ..Default::default()
            },
            // Unchanged interval -> not sent again.
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                ..Default::default()
            },
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_LONGER_SAMPLE_INTERVAL),
                ..Default::default()
            },
            // No interval -> the vote is withdrawn.
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: None,
                ..Default::default()
            },
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: None,
                ..Default::default()
            },
        ]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let _ = h.accept_set_control();
        h.accept_sample_interval(Some(TEST_SAMPLE_INTERVAL));

        // battery-manager owns sampling: nothing happens locally when the interval elapses.
        let watch = h.expect_battery_watch();
        h.advance(TEST_LONG_IDLE);
        h.assert_no_battery_request();
        assert_eq!(h.evaluation_count(), 1);

        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        let watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 2);

        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.accept_sample_interval(Some(TEST_LONGER_SAMPLE_INTERVAL));

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.accept_sample_interval(None);

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        let _pending_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 5);
        h.assert_no_controller_request();
        h.assert_no_charger_request();
    }

    /// Verifies that a declared watch interest is configured on both connections before the first
    /// `Watch`, sent again with every sample interval, and configured again on a new session.
    #[fuchsia::test]
    fn test_loop_configures_watch_interest() {
        let battery_interest = fbattery::Status { present: Some(true), ..Default::default() };
        let charger_interest = fcharger::Status { online: Some(true), ..Default::default() };
        let mut h = LoopHarness::with_interest(
            vec![Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: Some(TEST_SAMPLE_INTERVAL),
                ..Default::default()
            }],
            WatchInterest {
                battery: Some(battery_interest.clone()),
                charger: Some(charger_interest.clone()),
                ..Default::default()
            },
        );
        let interest_only = fbattery::WatchOptions {
            interest: Some(battery_interest.clone()),
            ..Default::default()
        };
        let with_interval = fbattery::WatchOptions {
            max_interval: Some(TEST_SAMPLE_INTERVAL.into_nanos()),
            ..interest_only.clone()
        };
        let charger_options =
            fcharger::WatchOptions { interest: Some(charger_interest), ..Default::default() };

        for _ in 0..2 {
            let (options, responder) = h.expect_battery_configure_watch();
            assert_eq!(options, interest_only);
            responder.send(Ok(&options)).unwrap();
            assert_eq!(h.accept_charger_configure_watch(), charger_options);

            let charger_watch = h.expect_charger_watch();
            let watch = h.expect_battery_watch();
            watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
            h.answer_no_charger();
            let _ = h.accept_set_control();
            let (options, responder) = h.expect_battery_configure_watch();
            assert_eq!(options, with_interval);
            responder.send(Ok(&options)).unwrap();
            let _pending_watch = h.expect_battery_watch();

            drop(charger_watch);
            h.charger = None;
            h.expect_reconnect();
        }
    }

    /// Verifies that a domain error from `Charger.ConfigureWatch` does not end the session.
    #[fuchsia::test]
    fn test_loop_watch_interest_rejection_is_not_fatal() {
        let mut h = LoopHarness::with_interest(
            vec![Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: None,
                ..Default::default()
            }],
            WatchInterest {
                charger: Some(fcharger::Status { online: Some(true), ..Default::default() }),
                ..Default::default()
            },
        );
        match h.next_charger_request() {
            Some(fcharger::ChargerRequest::ConfigureWatch { responder, .. }) => {
                responder.send(Err(fcharger::Error::NotSupported)).unwrap()
            }
            other => panic!("expected Charger.ConfigureWatch, got {other:?}"),
        }

        let _pending_charger_watch = h.expect_charger_watch();
        let _pending_battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        assert_eq!(h.evaluation_count(), 1);
    }

    /// Verifies that a domain error from `Battery.ConfigureWatch` does not end the session or
    /// trigger retries when `sample_interval` is unchanged.
    #[fuchsia::test]
    fn test_loop_sample_interval_rejection_is_not_fatal_or_retried() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: Some(TEST_SAMPLE_INTERVAL),
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let _ = h.accept_set_control();
        let (_, responder) = h.expect_battery_configure_watch();
        responder.send(Err(fbattery::Error::NotSupported)).unwrap();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        let _pending_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 2);
        h.assert_no_charger_request();
    }

    /// Verifies that a `Charger.Watch` completion refreshes the battery status via
    /// `Battery.GetStatus` before re-arming `Charger.Watch`, then evaluates with both statuses.
    #[fuchsia::test]
    fn test_loop_charger_watch_refreshes_battery_status() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        let _pending_battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        assert_eq!(h.last_telemetry().charger, None);
        let online_charger = fcharger::Status { online: Some(true), ..Default::default() };

        charger_watch.send(Ok((&online_charger, None))).unwrap();
        // While the battery refresh is in flight, Charger.Watch is not re-armed.
        let get_status = h.expect_battery_get_status();
        h.assert_no_charger_request();
        get_status.send(Ok(&battery_status(TEST_VBAT_UPDATED_UV))).unwrap();

        let _pending_charger_watch = h.expect_charger_watch();
        h.assert_no_battery_request();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 2);
        assert_eq!(
            h.last_telemetry(),
            Telemetry {
                battery: battery_status(TEST_VBAT_UPDATED_UV),
                charger: Some(online_charger)
            }
        );
    }

    /// Starts a governor whose charger interest is `online`, and completes its initial battery
    /// and charger updates, the latter with `charger_status`. Returns the harness and the pending
    /// `Battery.Watch` and `Charger.Watch` responders.
    fn start_with_online_charger_interest(
        charger_status: &fcharger::Status,
    ) -> (LoopHarness, fbattery::BatteryWatchResponder, fcharger::ChargerWatchResponder) {
        let mut h = LoopHarness::with_interest(
            vec![Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: None,
                ..Default::default()
            }],
            WatchInterest {
                charger: Some(fcharger::Status { online: Some(true), ..Default::default() }),
                ..Default::default()
            },
        );
        let _ = h.accept_charger_configure_watch();
        let charger_watch = h.expect_charger_watch();
        let battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        charger_watch.send(Ok((charger_status, None))).unwrap();
        h.expect_battery_get_status().send(Ok(&battery_status(TEST_VBAT_UV))).unwrap();
        let charger_watch = h.expect_charger_watch();
        (h, battery_watch, charger_watch)
    }

    /// Verifies that a `Battery.Watch` update reads the charger status via `Charger.GetStatus`, so
    /// that a governor that narrowed its charger interest sees charger fields outside it, such as
    /// the charge phase, at the next sample.
    #[fuchsia::test]
    fn test_loop_battery_watch_refreshes_narrowed_charger_status() {
        let fast = fcharger::Status {
            online: Some(true),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            ..Default::default()
        };
        let (mut h, battery_watch, _pending_charger_watch) =
            start_with_online_charger_interest(&fast);

        battery_watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        // While the charger refresh is in flight, Battery.Watch is not re-armed.
        let get_status = h.expect_charger_get_status();
        h.assert_no_battery_request();
        let taper = fcharger::Status { charge_phase: Some(fcharger::ChargePhase::Taper), ..fast };
        get_status.send(Ok(&taper)).unwrap();

        let _pending_battery_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 3);
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UPDATED_UV), charger: Some(taper) }
        );
    }

    /// Verifies that if `Charger.GetStatus` returns a domain error after a `Battery.Watch` update,
    /// the governor evaluates using the last known charger status.
    #[fuchsia::test]
    fn test_loop_charger_refresh_error_uses_last_charger_status() {
        let online = fcharger::Status { online: Some(true), ..Default::default() };
        let (mut h, battery_watch, _pending_charger_watch) =
            start_with_online_charger_interest(&online);

        battery_watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.expect_charger_get_status().send(Err(fcharger::Error::Io)).unwrap();
        let _pending_battery_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 3);
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UPDATED_UV), charger: Some(online) }
        );
    }

    /// Verifies that if `Battery.GetStatus` returns a domain error after a `Charger.Watch` update,
    /// the governor evaluates using the last known battery status.
    #[fuchsia::test]
    fn test_loop_battery_refresh_error_uses_last_battery_status() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        let _pending_battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        let online_charger = fcharger::Status { online: Some(true), ..Default::default() };

        charger_watch.send(Ok((&online_charger, None))).unwrap();
        // battery-manager returns IO while no battery driver is connected.
        h.expect_battery_get_status().send(Err(fbattery::Error::Io)).unwrap();
        let _pending_charger_watch = h.expect_charger_watch();
        assert_eq!(h.evaluation_count(), 2);
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UV), charger: Some(online_charger) }
        );
    }

    /// Verifies that a transport error during `Battery.GetStatus` ends the session and reconnects.
    #[fuchsia::test]
    fn test_loop_battery_refresh_transport_error_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        let battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);

        charger_watch.send(Ok((&fcharger::Status::default(), None))).unwrap();
        let get_status = h.expect_battery_get_status();
        drop((battery_watch, get_status));
        h.battery = None;
        h.expect_reconnect();
        assert_eq!(h.evaluation_count(), 1);
    }

    /// Verifies that if `Charger.Watch` completes before any battery status is available,
    /// evaluation waits until the first battery status arrives.
    #[fuchsia::test]
    fn test_loop_charger_before_battery_waits_for_battery() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let battery_watch = h.expect_battery_watch();
        let charger_watch = h.expect_charger_watch();
        let online_charger = fcharger::Status { online: Some(true), ..Default::default() };

        // Charger reports first, before any battery status: nothing to evaluate yet.
        charger_watch.send(Ok((&online_charger, None))).unwrap();
        h.expect_battery_get_status().send(Err(fbattery::Error::Io)).unwrap();
        let _pending_charger_watch = h.expect_charger_watch();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 0);

        battery_watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.expect_charger_get_status().send(Ok(&online_charger)).unwrap();
        assert_eq!(h.accept_set_control(), fcc_vote(TEST_FCC_HIGH_UA));
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UV), charger: Some(online_charger) }
        );
    }

    /// Verifies that `Battery.Watch` updates are evaluated with `charger: None` even when
    /// `Charger.Watch` never completes (e.g. on a board without a charger driver).
    #[fuchsia::test]
    fn test_loop_pending_charger_watch_does_not_block_battery() {
        let mut h = LoopHarness::new(vec![
            Evaluation {
                vote: fcc_vote(TEST_FCC_HIGH_UA),
                sample_interval: None,
                ..Default::default()
            },
            Evaluation {
                vote: fcc_vote(TEST_FCC_LOW_UA),
                sample_interval: None,
                ..Default::default()
            },
        ]);
        // As on a board without a charger driver, Charger.Watch never completes.
        let _pending_charger_watch = h.expect_charger_watch();
        let battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);

        battery_watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        assert_eq!(h.accept_set_control(), fcc_vote(TEST_FCC_LOW_UA));
        let _pending_battery_watch = h.expect_battery_watch();
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UPDATED_UV), charger: None }
        );
        h.assert_no_charger_request();
    }

    /// Verifies that a domain error from `Battery.Watch` ends the session and reconnects after
    /// `RECONNECT_DELAY`.
    #[fuchsia::test]
    fn test_loop_battery_watch_error_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let watch = h.expect_battery_watch();
        watch.send(Err(fbattery::Error::AlreadyWatching)).unwrap();
        h.expect_reconnect();
        assert_eq!(h.evaluation_count(), 0);
    }

    /// Verifies that a domain error from `Charger.Watch` ends the session and reconnects after
    /// `RECONNECT_DELAY`.
    #[fuchsia::test]
    fn test_loop_charger_watch_error_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        h.expect_charger_watch().send(Err(fcharger::Error::AlreadyWatching)).unwrap();
        h.expect_reconnect();
    }

    /// Verifies that closing the `Battery` channel ends the session and reconnects after
    /// `RECONNECT_DELAY`.
    #[fuchsia::test]
    fn test_loop_battery_channel_closed_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let watch = h.expect_battery_watch();
        drop(watch);
        h.battery = None;
        h.expect_reconnect();
    }

    /// Verifies that closing the `Charger` channel reconnects all three protocols and re-sends the
    /// full vote and sample interval on the new session.
    #[fuchsia::test]
    fn test_loop_charger_channel_closed_reconnects_and_votes_again() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: full_vote(),
            sample_interval: Some(TEST_SAMPLE_INTERVAL),
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        assert_eq!(h.accept_set_control(), full_vote());
        h.accept_sample_interval(Some(TEST_SAMPLE_INTERVAL));
        let _pending_watch = h.expect_battery_watch();

        drop(charger_watch);
        h.charger = None;
        h.expect_reconnect();

        // The new session starts over: the full vote and the interval are sent again.
        let _pending_charger_watch = h.expect_charger_watch();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.answer_no_charger();
        assert_eq!(h.accept_set_control(), full_vote());
        h.accept_sample_interval(Some(TEST_SAMPLE_INTERVAL));
        let _pending_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 2);
    }

    /// Verifies that a `SetControl` rejection keeps the current session alive and retries the
    /// rejected vote on the next telemetry update.
    #[fuchsia::test]
    fn test_loop_controller_rejection_keeps_session_and_retries() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let (_, responder) = h.expect_set_control();
        responder.send(Err(fcharger::Error::InvalidArgs)).unwrap();

        // The session is kept: Battery.Watch is re-armed and the next update retries the vote.
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        assert_eq!(h.accept_set_control(), fcc_vote(TEST_FCC_HIGH_UA));
        let _pending_watch = h.expect_battery_watch();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 2);
        assert!(h.connected.borrow().is_empty(), "governor reconnected");
    }

    /// Verifies that when both `Battery.Watch` and `Charger.Watch` are ready in the same turn,
    /// they are coalesced into a single evaluation without calling `Battery.GetStatus`.
    #[fuchsia::test]
    fn test_loop_battery_and_charger_ready_together_evaluate_once() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        let battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        let online_charger = fcharger::Status { online: Some(true), ..Default::default() };

        // As after a charger event that also changed the battery, battery-manager publishes the
        // battery status and then the charger status before the governor runs.
        battery_watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        charger_watch.send(Ok((&online_charger, None))).unwrap();

        // Both are evaluated together, without a Battery.GetStatus refresh, and both re-armed.
        let _pending_battery_watch = h.expect_battery_watch();
        let _pending_charger_watch = h.expect_charger_watch();
        h.assert_no_battery_request();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 2);
        assert_eq!(
            h.last_telemetry(),
            Telemetry {
                battery: battery_status(TEST_VBAT_UPDATED_UV),
                charger: Some(online_charger)
            }
        );
    }

    /// Verifies that a `Battery.Watch` update evaluates with the charger status read via
    /// `Charger.GetStatus` rather than the last `Charger.Watch` response, as when the governor
    /// wakes between battery-manager's battery and charger responses to one charger event, and that
    /// the charger is read again once `Charger.Watch` reports after an earlier read found none.
    #[fuchsia::test]
    fn test_loop_battery_update_reads_current_charger_status() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let charger_watch = h.expect_charger_watch();
        // No charger status yet: the read fails, and the next battery update does not retry it.
        let battery_watch = h.complete_initial_battery_update(TEST_VBAT_UV);
        battery_watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        let battery_watch = h.expect_battery_watch();
        h.assert_no_charger_request();

        // The charger reports, so later battery updates read it again.
        let offline = fcharger::Status { online: Some(false), ..Default::default() };
        charger_watch.send(Ok((&offline, None))).unwrap();
        h.expect_battery_get_status().send(Ok(&battery_status(TEST_VBAT_UV))).unwrap();
        let charger_watch = h.expect_charger_watch();
        assert_eq!(h.evaluation_count(), 3);

        // battery-manager has published both statuses, but only the battery response has arrived.
        let online = fcharger::Status { online: Some(true), ..Default::default() };
        battery_watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        h.expect_charger_get_status().send(Ok(&online)).unwrap();
        let _pending_battery_watch = h.expect_battery_watch();
        assert_eq!(h.evaluation_count(), 4);
        assert_eq!(
            h.last_telemetry(),
            Telemetry {
                battery: battery_status(TEST_VBAT_UPDATED_UV),
                charger: Some(online.clone())
            }
        );

        // The late charger response evaluates the same snapshot again.
        charger_watch.send(Ok((&online, None))).unwrap();
        h.expect_battery_get_status().send(Ok(&battery_status(TEST_VBAT_UPDATED_UV))).unwrap();
        let _pending_charger_watch = h.expect_charger_watch();
        h.assert_no_controller_request();
        assert_eq!(h.evaluation_count(), 5);
        assert_eq!(
            h.last_telemetry(),
            Telemetry { battery: battery_status(TEST_VBAT_UPDATED_UV), charger: Some(online) }
        );
    }

    /// Verifies that closing the `Controller` channel ends the session and reconnects after
    /// `RECONNECT_DELAY`.
    #[fuchsia::test]
    fn test_loop_controller_closed_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: None,
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();
        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let (_, responder) = h.expect_set_control();
        drop(responder);
        h.controller = None;
        h.expect_reconnect();
    }

    /// Verifies that when `Battery.ConfigureWatch` returns a clamped `max_interval` different from
    /// the requested `sample_interval`, the session remains healthy and caches the requested vote.
    #[fuchsia::test]
    fn test_loop_sample_interval_clamped_by_server() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: Some(TEST_SAMPLE_INTERVAL),
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let _ = h.accept_set_control();
        let (_, responder) = h.expect_battery_configure_watch();
        responder
            .send(Ok(&fbattery::WatchOptions {
                max_interval: Some(TEST_LONGER_SAMPLE_INTERVAL.into_nanos()),
                ..Default::default()
            }))
            .unwrap();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UPDATED_UV), None))).unwrap();
        let _pending_watch = h.expect_battery_watch();
        h.assert_no_battery_request();
        assert_eq!(h.evaluation_count(), 2);
    }

    /// Verifies that a transport error on `Battery.ConfigureWatch` ends the session and reconnects.
    #[fuchsia::test]
    fn test_loop_sample_interval_transport_error_reconnects() {
        let mut h = LoopHarness::new(vec![Evaluation {
            vote: fcc_vote(TEST_FCC_HIGH_UA),
            sample_interval: Some(TEST_SAMPLE_INTERVAL),
            ..Default::default()
        }]);
        let _pending_charger_watch = h.expect_charger_watch();

        let watch = h.expect_battery_watch();
        watch.send(Ok((&battery_status(TEST_VBAT_UV), None))).unwrap();
        h.answer_no_charger();
        let _ = h.accept_set_control();
        let (_, configure_responder) = h.expect_battery_configure_watch();
        drop(configure_responder);
        h.battery = None;
        h.expect_reconnect();
    }

    /// Verifies that when `connect` returns `Err`, `supervise` waits `RECONNECT_DELAY` before
    /// retrying and starting the session.
    #[fuchsia::test]
    fn test_supervise_retries_failed_connect_after_delay() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let mut governor = SequenceGovernor::new(vec![Evaluation::default()]);
        let attempts = Rc::new(RefCell::new(0usize));
        let attempts_clone = attempts.clone();
        let connected: Rc<RefCell<Option<ServerEnds>>> = Rc::default();
        let connected_clone = connected.clone();

        let mut run = Box::pin(async move {
            supervise(&mut governor, || {
                let mut count = attempts_clone.borrow_mut();
                *count += 1;
                if *count == 1 {
                    return Err(anyhow!("connect failed"));
                }
                let (session, server) = new_session();
                *connected_clone.borrow_mut() = Some(server);
                Ok(session)
            })
            .await
        });

        assert!(executor.run_until_stalled(&mut run).is_pending());
        assert_eq!(*attempts.borrow(), 1);
        assert!(connected.borrow().is_none());

        executor.set_fake_time(executor.now() + RECONNECT_DELAY - ONE_NANO);
        let _ = executor.wake_expired_timers();
        assert!(executor.run_until_stalled(&mut run).is_pending());
        assert_eq!(*attempts.borrow(), 1);

        executor.set_fake_time(executor.now() + ONE_NANO);
        let _ = executor.wake_expired_timers();
        assert!(executor.run_until_stalled(&mut run).is_pending());
        assert_eq!(*attempts.borrow(), 2);
        assert!(connected.borrow().is_some());
    }
}
