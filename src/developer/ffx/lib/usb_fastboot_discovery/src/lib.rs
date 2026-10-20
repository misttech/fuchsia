// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_async::{Task, TimeoutExt, Timer, unblock};
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
pub use usb_rs::bulk_interface::BulkInterface as Interface;
use usb_rs::enumerate_devices;

// USB fastboot interface IDs
const FASTBOOT_USB_INTERFACE_CLASS: u8 = 0xff;
const FASTBOOT_USB_INTERFACE_SUBCLASS: u8 = 0x42;
const FASTBOOT_USB_INTERFACE_PROTOCOL: u8 = 0x03;

// Vendor ID
const USB_DEV_VENDOR: u16 = 0x18d1;

/// The maximum time allowed for scanning a single USB device's serial number
/// and interface descriptors. If a device does not respond within this time,
/// it is considered unresponsive and skipped to prevent blocking the discovery loop.
const SCAN_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Error, Debug)]
pub enum UsbDiscoveryError {
    #[error("No valid interface found")]
    InterfaceNotFound,

    #[error("USB error: {0}")]
    Usb(#[from] usb_rs::Error),
}

/// Loops continually testing if the usb device with given `serial` is
/// accepting fastboot connections, returning when it does.
///
/// Sleeps for `sleep_interval` between tests
pub async fn wait_for_live(
    serial: &str,
    tester: &mut impl FastbootUsbLiveTester,
    sleep_interval: Duration,
) {
    loop {
        if tester.is_fastboot_usb_live(serial).await {
            return;
        }
        Timer::new(sleep_interval).await;
    }
}

pub trait FastbootUsbLiveTester: Send + 'static {
    /// Checks if the interface with the given serial number is Ready to accept
    /// fastboot commands
    fn is_fastboot_usb_live(&mut self, serial: &str) -> impl Future<Output = bool> + Send;
}

pub struct GetVarFastbootUsbLiveTester;

impl FastbootUsbLiveTester for GetVarFastbootUsbLiveTester {
    async fn is_fastboot_usb_live(&mut self, serial: &str) -> bool {
        open_interface_with_serial(serial).is_ok()
    }
}

pub trait FastbootUsbTester: Send + 'static {
    /// Checks if the interface with the given serial number is in Fastboot
    fn is_fastboot_usb(&mut self, serial: &str) -> impl Future<Output = bool> + Send;
}

/// Checks if the USB Interface for the given serial is live and a fastboot match
/// by inspecting the Interface Information.
///
/// This does not mean that the device is ready to respond to any fastboot commands,
/// only that the USB Device with the given serial number exists and declares itself
/// to be a Fastboot device.
///
/// This calls AsyncInterface::check which does _not_ drain the USB's device's
/// buffer upon connecting
pub struct UnversionedFastbootUsbTester;

impl FastbootUsbTester for UnversionedFastbootUsbTester {
    async fn is_fastboot_usb(&mut self, serial: &str) -> bool {
        !find_target_serial_in_sysfs(serial, Path::new("/sys/bus/usb/devices")).await.is_empty()
    }
}

pub struct FastbootUsbWatcher {
    // Task for the discovery loop
    discovery_task: Option<Task<()>>,
    // Task for the drain loop
    drain_task: Option<Task<()>>,
}

#[derive(Debug, PartialEq)]
pub enum FastbootEvent {
    Discovered(String),
    Lost(String),
}

pub trait FastbootEventHandler: Send + 'static {
    /// Handles an event.
    fn handle_event(&mut self, event: FastbootEvent) -> impl Future<Output = ()> + Send;
}

impl<F> FastbootEventHandler for F
where
    F: FnMut(FastbootEvent) -> () + Send + 'static,
{
    async fn handle_event(&mut self, x: FastbootEvent) {
        self(x)
    }
}

pub trait SerialNumberFinder: Send + 'static {
    fn find_serial_numbers(&mut self) -> impl Future<Output = Vec<String>> + Send;
}

pub struct DefaultSerialFinder {}
impl SerialNumberFinder for DefaultSerialFinder {
    async fn find_serial_numbers(&mut self) -> Vec<String> {
        find_serial_numbers().await
    }
}

