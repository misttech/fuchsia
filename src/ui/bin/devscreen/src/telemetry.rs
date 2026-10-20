// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Best-effort data collection for the dashboard. Every source is optional:
//! a missing capability or a failing call turns into `None` on the snapshot,
//! never into a failed start.

use crate::DevscreenMessage;
use carnelian::{AppSender, MessageTarget, ViewKey, make_message};
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_buildinfo as fbuildinfo;
use fidl_fuchsia_device as fdevice;
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_hardware_cpu_ctrl as fcpu;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_hardware_temperature as ftemperature;
use fidl_fuchsia_hwinfo as fhwinfo;
use fidl_fuchsia_kernel as fkernel;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_interfaces as fnet_interfaces;
use fidl_fuchsia_wlan_device_service as fwlan_service;
use fidl_fuchsia_wlan_policy as fwlan_policy;
use fuchsia_async::{self as fasync, TimeoutExt};
use fuchsia_component::client::{Service, connect_to_protocol};
use futures::{FutureExt, TryStreamExt};
use log::{info, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

/// How long to wait on any single FIDL call before giving up on it for this
/// round. Services come up at different times during boot; we simply retry on
/// the next round.
const CALL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(3);

/// Interval between dashboard refreshes. The CPU load sample takes
/// `CPU_LOAD_WINDOW` on top of this.
const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
const CPU_LOAD_WINDOW: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(1000);

/// Rounds between retries for sources that were unavailable.
const RETRY_EVERY_N_ROUNDS: u64 = 5;

/// Smoothing factor for the battery current. The fuel gauge is noisy at the
/// mA level; three-ish samples is enough to make the reading legible.
const CURRENT_EMA_ALPHA: f32 = 1.0 / 3.0;

/// Pause between WLAN scans, and the longest a single scan may take.
const WLAN_SCAN_INTERVAL: Duration = Duration::from_secs(30);
const WLAN_SCAN_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(25);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Battery {
    pub present: bool,
    pub level_percent: Option<f32>,
    pub voltage_uv: Option<u32>,
    pub current_ua: Option<i32>,
    /// Exponential moving average of `current_ua` across rounds.
    pub current_avg_ua: Option<f32>,
    pub temp_celsius: Option<f32>,
    pub health: Option<fbattery::HealthStatus>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Charger {
    pub online: Option<bool>,
    pub input_voltage_uv: Option<u32>,
    pub input_current_ua: Option<i32>,
    pub float_voltage_uv: Option<u32>,
    pub source_type: Option<fcharger::SourceType>,
    pub charge_phase: Option<fcharger::ChargePhase>,
    pub operating_mode: Option<fcharger::OperatingMode>,
    pub health: Option<fcharger::Health>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastBoot {
    pub graceful: Option<bool>,
    pub reason: Option<ffeedback::RebootReason>,
    /// Uptime of the previous boot.
    pub uptime: Option<zx::MonotonicDuration>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ScanState {
    /// No policy controller, or scanning is not possible on this build.
    #[default]
    Unavailable,
    /// First scan in flight.
    Scanning,
    Done {
        networks: usize,
        at: zx::MonotonicInstant,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wlan {
    pub phys: usize,
    pub ifaces: usize,
    /// Station MAC of the first client interface.
    pub mac: Option<String>,
    pub scan: ScanState,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub device_name: Option<String>,
    pub serial: Option<String>,
    pub product: Option<String>,
    pub board: Option<String>,
    pub version: Option<String>,
    /// `(interface name, address)` for every address on every online,
    /// non-loopback interface. IPv4 first.
    pub addresses: Vec<(String, String)>,
    /// `None` until a battery service instance has answered.
    pub battery: Option<Battery>,
    /// `None` until a charger service instance has answered.
    pub charger: Option<Charger>,
    /// `None` until the feedback service has answered (asked once).
    pub last_boot: Option<LastBoot>,
    /// `None` until the WLAN device monitor has answered.
    pub wlan: Option<Wlan>,
    /// `(domain id, current frequency in Hz)` per CPU frequency domain,
    /// ordered by domain id. Empty when no cpu-ctrl driver answered.
    pub cpu_freq_hz: Vec<(u32, i64)>,
    pub mem_total_bytes: Option<u64>,
    pub mem_free_bytes: Option<u64>,
    /// Per-CPU load percentages over the last sample window.
    pub cpu_load: Option<Vec<f32>>,
    /// `(sensor name, degrees C)`.
    pub temps: Vec<(String, f32)>,
    pub uptime: zx::MonotonicDuration,
}

async fn with_timeout<T>(f: impl Future<Output = T>) -> Option<T> {
    f.map(Some).on_timeout(fasync::MonotonicInstant::after(CALL_TIMEOUT), || None).await
}

async fn fetch_build_info(snapshot: &mut Snapshot) {
    let Ok(proxy) = connect_to_protocol::<fbuildinfo::ProviderMarker>() else { return };
    match with_timeout(proxy.get_build_info()).await {
        Some(Ok(info)) => {
            snapshot.product = info.product_config;
            snapshot.board = info.board_config;
            snapshot.version = info.version;
        }
        other => warn!("build info unavailable: {other:?}"),
    }
}

async fn fetch_device_name(snapshot: &mut Snapshot) {
    let Ok(proxy) = connect_to_protocol::<fdevice::NameProviderMarker>() else { return };
    match with_timeout(proxy.get_device_name()).await {
        Some(Ok(Ok(name))) => snapshot.device_name = Some(name),
        other => warn!("device name unavailable: {other:?}"),
    }
}

async fn fetch_serial(snapshot: &mut Snapshot) {
    let Ok(proxy) = connect_to_protocol::<fhwinfo::DeviceMarker>() else { return };
    match with_timeout(proxy.get_info()).await {
        Some(Ok(info)) => snapshot.serial = info.serial_number,
        other => warn!("hwinfo unavailable: {other:?}"),
    }
}

async fn fetch_last_boot() -> Option<LastBoot> {
    let proxy = connect_to_protocol::<ffeedback::LastRebootInfoProviderMarker>().ok()?;
    match with_timeout(proxy.get()).await {
        Some(Ok(last)) => Some(LastBoot {
            graceful: last.graceful,
            reason: last.reason,
            uptime: last.uptime.map(zx::MonotonicDuration::from_nanos),
        }),
        other => {
            warn!("last reboot info unavailable: {other:?}");
            None
        }
    }
}

fn format_ip(subnet: &fnet::Subnet) -> (bool, String) {
    match subnet.addr {
        fnet::IpAddress::Ipv4(a) => (true, std::net::Ipv4Addr::from(a.addr).to_string()),
        fnet::IpAddress::Ipv6(a) => (false, std::net::Ipv6Addr::from(a.addr).to_string()),
    }
}

async fn fetch_addresses() -> Option<Vec<(String, String)>> {
    let state = connect_to_protocol::<fnet_interfaces::StateMarker>().ok()?;
    let (watcher, server) = fidl::endpoints::create_proxy::<fnet_interfaces::WatcherMarker>();
    state.get_watcher(&fnet_interfaces::WatcherOptions::default(), server).ok()?;
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    loop {
        match with_timeout(watcher.watch()).await? {
            Ok(fnet_interfaces::Event::Existing(props)) => {
                if props.online != Some(true) {
                    continue;
                }
                let name = props.name.clone().unwrap_or_default();
                if name == "lo" {
                    continue;
                }
                for address in props.addresses.unwrap_or_default() {
                    if let Some(subnet) = address.addr.as_ref() {
                        let (is_v4, text) = format_ip(subnet);
                        if is_v4 { &mut v4 } else { &mut v6 }.push((name.clone(), text));
                    }
                }
            }
            Ok(fnet_interfaces::Event::Idle(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                warn!("interface watcher failed: {e:?}");
                return None;
            }
        }
    }
    v4.extend(v6);
    Some(v4)
}

// Battery / charger ---------------------------------------------------------

async fn connect_battery() -> Option<fbattery::BatteryProxy> {
    let service = Service::open(fbattery::ServiceMarker).ok()?;
    let instance = match with_timeout(service.watch_for_any()).await? {
        Ok(instance) => instance,
        Err(e) => {
            warn!("battery service unavailable: {e:?}");
            return None;
        }
    };
    match instance.connect_to_battery() {
        Ok(proxy) => {
            info!("connected to fuchsia.hardware.power.battery");
            Some(proxy)
        }
        Err(e) => {
            warn!("battery instance connect failed: {e:?}");
            None
        }
    }
}

async fn read_battery(
    proxy: &fbattery::BatteryProxy,
    previous: Option<&Battery>,
) -> Option<Battery> {
    match with_timeout(proxy.get_status()).await? {
        Ok(Ok(status)) => {
            let current_avg_ua = match (status.current_ua, previous.and_then(|p| p.current_avg_ua))
            {
                (Some(now), Some(avg)) => Some(avg + CURRENT_EMA_ALPHA * (now as f32 - avg)),
                (Some(now), None) => Some(now as f32),
                (None, _) => None,
            };
            Some(Battery {
                present: status.present.unwrap_or(false),
                level_percent: status.level_percent,
                voltage_uv: status.voltage_uv,
                current_ua: status.current_ua,
                current_avg_ua,
                temp_celsius: status.temp_celsius,
                health: status.health,
            })
        }
        other => {
            warn!("battery GetStatus failed: {other:?}");
            None
        }
    }
}

async fn connect_charger() -> Option<fcharger::ChargerProxy> {
    let service = Service::open(fcharger::ServiceMarker).ok()?;
    let instance = match with_timeout(service.watch_for_any()).await? {
        Ok(instance) => instance,
        Err(e) => {
            warn!("charger service unavailable: {e:?}");
            return None;
        }
    };
    match instance.connect_to_charger() {
        Ok(proxy) => {
            info!("connected to fuchsia.hardware.power.charger");
            Some(proxy)
        }
        Err(e) => {
            warn!("charger instance connect failed: {e:?}");
            None
        }
    }
}

async fn read_charger(proxy: &fcharger::ChargerProxy) -> Option<Charger> {
    match with_timeout(proxy.get_status()).await? {
        Ok(Ok(status)) => Some(Charger {
            online: status.online,
            input_voltage_uv: status.input_voltage_uv,
            input_current_ua: status.input_current_ua,
            float_voltage_uv: status.float_voltage_uv,
            source_type: status.source_type,
            charge_phase: status.charge_phase,
            operating_mode: status.operating_mode,
            health: status.health,
        }),
        other => {
            warn!("charger GetStatus failed: {other:?}");
            None
        }
    }
}

// Thermal -------------------------------------------------------------------

struct Sensor {
    /// Service instance name; a restarted driver publishes a new one.
    instance: String,
    name: String,
    proxy: ftemperature::DeviceProxy,
    /// Rounds to skip before asking this sensor again. Grows exponentially
    /// while reads fail (e.g. the sensor's IP block is powered down), so a
    /// permanently unavailable sensor is only touched about once a minute.
    skip_rounds: u32,
    backoff_rounds: u32,
}

/// Longest a failing sensor goes between read attempts.
const SENSOR_MAX_BACKOFF_ROUNDS: u32 = 60;

/// Adds any temperature service instances that are not already in `sensors`.
async fn connect_temperature_sensors(sensors: &mut Vec<Sensor>) {
    let Ok(service) = Service::open(ftemperature::ServiceMarker) else { return };
    let Some(Ok(instances)) = with_timeout(service.enumerate()).await else { return };
    let before = sensors.len();
    for instance in instances {
        if sensors.iter().any(|s| s.instance == instance.instance_name()) {
            continue;
        }
        let Ok(proxy) = instance.connect_to_device() else { continue };
        let name = match with_timeout(proxy.get_sensor_name()).await {
            Some(Ok(name)) => name,
            _ => format!("sensor{}", sensors.len()),
        };
        sensors.push(Sensor {
            instance: instance.instance_name().to_string(),
            name,
            proxy,
            skip_rounds: 0,
            backoff_rounds: 0,
        });
    }
    if sensors.len() != before {
        info!("temperature sensors: {} (+{})", sensors.len(), sensors.len() - before);
    }
}

async fn read_temperatures(sensors: &mut [Sensor]) -> Vec<(String, f32)> {
    let mut out = Vec::new();
    for sensor in sensors.iter_mut() {
        if sensor.skip_rounds > 0 {
            sensor.skip_rounds -= 1;
            continue;
        }
        match with_timeout(sensor.proxy.get_temperature_celsius()).await {
            Some(Ok((status, temp))) if status == zx::sys::ZX_OK => {
                sensor.backoff_rounds = 0;
                out.push((sensor.name.clone(), temp));
            }
            _ => {
                sensor.backoff_rounds =
                    (sensor.backoff_rounds * 2).clamp(1, SENSOR_MAX_BACKOFF_ROUNDS);
                sensor.skip_rounds = sensor.backoff_rounds;
            }
        }
    }
    out
}

// CPU -----------------------------------------------------------------------

struct CpuDomain {
    /// Service instance name; a restarted driver publishes a new one.
    instance: String,
    id: u32,
    proxy: fcpu::DeviceProxy,
    /// Operating point -> frequency, filled lazily; the table is static.
    opp_freq_hz: HashMap<u32, i64>,
}

/// Adds any cpu-ctrl service instances that are not already in `domains`.
async fn connect_cpu_domains(domains: &mut Vec<CpuDomain>) {
    let Ok(service) = Service::open(fcpu::ServiceMarker) else { return };
    let Some(Ok(instances)) = with_timeout(service.enumerate()).await else { return };
    let before = domains.len();
    for instance in instances {
        if domains.iter().any(|d| d.instance == instance.instance_name()) {
            continue;
        }
        let Ok(proxy) = instance.connect_to_device() else { continue };
        let Some(Ok(id)) = with_timeout(proxy.get_domain_id()).await else { continue };
        domains.push(CpuDomain {
            instance: instance.instance_name().to_string(),
            id,
            proxy,
            opp_freq_hz: HashMap::new(),
        });
    }
    if domains.len() != before {
        domains.sort_by_key(|d| d.id);
        info!("cpu frequency domains: {} (+{})", domains.len(), domains.len() - before);
    }
}

async fn read_cpu_frequencies(domains: &mut [CpuDomain]) -> Vec<(u32, i64)> {
    let mut out = Vec::new();
    for domain in domains.iter_mut() {
        let Some(Ok(opp)) = with_timeout(domain.proxy.get_current_operating_point()).await else {
            continue;
        };
        if let Some(hz) = domain.opp_freq_hz.get(&opp) {
            out.push((domain.id, *hz));
            continue;
        }
        if let Some(Ok(Ok(info))) = with_timeout(domain.proxy.get_operating_point_info(opp)).await {
            domain.opp_freq_hz.insert(opp, info.frequency_hz);
            out.push((domain.id, info.frequency_hz));
        }
    }
    out
}

async fn read_kernel_stats(stats: &fkernel::StatsProxy, snapshot: &mut Snapshot) {
    if let Some(Ok(mem)) = with_timeout(stats.get_memory_stats()).await {
        snapshot.mem_total_bytes = mem.total_bytes;
        snapshot.mem_free_bytes = mem.free_bytes;
    }
    // This call blocks for the sample window; it doubles as part of the
    // refresh interval.
    let load = stats
        .get_cpu_load(CPU_LOAD_WINDOW.into_nanos())
        .map(Some)
        .on_timeout(fasync::MonotonicInstant::after(CPU_LOAD_WINDOW + CALL_TIMEOUT), || None)
        .await;
    if let Some(Ok(load)) = load {
        snapshot.cpu_load = Some(load);
    }
}

// WLAN ----------------------------------------------------------------------

fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Phy / interface inventory from the device monitor. `None` when the monitor
/// is unavailable.
async fn fetch_wlan_devices() -> Option<(usize, usize, Option<String>)> {
    let monitor = connect_to_protocol::<fwlan_service::DeviceMonitorMarker>().ok()?;
    let phys = match with_timeout(monitor.list_phys()).await? {
        Ok(phys) => phys,
        Err(e) => {
            warn!("wlan ListPhys failed: {e:?}");
            return None;
        }
    };
    let ifaces = with_timeout(monitor.list_ifaces()).await?.ok()?;
    let mut mac = None;
    for iface in &ifaces {
        if let Some(Ok(Ok(info))) = with_timeout(monitor.query_iface(*iface)).await {
            mac = Some(format_mac(&info.sta_addr));
            break;
        }
    }
    Some((phys.len(), ifaces.len(), mac))
}

/// Serves the state-updates channel wlancfg insists on; we ignore the content
/// but must keep acknowledging it or wlancfg drops the controller.
async fn drain_client_updates(stream: fwlan_policy::ClientStateUpdatesRequestStream) {
    let _ = stream
        .try_for_each(|request| async move {
            let fwlan_policy::ClientStateUpdatesRequest::OnClientStateUpdate { responder, .. } =
                request;
            let _ = responder.send();
            Ok(())
        })
        .await;
}

async fn scan_once(controller: &fwlan_policy::ClientControllerProxy) -> Result<usize, String> {
    let (iterator, server) =
        fidl::endpoints::create_proxy::<fwlan_policy::ScanResultIteratorMarker>();
    controller.scan_for_networks(server).map_err(|e| format!("{e:?}"))?;
    let mut networks = 0;
    loop {
        let next = iterator
            .get_next()
            .map(Some)
            .on_timeout(fasync::MonotonicInstant::after(WLAN_SCAN_TIMEOUT), || None)
            .await;
        match next {
            Some(Ok(Ok(results))) if results.is_empty() => return Ok(networks),
            Some(Ok(Ok(results))) => networks += results.len(),
            Some(Ok(Err(code))) => return Err(format!("{code:?}")),
            Some(Err(e)) => return Err(format!("{e:?}")),
            None => return Err(String::from("timeout")),
        }
    }
}

/// Periodically counts visible networks through the WLAN policy layer.
/// Network names never leave this function. Runs until dropped.
async fn wlan_scan_loop(state: Rc<RefCell<ScanState>>, ifaces: Rc<RefCell<usize>>) {
    loop {
        let controller = (|| {
            let provider = connect_to_protocol::<fwlan_policy::ClientProviderMarker>().ok()?;
            let (controller, server) =
                fidl::endpoints::create_proxy::<fwlan_policy::ClientControllerMarker>();
            let (updates_client, updates_server) =
                fidl::endpoints::create_endpoints::<fwlan_policy::ClientStateUpdatesMarker>();
            provider.get_controller(server, updates_client).ok()?;
            Some((controller, updates_server.into_stream()))
        })();
        let Some((controller, updates)) = controller else {
            *state.borrow_mut() = ScanState::Unavailable;
            fasync::Timer::new(WLAN_SCAN_INTERVAL).await;
            continue;
        };
        let _updates_task = fasync::Task::local(drain_client_updates(updates));
        if matches!(*state.borrow(), ScanState::Unavailable) {
            *state.borrow_mut() = ScanState::Scanning;
        }
        let mut started_connections = false;
        loop {
            // Scanning needs a client iface; on builds without a connectivity
            // policy nobody else asks for one.
            if !started_connections && *ifaces.borrow() == 0 {
                match with_timeout(controller.start_client_connections()).await {
                    Some(Ok(status)) => {
                        info!("StartClientConnections: {status:?}");
                        if status == fwlan_policy::RequestStatus::Acknowledged {
                            started_connections = true;
                        }
                    }
                    other => warn!("StartClientConnections failed: {other:?}"),
                }
            }
            if let ScanState::Done { at, .. } = *state.borrow() {
                let remaining = (at + zx::MonotonicDuration::from(WLAN_SCAN_INTERVAL))
                    - zx::MonotonicInstant::get();
                if remaining.into_nanos() > 0 {
                    fasync::Timer::new(remaining).await;
                }
            }
            let result = scan_once(&controller).await;
            *state.borrow_mut() = match result {
                Ok(networks) => ScanState::Done { networks, at: zx::MonotonicInstant::get() },
                Err(e) => {
                    warn!("wlan scan failed: {e}");
                    if controller.is_closed() {
                        break;
                    }
                    ScanState::Failed(e)
                }
            };
            fasync::Timer::new(WLAN_SCAN_INTERVAL).await;
        }
        // Controller went away (e.g. another client took it); reconnect.
        fasync::Timer::new(WLAN_SCAN_INTERVAL).await;
    }
}

// Drivers -------------------------------------------------------------------

/// Latest values read from drivers. Filled by [`driver_loop`] on its own
/// task so that a slow, hung or restarting driver never delays the
/// dashboard refresh in [`run`].
#[derive(Default)]
struct DriverReadings {
    battery: Option<Battery>,
    charger: Option<Charger>,
    temps: Vec<(String, f32)>,
    cpu_freq_hz: Vec<(u32, i64)>,
}

async fn driver_loop(readings: Rc<RefCell<DriverReadings>>) {
    let mut battery: Option<fbattery::BatteryProxy> = None;
    let mut charger: Option<fcharger::ChargerProxy> = None;
    let mut sensors: Vec<Sensor> = Vec::new();
    let mut cpu_domains: Vec<CpuDomain> = Vec::new();
    let mut round: u64 = 0;

    loop {
        // Forget connections whose driver went away (e.g. restarted by a
        // test). They are picked up again below on the next retry round.
        if battery.as_ref().is_some_and(|p| p.is_closed()) {
            battery = None;
        }
        if charger.as_ref().is_some_and(|p| p.is_closed()) {
            charger = None;
        }
        sensors.retain(|s| !s.proxy.is_closed());
        cpu_domains.retain(|d| !d.proxy.is_closed());

        if round % RETRY_EVERY_N_ROUNDS == 0 {
            if battery.is_none() {
                battery = connect_battery().await;
            }
            if charger.is_none() {
                charger = connect_charger().await;
            }
            connect_temperature_sensors(&mut sensors).await;
            connect_cpu_domains(&mut cpu_domains).await;
        }

        let previous_battery = readings.borrow().battery.clone();
        let new_battery = match &battery {
            Some(proxy) => read_battery(proxy, previous_battery.as_ref()).await,
            None => None,
        };
        if battery.is_some() && new_battery.is_none() {
            // Failed read: reconnect on the next retry round.
            battery = None;
        }
        let new_charger = match &charger {
            Some(proxy) => read_charger(proxy).await,
            None => None,
        };
        if charger.is_some() && new_charger.is_none() {
            charger = None;
        }
        let temps = read_temperatures(&mut sensors).await;
        let cpu_freq_hz = read_cpu_frequencies(&mut cpu_domains).await;

        {
            let mut r = readings.borrow_mut();
            // Keep the last good battery/charger reading across a failed
            // read so the display does not flicker to "n/a"; temps and cpu
            // reflect exactly what answered this round.
            if new_battery.is_some() {
                r.battery = new_battery;
            }
            if new_charger.is_some() {
                r.charger = new_charger;
            }
            r.temps = temps;
            r.cpu_freq_hz = cpu_freq_hz;
        }

        round += 1;
        fasync::Timer::new(REFRESH_INTERVAL).await;
    }
}

fn publish(app_sender: &AppSender, view_key: ViewKey, snapshot: &Snapshot) {
    app_sender.queue_message(
        MessageTarget::View(view_key),
        make_message(DevscreenMessage::Snapshot(Box::new(snapshot.clone()))),
    );
}

/// Collects a fresh [`Snapshot`] roughly every two seconds and posts it to
/// the view. Runs until the task is dropped.
pub async fn run(app_sender: AppSender, view_key: ViewKey, mut snapshot: Snapshot) {
    publish(&app_sender, view_key, &snapshot);

    let stats = connect_to_protocol::<fkernel::StatsMarker>().ok();
    let readings = Rc::new(RefCell::new(DriverReadings {
        battery: snapshot.battery.clone(),
        charger: snapshot.charger.clone(),
        ..Default::default()
    }));
    let _drivers = fasync::Task::local(driver_loop(readings.clone()));
    let (initial_state, initial_ifaces) = snapshot
        .wlan
        .as_ref()
        .map(|w| (w.scan.clone(), w.ifaces))
        .unwrap_or((ScanState::Unavailable, 0));
    let scan_state = Rc::new(RefCell::new(initial_state));
    let wlan_ifaces = Rc::new(RefCell::new(initial_ifaces));
    let _scanner = fasync::Task::local(wlan_scan_loop(scan_state.clone(), wlan_ifaces.clone()));
    let mut round: u64 = 0;

    loop {
        let retry_round = round % RETRY_EVERY_N_ROUNDS == 0;

        if snapshot.product.is_none() && retry_round {
            fetch_build_info(&mut snapshot).await;
        }
        if snapshot.device_name.is_none() && retry_round {
            fetch_device_name(&mut snapshot).await;
        }
        if snapshot.serial.is_none() && retry_round {
            fetch_serial(&mut snapshot).await;
        }
        if snapshot.last_boot.is_none() && retry_round {
            snapshot.last_boot = fetch_last_boot().await;
        }
        if retry_round {
            if let Some(addresses) = fetch_addresses().await {
                snapshot.addresses = addresses;
            }
            if let Some((phys, ifaces, mac)) = fetch_wlan_devices().await {
                *wlan_ifaces.borrow_mut() = ifaces;
                snapshot.wlan = Some(Wlan { phys, ifaces, mac, scan: ScanState::Unavailable });
            }
        }
        if let Some(wlan) = snapshot.wlan.as_mut() {
            wlan.scan = scan_state.borrow().clone();
        }
        {
            let r = readings.borrow();
            snapshot.battery = r.battery.clone();
            snapshot.charger = r.charger.clone();
            snapshot.temps = r.temps.clone();
            snapshot.cpu_freq_hz = r.cpu_freq_hz.clone();
        }
        if let Some(stats) = &stats {
            read_kernel_stats(stats, &mut snapshot).await;
        }
        snapshot.uptime =
            zx::MonotonicDuration::from_nanos(zx::MonotonicInstant::get().into_nanos());

        publish(&app_sender, view_key, &snapshot);
        round += 1;
        fasync::Timer::new(REFRESH_INTERVAL).await;
    }
}
