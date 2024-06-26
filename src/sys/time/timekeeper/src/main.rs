// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(missing_docs)]

//! `timekeeper` is responsible for external time synchronization in Fuchsia.

mod clock_manager;
mod diagnostics;
mod enums;
mod estimator;
mod power_topology_integration;
mod rtc;
mod rtc_testing;
mod time_source;
mod time_source_manager;

use crate::clock_manager::ClockManager;
use crate::diagnostics::{
    CobaltDiagnostics, CompositeDiagnostics, Diagnostics, Event, InspectDiagnostics,
};
use crate::enums::{InitialClockState, InitializeRtcOutcome, Role, StartClockSource, Track};
use crate::rtc::{Rtc, RtcCreationError, RtcImpl};
use crate::time_source::{TimeSource, TimeSourceLauncher};
use crate::time_source_manager::TimeSourceManager;
use anyhow::{Context as _, Result};
use chrono::prelude::*;
use fidl::AsHandleRef;
use fuchsia_component::server::ServiceFs;
use futures::channel::mpsc;
use futures::future::{self, OptionFuture};
use futures::stream::StreamExt as _;
use std::sync::Arc;
use time_metrics_registry::TimeMetricDimensionExperiment;
use tracing::{debug, error, info, warn};
use {
    fidl_fuchsia_time as ftime, fidl_fuchsia_time_test as fftt, fuchsia_async as fasync,
    fuchsia_zircon as zx,
};

/// A command sent from various FIDL clients.
#[derive(Debug)]
pub enum Command {
    /// A power management command.
    PowerManagement,
    /// A real time clock (RTC) command, only used in tests.
    Rtc {
        /// If true, the RTC will be used. If false, the RTC will not be used.
        /// This setting persists across reboots.
        persistent_enabled: bool,

        /// The responder used to get a notification of job done.
        done: mpsc::Sender<()>,
    },
}

/// The type union of FIDL messages served by Timekeeper.
pub enum Rpcs {
    /// Time test protocol commands.
    TimeTest(fftt::RtcRequestStream),
}

/// Timekeeper config, populated from build-time generated structured config.
#[derive(Debug)]
pub struct Config {
    source_config: timekeeper_config::Config,
}

const MILLION: u64 = 1_000_000;

impl From<timekeeper_config::Config> for Config {
    fn from(source_config: timekeeper_config::Config) -> Self {
        Config { source_config }
    }
}

impl Config {
    fn get_primary_time_source_url(&self) -> String {
        self.source_config.primary_time_source_url.clone()
    }

    fn get_monitor_time_source_url(&self) -> Option<String> {
        Some(self.source_config.monitor_time_source_url.clone()).filter(|s| !s.is_empty())
    }

    fn get_oscillator_error_std_dev_ppm(&self) -> f64 {
        self.source_config.oscillator_error_std_dev_ppm as f64
    }

    fn get_oscillator_error_variance(&self) -> f64 {
        (self.source_config.oscillator_error_std_dev_ppm as f64 / MILLION as f64).powi(2)
    }

    fn get_max_frequency_error(&self) -> f64 {
        self.source_config.max_frequency_error_ppm as f64 / MILLION as f64
    }

    fn get_disable_delays(&self) -> bool {
        self.source_config.disable_delays
    }

    fn get_initial_frequency(&self) -> f64 {
        self.source_config.initial_frequency_ppm as f64 / MILLION as f64
    }

    fn get_monitor_uses_pull(&self) -> bool {
        self.source_config.monitor_uses_pull
    }

    fn get_back_off_time_between_pull_samples(&self) -> zx::Duration {
        zx::Duration::from_seconds(self.source_config.back_off_time_between_pull_samples_sec)
    }

    fn get_first_sampling_delay(&self) -> zx::Duration {
        zx::Duration::from_seconds(self.source_config.first_sampling_delay_sec)
    }

    fn get_primary_uses_pull(&self) -> bool {
        self.source_config.primary_uses_pull
    }

    fn get_utc_start_at_startup(&self) -> bool {
        self.source_config.utc_start_at_startup
    }

    fn get_early_exit(&self) -> bool {
        self.source_config.early_exit
    }