/// A SerialNumberFinder that targets a specific known serial number, using direct
/// sysfs inspection to avoid enumerating all USB devices on the host bus.
pub struct TargetSerialFinder<S: AsRef<str> = String> {
    serial: S,
    sysfs_path: PathBuf,
}

impl<S: AsRef<str>> TargetSerialFinder<S> {
    pub fn new(serial: S) -> Self {
        Self { serial, sysfs_path: PathBuf::from("/sys/bus/usb/devices") }
    }
}

impl<S: AsRef<str> + Send + 'static> SerialNumberFinder for TargetSerialFinder<S> {
    async fn find_serial_numbers(&mut self) -> Vec<String> {
        find_target_serial_in_sysfs(self.serial.as_ref(), &self.sysfs_path).await
    }
}

pub fn recommended_watcher<F>(event_handler: F) -> FastbootUsbWatcher
where
    F: FastbootEventHandler,
{
    FastbootUsbWatcher::new(
        event_handler,
        DefaultSerialFinder {},
        UnversionedFastbootUsbTester {},
        // This interval should be longer than libdiscovery::DEFAULT_TIMEOUT_SECS (2 secs)
        // so in the normal case, we don't scan for USB devices more than once.
        Duration::from_secs(3),
    )
}

impl FastbootUsbWatcher {
    pub fn new<F, W, O>(event_handler: F, finder: W, opener: O, interval: Duration) -> Self
    where
        F: FastbootEventHandler,
        W: SerialNumberFinder,
        O: FastbootUsbTester,
    {
        let mut res = Self { discovery_task: None, drain_task: None };

        let (sender, receiver) = async_channel::bounded::<FastbootEvent>(1);

        res.discovery_task.replace(Task::local(discovery_loop(sender, finder, opener, interval)));
        res.drain_task.replace(Task::local(handle_events_loop(receiver, event_handler)));

        res
    }
}

async fn discovery_loop<F, O>(
    events_out: async_channel::Sender<FastbootEvent>,
    mut finder: F,
    mut opener: O,
    discovery_interval: Duration,
) -> ()
where
    F: SerialNumberFinder,
    O: FastbootUsbTester,
{
    let mut serials = BTreeSet::<String>::new();
    loop {
        // Enumerate interfaces
        let new_serials = finder.find_serial_numbers().await;
        let new_serials = BTreeSet::from_iter(new_serials);
        log::trace!("found serials: {:#?}", new_serials);
        // Update Cache
        for serial in &new_serials {
            // Just because the serial is found doesnt mean that the target is ready
            if !opener.is_fastboot_usb(serial.as_str()).await {
                log::trace!(
                    "Skipping adding serial number: {serial} as it is not a Fastboot interface"
                );
                continue;
            }

            log::trace!("Inserting new serial: {}", serial);
            if serials.insert(serial.clone()) {
                log::trace!("Sending discovered event for serial: {}", serial);
                let _ = events_out.send(FastbootEvent::Discovered(serial.clone())).await;
                log::trace!("Sent discovered event for serial: {}", serial);
            }
        }

        // Check for any missing Serials
        let missing_serials: Vec<_> = serials.difference(&new_serials).cloned().collect();
        log::trace!("missing serials: {:#?}", missing_serials);
        for serial in missing_serials {
            serials.remove(&serial);
            log::trace!("Sending lost event for serial: {}", serial);
            let _ = events_out.send(FastbootEvent::Lost(serial.clone())).await;
            log::trace!("Sent lost event for serial: {}", serial);
        }

        log::trace!("discovery loop... waiting for {:#?}", discovery_interval);
        Timer::new(discovery_interval).await;
    }
}

async fn handle_events_loop<F>(receiver: async_channel::Receiver<FastbootEvent>, mut handler: F)
where
    F: FastbootEventHandler,
{
    loop {
        let event = receiver.recv().await.expect("FastbootEvent stream closed?");
        log::trace!("Event loop received event: {:#?}", event);
        handler.handle_event(event).await;
    }
}

fn device_is_fastboot(
    device: &usb_rs::DeviceHandle,
    usb_device: &usb_rs::DeviceDescriptor,
    interface: &usb_rs::InterfaceDescriptor,
) -> bool {
    let subclass_match = u16::from(usb_device.vendor) == USB_DEV_VENDOR
        && u8::from(interface.class) == FASTBOOT_USB_INTERFACE_CLASS
        && u8::from(interface.subclass) == FASTBOOT_USB_INTERFACE_SUBCLASS;
    let protocol_match = u8::from(interface.protocol) == FASTBOOT_USB_INTERFACE_PROTOCOL;
    log::debug!(
        "Device: {:?} subclass_match: {}, protocol_match: {}",
        device,
        subclass_match,
        protocol_match
    );
    subclass_match && protocol_match
}

