// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use analytics::metrics_state::MetricsStatus;
pub use analytics::{AnalyticsError, GA4Value};
use analytics::{
    add_custom_event, ga4_metrics, initialize_ga4_metrics_service, redact_host_and_user_from,
};
use anyhow::Context;
use ffx_build_version::VersionInfo;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, AnalyticsError>;

pub const GA4_PROPERTY_ID: &str = "G-L10R82HSYT";
pub const GA4_KEY: &str = "mHeVJ5GxQTCvAVCmVHn_dw";

pub const USB_DRIVER_LAUNCH_EVENT_NAME: &str = "ffx_usb_driver_launch";
pub const USB_DRIVER_SHUTDOWN_EVENT_NAME: &str = "ffx_usb_driver_shutdown";
pub const USB_DRIVER_CONNECTION_EVENT_NAME: &str = "ffx_usb_driver_connection";
pub const USB_DRIVER_PROTOCOL_EVENT_NAME: &str = "ffx_usb_driver_protocol";

/// Aggregated invocation counts for the `fuchsia.ffx.usb` FIDL protocols
/// (`FfxUsb`, `Control`, and `ListDevices`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbProtocolCounts {
    pub initialize_control: u64,
    pub initialize_list_devices: u64,
    pub initialize_connect_to: u64,
    pub initialize_accept: u64,
    pub listen: u64,
    pub stop_listen: u64,
    pub reject: u64,
    pub on_incoming: u64,
    pub on_device_appeared: u64,
    pub on_device_disappeared: u64,
}

impl UsbProtocolCounts {
    /// Returns the total number of protocol method and event invocations across all protocols.
    pub fn total(&self) -> u64 {
        self.initialize_control
            .saturating_add(self.initialize_list_devices)
            .saturating_add(self.initialize_connect_to)
            .saturating_add(self.initialize_accept)
            .saturating_add(self.listen)
            .saturating_add(self.stop_listen)
            .saturating_add(self.reject)
            .saturating_add(self.on_incoming)
            .saturating_add(self.on_device_appeared)
            .saturating_add(self.on_device_disappeared)
    }

    /// Accumulates counts from `other` into `self`.
    pub fn accumulate(&mut self, other: &Self) {
        self.initialize_control = self.initialize_control.saturating_add(other.initialize_control);
        self.initialize_list_devices =
            self.initialize_list_devices.saturating_add(other.initialize_list_devices);
        self.initialize_connect_to =
            self.initialize_connect_to.saturating_add(other.initialize_connect_to);
        self.initialize_accept = self.initialize_accept.saturating_add(other.initialize_accept);
        self.listen = self.listen.saturating_add(other.listen);
        self.stop_listen = self.stop_listen.saturating_add(other.stop_listen);
        self.reject = self.reject.saturating_add(other.reject);
        self.on_incoming = self.on_incoming.saturating_add(other.on_incoming);
        self.on_device_appeared = self.on_device_appeared.saturating_add(other.on_device_appeared);
        self.on_device_disappeared =
            self.on_device_disappeared.saturating_add(other.on_device_disappeared);
    }

    /// Inserts the 10 protocol method and event counters into a GA4 custom dimensions map.
    pub fn insert_custom_dimensions(&self, dims: &mut BTreeMap<&'static str, GA4Value>) {
        dims.insert("initialize_control", self.initialize_control.into());
        dims.insert("initialize_list_devices", self.initialize_list_devices.into());
        dims.insert("initialize_connect_to", self.initialize_connect_to.into());
        dims.insert("initialize_accept", self.initialize_accept.into());
        dims.insert("listen", self.listen.into());
        dims.insert("stop_listen", self.stop_listen.into());
        dims.insert("reject", self.reject.into());
        dims.insert("on_incoming", self.on_incoming.into());
        dims.insert("on_device_appeared", self.on_device_appeared.into());
        dims.insert("on_device_disappeared", self.on_device_disappeared.into());
    }

