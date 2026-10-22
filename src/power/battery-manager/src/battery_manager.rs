// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::BatteryInfoSource;
use crate::battery_info_recorders::{BatteryInfoRecorders, FaultRecoveryEvent, RecorderConfig};
use crate::cache::{Cache, CachedStatus, StatusFields, Subscription, duplicate_lease};
use crate::polisher::Polisher;
use anyhow::{Context as _, Error, anyhow};
use fidl::endpoints::Proxy;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_system as fsystem;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use futures::{StreamExt, TryStreamExt, stream};
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::rc::Rc;
use zx;

/// Watch options battery-manager configures on the battery driver.
fn battery_watch_options() -> fbattery::WatchOptions {
    fbattery::WatchOptions {
        // Omitting `interest` defaults to receiving change notifications on any field change.
        wake_on: Some(fbattery::Status {
            present: Some(true),
            level_percent: Some(0.0),
            remaining_capacity_uah: Some(0),
            full_charge_capacity_uah: Some(0),
            health: Some(fbattery::HealthStatus::Good),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Charger status fields that describe how the device is supplied. Changes to them move the
/// battery current, so the battery is re-read before they are published.
fn charger_supply_fields() -> fcharger::Status {
    fcharger::Status {
        online: Some(true),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        source_type: Some(fcharger::SourceType::Usb),
        ..Default::default()
    }
}

/// Watch options battery-manager configures on the charger driver: changes in the charger status
/// fields relevant to battery and power management.
fn charger_watch_options() -> fcharger::WatchOptions {
    let interest = fcharger::Status {
        charge_phase: Some(fcharger::ChargePhase::None),
        ..charger_supply_fields()
    };
    fcharger::WatchOptions {
        interest: Some(interest.clone()),
        wake_on: Some(interest),
        ..Default::default()
    }
}

impl<'a> From<CachedStatus<'a>> for fpower::BatteryInfo {
    fn from(cached: CachedStatus<'a>) -> Self {
        let mut info = match cached.battery {
            Some(battery) => battery_status_to_battery_info(battery),
            None => fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::NotAvailable),
                charge_status: Some(fpower::ChargeStatus::Unknown),
                charge_source: Some(fpower::ChargeSource::Unknown),
                timestamp: Some(zx::BootInstant::get().into_nanos()),
                ..Default::default()
            },
        };

        if let Some(charger) = cached.charger {
            let (charge_source, charge_status) = resolve_charger_state(charger, info.level_percent);
            info.charge_source = Some(charge_source);
            info.charge_status = Some(charge_status);
        }

        if cached.battery_spec.is_some() || cached.charger_spec.is_some() {
            info.battery_spec = Some(fpower::BatterySpec {
                design_capacity_uah: cached
                    .battery_spec
                    .and_then(|s| s.design_capacity_uah)
                    .map(|v| v.try_into().unwrap_or(i32::MAX)),
                max_charging_current_ua: cached
                    .charger_spec
                    .and_then(|s| s.max_charge_current_ua)
                    .map(|v| v.try_into().unwrap_or(i32::MAX)),
                max_charging_voltage_uv: cached
                    .charger_spec
                    .and_then(|s| s.max_charge_voltage_uv)
                    .map(|v| v.try_into().unwrap_or(i32::MAX)),
                ..Default::default()
            });
        }

        info
    }
}

/// An event from one of the driver watch loops, destined for `BatteryManager::apply_update`.
///
/// The watch loops are independent tasks, so they never touch cached state themselves; they
/// describe what happened and let a single consumer serialize the state change and the
/// resulting publish.
pub(crate) enum DriverUpdate {
    /// The battery driver became available. Carries its static spec, what battery-manager's watch
    /// on it delivers, and a proxy the consumer uses for on-demand reads.
    BatteryConnected {
        spec: Option<fbattery::Spec>,
        subscription: Subscription<fbattery::Status>,
        proxy: fbattery::BatteryProxy,
    },
    /// New telemetry from the battery driver.
    BatteryStatus { status: fbattery::Status, wake_lease: Option<zx::EventPair> },
    /// The battery driver went away.
    BatteryGone,
    /// A pre-assembled snapshot from the legacy `fuchsia.power.battery` provider, which reports
    /// `BatteryInfo` directly rather than raw driver telemetry.
    LegacyBatteryInfo { info: fpower::BatteryInfo, wake_lease: Option<zx::EventPair> },
    /// The charger driver became available. Carries its static spec, what battery-manager's
    /// watch on it delivers, and a proxy the consumer uses for on-demand reads.
    ChargerConnected {
        spec: Option<fcharger::Spec>,
        subscription: Subscription<fcharger::Status>,
        proxy: fcharger::ChargerProxy,
    },
    /// New telemetry from the charger driver.
    ChargerStatus { status: fcharger::Status, wake_lease: Option<zx::EventPair> },
    /// The charger driver went away.
    ChargerGone,
}

struct OnDropUpdate(mpsc::UnboundedSender<DriverUpdate>, Option<DriverUpdate>);
impl Drop for OnDropUpdate {
    fn drop(&mut self) {
        if let Some(update) = self.1.take() {
            let _ = self.0.unbounded_send(update);
        }
    }
}

fn battery_status_to_battery_info(battery: &fbattery::Status) -> fpower::BatteryInfo {
    let is_present = battery.present.unwrap_or_else(|| {
        battery.voltage_uv.is_some()
            || battery.level_percent.is_some()
            || battery.remaining_capacity_uah.is_some()
            || battery.health.is_some()
    });
    let status = Some(match (is_present, battery.health) {
        (false, _) => fpower::BatteryStatus::NotPresent,
        (true, Some(fbattery::HealthStatus::Dead)) => fpower::BatteryStatus::NotAvailable,
        (true, _) => fpower::BatteryStatus::Ok,
    });

    let present_voltage_mv = battery.voltage_uv.map(|v| v / 1000);
    let present_charging_current_ua = battery.current_ua;

    // `fuchsia.hardware.power.battery` deliberately describes only the pack, not the external
    // supply, so when no charger driver telemetry is present, the fallback `charge_source` and
    // `charge_status` are inferred from the pack's net current direction.
    let (charge_source, charge_status) = match battery.current_ua {
        Some(current_ua) if current_ua > 0 => {
            (Some(fpower::ChargeSource::AcAdapter), Some(fpower::ChargeStatus::Charging))
        }
        Some(current_ua) if current_ua < 0 => {
            (Some(fpower::ChargeSource::None), Some(fpower::ChargeStatus::Discharging))
        }
        Some(_) => (Some(fpower::ChargeSource::AcAdapter), Some(fpower::ChargeStatus::NotCharging)),
        None => (None, None),
    };

    const MILLIDEGREES_PER_DEGREE: f32 = 1000.0;
    let temperature_mc = battery
        .temp_celsius
        .filter(|t| t.is_finite())
        .map(|t| (t * MILLIDEGREES_PER_DEGREE).round() as i32);

    // Prefer the hardware-reported `health` when provided (e.g. Sorrel / SW5100), and otherwise
    // synthesize thermal/overvoltage health from pack telemetry (`temp_celsius` / `voltage_uv`)
    // for fuel gauges that do not expose a dedicated health register (e.g. MAX77779 FG).
    let health = battery
        .health
        .map(|health| match health {
            fbattery::HealthStatus::Good => fpower::HealthStatus::Good,
            fbattery::HealthStatus::Cold => fpower::HealthStatus::Cold,
            fbattery::HealthStatus::Cool => fpower::HealthStatus::Cool,
            fbattery::HealthStatus::Warm => fpower::HealthStatus::Warm,
            fbattery::HealthStatus::Hot => fpower::HealthStatus::Hot,
            fbattery::HealthStatus::Dead => fpower::HealthStatus::Dead,
            fbattery::HealthStatus::OverVoltage => fpower::HealthStatus::OverVoltage,
            fbattery::HealthStatus::UnspecifiedFailure => fpower::HealthStatus::UnspecifiedFailure,
            _ => fpower::HealthStatus::Unknown,
        })
        .or_else(|| {
            if present_voltage_mv.is_some_and(|v| v > 4600) {
                Some(fpower::HealthStatus::OverVoltage)
            } else {
                temperature_mc.map(|temp_mc| {
                    if temp_mc < 0 {
                        fpower::HealthStatus::Cold
                    } else if temp_mc < 10_000 {
                        fpower::HealthStatus::Cool
                    } else if temp_mc <= 45_000 {
                        fpower::HealthStatus::Good
                    } else if temp_mc <= 55_000 {
                        fpower::HealthStatus::Warm
                    } else {
                        fpower::HealthStatus::Hot
                    }
                })
            }
        });

    // `time_remaining` is deliberately left unset: `Polisher::calculate_time_to_full` recomputes
    // it on every path from the polished level and the averaged current.
    fpower::BatteryInfo {
        status,
        charge_status,
        charge_source,
        level_percent: battery.level_percent,
        remaining_charge_uah: battery.remaining_capacity_uah,
        full_capacity_uah: battery
            .full_charge_capacity_uah
            .map(|v| v.try_into().unwrap_or(i32::MAX)),
        health,
        temperature_mc,
        present_voltage_mv,
        present_charging_current_ua,
        timestamp: Some(zx::BootInstant::get().into_nanos()),
        ..Default::default()
    }
}

/// Minimum raw battery level, in percent, at which charge termination (`ChargePhase::Done`) is
/// reported as `ChargeStatus::Full`.
///
/// Termination only means the charger reached its programmed float voltage and termination
/// current, which can happen well below a full pack (e.g. a float voltage below the pack's
/// full-charge voltage, an intermediate step-charging tier, or a charge limit). Reporting `Full`
/// there would splice the UI level to 100% (see `Polisher::apply_spoofing`), so termination below
/// this level is reported as `NotCharging` instead.
const FULL_CHARGE_MIN_LEVEL_PERCENT: f32 = 95.0;

/// Resolves `(ChargeSource, ChargeStatus)` from charger telemetry and the raw battery level.
///
/// The charger is the authority on whether external power is attached and whether charging has
/// terminated, so `Full` is only reported while the charger is online and reports
/// `ChargePhase::Done`. Losing input power always resolves to `Discharging`.
fn resolve_charger_state(
    charger: &fcharger::Status,
    level_percent: Option<f32>,
) -> (fpower::ChargeSource, fpower::ChargeStatus) {
    if !charger.online.unwrap_or(false)
        || matches!(
            charger.operating_mode,
            Some(fcharger::OperatingMode::Discharging | fcharger::OperatingMode::Otg)
        )
    {
        return (fpower::ChargeSource::None, fpower::ChargeStatus::Discharging);
    }

    let source = match charger.source_type {
        Some(fcharger::SourceType::Ac) => fpower::ChargeSource::AcAdapter,
        Some(fcharger::SourceType::Usb) => fpower::ChargeSource::Usb,
        Some(fcharger::SourceType::Wireless) => fpower::ChargeSource::Wireless,
        _ => fpower::ChargeSource::Unknown,
    };

    let status = if charger.operating_mode == Some(fcharger::OperatingMode::Passthrough) {
        fpower::ChargeStatus::NotCharging
    } else {
        match charger.charge_phase {
            // Without a level to compare against, trust the charger's termination.
            Some(fcharger::ChargePhase::Done)
                if level_percent.is_none_or(|level| level >= FULL_CHARGE_MIN_LEVEL_PERCENT) =>
            {
                fpower::ChargeStatus::Full
            }
            Some(fcharger::ChargePhase::Done | fcharger::ChargePhase::None) => {
                fpower::ChargeStatus::NotCharging
            }
            _ => fpower::ChargeStatus::Charging,
        }
    };

    (source, status)
}

pub(crate) trait BatterySimulationStateObserver {
    fn update_simulation(&self, new_state: bool);
    fn update_simulated_battery_info(&self, battery_info: fpower::BatteryInfo);
}

impl BatterySimulationStateObserver for BatteryManager {
    fn update_simulation(&self, is_simulating: bool) {
        let mut sim_state = self.simulation_state.borrow_mut();
        *sim_state = is_simulating;
        drop(sim_state);
        if !is_simulating {
            self.common_update_watchers(self.get_battery_info_copy(), None);
        }
    }
    fn update_simulated_battery_info(&self, battery_info: fpower::BatteryInfo) {
        self.update_watchers_conditionally(true, battery_info, None);
    }
}

/// Core component for the battery manager system.
///
/// BatteryManager maintains the current state info for the battery system
/// as well as the watchers that share this information with subscribed clients.
///
/// simulation_state: true when the simulator is running
pub struct BatteryManager {
    /// Cached battery info representing the last committed state.
    ///
    /// Used to:
    /// - Send an initial snapshot to newly registered `Watch()` clients.
    /// - Check previous state transitions (e.g. plug/unplug events) when processing new updates.
    /// - Maintain state across simulation mode toggles.
    cached_battery_info: RefCell<fpower::BatteryInfo>,
    watchers: Rc<RefCell<Vec<fpower::BatteryInfoWatcherProxy>>>,
    simulation_state: RefCell<bool>,
    simulated_battery_info: RefCell<fpower::BatteryInfo>,
    data_polisher: RefCell<Polisher>,
    info_recorders: BatteryInfoRecorders,
    /// Blocking suspension if charging
    charge_wake_lease: RefCell<Option<fsystem::LeaseToken>>,
    /// Caches raw battery and charger status and spec and serves them to governors.
    cache: Rc<Cache>,

    update_sender: mpsc::Sender<(fpower::BatteryInfo, Option<zx::EventPair>)>,
    _worker_task: fasync::Task<()>,
}

#[inline]
fn get_current_time() -> i64 {
    zx::BootInstant::get().into_nanos()
}

impl BatteryManager {
    #[cfg(test)]
    pub fn new(recorder_config: RecorderConfig) -> BatteryManager {
        Self::new_with_battery_manager_config(
            recorder_config,
            crate::BatteryManagerConfig::default(),
        )
    }

    pub fn new_with_battery_manager_config(
        recorder_config: RecorderConfig,
        battery_manager_config: crate::BatteryManagerConfig,
    ) -> BatteryManager {
        let watchers_rc = Rc::new(RefCell::new(Vec::new()));
        // For now the size is arbitrary chosen. Will log error and catch in CQ.
        let (sender, receiver) = futures::channel::mpsc::channel(10);
        let worker_task = Self::start_watcher_worker(watchers_rc.clone(), receiver);
        let cache = Rc::new(Cache::new(
            Subscription::from_battery_driver(None, Some(battery_watch_options())),
            Subscription::from_charger_driver(None, Some(charger_watch_options())),
        ));

        BatteryManager {
            cached_battery_info: RefCell::new(fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::NotAvailable),
                charge_status: Some(fpower::ChargeStatus::Unknown),
                charge_source: Some(fpower::ChargeSource::Unknown),
                level_percent: None,
                level_status: Some(fpower::LevelStatus::Unknown),
                health: Some(fpower::HealthStatus::Unknown),
                time_remaining: Some(fpower::TimeRemaining::Indeterminate(0)),
                timestamp: Some(get_current_time()),
                ..Default::default()
            }),
            watchers: watchers_rc,
            simulation_state: RefCell::new(false),
            simulated_battery_info: RefCell::new(fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::NotAvailable),
                charge_status: Some(fpower::ChargeStatus::Unknown),
                charge_source: Some(fpower::ChargeSource::Unknown),
                level_percent: None,
                level_status: Some(fpower::LevelStatus::Unknown),
                health: Some(fpower::HealthStatus::Unknown),
                time_remaining: Some(fpower::TimeRemaining::Indeterminate(0)),
                timestamp: Some(get_current_time()),
                ..Default::default()
            }),
            data_polisher: RefCell::new(Polisher::new_with_battery_manager_config(
                battery_manager_config,
            )),
            info_recorders: BatteryInfoRecorders::new(recorder_config),
            charge_wake_lease: RefCell::new(None),
            cache,
            update_sender: sender,
            _worker_task: worker_task,
        }
    }

    // Global Worker Task (This runs only once)
    fn start_watcher_worker(
        watchers_rc: Rc<RefCell<Vec<fpower::BatteryInfoWatcherProxy>>>,
        mut receiver: mpsc::Receiver<(fpower::BatteryInfo, Option<zx::EventPair>)>,
    ) -> fasync::Task<()> {
        fasync::Task::local(async move {
            // Processes updates sequentially, guaranteeing order.
            while let Some((info, wake_lease)) = receiver.next().await {
                let watchers_to_send = {
                    let mut watchers_guard = watchers_rc.borrow_mut();
                    watchers_guard.retain(|w| !w.is_closed()); // Cleanup of closed channels
                    watchers_guard.clone() // Clone the cleaned list for concurrent sending
                };

                stream::iter(watchers_to_send)
                    .for_each_concurrent(None, |w| {
                        let info_clone = info.clone();
                        let wake_lease_dup = duplicate_lease(wake_lease.as_ref());

                        async move {
                            if let Err(e) =
                                w.on_change_battery_info(&info_clone.into(), wake_lease_dup).await
                            {
                                warn!("failed to send battery info to watcher {:?}", e);
                            }
                        }
                    })
                    .await;
            }
        })
    }

    /// Returns the cache that holds raw battery and charger status and serves governors.
    pub(crate) fn cache(&self) -> Rc<Cache> {
        self.cache.clone()
    }

    // Adds watcher
    pub fn add_watcher(&self, watcher: fpower::BatteryInfoWatcherProxy) {
        let mut watchers = self.watchers.borrow_mut();
        debug!("::manager:: adding watcher: {:?} [{:?}]", watcher, watchers.len());
        watchers.push(watcher)
    }

    // Call update_watchers if expecting_simulating == simulation_state.
    // This behavior avoids unnecessary updates.
    fn update_watchers_conditionally(
        &self,
        expect_simulating: bool,
        info: fpower::BatteryInfo,
        wake_lease: Option<zx::EventPair>,
    ) {
        if expect_simulating {
            *self.simulated_battery_info.borrow_mut() = info.clone();
        } else {
            *self.cached_battery_info.borrow_mut() = info.clone();
        }
        if self.is_simulating() == expect_simulating {
            self.common_update_watchers(info, wake_lease);
        }
    }

    pub fn common_update_watchers(
        &self,
        info: fpower::BatteryInfo,
        wake_lease: Option<zx::EventPair>,
    ) {
        debug!("::manager:: update watchers...");
        if let Err(e) = self.update_sender.clone().try_send((info, wake_lease)) {
            self.info_recorders.record_watcher_send_error();
            log::error!("Failed to send watcher update: {:?}", e);
        }
    }

    async fn determine_suspend_status(
        &self,
        source: Option<fpower::ChargeSource>,
        charge_status: Option<fpower::ChargeStatus>,
        sag: Option<fsystem::ActivityGovernorProxy>,
    ) {
        let Some(sag) = sag else {
            return;
        };

        if source.is_none() && charge_status.is_none() {
            return;
        }

        let is_charging = match (source, charge_status) {
            (_, Some(fpower::ChargeStatus::NotCharging | fpower::ChargeStatus::Discharging)) => {
                false
            }
            (_, Some(fpower::ChargeStatus::Charging | fpower::ChargeStatus::Full)) => true,
            (Some(fpower::ChargeSource::Unknown | fpower::ChargeSource::None) | None, _) => false,
            _ => true,
        };

        if is_charging && self.charge_wake_lease.borrow().is_none() {
            let res = sag.acquire_unmonitored_wake_lease("charging_block_suspension").await;

            match res {
                Ok(Ok(token)) => {
                    info!("Acquired wake lock to block suspension while charging.");
                    *self.charge_wake_lease.borrow_mut() = Some(token);
                    self.info_recorders.record_wake_lease_state(true, true);
                }
                Ok(Err(e)) => {
                    self.info_recorders.record_sag_lease_error();
                    error!("Can't block suspension due to error: {:?}", e);
                }
                Err(e) => {
                    self.info_recorders.record_sag_lease_error();
                    error!("Can't block suspension due to FIDL error {:?}", e);
                }
            }
        }

        if !is_charging && self.charge_wake_lease.borrow().is_some() {
            *self.charge_wake_lease.borrow_mut() = None;
            self.info_recorders.record_wake_lease_state(false, false);
            info!("Dropped wake lease token, allowing suspension.");
        }
    }

    async fn process_battery_info(
        &self,
        info: fpower::BatteryInfo,
        sag: Option<fsystem::ActivityGovernorProxy>,
    ) -> fpower::BatteryInfo {
        let start_time_ns = get_current_time();
        let raw_level = info.level_percent;
        let new_charge_status = info.charge_status;
        let recovery_event = self.info_recorders.update(raw_level, new_charge_status);
        self.info_recorders.record_raw_level_on_change(&info);

        // Detect when the charger reports charge termination (`ChargePhase::Done`) while online
        // and in an active charging mode (not discharging, OTG, or passthrough), but the raw fuel
        // gauge SOC is still below `FULL_CHARGE_MIN_LEVEL_PERCENT` (95%). `resolve_charger_state`
        // maps this condition to `ChargeStatus::NotCharging` to prevent `Polisher` from splicing
        // the UI level to 100%, so we inspect the cached charger status directly to record the
        // premature termination in Inspect diagnostics.
        let premature_charge_done = self.cache.with_cached(|cached| {
            cached.charger.is_some_and(|charger| {
                charger.online.unwrap_or(false)
                    && !matches!(
                        charger.operating_mode,
                        Some(
                            fcharger::OperatingMode::Discharging
                                | fcharger::OperatingMode::Otg
                                | fcharger::OperatingMode::Passthrough
                        )
                    )
                    && charger.charge_phase == Some(fcharger::ChargePhase::Done)
                    && raw_level.is_some_and(|level| level < FULL_CHARGE_MIN_LEVEL_PERCENT)
            })
        });

        let old_is_plugged_in = Polisher::is_plugged_in(&self.cached_battery_info.borrow());
        let new_is_plugged_in = Polisher::is_plugged_in(&info);

        let mut info = {
            let mut data_polisher = self.data_polisher.borrow_mut();
            if !old_is_plugged_in && new_is_plugged_in {
                data_polisher.reset_average_current();
            }
            if recovery_event == FaultRecoveryEvent::Recovered {
                data_polisher.reset_rate_limiter();
            }
            data_polisher.polish_info(info)
        };

        self.determine_suspend_status(info.charge_source, info.charge_status, sag).await;

        if info.timestamp.is_none() {
            info.timestamp = Some(get_current_time());
        }

        self.publish_to_inspect(&info, premature_charge_done);

        let elapsed_ns = (get_current_time() - start_time_ns).max(0) as u64;
        self.info_recorders.record_update_latency_us(elapsed_ns / 1_000);

        info
    }

    /// Serializes driver updates onto a single task.
    ///
    /// The battery and charger watch loops both derive `BatteryInfo` from the same cached
    /// state, and deriving it awaits (`determine_suspend_status`, and a battery re-read on
    /// some charger events). Funnelling every update through one consumer keeps
    /// read-modify-publish atomic, so the loops can no longer interleave and publish stale
    /// state or leak the charging wake lease.
    pub(crate) async fn run_update_consumer(
        &self,
        mut receiver: mpsc::UnboundedReceiver<DriverUpdate>,
        sag: Option<fsystem::ActivityGovernorProxy>,
    ) {
        while let Some(update) = receiver.next().await {
            self.apply_update(update, sag.clone()).await;
        }
    }

    /// Applies one driver update. This is the only writer of the cached driver state and the only
    /// caller of `process_battery_info`.
    ///
    /// Returns whether watchers were updated.
    async fn apply_update(
        &self,
        update: DriverUpdate,
        sag: Option<fsystem::ActivityGovernorProxy>,
    ) -> bool {
        let wake_lease = match update {
            DriverUpdate::BatteryConnected { spec, subscription, proxy } => {
                self.cache.battery_connected(spec, subscription, proxy);
                return false;
            }
            DriverUpdate::BatteryStatus { status, wake_lease } => {
                self.cache.publish_battery(&status, wake_lease.as_ref());
                wake_lease
            }
            // Republish so clients see the battery is gone and the charging wake lease is
            // re-evaluated, rather than latching the last state the battery reported.
            DriverUpdate::BatteryGone => {
                self.cache.battery_gone();
                self.info_recorders.record_disconnected();
                None
            }
            DriverUpdate::LegacyBatteryInfo { info, wake_lease } => {
                let info = self.process_battery_info(info, sag).await;
                self.update_watchers_conditionally(false, info, wake_lease);
                return true;
            }
            DriverUpdate::ChargerConnected { spec, subscription, proxy } => {
                self.cache.charger_connected(spec, subscription, proxy);
                return false;
            }
            DriverUpdate::ChargerStatus { status, wake_lease } => {
                // The re-read shares the charger update's wake lease, so that battery `wake_on`
                // changes it reveals carry it.
                if self.charger_update_needs_battery(&status)
                    && let Err(e) = self.refresh_battery_status(wake_lease.as_ref()).await
                {
                    self.info_recorders.record_battery_refresh_error();
                    warn!("Failed to refresh battery status on charger update: {e:?}");
                }
                self.cache.publish_charger(&status, wake_lease.as_ref());
                wake_lease
            }
            // Republish so clients see the battery-only view and the charging wake lease is
            // re-evaluated, rather than latching the last state the charger reported.
            DriverUpdate::ChargerGone => {
                self.cache.charger_gone();
                self.info_recorders.record_charger_disconnect();
                None
            }
        };

        let raw_info = self.cache.with_cached(|cached| fpower::BatteryInfo::from(cached));
        let info = self.process_battery_info(raw_info, sag).await;
        self.update_watchers_conditionally(false, info, wake_lease);
        true
    }

    /// Whether the battery should be re-read with [`Self::refresh_battery_status`] before
    /// publishing charger `status`.
    ///
    /// The battery driver only pushes on its own change triggers, but charger events move
    /// quantities it reports (notably charge current). The battery is re-read when `status`
    /// changes one of [`charger_supply_fields`], or would complete a governor connection's
    /// `Watch`, since the governor then reads the battery with `GetStatus`, which is served from
    /// the cache. Other changes, such as the charge phase alone, are published with the cached
    /// battery status.
    fn charger_update_needs_battery(&self, status: &fcharger::Status) -> bool {
        let supply = charger_supply_fields();
        let supply_changed = self.cache.with_cached(|cached| {
            cached.charger.is_none_or(|cached| cached.masked(&supply) != status.masked(&supply))
        });
        supply_changed || self.cache.charger_update_is_news(status)
    }

    /// Re-reads battery status from the connected driver, if any, and publishes it to the cache.
    async fn refresh_battery_status(&self, lease: Option<&zx::EventPair>) -> Result<(), Error> {
        let Some(proxy) = self.cache.battery_proxy() else {
            return Ok(());
        };
        let status = proxy
            .get_status()
            .await
            .context("FIDL error querying battery status")?
            .map_err(|e| anyhow!("Failed to read battery status: {e:?}"))?;
        self.cache.publish_battery(&status, lease);
        Ok(())
    }

    fn publish_to_inspect(&self, info: &fpower::BatteryInfo, premature_charge_done: bool) {
        self.info_recorders.record_battery_update(info, premature_charge_done);
    }

    pub fn get_battery_info_copy(&self) -> fpower::BatteryInfo {
        if *self.simulation_state.borrow() {
            let info_lock = self.simulated_battery_info.borrow();
            (*info_lock).clone()
        } else {
            let info_lock = self.cached_battery_info.borrow();
            (*info_lock).clone()
        }
    }

    pub fn is_simulating(&self) -> bool {
        *self.simulation_state.borrow()
    }

    pub(crate) async fn serve(
        &self,
        stream: fpower::BatteryManagerRequestStream,
    ) -> Result<(), Error> {
        stream
            .try_for_each_concurrent(None, move |request| {
                async move {
                    match request {
                        fpower::BatteryManagerRequest::GetBatteryInfo { responder, .. } => {
                            let info = self.get_battery_info_copy();
                            debug!(
                                info:?;
                                "::battery_manager_request:: handle GetBatteryInfo request"
                            );
                            responder.send(&info)?;
                        }
                        fpower::BatteryManagerRequest::Watch { watcher, .. } => {
                            let watcher = watcher.into_proxy();
                            debug!("::battery_manager_request:: handle Watch request");
                            self.add_watcher(watcher.clone());

                            // Make sure watcher has current battery info.
                            // But there is no copy of the wake lease.
                            let info = self.get_battery_info_copy();
                            debug!(info:?; "::battery_manager_request:: callback on new watcher");
                            watcher.on_change_battery_info(&info, None).await?;
                        }
                    }
                    Ok(())
                }
            })
            .await?;

        Ok(())
    }

    // Called by start_watching_battery_info to process the OnChangeBatteryInfo Call
    async fn wait_on_updates(
        &self,
        watcher: fidl::endpoints::ServerEnd<fpower::BatteryInfoWatcherMarker>,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        let mut stream = watcher.into_stream();
        while let Some(event) = stream.try_next().await? {
            match event {
                fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                    info,
                    wake_lease,
                    responder,
                } => {
                    let _ = updates
                        .unbounded_send(DriverUpdate::LegacyBatteryInfo { info, wake_lease });
                    responder.send()?;
                }
            }
        }
        Ok(())
    }

    // Main should explicitly call this, so Battery Manager starts to watch the battery info from
    // battery driver, and conditionally dispatches to clients according to simulating state.
    pub(crate) async fn start_watching_battery_info(
        &self,
        source: BatteryInfoSource,
        sag: Option<fsystem::ActivityGovernorProxy>,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        match source {
            BatteryInfoSource::New(proxy) => {
                self.wait_on_new_driver_updates(proxy, sag, updates).await
            }
            BatteryInfoSource::ModernService(proxy) => {
                self.wait_on_modern_service_updates(proxy, updates).await
            }
        }
    }

    pub(crate) async fn start_watching_charger_info(
        &self,
        proxy: fcharger::ChargerProxy,
        sag: Option<fsystem::ActivityGovernorProxy>,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        self.wait_on_charger_driver_updates(proxy, sag, updates).await
    }

    async fn wait_on_new_driver_updates(
        &self,
        proxy: fbattery::BatteryProxy,
        sag: Option<fsystem::ActivityGovernorProxy>,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        info!("Waiting on updates from new fuchsia.hardware.power.battery driver");

        let battery_spec = proxy.get_spec().await.ok().and_then(Result::ok);

        let effective_options = match proxy.configure_watch(&battery_watch_options()).await {
            Ok(Ok(effective_options)) => Some(effective_options),
            Ok(Err(status)) => {
                self.info_recorders.record_battery_watch_error();
                warn!("Battery driver returned error configuring watch triggers: {:?}", status);
                None
            }
            Err(e) => {
                self.info_recorders.record_battery_watch_error();
                warn!("Failed to configure watch triggers on battery driver: {:?}", e);
                None
            }
        };
        let subscription =
            Subscription::from_battery_driver(battery_spec.as_ref(), effective_options);
        let _ = updates.unbounded_send(DriverUpdate::BatteryConnected {
            spec: battery_spec,
            subscription,
            proxy: proxy.clone(),
        });

        let mut current_lease: Option<fsystem::LeaseToken> = if let Some(sag) = &sag {
            match sag.acquire_wake_lease("battery_manager").await {
                Ok(Ok(token)) => {
                    info!("Acquired wake lock for battery manager.");
                    Some(token)
                }
                Ok(Err(e)) => {
                    self.info_recorders.record_sag_lease_error();
                    warn!("Can't acquire wake lock due to error: {:?}", e);
                    None
                }
                Err(e) => {
                    self.info_recorders.record_sag_lease_error();
                    warn!("Can't acquire wake lock due to FIDL error {:?}", e);
                    None
                }
            }
        } else {
            warn!("No ActivityGovernor service available, can't acquire wake lock");
            None
        };
        let _cleanup = OnDropUpdate(updates.clone(), Some(DriverUpdate::BatteryGone));

        loop {
            match proxy.watch(current_lease.take()).await {
                Ok(Ok((status, wake_lease))) => {
                    let watcher_wake_lease = duplicate_lease(wake_lease.as_ref());
                    current_lease = wake_lease;

                    let _ = updates.unbounded_send(DriverUpdate::BatteryStatus {
                        status,
                        wake_lease: watcher_wake_lease,
                    });
                }
                Ok(Err(e)) => {
                    self.info_recorders.record_battery_watch_error();
                    return Err(anyhow::anyhow!("Battery driver error: {:?}", e));
                }
                Err(e) => {
                    self.info_recorders.record_battery_watch_error();
                    return Err(anyhow::Error::from(e).context("Error in Watch"));
                }
            }
        }
    }

    async fn wait_on_charger_driver_updates(
        &self,
        proxy: fcharger::ChargerProxy,
        sag: Option<fsystem::ActivityGovernorProxy>,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        info!("Waiting on updates from new fuchsia.hardware.power.charger driver");
        let _cleanup = OnDropUpdate(updates.clone(), Some(DriverUpdate::ChargerGone));

        let charger_spec = match proxy.get_spec().await {
            Ok(Ok(s)) => Some(s),
            Ok(Err(status)) => {
                self.info_recorders.record_charger_watch_error();
                warn!("Charger driver failed to return spec: {:?}", status);
                None
            }
            Err(e) => {
                self.info_recorders.record_charger_watch_error();
                warn!("FIDL error querying charger spec: {:?}", e);
                None
            }
        };

        let effective_options = match proxy.configure_watch(&charger_watch_options()).await {
            Ok(Ok(effective_options)) => {
                debug!("Configured charger watch options");
                Some(effective_options)
            }
            Ok(Err(status)) => {
                self.info_recorders.record_charger_watch_error();
                warn!("Charger driver rejected watch options: {:?}", status);
                None
            }
            Err(e) => {
                self.info_recorders.record_charger_watch_error();
                warn!("FIDL error configuring charger watch options: {:?}", e);
                None
            }
        };
        let subscription =
            Subscription::from_charger_driver(charger_spec.as_ref(), effective_options);
        let _ = updates.unbounded_send(DriverUpdate::ChargerConnected {
            spec: charger_spec,
            subscription,
            proxy: proxy.clone(),
        });

        let mut current_lease: Option<fsystem::LeaseToken> = if let Some(sag) = &sag {
            match sag.acquire_wake_lease("battery_manager_charger").await {
                Ok(Ok(token)) => {
                    info!("Acquired initial wake lock for charger watcher.");
                    Some(token)
                }
                Ok(Err(e)) => {
                    self.info_recorders.record_sag_lease_error();
                    warn!("Can't acquire charger wake lock due to error: {:?}", e);
                    None
                }
                Err(e) => {
                    self.info_recorders.record_sag_lease_error();
                    warn!("Can't acquire charger wake lock due to FIDL error {:?}", e);
                    None
                }
            }
        } else {
            None
        };
        loop {
            match proxy.watch(current_lease.take()).await {
                Ok(Ok((status, wake_lease))) => {
                    let downstream_lease = duplicate_lease(wake_lease.as_ref());
                    current_lease = wake_lease;

                    let _ = updates.unbounded_send(DriverUpdate::ChargerStatus {
                        status,
                        wake_lease: downstream_lease,
                    });
                }
                Ok(Err(e)) => {
                    self.info_recorders.record_charger_watch_error();
                    return Err(anyhow::anyhow!("Charger error: {:?}", e));
                }
                Err(e) => {
                    self.info_recorders.record_charger_watch_error();
                    return Err(anyhow::Error::from(e).context("Error in WatchCharger"));
                }
            }
        }
    }

    async fn wait_on_modern_service_updates(
        &self,
        proxy: fpower::BatteryInfoProviderProxy,
        updates: mpsc::UnboundedSender<DriverUpdate>,
    ) -> Result<(), Error> {
        info!("Waiting on updates from fuchsia.power.battery service");
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fpower::BatteryInfoWatcherMarker>();
        if let Err(e) = proxy.watch(client_end) {
            self.info_recorders.record_battery_watch_error();
            return Err(e.into());
        }

        info!("Waiting on updates from driver");
        let res = self.wait_on_updates(server_end, updates).await;
        if res.is_err() {
            self.info_recorders.record_battery_watch_error();
        }
        warn!("Driver disconnected");

        self.info_recorders.record_disconnected();

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery_info_recorders::PersistenceDirs;
    use assert_matches::assert_matches;
    use async_utils::hanging_get::server::HangingGet;
    use fidl::endpoints::create_request_stream;
    use fuchsia_inspect::{self as inspect};
    use futures::channel::oneshot;
    use futures::future::{join, join3};
    use log::info;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Arc;
    use tempfile::{TempDir, tempdir};

    pub fn create_manager() -> (TempDir, BatteryManager) {
        let dir = tempdir().unwrap();
        let storage_path = dir.path().join("data");
        let volatile_path = dir.path().join("tmp");
        fs::create_dir(&storage_path).unwrap();
        fs::create_dir(&volatile_path).unwrap();

        let storage_dir = storage_path.to_str().unwrap().to_string();
        let volatile_dir = volatile_path.to_str().unwrap().to_string();

        let recorder_config = RecorderConfig {
            persistence_dirs: Some(PersistenceDirs { storage_dir, volatile_dir }),
        };
        let battery_manager = BatteryManager::new_with_battery_manager_config(
            recorder_config,
            crate::BatteryManagerConfig { shutdown_offset_percent: 3.0, ..Default::default() },
        );
        (dir, battery_manager)
    }

    /// Runs the production consumer loop and reports every published update.
    ///
    /// Producers hand updates off asynchronously, so tests await on the returned receiver
    /// rather than on producer-side progress.
    fn spawn_test_consumer(
        manager: Arc<BatteryManager>,
        mut receiver: mpsc::UnboundedReceiver<DriverUpdate>,
        sag: Option<fsystem::ActivityGovernorProxy>,
    ) -> (fasync::Task<()>, mpsc::UnboundedReceiver<()>) {
        let (published_tx, published_rx) = mpsc::unbounded();
        let task = fasync::Task::local(async move {
            while let Some(update) = receiver.next().await {
                if manager.apply_update(update, sag.clone()).await {
                    let _ = published_tx.unbounded_send(());
                }
            }
        });
        (task, published_rx)
    }

    #[fuchsia::test]
    async fn test_run_watcher() {
        info!("Starting");
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        let (_dir, mut battery_manager) = create_manager();
        let mut battery_info: fpower::BatteryInfo = battery_manager.get_battery_info_copy();
        battery_info.level_percent = Some(50.0);

        let (watcher_client_end, mut stream) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher = watcher_client_end.into_proxy();

        battery_manager.add_watcher(watcher.clone());

        // Create a zx::EventPair for the test
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id = token_info.koid;
        let wake_lease = Some(rx); // The rx handle is what we'll pass to the server

        let serve_fut = async move {
            info!("Try_nest");
            let request = stream.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: Some(received_lease),
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);

                let token_info = received_lease.basic_info().unwrap();
                let related_id = token_info.related_koid;
                assert_eq!(related_id, tx_id);
                info!("fake watcher ends checking lease");

                responder.send().unwrap();
                let _ = tx_signal.send(());
            } else {
                panic!("Unexpected message received: {:?}", request);
            };
        };
        let request_fut = async {
            info!("Updating watchers");
            battery_manager.common_update_watchers(battery_info, wake_lease);
        };

        join(serve_fut, request_fut).await;
        rx_signal.await.unwrap();
        battery_manager.update_sender.close_channel();
        battery_manager._worker_task.await;
    }

    #[fuchsia::test]
    async fn test_run_watchers_channel_closed() {
        let (_dir, battery_manager) = create_manager();
        let mut battery_info: fpower::BatteryInfo = battery_manager.get_battery_info_copy();
        battery_info.level_percent = Some(50.0);

        let (watcher1_client_end, mut stream1) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher1 = watcher1_client_end.into_proxy();

        let (watcher2_client_end, mut stream2) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher2 = watcher2_client_end.into_proxy();

        battery_manager.add_watcher(watcher1);
        battery_manager.add_watcher(watcher2);

        let serve1_fut = async move {
            // first request should match first change notification sent
            // at 50%
            let request = stream1.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received: {:?}", request);
            };
            // second should match subsequent notification at 60%
            let request = stream1.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 60);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received: {:?}", request);
            };
        };

        let serve2_fut = async move {
            // first request should match first change notification sent
            // at 50%
            let request = stream2.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);
                // but then we drop the channel...
                std::mem::drop(responder);
                std::mem::drop(stream2);
            } else {
                panic!("Unexpected message received: {:?}", request);
            };
        };

        let request_fut = async {
            battery_manager.common_update_watchers(battery_info.clone(), None);
            battery_info.level_percent = Some(60.0);
            battery_manager.common_update_watchers(battery_info, None);
        };

        join3(serve1_fut, serve2_fut, request_fut).await;
    }

    // This function acts as a fake watcher, processing FIDL messages
    fn fake_watcher(
        info_checker: impl Fn(fpower::BatteryInfo) + 'static,
        lease_checker: impl FnOnce(Option<zx::EventPair>) + 'static,
    ) -> fpower::BatteryInfoWatcherProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::BatteryInfoWatcherMarker>();
        fasync::Task::local(async move {
            if let Ok(req) = stream.try_next().await {
                match req {
                    Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                        info,
                        wake_lease,
                        responder,
                    }) => {
                        info_checker(info);
                        lease_checker(wake_lease);
                        let _ = responder.send();
                    }
                    e => panic!("Unexpected request: {:?}", e),
                }
            }
        })
        .detach();

        proxy
    }

    #[fuchsia::test]
    async fn test_wait_on_updates() {
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        // Prepare the wake_lease. For the tx, we need to obtain its koid.
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id = token_info.koid;
        let wake_lease = Some(rx);

        let (_dir, battery_manager) = create_manager();
        let battery_manager = Arc::new(battery_manager);
        let (update_tx, update_rx) = mpsc::unbounded();
        let (_consumer, mut published) =
            spawn_test_consumer(battery_manager.clone(), update_rx, None);

        // Create a client and server pair for the FIDL call to be used by the pair of
        // wait_on_updates(business logic) and on_change_battery_info(test)
        let (proxy, server_end) =
            fidl::endpoints::create_proxy::<fpower::BatteryInfoWatcherMarker>();

        // Set some battery info, and add a fake watcher.
        let mut updated_info = battery_manager.get_battery_info_copy();
        updated_info.level_percent = Some(100.0);
        updated_info.status = Some(fpower::BatteryStatus::Ok);
        battery_manager.add_watcher(fake_watcher(
            move |info| {
                assert_eq!(info.level_percent, Some(100.0));
                assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
            },
            move |lease| {
                let lease = lease.expect("Should not be None");
                let token_info = lease.basic_info().unwrap();
                let related_id = token_info.related_koid;
                assert_eq!(related_id, tx_id);
                info!("fake watcher ends checking lease");
                let _ = tx_signal.send(());
            },
        ));

        // The 'server' task: run wait_on_updates in the background
        let battery_clone = battery_manager.clone();
        let server_task = fasync::Task::local(async move {
            battery_clone.wait_on_updates(server_end, update_tx).await
        });

        let client_fut = async move {
            proxy.on_change_battery_info(&updated_info, wake_lease).await.unwrap();
        };

        // Run both the server task and client future concurrently
        let _ = join(server_task, client_fut).await;
        published.next().await.expect("update should be published");

        // After the futures complete, check the state of the BatteryManager
        let final_info = battery_manager.get_battery_info_copy();

        // Assert that the state was updated
        assert_eq!(final_info.level_percent, Some(100.0));
        assert_eq!(final_info.status, Some(fpower::BatteryStatus::Ok));

        rx_signal.await.unwrap();
    }

    // This function acts as a fake driver, provide battery info and lease.
    fn fake_driver(
        info: fpower::BatteryInfo,
        lease: Option<zx::EventPair>,
    ) -> fpower::BatteryInfoProviderProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::BatteryInfoProviderMarker>();
        fasync::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    fpower::BatteryInfoProviderRequest::Watch { watcher, .. } => {
                        let watcher = watcher.into_proxy();
                        let duplicated_lease = duplicate_lease(lease.as_ref());
                        assert!(
                            watcher.on_change_battery_info(&info, duplicated_lease).await.is_ok()
                        );
                    }
                    e => panic!("Unexpected request: {:?}", e),
                }
            }
        })
        .detach();

        proxy
    }

    #[fuchsia::test]
    async fn test_start_watching_battery_info() -> Result<(), Error> {
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        // Prepare the wake_lease. For the tx, we need to obtain its koid.
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info()?;
        let tx_id = token_info.koid;
        let wake_lease = Some(rx);

        // Set some battery info, and add a fake watcher.
        let (_dir, battery_manager) = create_manager();
        let battery_manager = Arc::new(battery_manager);
        let (update_tx, update_rx) = mpsc::unbounded();
        let (_consumer, mut published) =
            spawn_test_consumer(battery_manager.clone(), update_rx, None);
        let mut updated_info = battery_manager.get_battery_info_copy();
        updated_info.level_percent = Some(100.0);
        updated_info.status = Some(fpower::BatteryStatus::Ok);
        updated_info.charge_source = Some(fpower::ChargeSource::Usb);
        updated_info.timestamp = Some(20);

        battery_manager.add_watcher(fake_watcher(
            move |info| {
                assert_eq!(info.level_percent, Some(100.0));
                assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
                let timestamp = info.timestamp.unwrap();
                assert_eq!(timestamp, 20);
            },
            move |lease| {
                let lease = lease.expect("Should not be None");
                let token_info = lease.basic_info().unwrap();
                let related_id = token_info.related_koid;
                assert_eq!(related_id, tx_id);
                info!("fake watcher ends checking lease");
                let _ = tx_signal.send(());
            },
        ));

        // test start_watching_battery_info
        let _ = battery_manager
            .start_watching_battery_info(
                BatteryInfoSource::ModernService(fake_driver(updated_info, wake_lease)),
                None,
                update_tx,
            )
            .await;
        published.next().await.expect("update should be published");

        // After the futures complete, check the state of the BatteryManager
        let final_info = battery_manager.get_battery_info_copy();

        // Assert that the state was updated
        assert_eq!(final_info.level_percent, Some(100.0));
        assert_eq!(final_info.status, Some(fpower::BatteryStatus::Ok));

        rx_signal.await.unwrap();
        Ok(())
    }

    // This function acts as a fake sag server and respond with leases from the queue.
    fn fake_sag_vec(
        lease_sequence: Rc<RefCell<VecDeque<fsystem::LeaseToken>>>,
    ) -> fsystem::ActivityGovernorProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        fasync::Task::local(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(fsystem::ActivityGovernorRequest::AcquireUnmonitoredWakeLease {
                        responder,
                        ..
                    }) => {
                        let mut queue = lease_sequence.borrow_mut();
                        let result = queue.pop_front().unwrap();
                        responder.send(Ok(result)).unwrap();
                    }
                    e => panic!("Unexpected request: {:?}", e),
                }
            }
        })
        .detach();

        proxy
    }

    #[fuchsia::test]
    async fn test_block_suspend() {
        info!("Starting");
        let (_dir, battery_manager) = create_manager();
        {
            let charge_wake_lease = battery_manager.charge_wake_lease.borrow();
            assert!(charge_wake_lease.is_none());
        }

        let (tx, rx1) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id1 = token_info.koid;

        let (tx, rx2) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id2 = token_info.koid;

        let vector = vec![rx1, rx2];
        let sag = Some(fake_sag_vec(Rc::new(RefCell::new(VecDeque::from(vector)))));

        battery_manager
            .determine_suspend_status(
                Some(fpower::ChargeSource::Usb),
                Some(fpower::ChargeStatus::Charging),
                sag.clone(),
            )
            .await;
        {
            let charge_wake_lease = battery_manager.charge_wake_lease.borrow();
            assert!(!charge_wake_lease.is_none());
            let lease_token =
                charge_wake_lease.as_ref().expect("LeaseToken be present inside the RefCell");
            let token_info = lease_token.basic_info().unwrap();
            let related_id = token_info.related_koid;
            assert_eq!(related_id, tx_id1);
        }

        // Call again, and expect the same lease.
        battery_manager
            .determine_suspend_status(
                Some(fpower::ChargeSource::Usb),
                Some(fpower::ChargeStatus::Charging),
                sag.clone(),
            )
            .await;
        {
            let charge_wake_lease = battery_manager.charge_wake_lease.borrow();
            assert!(!charge_wake_lease.is_none());
            let lease_token =
                charge_wake_lease.as_ref().expect("LeaseToken be present inside the RefCell");
            let token_info = lease_token.basic_info().unwrap();
            let related_id = token_info.related_koid;
            assert_eq!(related_id, tx_id1);
        }

        // Call with passthrough (NotCharging), and expect the lease dropped.
        battery_manager
            .determine_suspend_status(
                Some(fpower::ChargeSource::Usb),
                Some(fpower::ChargeStatus::NotCharging),
                sag.clone(),
            )
            .await;
        {
            let charge_wake_lease = battery_manager.charge_wake_lease.borrow();
            assert!(charge_wake_lease.is_none());
        }

        // Call again with Unknown source but Charging status, and expect a new lease.
        battery_manager
            .determine_suspend_status(
                Some(fpower::ChargeSource::Unknown),
                Some(fpower::ChargeStatus::Charging),
                sag.clone(),
            )
            .await;
        {
            let charge_wake_lease = battery_manager.charge_wake_lease.borrow();
            assert!(!charge_wake_lease.is_none());
            let lease_token =
                charge_wake_lease.as_ref().expect("LeaseToken be present inside the RefCell");
            let token_info = lease_token.basic_info().unwrap();
            let related_id = token_info.related_koid;
            assert_eq!(related_id, tx_id2);
        }
    }

    // This function acts as a fake driver for the new fuchsia.hardware.power.battery protocol.
    // It uses the HangingGet server crate to correctly mimic hanging get behavior and avoid
    // tight busy loops in the test.
    //
    // `get_status_override`, when set, is what `GetStatus` returns; `info` is still what `Watch`
    // reports. That lets a test distinguish a pushed snapshot from an on-demand re-read.
    fn fake_battery_driver_new(
        info: fbattery::Status,
        spec: fbattery::Spec,
        wake_lease: Option<zx::EventPair>,
        get_status_override: Option<fbattery::Status>,
    ) -> fbattery::BatteryProxy {
        struct State {
            status: fbattery::Status,
            wake_lease: std::sync::Mutex<Option<zx::EventPair>>,
        }

        let mut hanging_get = HangingGet::new(
            State { status: info.clone(), wake_lease: std::sync::Mutex::new(wake_lease) },
            |state: &State, responder: fbattery::BatteryWatchResponder| {
                let status = &state.status;
                let lease = state.wake_lease.lock().unwrap().take();
                responder.send(Ok((status, lease))).is_ok()
            },
        );

        let publisher = hanging_get.new_publisher();
        let subscriber = hanging_get.new_subscriber();

        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        fasync::Task::local(async move {
            let _publisher = publisher; // Keep alive
            let get_status_response = get_status_override.unwrap_or_else(|| info.clone());
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    fbattery::BatteryRequest::GetSpec { responder } => {
                        let _ = responder.send(Ok(&spec));
                    }
                    fbattery::BatteryRequest::GetStatus { responder } => {
                        let _ = responder.send(Ok(&get_status_response));
                    }
                    fbattery::BatteryRequest::ConfigureWatch { options, responder } => {
                        let _ = responder.send(Ok(&options));
                    }
                    fbattery::BatteryRequest::Watch { lease, responder, .. } => {
                        let _ = lease;
                        if let Err(e) = subscriber.register(responder) {
                            error!("Failed to register watcher: {:?}", e);
                        }
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();
        proxy
    }

    #[fuchsia::test]
    async fn test_wait_on_new_driver_updates() {
        let (_dir, battery_manager) = create_manager();

        let (_tx, rx) = zx::EventPair::create();
        let info = fbattery::Status {
            level_percent: Some(100.0),
            current_ua: Some(1000000),
            voltage_uv: Some(4200000),
            ..Default::default()
        };

        let spec = fbattery::Spec { design_capacity_uah: Some(5000000), ..Default::default() };

        let proxy = fake_battery_driver_new(info, spec, Some(rx), None);

        let battery_manager = Arc::new(battery_manager);
        let (update_tx, update_rx) = mpsc::unbounded();
        let (_consumer, mut published) =
            spawn_test_consumer(battery_manager.clone(), update_rx, None);
        let bm_clone = battery_manager.clone();

        // We need to run wait_on_new_driver_updates and then check if the info was updated.
        // Since it's a loop, we'll run it in a task.
        let _server_task = fasync::Task::local(async move {
            let _ = bm_clone.wait_on_new_driver_updates(proxy, None, update_tx).await;
        });

        published.next().await.expect("update should be published");

        let battery_info = battery_manager.get_battery_info_copy();
        assert_eq!(battery_info.level_percent, Some(100.0));
        assert_eq!(battery_info.charge_status, Some(fpower::ChargeStatus::Charging));
        assert_eq!(battery_info.status, Some(fpower::BatteryStatus::Ok));
        assert_eq!(battery_info.present_voltage_mv, Some(4200));
        assert_eq!(battery_info.present_charging_current_ua, Some(1000000));

        // Check if battery spec was applied
        assert_eq!(battery_info.battery_spec.unwrap().design_capacity_uah, Some(5000000));
    }

    #[fuchsia::test]
    async fn test_process_battery_info_records_polished_vs_raw() {
        use diagnostics_assertions::assert_data_tree;

        let (_dir, battery_manager) = create_manager();

        // Raw level 3.0 should be polished to 0.0 (by InitialScaler)
        let raw_level = 3.0;
        let info = fpower::BatteryInfo {
            level_percent: Some(raw_level),
            charge_status: Some(fpower::ChargeStatus::Discharging),
            ..Default::default()
        };

        battery_manager.process_battery_info(info.clone(), None).await;

        // Verify recordings in Inspect
        let global_inspector = inspect::component::inspector();
        assert_data_tree!(global_inspector, root: contains {
            power_observability_state_recorders: contains {
                raw_level_percent: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![3u64],
                            }
                        }
                    }
                },
                level_percent: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![0u64],
                            }
                        }
                    }
                },
                charge_status: contains {
                    metadata: contains {
                        name: "charge_status",
                        type: "enum",
                    },
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![2u64],
                            }
                        }
                    }
                },
            }
        });
    }

    #[fuchsia::test]
    async fn test_start_watching_battery_info_driver_disconnected() -> Result<(), Error> {
        use diagnostics_assertions::assert_data_tree;

        let (_dir, battery_manager) = create_manager();

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::BatteryInfoProviderMarker>();

        // Drop the stream immediately to simulate driver disconnect
        drop(stream);

        let (update_tx, _update_rx) = mpsc::unbounded();
        let _ = battery_manager
            .start_watching_battery_info(BatteryInfoSource::ModernService(proxy), None, update_tx)
            .await;

        let global_inspector = inspect::component::inspector();
        assert_data_tree!(global_inspector, root: contains {
            power_observability_state_recorders: contains {
                battery_level_fault: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![0u64, 2u64],
                            }
                        }
                    }
                }
            }
        });
        Ok(())
    }

    // Note: Must be `async fn` because `create_manager()` spawns an `fasync::Task`,
    // which requires `#[fuchsia::test]` to initialize a Fuchsia async executor.
    #[fuchsia::test]
    async fn test_update_watchers_conditionally_updates_simulated_cache() {
        let (_dir, battery_manager) = create_manager();

        // Ensure we are initially in real mode (not simulating)
        assert_eq!(battery_manager.is_simulating(), false);

        // Call update_watchers_conditionally with expect_simulating = true (simulated update)
        // Even though is_simulating() is false, simulated_battery_info should still be updated.
        let mut simulated_info = fpower::BatteryInfo::default();
        simulated_info.level_percent = Some(42.0);
        battery_manager.update_watchers_conditionally(true, simulated_info, None);
        assert_eq!(battery_manager.simulated_battery_info.borrow().level_percent, Some(42.0));
    }

    // Note: Must be `async fn` because `create_manager()` spawns an `fasync::Task`,
    // which requires `#[fuchsia::test]` to initialize a Fuchsia async executor.
    #[fuchsia::test]
    async fn test_update_watchers_conditionally_updates_real_cache() {
        let (_dir, battery_manager) = create_manager();

        // Switch to simulating mode
        battery_manager.update_simulation(true);
        assert_eq!(battery_manager.is_simulating(), true);

        // Call update_watchers_conditionally with expect_simulating = false (real update)
        // Even though is_simulating() is true, cached_battery_info should still be updated.
        let mut real_info = fpower::BatteryInfo::default();
        real_info.level_percent = Some(84.0);
        battery_manager.update_watchers_conditionally(false, real_info, None);
        assert_eq!(battery_manager.cached_battery_info.borrow().level_percent, Some(84.0));
    }

    fn fake_charger_driver(
        status: fcharger::Status,
        spec: fcharger::Spec,
    ) -> fcharger::ChargerProxy {
        struct State {
            status: fcharger::Status,
        }

        let mut hanging_get = HangingGet::new(
            State { status: status.clone() },
            |state: &State, responder: fcharger::ChargerWatchResponder| {
                responder.send(Ok((&state.status, None))).is_ok()
            },
        );

        let publisher = hanging_get.new_publisher();
        let subscriber = hanging_get.new_subscriber();

        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        fasync::Task::local(async move {
            let _publisher = publisher; // Keep alive
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    fcharger::ChargerRequest::GetSpec { responder } => {
                        let _ = responder.send(Ok(&spec));
                    }
                    fcharger::ChargerRequest::GetStatus { responder } => {
                        let _ = responder.send(Ok(&status));
                    }
                    fcharger::ChargerRequest::ConfigureWatch { options, responder } => {
                        let _ = responder.send(Ok(&options));
                    }
                    fcharger::ChargerRequest::Watch { responder, .. } => {
                        if let Err(e) = subscriber.register(responder) {
                            error!("Failed to register charger watcher: {:?}", e);
                        }
                    }
                    _ => panic!("Unexpected charger request"),
                }
            }
        })
        .detach();
        proxy
    }

    #[fuchsia::test]
    async fn test_wait_on_charger_driver_updates() {
        let (_dir, battery_manager) = create_manager();

        let status = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            input_voltage_uv: Some(5000000),
            input_current_ua: Some(1500000),
            ..Default::default()
        };
        let spec = fcharger::Spec {
            max_charge_current_ua: Some(3000000),
            max_charge_voltage_uv: Some(4400000),
            model: Some("MAX77779".to_string()),
            ..Default::default()
        };

        let proxy = fake_charger_driver(status, spec);

        let battery_manager = Arc::new(battery_manager);
        let (update_tx, update_rx) = mpsc::unbounded();
        let (_consumer, mut published) =
            spawn_test_consumer(battery_manager.clone(), update_rx, None);
        let bm_clone = battery_manager.clone();

        let _server_task = fasync::Task::local(async move {
            let _ = bm_clone.wait_on_charger_driver_updates(proxy, None, update_tx).await;
        });

        published.next().await.expect("update should be published");

        let battery_info = battery_manager.get_battery_info_copy();
        assert_eq!(battery_info.charge_source, Some(fpower::ChargeSource::Usb));
        assert_eq!(battery_info.charge_status, Some(fpower::ChargeStatus::Charging));
    }

    #[fuchsia::test]
    async fn test_charger_change_refreshes_battery_status() {
        let (_dir, battery_manager) = create_manager();

        let initial_battery_info = fbattery::Status {
            level_percent: Some(50.0),
            present: Some(true),
            voltage_uv: Some(3800_000),
            current_ua: Some(-200_000),
            ..Default::default()
        };
        // `GetStatus` reports charging current, so a successful re-read is distinguishable from
        // the initial `Watch` snapshot.
        let refreshed_battery_info =
            fbattery::Status { current_ua: Some(1_000_000), ..initial_battery_info.clone() };
        let battery_spec =
            fbattery::Spec { design_capacity_uah: Some(5000000), ..Default::default() };
        let battery_proxy = fake_battery_driver_new(
            initial_battery_info,
            battery_spec,
            None,
            Some(refreshed_battery_info),
        );

        let battery_manager = Arc::new(battery_manager);
        let (update_tx, update_rx) = mpsc::unbounded();
        let (_consumer, mut published) =
            spawn_test_consumer(battery_manager.clone(), update_rx, None);

        let bm_for_battery = battery_manager.clone();
        let battery_update_tx = update_tx.clone();
        let _battery_task = fasync::Task::local(async move {
            let _ = bm_for_battery
                .wait_on_new_driver_updates(battery_proxy, None, battery_update_tx)
                .await;
        });

        published.next().await.expect("battery update should be published");

        let charger_status = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            ..Default::default()
        };
        let charger_spec = fcharger::Spec::default();
        let charger_proxy = fake_charger_driver(charger_status, charger_spec);

        let bm_for_charger = battery_manager.clone();
        let _charger_task = fasync::Task::local(async move {
            let _ =
                bm_for_charger.wait_on_charger_driver_updates(charger_proxy, None, update_tx).await;
        });

        published.next().await.expect("charger update should be published");

        let battery_info = battery_manager.get_battery_info_copy();
        assert_eq!(battery_info.charge_source, Some(fpower::ChargeSource::Usb));
        assert_eq!(battery_info.charge_status, Some(fpower::ChargeStatus::Charging));
        assert_eq!(battery_info.level_percent, Some(49.0));
        assert_eq!(battery_info.present_charging_current_ua, Some(1_000_000));

        // Governors are served the re-read battery status too.
        let governor_battery = connect_governor_battery(&battery_manager);
        let status = governor_battery.get_status().await.unwrap().unwrap();
        assert_eq!(status.current_ua, Some(1_000_000));
    }

    /// Verifies that a charger update that leaves the supply unchanged but completes a governor's
    /// `Charger.Watch` re-reads the battery before it is published, so that the governor's
    /// `Battery.GetStatus` then returns the re-read status.
    #[fuchsia::test]
    async fn test_charger_update_for_governor_refreshes_battery_first() {
        let (_dir, battery_manager) = create_manager();
        let cache = battery_manager.cache();

        // A battery driver whose `GetStatus` reports a higher current on every read, so each read
        // is distinguishable.
        let (battery_proxy, mut battery_stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        let reads = Rc::new(std::cell::Cell::new(0));
        let read_current_ua = |read: i32| read * 100_000;
        let driver_reads = reads.clone();
        let _battery_driver = fasync::Task::local(async move {
            while let Ok(Some(request)) = battery_stream.try_next().await {
                if let fbattery::BatteryRequest::GetStatus { responder } = request {
                    driver_reads.set(driver_reads.get() + 1);
                    let status = fbattery::Status {
                        current_ua: Some(read_current_ua(driver_reads.get())),
                        ..Default::default()
                    };
                    let _ = responder.send(Ok(&status));
                }
            }
        });
        cache.battery_connected(None, Subscription::from_driver(None, None, None), battery_proxy);
        let charger_subscription = Subscription::from_driver(
            Some(fcharger::Status {
                online: Some(true),
                charge_phase: Some(fcharger::ChargePhase::None),
                ..Default::default()
            }),
            None,
            None,
        );
        let (charger_proxy, _) = fidl::endpoints::create_proxy::<fcharger::ChargerMarker>();
        cache.charger_connected(None, charger_subscription, charger_proxy);

        let charger_status = |charge_phase| fcharger::Status {
            online: Some(true),
            charge_phase: Some(charge_phase),
            ..Default::default()
        };
        battery_manager
            .apply_update(
                DriverUpdate::ChargerStatus {
                    status: charger_status(fcharger::ChargePhase::Fast),
                    wake_lease: None,
                },
                None,
            )
            .await;

        // A governor watching every charger field has seen the current charger status.
        let governor_charger = connect_governor_charger(&battery_manager);
        let _ = governor_charger.watch(None).await.unwrap().unwrap();
        let governor_battery = connect_governor_battery(&battery_manager);
        let reads_before = reads.get();

        // Only the charge phase changes, which completes the governor's `Watch`.
        let watch = governor_charger.watch(None);
        battery_manager
            .apply_update(
                DriverUpdate::ChargerStatus {
                    status: charger_status(fcharger::ChargePhase::Taper),
                    wake_lease: None,
                },
                None,
            )
            .await;
        let (status, _) = watch.await.unwrap().unwrap();
        assert_eq!(status.charge_phase, Some(fcharger::ChargePhase::Taper));
        assert_eq!(reads.get(), reads_before + 1);
        assert_eq!(
            governor_battery.get_status().await.unwrap().unwrap().current_ua,
            Some(read_current_ua(reads.get()))
        );
    }

    /// Verifies that the battery is re-read before a charger update that changes the supply or
    /// would complete a governor's `Watch`, but not before a charge phase change no governor
    /// watches.
    #[fuchsia::test]
    async fn test_charger_update_needs_battery() {
        let (_dir, battery_manager) = create_manager();
        let subscription = Subscription::from_driver(
            Some(fcharger::Status {
                online: Some(true),
                charge_phase: Some(fcharger::ChargePhase::None),
                ..Default::default()
            }),
            None,
            None,
        );
        let (charger_proxy, _) = fidl::endpoints::create_proxy::<fcharger::ChargerMarker>();
        battery_manager.cache.charger_connected(None, subscription, charger_proxy);
        let charger_status = |online, charge_phase| fcharger::Status {
            online: Some(online),
            charge_phase: Some(charge_phase),
            ..Default::default()
        };
        let fast = charger_status(true, fcharger::ChargePhase::Fast);
        let taper = charger_status(true, fcharger::ChargePhase::Taper);
        let offline = charger_status(false, fcharger::ChargePhase::None);

        assert!(battery_manager.charger_update_needs_battery(&fast));
        battery_manager.cache.publish_charger(&fast, None);
        assert!(!battery_manager.charger_update_needs_battery(&taper));
        assert!(battery_manager.charger_update_needs_battery(&offline));

        // The charge phase completes the `Watch` of a governor watching every field.
        let charger = connect_governor_charger(&battery_manager);
        let _ = charger.watch(None).await.unwrap().unwrap();
        assert!(battery_manager.charger_update_needs_battery(&taper));

        // A governor watching only `online` is not.
        let options = fcharger::WatchOptions {
            interest: Some(fcharger::Status { online: Some(true), ..Default::default() }),
            ..Default::default()
        };
        let _ = charger.configure_watch(&options).await.unwrap().unwrap();
        assert!(!battery_manager.charger_update_needs_battery(&taper));
        assert!(battery_manager.charger_update_needs_battery(&offline));
    }

    const GOVERNOR: &str = "governor";

    /// Connects to `battery_manager`'s cache as a governor's `Battery` client would.
    fn connect_governor_battery(battery_manager: &BatteryManager) -> fbattery::BatteryProxy {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        let cache = battery_manager.cache();
        fasync::Task::local(async move {
            let _ = cache.serve_battery(GOVERNOR, stream).await;
        })
        .detach();
        proxy
    }

    /// Connects to `battery_manager`'s cache as a governor's `Charger` client would.
    fn connect_governor_charger(battery_manager: &BatteryManager) -> fcharger::ChargerProxy {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        let cache = battery_manager.cache();
        fasync::Task::local(async move {
            let _ = cache.serve_charger(GOVERNOR, stream).await;
        })
        .detach();
        proxy
    }

    /// Connects a battery driver that is never read, so that `battery_manager` caches the battery
    /// statuses it is sent.
    fn connect_unread_battery_driver(battery_manager: &BatteryManager) {
        let (proxy, _) = fidl::endpoints::create_proxy::<fbattery::BatteryMarker>();
        battery_manager.cache.battery_connected(
            None,
            Subscription::from_driver(None, None, None),
            proxy,
        );
    }

    /// Verifies that `BatteryManager::apply_update` publishes battery driver statuses and
    /// disconnections to `Cache` so governors can read them.
    #[fuchsia::test]
    async fn test_driver_updates_feed_governor_telemetry() {
        const LEVEL_PERCENT: f32 = 50.0;
        let (_dir, battery_manager) = create_manager();
        let governor_battery = connect_governor_battery(&battery_manager);
        assert_matches!(governor_battery.get_status().await, Ok(Err(fbattery::Error::Io)));

        connect_unread_battery_driver(&battery_manager);
        let status = fbattery::Status { level_percent: Some(LEVEL_PERCENT), ..Default::default() };
        assert!(
            battery_manager
                .apply_update(
                    DriverUpdate::BatteryStatus { status: status.clone(), wake_lease: None },
                    None
                )
                .await
        );
        assert_eq!(governor_battery.get_status().await.unwrap().unwrap(), status);

        assert!(battery_manager.apply_update(DriverUpdate::BatteryGone, None).await);
        assert_matches!(governor_battery.get_status().await, Ok(Err(fbattery::Error::Io)));
    }

    #[fuchsia::test]
    async fn test_battery_gone_publishes_not_available() {
        let (_dir, battery_manager) = create_manager();
        connect_unread_battery_driver(&battery_manager);
        let status = fbattery::Status {
            level_percent: Some(50.0),
            current_ua: Some(1_000_000),
            ..Default::default()
        };
        assert!(
            battery_manager
                .apply_update(DriverUpdate::BatteryStatus { status, wake_lease: None }, None)
                .await
        );
        assert_eq!(battery_manager.get_battery_info_copy().status, Some(fpower::BatteryStatus::Ok));

        assert!(battery_manager.apply_update(DriverUpdate::BatteryGone, None).await);
        let info = battery_manager.get_battery_info_copy();
        assert_eq!(info.status, Some(fpower::BatteryStatus::NotAvailable));
        assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Unknown));
        assert_eq!(info.level_percent, None);
    }

    #[fuchsia::test]
    fn test_driver_telemetry_conversion_empty() {
        let telemetry =
            CachedStatus { battery: None, battery_spec: None, charger: None, charger_spec: None };
        let info: fpower::BatteryInfo = telemetry.into();
        assert_eq!(info.status, Some(fpower::BatteryStatus::NotAvailable));
        assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Unknown));
        assert_eq!(info.charge_source, Some(fpower::ChargeSource::Unknown));
    }

    #[fuchsia::test]
    fn test_driver_telemetry_conversion_battery_and_charger_full() {
        let battery = fbattery::Status {
            level_percent: Some(100.0),
            present: Some(true),
            voltage_uv: Some(4200000),
            current_ua: Some(100000),
            ..Default::default()
        };
        let charger = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Done),
            ..Default::default()
        };
        let telemetry = CachedStatus {
            battery: Some(&battery),
            battery_spec: None,
            charger: Some(&charger),
            charger_spec: None,
        };
        let info: fpower::BatteryInfo = telemetry.into();
        assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
        assert_eq!(info.charge_source, Some(fpower::ChargeSource::Usb));
        assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Full));
    }

    #[fuchsia::test]
    fn test_driver_telemetry_conversion_charger_done_below_full_level() {
        // The charger terminated before the pack was full (e.g. its float voltage is below the
        // pack's full-charge voltage), so this must not be reported as `Full`.
        let battery = fbattery::Status {
            level_percent: Some(88.5),
            present: Some(true),
            voltage_uv: Some(4318000),
            current_ua: Some(781),
            ..Default::default()
        };
        let charger = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Done),
            ..Default::default()
        };
        let telemetry = CachedStatus {
            battery: Some(&battery),
            battery_spec: None,
            charger: Some(&charger),
            charger_spec: None,
        };
        let info: fpower::BatteryInfo = telemetry.into();
        assert_eq!(info.charge_source, Some(fpower::ChargeSource::Usb));
        assert_eq!(info.charge_status, Some(fpower::ChargeStatus::NotCharging));
    }

    #[fuchsia::test]
    fn test_driver_telemetry_conversion_charger_offline() {
        let battery = fbattery::Status {
            level_percent: Some(80.0),
            present: Some(true),
            voltage_uv: Some(3800000),
            current_ua: Some(-500000),
            ..Default::default()
        };
        let charger = fcharger::Status {
            online: Some(false),
            operating_mode: Some(fcharger::OperatingMode::Discharging),
            charge_phase: Some(fcharger::ChargePhase::None),
            source_type: None,
            ..Default::default()
        };
        let telemetry = CachedStatus {
            battery: Some(&battery),
            battery_spec: None,
            charger: Some(&charger),
            charger_spec: None,
        };
        let info: fpower::BatteryInfo = telemetry.into();
        assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
        assert_eq!(info.charge_source, Some(fpower::ChargeSource::None));
        assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Discharging));
    }

    #[fuchsia::test]
    fn test_resolve_charger_state_modes() {
        // Offline -> Discharging, None
        let offline = fcharger::Status { online: Some(false), ..Default::default() };
        assert_eq!(
            resolve_charger_state(&offline, None),
            (fpower::ChargeSource::None, fpower::ChargeStatus::Discharging)
        );

        // Discharging mode while online -> Discharging, None
        let discharging = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Discharging),
            source_type: Some(fcharger::SourceType::Usb),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&discharging, None),
            (fpower::ChargeSource::None, fpower::ChargeStatus::Discharging)
        );

        // OTG while online -> Discharging, None
        let otg = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Otg),
            source_type: Some(fcharger::SourceType::Usb),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&otg, None),
            (fpower::ChargeSource::None, fpower::ChargeStatus::Discharging)
        );

        // Passthrough while online with AC adapter -> NotCharging, AcAdapter
        let passthrough_ac = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Passthrough),
            source_type: Some(fcharger::SourceType::Ac),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&passthrough_ac, None),
            (fpower::ChargeSource::AcAdapter, fpower::ChargeStatus::NotCharging)
        );

        // Charging with USB -> Charging, Usb
        let charging_usb = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            source_type: Some(fcharger::SourceType::Usb),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&charging_usb, None),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::Charging)
        );

        // Still charging at a full level -> Charging, Usb (only termination reports Full)
        assert_eq!(
            resolve_charger_state(&charging_usb, Some(100.0)),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::Charging)
        );

        // Terminated at the full level -> Full, Usb
        let done_usb = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Done),
            source_type: Some(fcharger::SourceType::Usb),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&done_usb, Some(FULL_CHARGE_MIN_LEVEL_PERCENT)),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::Full)
        );

        // Terminated with no level to compare against -> Full, Usb
        assert_eq!(
            resolve_charger_state(&done_usb, None),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::Full)
        );

        // Terminated below the full level -> NotCharging, Usb
        assert_eq!(
            resolve_charger_state(&done_usb, Some(88.5)),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::NotCharging)
        );

        // Terminated but unplugged -> Discharging, None (Full requires external power)
        let done_offline = fcharger::Status { online: Some(false), ..done_usb.clone() };
        assert_eq!(
            resolve_charger_state(&done_offline, Some(100.0)),
            (fpower::ChargeSource::None, fpower::ChargeStatus::Discharging)
        );

        // Charging with unknown/None source -> Charging, Unknown
        let charging_unknown = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            source_type: None,
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&charging_unknown, None),
            (fpower::ChargeSource::Unknown, fpower::ChargeStatus::Charging)
        );

        // Online with ChargePhase::None (e.g. charger fault or suspended) -> NotCharging, Usb
        let online_phase_none = fcharger::Status {
            online: Some(true),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::None),
            source_type: Some(fcharger::SourceType::Usb),
            ..Default::default()
        };
        assert_eq!(
            resolve_charger_state(&online_phase_none, None),
            (fpower::ChargeSource::Usb, fpower::ChargeStatus::NotCharging)
        );
    }

    #[fuchsia::test]
    async fn test_battery_manager_inspect_diagnostics_end_to_end() {
        use diagnostics_assertions::assert_data_tree;

        let (_dir, battery_manager) = create_manager();

        let battery_spec =
            fbattery::Spec { design_capacity_uah: Some(5_000_000), ..Default::default() };
        let (battery_proxy, _battery_stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        assert!(
            !battery_manager
                .apply_update(
                    DriverUpdate::BatteryConnected {
                        spec: Some(battery_spec),
                        subscription: Subscription::from_driver(None, None, None),
                        proxy: battery_proxy,
                    },
                    None,
                )
                .await
        );

        // Charger reports Done while battery is at 88% (< 95% threshold) -> premature charge done
        let charger_status = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Done),
            ..Default::default()
        };
        let (charger_proxy, _) = fidl::endpoints::create_proxy::<fcharger::ChargerMarker>();
        battery_manager.cache.charger_connected(
            None,
            Subscription::from_driver(None, None, None),
            charger_proxy,
        );
        battery_manager.cache.publish_charger(&charger_status, None);

        let battery_status = fbattery::Status {
            present: Some(true),
            level_percent: Some(88.0),
            voltage_uv: Some(4_300_000),
            current_ua: Some(500),
            full_charge_capacity_uah: Some(4_500_000),
            temp_celsius: Some(30.0),
            health: Some(fbattery::HealthStatus::Good),
            ..Default::default()
        };
        assert!(
            battery_manager
                .apply_update(
                    DriverUpdate::BatteryStatus { status: battery_status, wake_lease: None },
                    None,
                )
                .await
        );

        assert!(battery_manager.apply_update(DriverUpdate::ChargerGone, None).await);
        assert!(battery_manager.apply_update(DriverUpdate::BatteryGone, None).await);

        let global_inspector = inspect::component::inspector();
        assert_data_tree!(global_inspector, root: contains {
            diagnostics: contains {
                disconnects: {
                    battery_disconnect_count: 1u64,
                    charger_disconnect_count: 1u64,
                },
                state: contains {
                    temperature_mc: 30_000i64,
                    full_capacity_uah: 4_500_000i64,
                    design_capacity_uah: 5_000_000i64,
                    state_of_health_percent: 90.0f64,
                },
                anomalies: contains {
                    premature_charge_done_count: 1u64,
                    last_anomaly_reason: "premature_charge_done_below_95_percent",
                },
            },
            power_observability_state_recorders: contains {
                temperature_mc: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![30_000i64],
                            }
                        }
                    }
                },
                full_capacity: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![4_500_000i64],
                            }
                        }
                    }
                },
                state_of_health_percent: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![90.0f64],
                            }
                        }
                    }
                },
            }
        });
    }
}