#[derive(PartialEq, Debug)]
struct InterfaceHelper {
    class: u8,
    subclass: u8,
    protocol: u8,
}

impl TryFrom<&Path> for InterfaceHelper {
    type Error = ();

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        let class_path = path.join("bInterfaceClass");
        let subclass_path = path.join("bInterfaceSubClass");
        let protocol_path = path.join("bInterfaceProtocol");

        fn read_val(path: &Path) -> Option<u8> {
            let val = std::fs::read_to_string(path).ok()?;
            u8::from_str_radix(val.trim(), 16).ok()
        }

        let class = read_val(&class_path).ok_or(())?;
        let subclass = read_val(&subclass_path).ok_or(())?;
        let protocol = read_val(&protocol_path).ok_or(())?;
        Ok(Self { class, subclass, protocol })
    }
}

impl TryFrom<PathBuf> for InterfaceHelper {
    type Error = ();

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::try_from(path.as_path())
    }
}

/// Returns true if the given sysfs device directory matches Fastboot vendor and interface
/// descriptors.
fn is_fastboot_sysfs_path(sysfs_path: &Path) -> bool {
    let vendor_path = sysfs_path.join("idVendor");
    let Ok(vendor_str) = std::fs::read_to_string(vendor_path) else {
        return false;
    };
    let Ok(vendor_id) = u16::from_str_radix(vendor_str.trim(), 16) else {
        return false;
    };
    if vendor_id != USB_DEV_VENDOR {
        return false;
    }

    let Ok(entries) = std::fs::read_dir(sysfs_path) else {
        return false;
    };

    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| InterfaceHelper::try_from(p).ok())
        .any(|i| {
            i == InterfaceHelper {
                class: FASTBOOT_USB_INTERFACE_CLASS,
                subclass: FASTBOOT_USB_INTERFACE_SUBCLASS,
                protocol: FASTBOOT_USB_INTERFACE_PROTOCOL,
            }
        })
}

/// Inspects sysfs device attributes without opening device nodes or issuing ioctls.
/// Returns Some(true) if the device matches Fastboot vendor and interface descriptors in sysfs,
/// Some(false) if it is known not to match, or None if sysfs is unavailable on the platform.
pub fn is_fastboot_sysfs_match(device: &usb_rs::DeviceHandle) -> Option<bool> {
    let sysfs_path = device.sysfs_path()?;
    Some(is_fastboot_sysfs_path(&sysfs_path))
}

/// Checks whether a USB device is a Fastboot target, trying sysfs first and falling
/// back to interface descriptor scanning if sysfs is unavailable.
pub fn is_fastboot_device(device: &usb_rs::DeviceHandle) -> bool {
    if let Some(matched) = is_fastboot_sysfs_match(device) {
        return matched;
    }
    device
        .scan_interfaces(URB_POOL_SIZE, |usb_device, interface| {
            device_is_fastboot(device, usb_device, interface)
        })
        .is_ok()
}

/// How many URBs to allocate for each device we communicate with.
const URB_POOL_SIZE: usize = 32;