    /// Converts the protocol invocation counts into a GA4 custom dimensions map.
    pub fn to_custom_dimensions(&self) -> BTreeMap<&'static str, GA4Value> {
        let mut dims = BTreeMap::new();
        self.insert_custom_dimensions(&mut dims);
        dims
    }
}

/// Telemetry event data recorded when a client Unix domain socket connection closes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbDriverConnectionEvent {
    pub duration_ms: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub protocol_counts: UsbProtocolCounts,
}

impl UsbDriverConnectionEvent {
    /// Converts the socket connection metrics into a GA4 custom dimensions map.
    pub fn to_custom_dimensions(&self) -> BTreeMap<&'static str, GA4Value> {
        let mut dims = BTreeMap::from([
            ("duration_ms", self.duration_ms.into()),
            ("rx_bytes", self.rx_bytes.into()),
            ("tx_bytes", self.tx_bytes.into()),
        ]);
        self.protocol_counts.insert_custom_dimensions(&mut dims);
        dims
    }
}

/// Telemetry event data recorded when the USB host driver shuts down.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbDriverShutdownEvent {
    pub unique_devices_discovered: u64,
    pub unique_devices_communicated: u64,
    pub protocol_counts: UsbProtocolCounts,
}

impl UsbDriverShutdownEvent {
    /// Converts the driver shutdown metrics into a GA4 custom dimensions map.
    pub fn to_custom_dimensions(&self) -> BTreeMap<&'static str, GA4Value> {
        let mut dims = BTreeMap::from([
            ("unique_devices_discovered", self.unique_devices_discovered.into()),
            ("unique_devices_communicated", self.unique_devices_communicated.into()),
        ]);
        self.protocol_counts.insert_custom_dimensions(&mut dims);
        dims
    }
}

/// Lifecycle and connection telemetry events emitted by the USB host driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbDriverTelemetryEvent {
    Launch,
    ConnectionClosed(UsbDriverConnectionEvent),
    Shutdown(UsbDriverShutdownEvent),
}

pub async fn init_metrics_svc(
    analytics_path: Option<PathBuf>,
    build_info: VersionInfo,
    invoker: Option<String>,
    sdk_version: String,
) {
    let build_version = build_info.build_version;
    let _ = initialize_ga4_metrics_service(
        String::from("ffx"),
        analytics_path,
        build_version,
        sdk_version,
        GA4_PROPERTY_ID.to_string(),
        GA4_KEY.to_string(),
        invoker,
    )
    .await
    .with_context(|| "Could not initialize metrics service");
}

pub async fn enhanced_analytics() -> bool {
    match ga4_metrics().await {
        Ok(metrics_svc) => metrics_svc.opt_in_status() == MetricsStatus::OptedInEnhanced,
        Err(_) => false,
    }
}

pub fn sanitize(parameter: &str) -> String {
    redact_host_and_user_from(parameter)
}

pub async fn add_ffx_launch_event(
    connection_mode: Option<&str>,
    redacted_args: String,
    subcommand_chain: String,
    is_help: bool,
    enhanced_args: Option<String>,
    time: u128,
    exit_code: i32,
    error_message: Option<String>,
) -> Result<()> {
    let u64_time = u64::try_from(time).unwrap_or(0);
    let call_stack = std::env::var("FUCHSIA_METRICS_CALL_STACK").unwrap_or_default();
    let custom_dimensions = BTreeMap::from([
        ("time", u64_time.into()),
        ("exit_code", exit_code.to_string().into()),
        ("error_message", error_message.unwrap_or_else(|| "".to_string()).into()),
        ("redacted_args", redacted_args.into()),
        ("subcommand_chain", subcommand_chain.into()),
        ("is_help", is_help.into()),
        ("call_stack", call_stack.into()),
    ]);
    let mut metrics_svc = ga4_metrics().await?;
    let mut events = Vec::with_capacity(2);
    if let Some(connection_mode) = connection_mode {
        events.push(metrics_svc.make_custom_event(
            Some("ffx_connection_mode"),
            Some(connection_mode),
            None,
            BTreeMap::new(),
            Some("ffx_connection_mode"),
        ));
    }
    events.push(metrics_svc.make_custom_event(
        None,
        enhanced_args.as_ref().map(String::as_str),
        None,
        custom_dimensions,
        Some("invoke"),
    ));
    metrics_svc.add_events(events).await?;
    metrics_svc.drain().await?;
    Ok(())
}