    // TODO: b/295537795 - remove annotation once used.
    #[allow(dead_code)]
    fn power_topology_integration_enabled(&self) -> bool {
        self.source_config.power_topology_integration_enabled
    }

    fn serve_test_protocols(&self) -> bool {
        self.source_config.serve_test_protocols
    }
}

/// A definition which time sources to install, along with the URL and child names for each.
struct TimeSourceUrls {
    primary: TimeSourceDetails,
    monitor: Option<TimeSourceDetails>,
}

/// Describes the timesource to be installed.
struct TimeSourceDetails {
    url: String,
    name: String,
}

/// Instantiates a [TimeSource::Push] or [TimeSource::Pull] depending on
/// `use_pull`.
fn new_time_source(use_pull: bool, details: &TimeSourceDetails) -> TimeSource {
    let launcher = TimeSourceLauncher::new(&details.url, &details.name);
    if use_pull {
        info!("time source {} uses pull", &details.name);
        TimeSource::Pull(launcher.into())
    } else {
        info!("time source {} uses push", &details.name);
        TimeSource::Push(launcher.into())
    }
}

/// The experiment to record on Cobalt events.
const COBALT_EXPERIMENT: TimeMetricDimensionExperiment = TimeMetricDimensionExperiment::None;

/// The information required to maintain UTC for the primary track.
struct PrimaryTrack {
    time_source: TimeSource,
    clock: Arc<zx::Clock>,
}

/// The information required to maintain UTC for the monitor track.
struct MonitorTrack {
    time_source: TimeSource,
    clock: Arc<zx::Clock>,
}

fn koid_of(c: &zx::Clock) -> u64 {
    c.as_handle_ref().get_koid().expect("infallible").raw_koid()
}

#[fuchsia::main(logging_tags=["time", "timekeeper"])]
async fn main() -> Result<()> {
    let config: Arc<Config> =
        Arc::new(timekeeper_config::Config::take_from_startup_handle().into());

    // If we don't get this, timekeeper probably didn't even start.
    debug!("starting timekeeper: config: {:?}", &config);

    info!("retrieving UTC clock handle");
    let time_maintainer =
        fuchsia_component::client::connect_to_protocol::<ftime::MaintenanceMarker>().unwrap();
    let utc_clock = zx::Clock::from(
        time_maintainer
            .get_writable_utc_clock()
            .await
            .context("failed to get UTC clock from maintainer")?,
    );
    debug!("utc_clock handle with koid: {}", koid_of(&utc_clock));

    let time_source_urls = TimeSourceUrls {
        primary: TimeSourceDetails {
            url: config.get_primary_time_source_url().clone(),
            name: Role::Primary.to_string(),
        },
        monitor: config
            .get_monitor_time_source_url()
            .map(|url| TimeSourceDetails { url, name: Role::Monitor.to_string() }),
    };

    info!("constructing time sources");
    let primary_track = PrimaryTrack {
        time_source: new_time_source(config.get_primary_uses_pull(), &time_source_urls.primary),
        clock: Arc::new(utc_clock),
    };
    let monitor_track = time_source_urls.monitor.map(|details| MonitorTrack {
        time_source: new_time_source(config.get_monitor_uses_pull(), &details),
        clock: Arc::new(create_monitor_clock(&primary_track.clock)),
    });

    info!("initializing diagnostics and serving inspect on servicefs");
    let cobalt_experiment = COBALT_EXPERIMENT;
    let diagnostics = Arc::new(CompositeDiagnostics::new(
        InspectDiagnostics::new(diagnostics::INSPECTOR.root(), &primary_track, &monitor_track),
        CobaltDiagnostics::new(cobalt_experiment, &primary_track, &monitor_track),
    ));

    info!("connecting to real time clock");
    let optional_rtc = match RtcImpl::only_device() {
        Ok(rtc) => Some(rtc),
        Err(err) => {
            match err {
                RtcCreationError::NoDevices => info!("no RTC devices found."),
                _ => warn!("failed to connect to RTC: {}", err),
            };
            diagnostics.record(Event::InitializeRtc { outcome: err.into(), time: None });
            None
        }
    };

    let (cmd_send, cmd_rcv) = mpsc::channel(1);
    let serve_test_protocols = config.serve_test_protocols();

    let cmd_send_clone = cmd_send.clone();
    fasync::Task::spawn(async move {
        maintain_utc(
            primary_track,
            monitor_track,
            optional_rtc,
            diagnostics,
            config,
            cmd_send_clone,
            cmd_rcv,
        )
        .await;
    })
    .detach();

    let _inspect_server_task = inspect_runtime::publish(
        &diagnostics::INSPECTOR,
        inspect_runtime::PublishOptions::default(),
    );

    if serve_test_protocols {
        let mut fs = ServiceFs::new();
        fs.dir("svc").add_fidl_service(Rpcs::TimeTest);
        fs.take_and_serve_directory_handle()?;
        info!("serving test protocols: fuchsia.test.time/RTC");

        // Allows us to move cmd_send into the closure below.
        let send_fn = || cmd_send.clone();

        // Serves one client at a time.  Multiple clients at a time could produce conflicting
        // results.
        return Ok(fs
            .for_each(|request: Rpcs| async move {
                match request {
                    Rpcs::TimeTest(stream) => {
                        rtc_testing::serve(send_fn(), stream)
                            .await
                            .map_err(|e| {
                                tracing::error!("while serving fuchsia.time.test/RPC: {:?}", e)
                            })
                            .unwrap_or(());
                    }
                };
            })
            .await);
    } else {
        // fuchsia::main can only return () or Result<()>.
        let mut fs = ServiceFs::new();
        fs.take_and_serve_directory_handle()?;
        Ok(fs.collect().await)
    }
}