async fn find_serial_numbers() -> Vec<String> {
    log::debug!("finding serial numbers");
    let mut serials = Vec::new();

    let Ok(devices) = enumerate_devices() else {
        return serials;
    };

    // Iterate through all detected USB devices and initiate concurrent scans.
    // Since `device.serial()` and `device.scan_interfaces()` involve synchronous
    // I/O that can block if a device is hung, we run each scan on a blocking
    // thread pool and enforce a timeout limit per device.
    let futures: Vec<_> = devices
        .into_iter()
        .map(|device| {
            let debug_name = device.debug_name();
            async move {
                // Spawn the blocking USB I/O operations on a helper thread.
                let check_fut = unblock(move || {
                    // Fast path: inspect sysfs metadata without opening device node or calling ioctls
                    if let Some(is_match) = is_fastboot_sysfs_match(&device) {
                        if is_match {
                            return device.serial();
                        } else {
                            return None;
                        }
                    }

                    // Fallback (e.g. unit tests with fake USB environment):
                    // Retrieve the serial number (lazily loaded from sysfs on first call).
                    let serial = device.serial()?;
                    // Scan the interfaces to determine if this is a fastboot match.
                    let valid = match device.scan_interfaces(URB_POOL_SIZE, |usb_device, interface| {
                        device_is_fastboot(&device, usb_device, interface)
                    }) {
                        Ok(_) => true,
                        Err(usb_rs::Error::InterfaceNotFound) => {
                            log::debug!(device = device.debug_name().as_str(); "No interfaces found from scan");
                            false
                        }
                        Err(e) => {
                            log::warn!(device = device.debug_name().as_str(), error:? = e;
                                           "Error scanning USB device");
                            false
                        }
                    };
                    if valid {
                        Some(serial)
                    } else {
                        None
                    }
                });

                // Enforce the scan timeout on this device check.
                match check_fut.on_timeout(SCAN_TIMEOUT, || {
                    log::warn!(device = debug_name.as_str(); "Timeout scanning USB device");
                    None
                }).await {
                    Some(serial) => {
                        log::info!(device = debug_name.as_str(); "Fastboot usb device with serial {:?} found", serial);
                        Some(serial)
                    }
                    None => {
                        log::debug!(device = debug_name.as_str(); "Non-Fastboot usb device or error/timeout");
                        None
                    }
                }
            }
        })
        .collect();

    // Wait for all concurrent scan results to complete or time out.
    let results = futures::future::join_all(futures).await;
    for res in results {
        if let Some(serial) = res {
            serials.push(serial);
        }
    }

    log::debug!("Serials found: {:?}", serials);
    return serials;
}

/// Returns all USB device directories in `sysfs_dir` that match the Fastboot vendor and
/// interface descriptors (`idVendor == 0x18d1`, `ff/42/03`).
///
/// In the Linux kernel, reading `idVendor` and `bInterface*` reads cached descriptors in memory.
/// These reads do not acquire the per-device `usb_lock_device` mutex.
fn candidate_fastboot_sysfs_paths(sysfs_dir: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(sysfs_dir).ok()?;
    let paths = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p.file_name().and_then(|n| n.to_str()).is_some_and(|name| name.contains(':'))
                && is_fastboot_sysfs_path(p)
        })
        .collect();
    Some(paths)
}

fn sysfs_path_matches_serial(path: &Path, target_serial: &str) -> bool {
    std::fs::read_to_string(path.join("serial"))
        .is_ok_and(|dev_serial| dev_serial.trim() == target_serial)
}

async fn find_fastboot_sysfs_path_async(target_serial: &str, sysfs_dir: &Path) -> Option<PathBuf> {
    let sysfs_dir_owned = sysfs_dir.to_path_buf();
    let candidates = unblock(move || candidate_fastboot_sysfs_paths(&sysfs_dir_owned))
        .on_timeout(SCAN_TIMEOUT, || None)
        .await?;

    let mut tasks: FuturesUnordered<_> = candidates
        .into_iter()
        .map(|path| {
            let serial = target_serial.to_string();
            async move {
                unblock(move || sysfs_path_matches_serial(&path, &serial).then_some(path))
                    .on_timeout(SCAN_TIMEOUT, || None)
                    .await
            }
        })
        .collect();

    while let Some(res) = tasks.next().await {
        if let Some(matched_path) = res {
            return Some(matched_path);
        }
    }
    None
}

async fn find_target_serial_in_sysfs(target_serial: &str, sysfs_dir: &Path) -> Vec<String> {
    log::debug!("finding target serial number: {}", target_serial);
    if find_fastboot_sysfs_path_async(target_serial, sysfs_dir).await.is_some() {
        return vec![target_serial.to_string()];
    }

    // Fallback (for example, unit tests with a fake USB environment):
    let candidates = unblock(|| {
        enumerate_devices()
            .ok()
            .into_iter()
            .flatten()
            .filter(|d| is_fastboot_sysfs_match(d) != Some(false))
            .collect::<Vec<_>>()
    })
    .on_timeout(SCAN_TIMEOUT, Vec::new)
    .await;

    let mut tasks: FuturesUnordered<_> = candidates
        .into_iter()
        .map(|device| {
            let serial = target_serial.to_string();
            async move {
                unblock(move || {
                    (is_fastboot_device(&device) && device.serial().as_deref() == Some(&serial))
                        .then_some(serial)
                })
                .on_timeout(SCAN_TIMEOUT, || None)
                .await
            }
        })
        .collect();

    while let Some(res) = tasks.next().await {
        if let Some(matched) = res {
            return vec![matched];
        }
    }
    vec![]
}