pub async fn add_flash_partition_event(
    partition_name: &String,
    product_name: &String,
    board_name: &String,
    file_size: u64,
    flash_time: &Duration,
) -> Result<()> {
    let u64_time = u64::try_from(flash_time.as_millis()).unwrap_or(0);
    let custom_dimensions = BTreeMap::from([
        ("partition_name", partition_name.clone().into()),
        ("product_name", product_name.clone().into()),
        ("board_name", board_name.clone().into()),
        ("file_size", file_size.into()),
        ("flash_time", u64_time.into()),
    ]);
    add_custom_event(Some("ffx_flash"), None, None, custom_dimensions).await?;
    Ok(())
}

/// Records and flushes a GA4 custom event when the `ffx` USB driver launches.
pub async fn add_usb_driver_launch_event() -> Result<()> {
    let mut metrics_svc = match ga4_metrics().await {
        Ok(svc) => svc,
        Err(AnalyticsError::NotInitialized) => return Ok(()),
        Err(e) => return Err(e),
    };
    metrics_svc
        .add_custom_event(
            Some(USB_DRIVER_LAUNCH_EVENT_NAME),
            None,
            None,
            BTreeMap::new(),
            Some(USB_DRIVER_LAUNCH_EVENT_NAME),
        )
        .await?;
    metrics_svc.send_events().await?;
    Ok(())
}

/// Records and flushes a GA4 custom event when a USB driver socket connection closes.
pub async fn add_usb_driver_connection_event(event: &UsbDriverConnectionEvent) -> Result<()> {
    let mut metrics_svc = match ga4_metrics().await {
        Ok(svc) => svc,
        Err(AnalyticsError::NotInitialized) => return Ok(()),
        Err(e) => return Err(e),
    };
    metrics_svc
        .add_custom_event(
            Some(USB_DRIVER_CONNECTION_EVENT_NAME),
            None,
            None,
            event.to_custom_dimensions(),
            Some(USB_DRIVER_CONNECTION_EVENT_NAME),
        )
        .await?;
    metrics_svc.send_events().await?;
    Ok(())
}

/// Records and flushes an aggregated GA4 custom event for USB driver FIDL protocol invocations.
pub async fn add_usb_driver_protocol_event(counts: &UsbProtocolCounts) -> Result<()> {
    let mut metrics_svc = match ga4_metrics().await {
        Ok(svc) => svc,
        Err(AnalyticsError::NotInitialized) => return Ok(()),
        Err(e) => return Err(e),
    };
    metrics_svc
        .add_custom_event(
            Some(USB_DRIVER_PROTOCOL_EVENT_NAME),
            None,
            None,
            counts.to_custom_dimensions(),
            Some(USB_DRIVER_PROTOCOL_EVENT_NAME),
        )
        .await?;
    metrics_svc.send_events().await?;
    Ok(())
}