/// Creates a new userspace clock for use in the monitor track, set to the same backstop time as
/// the supplied primary clock.
fn create_monitor_clock(primary_clock: &zx::Clock) -> zx::Clock {
    // Note: Failure should not be possible from a valid zx::Clock.
    let backstop = primary_clock.get_details().expect("failed to get UTC clock details").backstop;
    // Note: Only failure mode is an OOM which we handle via panic.
    zx::Clock::create(zx::ClockOpts::empty(), Some(backstop))
        .expect("failed to create new monitor clock")
}

/// Determines whether the supplied clock has previously been set.
/// Returns the clock state and the backstop.
fn initial_clock_state(utc_clock: &zx::Clock) -> (InitialClockState, zx::ClockDetails) {
    // Note: Failure should not be possible from a valid zx::Clock.
    let clock_details = utc_clock.get_details().expect("failed to get UTC clock details");
    // When the clock is first initialized to the backstop time, its synthetic offset should
    // be identical. Once the clock is updated, this is no longer true.
    if clock_details.backstop.into_nanos() == clock_details.ticks_to_synthetic.synthetic_offset {
        (InitialClockState::NotSet, clock_details)
    } else {
        (InitialClockState::PreviouslySet, clock_details)
    }
}

/// Attempts to initialize a userspace clock from the current value of the real time clock.
/// sending progress to diagnostics as appropriate.
async fn set_clock_from_rtc<R: Rtc, D: Diagnostics>(
    rtc: &R,
    clock: &zx::Clock,
    diagnostics: Arc<D>,
) {
    info!("reading initial RTC time.");
    let mono_before = zx::Time::get_monotonic();
    let rtc_time = match rtc.get().await {
        Err(err) => {
            error!("failed to read RTC time: {}", err);
            diagnostics.record(Event::InitializeRtc {
                outcome: InitializeRtcOutcome::ReadFailed,
                time: None,
            });
            return;
        }
        Ok(time) => time,
    };
    let mono_after = zx::Time::get_monotonic();
    let mono_time = mono_before + (mono_after - mono_before) / 2;

    let rtc_chrono = Utc.timestamp_nanos(rtc_time.into_nanos());
    let backstop = clock.get_details().expect("failed to get UTC clock details").backstop;
    let backstop_chrono = Utc.timestamp_nanos(backstop.into_nanos());
    if rtc_time < backstop {
        warn!("initial RTC time {} is before backstop: {}", rtc_chrono, backstop_chrono);
        diagnostics.record(Event::InitializeRtc {
            outcome: InitializeRtcOutcome::InvalidBeforeBackstop,
            time: Some(rtc_time),
        });
        return;
    } else {
        debug!("RTC time {} is ahead of backstop {}, as expected", rtc_chrono, backstop_chrono);
    }

    diagnostics.record(Event::InitializeRtc {
        outcome: InitializeRtcOutcome::Succeeded,
        time: Some(rtc_time),
    });
    if let Err(status) =
        clock.update(zx::ClockUpdate::builder().absolute_value(mono_time, rtc_time))
    {
        error!("failed to start UTC clock from RTC at time {}: {}", rtc_chrono, status);
    } else {
        diagnostics
            .record(Event::StartClock { track: Track::Primary, source: StartClockSource::Rtc });
        info!("started UTC clock from RTC at time: {}", rtc_chrono);

        if let Err(status) = clock.signal_handle(
            zx::Signals::NONE,
            zx::Signals::from_bits(ftime::SIGNAL_UTC_CLOCK_LOGGING_QUALITY).unwrap(),
        ) {
            // Since userspace depends on this signal, we probably can not recover if
            // we can not signal.
            panic!("Failed to signal clock logging quality: {}", status);
        } else {
            debug!("sent SIGNAL_UTC_CLOCK_LOGGING_QUALITY");
        }
    }
}