fn check_and_log_usb_speed(device: &usb_rs::DeviceHandle, target_serial: &str) {
    if let Some(speed) = device.speed() {
        log::info!("USB Fastboot device '{}' negotiated speed: {}", target_serial, speed);
        if !speed.is_superspeed() {
            log::warn!(
                "USB Fastboot device '{}' is connected at {} (not SuperSpeed). \
                 Flashing throughput will be severely bottlenecked by USB bus bandwidth (~35-40 MB/s). \
                 For optimal flashing speed, connect to a USB 3.0 port with a SuperSpeed cable.",
                target_serial,
                speed
            );
        }
    }
}

fn device_to_interface(device: &usb_rs::DeviceHandle) -> Result<Interface, UsbDiscoveryError> {
    device
        .scan_interfaces(URB_POOL_SIZE, |usb_device, interface| {
            device_is_fastboot(device, usb_device, interface)
        })
        .map(Interface::new)
        .map_err(|e| {
            if matches!(e, usb_rs::Error::InterfaceNotFound) {
                UsbDiscoveryError::InterfaceNotFound
            } else {
                log::warn!(device = device.debug_name().as_str(), error:? = e;
                               "Error scanning USB device");
                UsbDiscoveryError::Usb(e)
            }
        })
}

fn find_matching_serial_blocking<T, F>(
    candidates: Vec<T>,
    target_serial: &str,
    matches_serial: F,
) -> Option<T>
where
    T: Send + 'static,
    F: Fn(&T, &str) -> bool + Copy + Send + 'static,
{
    if candidates.is_empty() {
        return None;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    for candidate in candidates {
        let tx = tx.clone();
        let serial = target_serial.to_string();
        std::thread::spawn(move || {
            let matched = matches_serial(&candidate, &serial).then_some(candidate);
            let _ = tx.send(matched);
        });
    }
    drop(tx);

    let deadline = std::time::Instant::now() + SCAN_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Some(matched)) => return Some(matched),
            Ok(None) => continue,
            Err(_) => return None,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn find_fastboot_sysfs_path_blocking(target_serial: &str, sysfs_dir: &Path) -> Option<PathBuf> {
    let candidates = candidate_fastboot_sysfs_paths(sysfs_dir)?;
    find_matching_serial_blocking(candidates, target_serial, |path, serial| {
        sysfs_path_matches_serial(path, serial)
    })
}

pub fn open_interface_with_serial<P>(serial: P) -> Result<Interface, UsbDiscoveryError>
where
    P: AsRef<str>,
{
    let target_serial = serial.as_ref();

    #[cfg(target_os = "linux")]
    if let Some(sysfs_path) =
        find_fastboot_sysfs_path_blocking(target_serial, Path::new("/sys/bus/usb/devices"))
        && let Ok(Some(device)) =
            usb_rs::DeviceHandle::from_sysfs_path(sysfs_path, target_serial.to_string())
    {
        check_and_log_usb_speed(&device, target_serial);
        return device_to_interface(&device);
    }

    // Fallback path (for example, unit tests with fake USB devices):
    let candidates: Vec<_> = enumerate_devices()?
        .into_iter()
        .filter(|d| is_fastboot_sysfs_match(d) != Some(false))
        .collect();
    let device = find_matching_serial_blocking(candidates, target_serial, |d, target| {
        is_fastboot_device(d) && d.serial().as_deref() == Some(target)
    })
    .ok_or(UsbDiscoveryError::InterfaceNotFound)?;
    check_and_log_usb_speed(&device, target_serial);
    device_to_interface(&device)
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::channel::mpsc::unbounded;
    use pretty_assertions::assert_eq;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    impl<S: AsRef<str>> TargetSerialFinder<S> {
        fn new_with_sysfs_path(serial: S, sysfs_path: impl Into<PathBuf>) -> Self {
            Self { serial, sysfs_path: sysfs_path.into() }
        }
    }

    struct TestFastbootUsbTester {
        serial_to_is_fastboot: HashMap<String, bool>,
    }

    impl FastbootUsbTester for TestFastbootUsbTester {
        async fn is_fastboot_usb(&mut self, _serial: &str) -> bool {
            *self.serial_to_is_fastboot.get(_serial).unwrap()
        }
    }

    struct TestSerialNumberFinder {
        responses: Vec<Vec<String>>,
        is_empty: Arc<Mutex<bool>>,
    }

    impl SerialNumberFinder for TestSerialNumberFinder {
        async fn find_serial_numbers(&mut self) -> Vec<String> {
            if let Some(res) = self.responses.pop() {
                res
            } else {
                let mut lock = self.is_empty.lock().unwrap();
                *lock = true;
                vec![]
            }
        }
    }

    #[fuchsia::test]
    async fn test_usb_watcher() -> () {
        let empty_signal = Arc::new(Mutex::new(false));
        let serial_finder = TestSerialNumberFinder {
            responses: vec![
                vec!["1234".to_string(), "2345".to_string(), "ABCD".to_string()],
                vec!["1234".to_string(), "5678".to_string()],
            ],
            is_empty: empty_signal.clone(),
        };

        let mut serial_to_is_fastboot = HashMap::new();
        serial_to_is_fastboot.insert("1234".to_string(), true);
        serial_to_is_fastboot.insert("2345".to_string(), true);
        serial_to_is_fastboot.insert("5678".to_string(), true);
        // Since this is not in fastboot then it should not appear in our results
        serial_to_is_fastboot.insert("ABCD".to_string(), false);
        let fastboot_tester = TestFastbootUsbTester { serial_to_is_fastboot };

        let mut serial_to_is_live = HashMap::new();
        serial_to_is_live.insert("1234".to_string(), true);
        serial_to_is_live.insert("2345".to_string(), true);
        serial_to_is_live.insert("5678".to_string(), true);

        let (sender, mut queue) = unbounded();
        let watcher = FastbootUsbWatcher::new(
            move |res: FastbootEvent| {
                let _ = sender.unbounded_send(res);
            },
            serial_finder,
            fastboot_tester,
            Duration::from_millis(1),
        );

        while !*empty_signal.lock().unwrap() {
            // Wait a tiny bit so the watcher can drain the finder queue
            Timer::new(Duration::from_millis(1)).await;
        }

        drop(watcher);
        let mut events = Vec::<FastbootEvent>::new();
        while let Ok(event) = queue.try_recv() {
            events.push(event);
        }

        // Assert state of events
        assert_eq!(events.len(), 6);
        assert_eq!(
            &events,
            &vec![
                // First set of discovery events
                FastbootEvent::Discovered("1234".to_string()),
                FastbootEvent::Discovered("5678".to_string()),
                // Second set of discovery events
                FastbootEvent::Discovered("2345".to_string()),
                FastbootEvent::Lost("5678".to_string()),
                // Last set... there are no more items left in the queue
                // so we lose all serials.
                FastbootEvent::Lost("1234".to_string()),
                FastbootEvent::Lost("2345".to_string()),
            ]
        );
        // Reiterating... serial ABCD was not in fastboot so it should not appear in our results
    }

    struct StackedFastbootUsbLiveTester {
        is_live_queue: VecDeque<bool>,
        call_count: u32,
    }

    impl FastbootUsbLiveTester for StackedFastbootUsbLiveTester {
        async fn is_fastboot_usb_live(&mut self, _serial: &str) -> bool {
            self.call_count += 1;
            self.is_live_queue.pop_front().expect("should have enough calls in the queue")
        }
    }

    #[fuchsia::test]
    async fn test_wait_for_live() -> () {
        let mut tester = StackedFastbootUsbLiveTester {
            is_live_queue: VecDeque::from([false, false, false, true]),
            call_count: 0,
        };

        wait_for_live("some-awesome-serial", &mut tester, Duration::from_millis(10)).await;

        assert_eq!(tester.call_count, 4);
    }

    #[fuchsia::test]
    async fn test_target_serial_finder_nonexistent() {
        let mut finder = TargetSerialFinder::new("nonexistent-device-serial");
        let results = finder.find_serial_numbers().await;
        assert_eq!(results, Vec::<String>::new());
    }

    #[fuchsia::test]
    async fn test_target_serial_finder_existent() {
        struct TempSysfsDir(PathBuf);
        impl Drop for TempSysfsDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let sysfs_root = std::env::temp_dir().join(format!(
            "usb_fastboot_test_sysfs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _guard = TempSysfsDir(sysfs_root.clone());

        let dev_dir = sysfs_root.join("1-1");
        let iface_dir = dev_dir.join("1-1:1.0");
        std::fs::create_dir_all(&iface_dir).unwrap();

        std::fs::write(dev_dir.join("serial"), "fastboot-device-123\n").unwrap();
        std::fs::write(dev_dir.join("idVendor"), format!("{:04x}\n", USB_DEV_VENDOR)).unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_CLASS),
        )
        .unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceSubClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_SUBCLASS),
        )
        .unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceProtocol"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_PROTOCOL),
        )
        .unwrap();

        let mut finder =
            TargetSerialFinder::new_with_sysfs_path("fastboot-device-123", &sysfs_root);
        let results = finder.find_serial_numbers().await;
        assert_eq!(results, vec!["fastboot-device-123".to_string()]);
    }

    #[fuchsia::test]
    async fn test_unversioned_fastboot_usb_tester_checks_interface_descriptors() {
        struct TempSysfsDir(PathBuf);
        impl Drop for TempSysfsDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let mut tester = UnversionedFastbootUsbTester {};
        // A nonexistent serial must not be reported as a fastboot USB device.
        assert!(!tester.is_fastboot_usb("nonexistent-device-serial").await);

        let sysfs_root = std::env::temp_dir().join(format!(
            "usb_fastboot_tester_sysfs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _guard = TempSysfsDir(sysfs_root.clone());

        let dev_dir = sysfs_root.join("1-1");
        let iface_dir = dev_dir.join("1-1:1.0");
        std::fs::create_dir_all(&iface_dir).unwrap();

        std::fs::write(dev_dir.join("serial"), "target-serial-456\n").unwrap();
        std::fs::write(dev_dir.join("idVendor"), format!("{:04x}\n", USB_DEV_VENDOR)).unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_CLASS),
        )
        .unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceSubClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_SUBCLASS),
        )
        .unwrap();
        // Non-fastboot protocol (e.g. ADB 0x01 instead of Fastboot 0x03) must be rejected.
        std::fs::write(iface_dir.join("bInterfaceProtocol"), "01\n").unwrap();
        assert!(find_target_serial_in_sysfs("target-serial-456", &sysfs_root).await.is_empty());

        // Matching Fastboot protocol (0x03) must be accepted.
        std::fs::write(
            iface_dir.join("bInterfaceProtocol"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_PROTOCOL),
        )
        .unwrap();
        assert_eq!(
            find_target_serial_in_sysfs("target-serial-456", &sysfs_root).await,
            vec!["target-serial-456".to_string()]
        );
    }

    #[cfg(unix)]
    fn write_sysfs_descriptors(dev_dir: &Path, iface_name: &str, protocol: u8) {
        let iface_dir = dev_dir.join(iface_name);
        std::fs::create_dir_all(&iface_dir).unwrap();
        std::fs::write(dev_dir.join("idVendor"), format!("{:04x}\n", USB_DEV_VENDOR)).unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_CLASS),
        )
        .unwrap();
        std::fs::write(
            iface_dir.join("bInterfaceSubClass"),
            format!("{:02x}\n", FASTBOOT_USB_INTERFACE_SUBCLASS),
        )
        .unwrap();
        std::fs::write(iface_dir.join("bInterfaceProtocol"), format!("{:02x}\n", protocol))
            .unwrap();
    }

    #[cfg(unix)]
    struct BlockingFifoGuard {
        fifo_path: PathBuf,
        holder: Option<std::fs::File>,
    }

    #[cfg(unix)]
    impl BlockingFifoGuard {
        fn new(fifo_path: PathBuf) -> Self {
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("failed to execute mkfifo");
            assert!(status.success(), "mkfifo failed");
            // Keep one O_RDWR descriptor open. A reader's open(O_RDONLY) call succeeds
            // immediately, and its read(2) call blocks until this guard drops.
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo_path)
                .expect("failed to open FIFO O_RDWR");
            Self { fifo_path, holder: Some(holder) }
        }
    }

    #[cfg(unix)]
    impl Drop for BlockingFifoGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.fifo_path);
            drop(self.holder.take());
        }
    }

    #[cfg(unix)]
    #[fuchsia::test]
    async fn test_target_serial_finder_skips_blocking_non_fastboot_sibling() {
        struct TempSysfsDir(PathBuf);
        impl Drop for TempSysfsDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let sysfs_root = std::env::temp_dir().join(format!(
            "usb_fastboot_non_fb_fifo_sysfs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _guard = TempSysfsDir(sysfs_root.clone());

        std::fs::create_dir_all(sysfs_root.join("1-1")).unwrap();
        std::fs::create_dir_all(sysfs_root.join("1-2")).unwrap();

        // Check the directory iteration order so the test places the blocked sibling in the
        // first entry.
        let dirs: Vec<PathBuf> =
            std::fs::read_dir(&sysfs_root).unwrap().flatten().map(|e| e.path()).collect();
        let (first_dir, second_dir) = (&dirs[0], &dirs[1]);
        let first_name = first_dir.file_name().unwrap().to_str().unwrap();
        let second_name = second_dir.file_name().unwrap().to_str().unwrap();

        // First directory: non-Fastboot interface (protocol 0x01) with a blocking FIFO serial file.
        write_sysfs_descriptors(first_dir, &format!("{first_name}:1.0"), 0x01);
        let _fifo = BlockingFifoGuard::new(first_dir.join("serial"));

        // Second directory: working Fastboot interface (protocol 0x03).
        write_sysfs_descriptors(
            second_dir,
            &format!("{second_name}:1.0"),
            FASTBOOT_USB_INTERFACE_PROTOCOL,
        );
        std::fs::write(second_dir.join("serial"), "fastboot-device-123\n").unwrap();

        let results = find_target_serial_in_sysfs("fastboot-device-123", &sysfs_root).await;
        assert_eq!(results, vec!["fastboot-device-123".to_string()]);

        let blocking_path = find_fastboot_sysfs_path_blocking("fastboot-device-123", &sysfs_root);
        assert_eq!(blocking_path.as_deref(), Some(second_dir.as_path()));
    }

    #[cfg(unix)]
    #[fuchsia::test]
    async fn test_target_serial_finder_does_not_block_on_hung_fastboot_sibling() {
        struct TempSysfsDir(PathBuf);
        impl Drop for TempSysfsDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let sysfs_root = std::env::temp_dir().join(format!(
            "usb_fastboot_fb_fifo_sysfs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _guard = TempSysfsDir(sysfs_root.clone());

        std::fs::create_dir_all(sysfs_root.join("1-1")).unwrap();
        std::fs::create_dir_all(sysfs_root.join("1-2")).unwrap();

        let dirs: Vec<PathBuf> =
            std::fs::read_dir(&sysfs_root).unwrap().flatten().map(|e| e.path()).collect();
        let (first_dir, second_dir) = (&dirs[0], &dirs[1]);
        let first_name = first_dir.file_name().unwrap().to_str().unwrap();
        let second_name = second_dir.file_name().unwrap().to_str().unwrap();

        // First directory: Fastboot interface with a blocked `serial` attribute
        // (for example, a sibling port that holds `usb_lock_device(udev)`).
        write_sysfs_descriptors(
            first_dir,
            &format!("{first_name}:1.0"),
            FASTBOOT_USB_INTERFACE_PROTOCOL,
        );
        let _fifo = BlockingFifoGuard::new(first_dir.join("serial"));

        // Second directory: working target Fastboot device.
        write_sysfs_descriptors(
            second_dir,
            &format!("{second_name}:1.0"),
            FASTBOOT_USB_INTERFACE_PROTOCOL,
        );
        std::fs::write(second_dir.join("serial"), "fastboot-device-123\n").unwrap();

        let results = find_target_serial_in_sysfs("fastboot-device-123", &sysfs_root).await;
        assert_eq!(results, vec!["fastboot-device-123".to_string()]);

        let blocking_path = find_fastboot_sysfs_path_blocking("fastboot-device-123", &sysfs_root);
        assert_eq!(blocking_path.as_deref(), Some(second_dir.as_path()));
    }
}