/// Records and flushes a GA4 custom event when the `ffx` USB driver shuts down.
pub async fn add_usb_driver_shutdown_event(event: &UsbDriverShutdownEvent) -> Result<()> {
    let mut metrics_svc = match ga4_metrics().await {
        Ok(svc) => svc,
        Err(AnalyticsError::NotInitialized) => return Ok(()),
        Err(e) => return Err(e),
    };
    metrics_svc
        .add_custom_event(
            Some(USB_DRIVER_SHUTDOWN_EVENT_NAME),
            None,
            None,
            event.to_custom_dimensions(),
            Some(USB_DRIVER_SHUTDOWN_EVENT_NAME),
        )
        .await?;
    metrics_svc.send_events().await?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context as TaskContext, Poll, Waker};

    const GA4_EVENT_NAME_MAX_LEN: usize = 40;
    const GA4_PARAM_NAME_MAX_LEN: usize = 40;
    const GA4_EVENT_PARAM_MAX_COUNT: usize = 100;

    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let mut cx = TaskContext::from_waker(Waker::noop());
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn test_usb_protocol_counts_total_and_accumulate() {
        let mut counts = UsbProtocolCounts {
            initialize_control: 1,
            initialize_list_devices: 2,
            initialize_connect_to: 3,
            initialize_accept: 4,
            listen: 5,
            stop_listen: 6,
            reject: 7,
            on_incoming: 8,
            on_device_appeared: 9,
            on_device_disappeared: 10,
        };
        assert_eq!(counts.total(), 55);

        let other = UsbProtocolCounts {
            initialize_control: 10,
            initialize_list_devices: 20,
            initialize_connect_to: 30,
            initialize_accept: 40,
            listen: 50,
            stop_listen: 60,
            reject: 70,
            on_incoming: 80,
            on_device_appeared: 90,
            on_device_disappeared: 100,
        };
        counts.accumulate(&other);

        assert_eq!(
            counts,
            UsbProtocolCounts {
                initialize_control: 11,
                initialize_list_devices: 22,
                initialize_connect_to: 33,
                initialize_accept: 44,
                listen: 55,
                stop_listen: 66,
                reject: 77,
                on_incoming: 88,
                on_device_appeared: 99,
                on_device_disappeared: 110,
            }
        );
        assert_eq!(counts.total(), 605);
    }

    #[test]
    fn test_usb_driver_event_names_and_dimension_keys_within_ga4_limits() {
        for event_name in [
            USB_DRIVER_LAUNCH_EVENT_NAME,
            USB_DRIVER_SHUTDOWN_EVENT_NAME,
            USB_DRIVER_CONNECTION_EVENT_NAME,
            USB_DRIVER_PROTOCOL_EVENT_NAME,
        ] {
            assert!(!event_name.is_empty());
            assert!(
                event_name.len() <= GA4_EVENT_NAME_MAX_LEN,
                "Event name {event_name} exceeds {GA4_EVENT_NAME_MAX_LEN} chars"
            );
        }

        let conn_dims = UsbDriverConnectionEvent::default().to_custom_dimensions();
        assert_eq!(conn_dims.len(), 13);
        assert!(conn_dims.len() <= GA4_EVENT_PARAM_MAX_COUNT);
        for key in conn_dims.keys() {
            assert!(!key.is_empty());
            assert!(
                key.len() <= GA4_PARAM_NAME_MAX_LEN,
                "Dimension key {key} exceeds {GA4_PARAM_NAME_MAX_LEN} chars"
            );
        }

        let shutdown_dims = UsbDriverShutdownEvent::default().to_custom_dimensions();
        assert_eq!(shutdown_dims.len(), 12);
        assert!(shutdown_dims.len() <= GA4_EVENT_PARAM_MAX_COUNT);
        for key in shutdown_dims.keys() {
            assert!(!key.is_empty());
            assert!(
                key.len() <= GA4_PARAM_NAME_MAX_LEN,
                "Dimension key {key} exceeds {GA4_PARAM_NAME_MAX_LEN} chars"
            );
        }
    }

    #[test]
    fn test_usb_driver_connection_and_shutdown_custom_dimensions() {
        let protocol_counts = UsbProtocolCounts {
            initialize_control: 1,
            initialize_list_devices: 2,
            initialize_connect_to: 3,
            initialize_accept: 4,
            listen: 5,
            stop_listen: 6,
            reject: 7,
            on_incoming: 8,
            on_device_appeared: 9,
            on_device_disappeared: 10,
        };
        let conn_event = UsbDriverConnectionEvent {
            duration_ms: 42,
            rx_bytes: 1024,
            tx_bytes: 2048,
            protocol_counts,
        };
        let conn_dims = conn_event.to_custom_dimensions();
        assert_eq!(conn_dims.get("duration_ms"), Some(&GA4Value::UInteger(42)));
        assert_eq!(conn_dims.get("rx_bytes"), Some(&GA4Value::UInteger(1024)));
        assert_eq!(conn_dims.get("tx_bytes"), Some(&GA4Value::UInteger(2048)));
        assert_eq!(conn_dims.get("initialize_control"), Some(&GA4Value::UInteger(1)));
        assert_eq!(conn_dims.get("initialize_list_devices"), Some(&GA4Value::UInteger(2)));
        assert_eq!(conn_dims.get("initialize_connect_to"), Some(&GA4Value::UInteger(3)));
        assert_eq!(conn_dims.get("initialize_accept"), Some(&GA4Value::UInteger(4)));
        assert_eq!(conn_dims.get("listen"), Some(&GA4Value::UInteger(5)));
        assert_eq!(conn_dims.get("stop_listen"), Some(&GA4Value::UInteger(6)));
        assert_eq!(conn_dims.get("reject"), Some(&GA4Value::UInteger(7)));
        assert_eq!(conn_dims.get("on_incoming"), Some(&GA4Value::UInteger(8)));
        assert_eq!(conn_dims.get("on_device_appeared"), Some(&GA4Value::UInteger(9)));
        assert_eq!(conn_dims.get("on_device_disappeared"), Some(&GA4Value::UInteger(10)));

        let shutdown_event = UsbDriverShutdownEvent {
            unique_devices_discovered: 3,
            unique_devices_communicated: 2,
            protocol_counts,
        };
        let shutdown_dims = shutdown_event.to_custom_dimensions();
        assert_eq!(shutdown_dims.get("unique_devices_discovered"), Some(&GA4Value::UInteger(3)));
        assert_eq!(shutdown_dims.get("unique_devices_communicated"), Some(&GA4Value::UInteger(2)));
        assert_eq!(shutdown_dims.get("initialize_connect_to"), Some(&GA4Value::UInteger(3)));
    }

    #[test]
    fn test_usb_driver_event_submission_functions() {
        let conn_event = UsbDriverConnectionEvent {
            duration_ms: 15,
            rx_bytes: 128,
            tx_bytes: 256,
            protocol_counts: UsbProtocolCounts { initialize_control: 1, ..Default::default() },
        };
        let shutdown_event = UsbDriverShutdownEvent {
            unique_devices_discovered: 2,
            unique_devices_communicated: 1,
            protocol_counts: conn_event.protocol_counts,
        };

        // Calling before init_metrics_svc treats NotInitialized as a clean no-op.
        assert!(block_on(add_usb_driver_launch_event()).is_ok());
        assert!(block_on(add_usb_driver_connection_event(&conn_event)).is_ok());
        assert!(block_on(add_usb_driver_protocol_event(&conn_event.protocol_counts)).is_ok());
        assert!(block_on(add_usb_driver_shutdown_event(&shutdown_event)).is_ok());

        // Initialize disabled/test metrics service and verify submission still succeeds.
        block_on(init_metrics_svc(None, VersionInfo::default(), None, "test-sdk".to_string()));
        assert!(
            block_on(add_ffx_launch_event(
                Some("direct"),
                "ffx target list".to_string(),
                "target list".to_string(),
                false,
                None,
                10,
                0,
                None,
            ))
            .is_ok()
        );
        assert!(block_on(add_usb_driver_launch_event()).is_ok());
        assert!(block_on(add_usb_driver_connection_event(&conn_event)).is_ok());
        assert!(block_on(add_usb_driver_protocol_event(&conn_event.protocol_counts)).is_ok());
        assert!(block_on(add_usb_driver_shutdown_event(&shutdown_event)).is_ok());
    }
}