/// The top-level control loop for time synchronization.
///
/// Maintains the utc clock using updates received over the `fuchsia.time.external` protocols.
async fn maintain_utc<R: 'static, D: 'static>(
    primary: PrimaryTrack,
    optional_monitor: Option<MonitorTrack>,
    optional_rtc: Option<R>,
    diagnostics: Arc<D>,
    config: Arc<Config>,
    cmd_send: mpsc::Sender<Command>,
    cmd_recv: mpsc::Receiver<Command>,
) where
    R: Rtc,
    D: Diagnostics,
{
    info!("record the state at initialization.");
    let (initial_clock_state, clock_details) = initial_clock_state(&primary.clock);
    diagnostics.record(Event::Initialized { clock_state: initial_clock_state });

    if let Some(rtc) = optional_rtc.as_ref() {
        match initial_clock_state {
            InitialClockState::NotSet => {
                set_clock_from_rtc(rtc, &primary.clock, Arc::clone(&diagnostics)).await;
            }
            InitialClockState::PreviouslySet => {
                diagnostics.record(Event::InitializeRtc {
                    outcome: InitializeRtcOutcome::ReadNotAttempted,
                    time: None,
                });
            }
        }
    }
    info!("launching time source managers...");
    let time_source_fn = match config.get_disable_delays() {
        true => TimeSourceManager::new_with_delays_disabled,
        false => TimeSourceManager::new,
    };

    if optional_rtc.is_none() && config.get_utc_start_at_startup() {
        // Legacy programs assume that UTC clock is always running.  If config allows it,
        // we start the clock from backstop and hope for the best.
        let backstop = &clock_details.backstop;
        // Not possible to start at backstop, so we start just a bit after.
        let b1 = *backstop + zx::Duration::from_nanos(1);
        let mono = zx::Time::get_monotonic();
        info!("starting the UTC clock from backstop time, to handle legacy programs");
        debug!("`- synthetic (backstop+1): {:?}, reference (monotonic): {:?}", &b1, &mono);
        if let Err(status) =
            primary.clock.update(zx::ClockUpdate::builder().absolute_value(mono, b1))
        {
            warn!("failed to start UTC clock from backstop time: {}", &status);
            // If we got here, the UTC clock is not started yet. We might have better luck with
            // time sources, provided that we have network access.
        } else {
            // Yay, the clock is started!  Announce to the world.
            diagnostics.record(Event::InitializeRtc {
                outcome: InitializeRtcOutcome::StartedFromBackstop,
                time: Some(b1),
            });
        }
    }
    if config.get_early_exit() {
        tracing::info!("early_exit=true: exiting early per request from configuration. UTC clock will not be managed");
        return;
    }
    let primary_source_manager = time_source_fn(
        clock_details.backstop,
        Role::Primary,
        primary.time_source,
        Arc::clone(&diagnostics),
    );
    let monitor_source_manager_and_clock = optional_monitor.map(|monitor| {
        let source_manager = time_source_fn(
            clock_details.backstop,
            Role::Monitor,
            monitor.time_source,
            Arc::clone(&diagnostics),
        );
        (source_manager, monitor.clock)
    });

    info!("launching clock managers...");
    let fut1 = ClockManager::execute(
        primary.clock,
        primary_source_manager,
        optional_rtc,
        Arc::clone(&diagnostics),
        Track::Primary,
        Arc::clone(&config),
        cmd_recv,
    );
    let (_, r2) = mpsc::channel(1);
    let fut2_cfg_clone = config.clone();
    let fut2: OptionFuture<_> = monitor_source_manager_and_clock
        .map(|(source_manager, clock)| {
            ClockManager::<R, D>::execute(
                clock,
                source_manager,
                None,
                diagnostics,
                Track::Monitor,
                fut2_cfg_clone,
                r2,
            )
        })
        .into();

    let pte = config.power_topology_integration_enabled();
    let oneshot = Box::pin(async move {
        info!("power_topology_integration_enabled: {}", pte);
        if pte {
            power_topology_integration::manage(cmd_send)
                .await
                .context("(timekeeper will ignore this error and just turn the integration off)")
                .map_err(|e| error!("power management integration: {:#}", e))
                .unwrap_or(fasync::Task::local(async {}))
                .await;
        }
    });
    future::join3(fut1, fut2, oneshot).await;
}

// Reexport test config creation to be used in other tests.
#[cfg(test)]
use tests::{make_test_config, make_test_config_with_delay};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::FakeDiagnostics;
    use crate::enums::WriteRtcOutcome;
    use crate::rtc::FakeRtc;
    use crate::time_source::{Event as TimeSourceEvent, FakePushTimeSource, Sample};
    use futures::FutureExt;
    use lazy_static::lazy_static;
    use std::matches;
    use test_case::test_case;
    use {fidl_fuchsia_time_external as ftexternal, fuchsia_zircon as zx};

    const NANOS_PER_SECOND: i64 = 1_000_000_000;
    const OFFSET: zx::Duration = zx::Duration::from_seconds(1111_000);
    const OFFSET_2: zx::Duration = zx::Duration::from_seconds(1111_333);
    const STD_DEV: zx::Duration = zx::Duration::from_millis(44);
    const INVALID_RTC_TIME: zx::Time = zx::Time::from_nanos(111111 * NANOS_PER_SECOND);
    const BACKSTOP_TIME: zx::Time = zx::Time::from_nanos(222222 * NANOS_PER_SECOND);
    const VALID_RTC_TIME: zx::Time = zx::Time::from_nanos(333333 * NANOS_PER_SECOND);

    lazy_static! {
        static ref CLOCK_OPTS: zx::ClockOpts = zx::ClockOpts::empty();
    }

    /// Creates and starts a new clock with default options, returning a tuple of the clock and its
    /// initial update time in ticks.
    fn create_clock() -> (Arc<zx::Clock>, i64) {
        let clock = zx::Clock::create(*CLOCK_OPTS, Some(BACKSTOP_TIME)).unwrap();
        clock.update(zx::ClockUpdate::builder().approximate_value(BACKSTOP_TIME)).unwrap();
        let initial_update_ticks = clock.get_details().unwrap().last_value_update_ticks;
        (Arc::new(clock), initial_update_ticks)
    }

    pub fn make_test_config_with_params(delay: i64, serve_test_protocols: bool) -> Arc<Config> {
        Arc::new(Config::from(timekeeper_config::Config {
            disable_delays: true,
            oscillator_error_std_dev_ppm: 15,
            max_frequency_error_ppm: 10,
            primary_time_source_url: "".to_string(),
            initial_frequency_ppm: 1_000_000,
            monitor_uses_pull: false,
            back_off_time_between_pull_samples_sec: 0,
            first_sampling_delay_sec: delay,
            monitor_time_source_url: "".to_string(),
            primary_uses_pull: false,
            utc_start_at_startup: false,
            early_exit: false,
            power_topology_integration_enabled: false,
            serve_test_protocols,
        }))
    }

    pub fn make_test_config_with_delay(delay: i64) -> Arc<Config> {
        make_test_config_with_params(delay, /*serve_test_protocols=*/ false)
    }

    pub fn make_test_config() -> Arc<Config> {
        make_test_config_with_delay(0)
    }

    pub fn make_test_config_with_test_protocols() -> Arc<Config> {
        make_test_config_with_params(/*delay=*/ 0, /*serve_test_protocols=*/ true)
    }

    #[fuchsia::test]
    fn successful_update_with_monitor() {
        let mut executor = fasync::TestExecutor::new();
        let (primary_clock, primary_ticks) = create_clock();
        let (monitor_clock, monitor_ticks) = create_clock();
        let rtc = FakeRtc::valid(INVALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config();

        let monotonic_ref = zx::Time::get_monotonic();

        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack {
                clock: Arc::clone(&primary_clock),
                time_source: FakePushTimeSource::events(vec![
                    TimeSourceEvent::StatusChange { status: ftexternal::Status::Ok },
                    TimeSourceEvent::from(Sample::new(
                        monotonic_ref + OFFSET,
                        monotonic_ref,
                        STD_DEV,
                    )),
                ])
                .into(),
            },
            Some(MonitorTrack {
                clock: Arc::clone(&monitor_clock),
                time_source: FakePushTimeSource::events(vec![
                    TimeSourceEvent::StatusChange { status: ftexternal::Status::Network },
                    TimeSourceEvent::StatusChange { status: ftexternal::Status::Ok },
                    TimeSourceEvent::from(Sample::new(
                        monotonic_ref + OFFSET_2,
                        monotonic_ref,
                        STD_DEV,
                    )),
                ])
                .into(),
            }),
            Some(rtc.clone()),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();
        let _ = executor.run_until_stalled(&mut fut);

        // Check that the clocks are set.
        assert!(primary_clock.get_details().unwrap().last_value_update_ticks > primary_ticks);
        assert!(monitor_clock.get_details().unwrap().last_value_update_ticks > monitor_ticks);
        assert!(rtc.last_set().is_some());

        // Check that the correct diagnostic events were logged.
        diagnostics.assert_events(&[
            Event::Initialized { clock_state: InitialClockState::NotSet },
            Event::InitializeRtc {
                outcome: InitializeRtcOutcome::InvalidBeforeBackstop,
                time: Some(INVALID_RTC_TIME),
            },
            Event::TimeSourceStatus { role: Role::Primary, status: ftexternal::Status::Ok },
            Event::KalmanFilterUpdated {
                track: Track::Primary,
                monotonic: monotonic_ref,
                utc: monotonic_ref + OFFSET,
                sqrt_covariance: STD_DEV,
            },
            Event::StartClock {
                track: Track::Primary,
                source: StartClockSource::External(Role::Primary),
            },
            Event::WriteRtc { outcome: WriteRtcOutcome::Succeeded },
            Event::TimeSourceStatus { role: Role::Monitor, status: ftexternal::Status::Network },
            Event::TimeSourceStatus { role: Role::Monitor, status: ftexternal::Status::Ok },
            Event::KalmanFilterUpdated {
                track: Track::Monitor,
                monotonic: monotonic_ref,
                utc: monotonic_ref + OFFSET_2,
                sqrt_covariance: STD_DEV,
            },
            Event::StartClock {
                track: Track::Monitor,
                source: StartClockSource::External(Role::Monitor),
            },
        ]);
    }

    #[test_case(0; "no pause")]
    #[test_case(1; "one second pause")]
    #[fuchsia::test]
    fn successful_update_with_delay(delay: i64) {
        let mut executor = fasync::TestExecutor::new();
        let (primary_clock, primary_ticks) = create_clock();
        let rtc = FakeRtc::valid(INVALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config_with_delay(delay);

        let monotonic_ref = zx::Time::get_monotonic();
        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack {
                clock: Arc::clone(&primary_clock),
                time_source: FakePushTimeSource::events(vec![
                    TimeSourceEvent::StatusChange { status: ftexternal::Status::Ok },
                    TimeSourceEvent::from(Sample::new(
                        monotonic_ref + OFFSET,
                        monotonic_ref,
                        STD_DEV,
                    )),
                ])
                .into(),
            },
            None,
            Some(rtc.clone()),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();

        // This is slightly silly, but allows us to run the clock maintenance
        // in fake time, waking up appropriate delay timers along the way.
        // Tests running in fake time always have similar silliness where
        // timer wakeups are involved.
        let _ = executor.run_until_stalled(&mut fut); // Get to the first delay.

        // This will wake the delay timer *if* one exists. This is a non-obvious
        // feature of run_until_stalled: it does *not* wake timers. So without
        // this the "one second delay" will never get out of the pause and the
        // test will fail, proving that the pause does exist.
        // On the other hand, the fact that both "no pause" and
        // "one second pause" have an identical result means that the delay does
        // not affect the normal operation of the clock manager.
        executor.wake_next_timer();
        let _ = executor.run_until_stalled(&mut fut); // Finish clock update work.

        // Check that the clocks are set.
        assert!(primary_clock.get_details().unwrap().last_value_update_ticks > primary_ticks);
        assert!(rtc.last_set().is_some());

        // Check that the correct diagnostic events were logged.
        diagnostics.assert_events_prefix(&[
            Event::Initialized { clock_state: InitialClockState::NotSet },
            Event::InitializeRtc {
                outcome: InitializeRtcOutcome::InvalidBeforeBackstop,
                time: Some(INVALID_RTC_TIME),
            },
            Event::TimeSourceStatus { role: Role::Primary, status: ftexternal::Status::Ok },
            Event::KalmanFilterUpdated {
                track: Track::Primary,
                monotonic: monotonic_ref,
                utc: monotonic_ref + OFFSET,
                sqrt_covariance: STD_DEV,
            },
            Event::StartClock {
                track: Track::Primary,
                source: StartClockSource::External(Role::Primary),
            },
            Event::WriteRtc { outcome: WriteRtcOutcome::Succeeded },
        ]);
    }

    #[fuchsia::test]
    fn fail_when_no_delays() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let (primary_clock, primary_ticks) = create_clock();
        let rtc = FakeRtc::valid(INVALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config_with_delay(1);

        let monotonic_ref = zx::Time::get_monotonic();
        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack {
                clock: Arc::clone(&primary_clock),
                time_source: FakePushTimeSource::events(vec![
                    TimeSourceEvent::StatusChange { status: ftexternal::Status::Ok },
                    TimeSourceEvent::from(Sample::new(
                        monotonic_ref + OFFSET,
                        monotonic_ref,
                        STD_DEV,
                    )),
                ])
                .into(),
            },
            None,
            Some(rtc),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();

        let start_time = executor.now();
        let _ = executor.run_until_stalled(&mut fut);

        // Half a second in, nothing to wake.
        executor.set_fake_time(start_time + fasync::Duration::from_millis(550));
        let _ = executor.run_until_stalled(&mut fut);
        assert_eq!(false, executor.wake_expired_timers());

        // One second in, there is something to wake.
        executor.set_fake_time(start_time + fasync::Duration::from_millis(1050));
        assert_eq!(true, executor.wake_expired_timers());
        let _ = executor.run_until_stalled(&mut fut);

        // And our clock was updated, too!
        assert!(primary_clock.get_details().unwrap().last_value_update_ticks > primary_ticks);
    }

    #[fuchsia::test]
    fn no_update_invalid_rtc() {
        let mut executor = fasync::TestExecutor::new();
        let (clock, initial_update_ticks) = create_clock();
        let rtc = FakeRtc::valid(INVALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config();

        let time_source = FakePushTimeSource::events(vec![TimeSourceEvent::StatusChange {
            status: ftexternal::Status::Network,
        }])
        .into();
        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack { clock: Arc::clone(&clock), time_source },
            None,
            Some(rtc.clone()),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();
        let _ = executor.run_until_stalled(&mut fut);

        // Checking that the clock has not been updated yet
        assert_eq!(initial_update_ticks, clock.get_details().unwrap().last_value_update_ticks);
        assert_eq!(rtc.last_set(), None);

        // Checking that the correct diagnostic events were logged.
        diagnostics.assert_events(&[
            Event::Initialized { clock_state: InitialClockState::NotSet },
            Event::InitializeRtc {
                outcome: InitializeRtcOutcome::InvalidBeforeBackstop,
                time: Some(INVALID_RTC_TIME),
            },
            Event::TimeSourceStatus { role: Role::Primary, status: ftexternal::Status::Network },
        ]);
    }

    #[fuchsia::test]
    fn no_update_valid_rtc() {
        let mut executor = fasync::TestExecutor::new();
        let (clock, initial_update_ticks) = create_clock();
        let rtc = FakeRtc::valid(VALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config();

        let time_source = FakePushTimeSource::events(vec![TimeSourceEvent::StatusChange {
            status: ftexternal::Status::Network,
        }])
        .into();
        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack { clock: Arc::clone(&clock), time_source },
            None,
            Some(rtc.clone()),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();
        let _ = executor.run_until_stalled(&mut fut);

        // Checking that the clock was updated to use the valid RTC time.
        assert!(clock.get_details().unwrap().last_value_update_ticks > initial_update_ticks);
        assert!(clock.read().unwrap() >= VALID_RTC_TIME);
        assert_eq!(rtc.last_set(), None);

        // Checking that the correct diagnostic events were logged.
        diagnostics.assert_events(&[
            Event::Initialized { clock_state: InitialClockState::NotSet },
            Event::InitializeRtc {
                outcome: InitializeRtcOutcome::Succeeded,
                time: Some(VALID_RTC_TIME),
            },
            Event::StartClock { track: Track::Primary, source: StartClockSource::Rtc },
            Event::TimeSourceStatus { role: Role::Primary, status: ftexternal::Status::Network },
        ]);
    }

    #[fuchsia::test]
    fn no_update_clock_already_running() {
        let mut executor = fasync::TestExecutor::new();

        // Create a clock and set it slightly after backstop
        let (clock, _) = create_clock();
        clock
            .update(
                zx::ClockUpdate::builder()
                    .approximate_value(BACKSTOP_TIME + zx::Duration::from_millis(1)),
            )
            .unwrap();
        let initial_update_ticks = clock.get_details().unwrap().last_value_update_ticks;
        let rtc = FakeRtc::valid(VALID_RTC_TIME);
        let diagnostics = Arc::new(FakeDiagnostics::new());
        let config = make_test_config();

        let time_source = FakePushTimeSource::events(vec![TimeSourceEvent::StatusChange {
            status: ftexternal::Status::Network,
        }])
        .into();

        let (s, r) = mpsc::channel(1);

        // Maintain UTC until no more work remains
        let mut fut = maintain_utc(
            PrimaryTrack { clock: Arc::clone(&clock), time_source },
            None,
            Some(rtc.clone()),
            Arc::clone(&diagnostics),
            Arc::clone(&config),
            s,
            r,
        )
        .boxed();
        let _ = executor.run_until_stalled(&mut fut);

        // Checking that neither the clock nor the RTC were updated.
        assert_eq!(clock.get_details().unwrap().last_value_update_ticks, initial_update_ticks);
        assert_eq!(rtc.last_set(), None);

        // Checking that the correct diagnostic events were logged.
        diagnostics.assert_events(&[
            Event::Initialized { clock_state: InitialClockState::PreviouslySet },
            Event::InitializeRtc { outcome: InitializeRtcOutcome::ReadNotAttempted, time: None },
            Event::TimeSourceStatus { role: Role::Primary, status: ftexternal::Status::Network },
        ]);
    }

    #[fuchsia::test]
    fn test_initial_clock_state() {
        let clock =
            zx::Clock::create(zx::ClockOpts::empty(), Some(zx::Time::from_nanos(1_000))).unwrap();
        // The clock must be started with an initial value.
        clock
            .update(zx::ClockUpdate::builder().approximate_value(zx::Time::from_nanos(1_000)))
            .unwrap();
        let (state, _) = initial_clock_state(&clock);
        assert!(matches!(state, InitialClockState::NotSet));

        // Update the clock, which is already running.
        clock
            .update(zx::ClockUpdate::builder().approximate_value(zx::Time::from_nanos(1_000_000)))
            .unwrap();
        let (state, _) = initial_clock_state(&clock);
        assert_eq!(state, InitialClockState::PreviouslySet);
    }
}
