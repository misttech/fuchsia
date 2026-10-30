// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Support for running a driver's [`crate::Driver::health_check`] on behalf of the lifecycle
//! server (the per-level deadlines and the root dispatcher liveness check that precedes every
//! check), the [`HealthAlertReporter`] through which a driver proactively reports faults, and the
//! opt-in [`HealthCheckInspect`] helper that mirrors results into Inspect.

use std::sync::Arc;

use fdf::{AsAsyncDispatcherRef, DriverDispatcherRef};
use fidl_next::Server;
use fidl_next_fuchsia_driver_framework::{
    Driver as DriverProtocol, HealthAlert, HealthCheckError, HealthCheckRequest,
    HealthCheckResponse, HealthDiagnosticItem, HealthDiagnosticValue, HealthStatus,
};
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use log::{debug, warn};

use crate::Driver;
use crate::server::Transport;

/// The result of a single health check.
pub type HealthCheckResult = Result<HealthCheckResponse, HealthCheckError>;

// Per-level health check deadlines. These mirror the deadlines driver-host enforces for each
// `HealthCheckRequest`; the server adds `HEALTH_CHECK_TIMEOUT_MARGIN` so the host's own TIMEOUT is
// reported first, and the margin only bounds how long a hung check (or wedged root dispatcher) can
// keep its future alive on the driver side.
const PING_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(250);
const FAST_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);
const DEEP_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(120);
const HEALTH_CHECK_TIMEOUT_MARGIN: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

/// Returns how long the server lets a check at `level` run before answering `TIMEOUT` itself.
fn health_check_timeout(level: HealthCheckRequest) -> zx::MonotonicDuration {
    let deadline = match level {
        HealthCheckRequest::Ping => PING_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Fast => FAST_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Deep => DEEP_HEALTH_CHECK_DEADLINE,
    };
    deadline + HEALTH_CHECK_TIMEOUT_MARGIN
}

/// Confirms that `root_dispatcher` is still processing tasks.
///
/// The lifecycle server runs on a dedicated fuchsia-async thread, not the root dispatcher, so
/// unlike the C++ server (whose hook runs on the root dispatcher and therefore proves its liveness
/// implicitly) it must ping the dispatcher explicitly. If the dispatcher is wedged this never
/// completes, and the caller's deadline turns that into a `TIMEOUT`.
async fn ping_root_dispatcher(
    root_dispatcher: DriverDispatcherRef<'static>,
) -> Result<(), HealthCheckError> {
    let (tx, rx) = oneshot::channel();
    root_dispatcher
        .post_task_sync(move |status| {
            if status.is_ok() {
                let _ = tx.send(());
            }
        })
        .map_err(|_| HealthCheckError::InvalidState)?;
    // The sender is dropped without sending if the dispatcher is shutting down.
    rx.await.map_err(|_| HealthCheckError::InvalidState)
}

/// Runs one health check to completion: confirms `root_dispatcher` is live, then invokes
/// [`Driver::health_check`], bounded by the per-level deadline.
pub(crate) async fn run_health_check<D: Driver>(
    driver: &D,
    root_dispatcher: DriverDispatcherRef<'static>,
    request: HealthCheckRequest,
) -> HealthCheckResult {
    let timeout = health_check_timeout(request);
    let check = async move {
        ping_root_dispatcher(root_dispatcher).await?;
        driver.health_check(request).await
    };
    fuchsia_async::TimeoutExt::on_timeout(check, timeout, || Err(HealthCheckError::Timeout)).await
}

/// The maximum number of health alerts that can be queued in a standalone test receiver created by
/// [`HealthAlertReporter::new_for_testing`].
const HEALTH_ALERT_QUEUE_CAPACITY: usize = 64;

enum HealthAlertSink {
    /// Production sink: writes `OnHealthAlert` events synchronously into the `Driver` channel.
    Server(Server<DriverProtocol, Transport>),
    /// Standalone unit-test queue created by [`HealthAlertReporter::new_for_testing`].
    TestQueue { tx: mpsc::Sender<HealthAlert>, dropped: u64 },
    /// Disconnected sink (`Default::default` or after the driver has been asked to stop).
    Closed,
}

/// Lets a driver proactively report faults to the driver framework as `OnHealthAlert` events.
///
/// Obtained from [`crate::DriverContext::health_alert_reporter`] while the driver is starting.
/// Unlike the rest of the driver's lifecycle, alerts are driver-initiated and may come from any
/// task or thread, so the reporter is cheap to clone and drivers typically keep one in their own
/// state.
#[derive(Clone)]
pub struct HealthAlertReporter {
    sink: Arc<Mutex<HealthAlertSink>>,
}

impl HealthAlertReporter {
    /// Creates a reporter backed by the lifecycle server's `Driver` FIDL channel.
    pub(crate) fn new(server: Server<DriverProtocol, Transport>) -> Self {
        Self { sink: Arc::new(Mutex::new(HealthAlertSink::Server(server))) }
    }

    /// Disconnects the reporter so subsequent alerts are dropped.
    pub(crate) fn close(&self) {
        *self.sink.lock() = HealthAlertSink::Closed;
    }

    /// Creates a reporter and alert receiver pair for unit tests that construct a driver directly
    /// without a lifecycle server.
    pub fn new_for_testing() -> (Self, mpsc::Receiver<HealthAlert>) {
        // A futures mpsc channel holds `buffer + number of senders` messages. All clones share a
        // single sender, so this bounds the queue at exactly `HEALTH_ALERT_QUEUE_CAPACITY`.
        let (tx, rx) = mpsc::channel(HEALTH_ALERT_QUEUE_CAPACITY - 1);
        (Self { sink: Arc::new(Mutex::new(HealthAlertSink::TestQueue { tx, dropped: 0 })) }, rx)
    }

    /// Emits an `OnHealthAlert` event over the `Driver` channel to the driver framework.
    ///
    /// This never blocks: on a running driver the event is written synchronously to the underlying
    /// [`fdf_fidl::DriverChannel`] via [`fidl_next::SendFuture::send_immediately`]; alerts
    /// reported after the driver has been asked to stop are dropped.
    ///
    /// Alerts are not mirrored into Inspect automatically; a driver that records its health there
    /// can pass `SelfFault` alerts to [`HealthCheckInspect::record_alert`] as well.
    pub fn report_health_alert(&self, alert: HealthAlert) {
        let mut sink = self.sink.lock();
        match &mut *sink {
            HealthAlertSink::Server(server) => {
                let server = server.clone();
                drop(sink);
                if let Err(e) = server.on_health_alert(alert).send_immediately() {
                    debug!("Dropping health alert on closed or failed driver channel: {e}");
                }
            }
            HealthAlertSink::TestQueue { tx, dropped } => {
                let Err(e) = tx.try_send(alert) else {
                    return;
                };
                if e.is_full() {
                    *dropped += 1;
                    let total_dropped = *dropped;
                    drop(sink);
                    if total_dropped.is_power_of_two() {
                        warn!("Health alert queue is full; dropped {total_dropped} alerts so far");
                    }
                } else {
                    drop(sink);
                    debug!("Dropping health alert because the receiver was dropped");
                }
            }
            HealthAlertSink::Closed => {
                drop(sink);
                debug!("Dropping health alert because the driver server has shut down");
            }
        }
    }
}

impl Default for HealthAlertReporter {
    /// Returns a disconnected reporter: every alert reported through it is dropped (logged at
    /// debug). Intended only for tests or placeholders; use
    /// [`crate::DriverContext::health_alert_reporter`] in a running driver or
    /// [`HealthAlertReporter::new_for_testing`] to observe alerts in a test.
    fn default() -> Self {
        Self { sink: Arc::new(Mutex::new(HealthAlertSink::Closed)) }
    }
}

/// Name of the Inspect node that holds the most recent health check result or self-reported
/// fault. Matches the C++ driver SDK (`root/driver_health_check`).
pub const HEALTH_INSPECT_NODE_NAME: &str = "driver_health_check";

/// Mirrors a driver's most recent health check result into its own Inspect hierarchy.
///
/// The Rust SDK does not record health check results into a driver's Inspect tree automatically.
/// A driver that wants its latest health check visible in its own component Inspect hierarchy (at
/// `root/driver_health_check`, matching the C++ driver SDK schema) creates one of these with the
/// node it wants the record under and calls [`Self::record`] from its
/// [`crate::Driver::health_check`]:
///
/// ```ignore
/// async fn health_check(&self, request: HealthCheckRequest) -> HealthCheckResult {
///     let result = self.check_hardware(request).await;
///     self.health_inspect.record(request, &result);
///     result
/// }
/// ```
///
/// Nothing here depends on how the driver publishes Inspect: it works the same with
/// [`crate::DriverContext::publish_inspect`], with
/// [`crate::DriverContext::inspect_content_publisher`], or with an inspector the driver serves
/// itself. Drivers that build their Inspect tree on demand can instead call
/// [`record_health_check_to_inspect`] while populating it.
pub struct HealthCheckInspect {
    parent: fuchsia_inspect::Node,
    /// The current [`HEALTH_INSPECT_NODE_NAME`] child and metadata about what produced it.
    current: Mutex<Option<CurrentRecord>>,
}

struct CurrentRecord {
    _node: fuchsia_inspect::Node,
    level: Option<HealthCheckRequest>,
    is_informative: bool,
}

impl HealthCheckInspect {
    /// Creates a recorder that writes its [`HEALTH_INSPECT_NODE_NAME`] child under `parent`.
    ///
    /// Only a weak reference to `parent` is kept: the driver stays the owner of its Inspect
    /// tree, and records are silently dropped once `parent` has been removed from it.
    pub fn new(parent: &fuchsia_inspect::Node) -> Self {
        Self { parent: parent.clone_weak(), current: Mutex::new(None) }
    }

    /// Records the result of a health check at `level`.
    ///
    /// To avoid erasing diagnostic state with a routine liveness sweep or a skipped check,
    /// non-informative errors ([`HealthCheckError::NotSupported`],
    /// [`HealthCheckError::Unavailable`], [`HealthCheckError::InvalidState`]) and a healthy
    /// [`HealthCheckRequest::Ping`] do not overwrite an existing informative record from a deeper
    /// check.
    pub fn record(&self, level: HealthCheckRequest, result: &HealthCheckResult) {
        self.replace(Some(level), result.as_ref().map_err(|e| *e));
    }

    /// Records the details of a [`HealthAlert::SelfFault`] the driver is reporting through its
    /// [`HealthAlertReporter`], replacing the previous record, so Inspect also reflects faults
    /// the driver notices between health checks. Neighbour faults describe other drivers and are
    /// ignored.
    pub fn record_alert(&self, alert: &HealthAlert) {
        if let HealthAlert::SelfFault(details) = alert {
            self.replace(None, Ok(details));
        }
    }

    fn replace(
        &self,
        level: Option<HealthCheckRequest>,
        result: Result<&HealthCheckResponse, HealthCheckError>,
    ) {
        let is_no_info_error = matches!(
            result,
            Err(HealthCheckError::NotSupported
                | HealthCheckError::Unavailable
                | HealthCheckError::InvalidState)
        );
        let is_healthy_ping = level == Some(HealthCheckRequest::Ping)
            && matches!(result, Ok(resp) if derive_health_status(resp) == HealthStatus::Ok);

        let mut current = self.current.lock();
        if let Some(prev) = current.as_ref()
            && prev.is_informative
        {
            if is_no_info_error {
                return;
            }
            if is_healthy_ping && prev.level != Some(HealthCheckRequest::Ping) {
                return;
            }
        }
        // The previous node must be dropped *before* the new one is created: Inspect nodes are
        // removed from the hierarchy on drop, so assigning directly would briefly leave two
        // same-named children under `parent`, which readers may observe.
        *current = None;
        *current = Some(CurrentRecord {
            _node: record_health_check_to_inspect(&self.parent, level, result),
            level,
            is_informative: !is_no_info_error,
        });
    }
}

/// Derives the overall status of `response` from its conditions: any `UNHEALTHY` (or omitted)
/// condition makes the whole response `UNHEALTHY`; otherwise any `DEGRADED` condition makes it
/// `DEGRADED`.
fn derive_health_status(response: &HealthCheckResponse) -> HealthStatus {
    let mut overall = HealthStatus::Ok;
    if let Some(conditions) = &response.conditions {
        for condition in conditions {
            match condition.status.unwrap_or(HealthStatus::Unhealthy) {
                HealthStatus::Ok => {}
                HealthStatus::Degraded => overall = HealthStatus::Degraded,
                HealthStatus::Unhealthy => return HealthStatus::Unhealthy,
            }
        }
    }
    overall
}

fn health_status_str(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Ok => "OK",
        HealthStatus::Degraded => "DEGRADED",
        HealthStatus::Unhealthy => "UNHEALTHY",
    }
}

fn health_level_str(level: HealthCheckRequest) -> &'static str {
    match level {
        HealthCheckRequest::Ping => "PING",
        HealthCheckRequest::Fast => "FAST",
        HealthCheckRequest::Deep => "DEEP",
    }
}

fn health_error_str(error: HealthCheckError) -> &'static str {
    match error {
        HealthCheckError::Timeout => "TIMEOUT",
        HealthCheckError::NotSupported => "NOT_SUPPORTED",
        HealthCheckError::InternalError => "INTERNAL_ERROR",
        HealthCheckError::InvalidState => "INVALID_STATE",
        HealthCheckError::Unavailable => "UNAVAILABLE",
        HealthCheckError::CommunicationError => "COMMUNICATION_ERROR",
        HealthCheckError::NotFound => "NOT_FOUND",
        _ => "UNKNOWN",
    }
}

/// Records `diagnostics` as properties of a new `diagnostics` child of `parent`.
fn record_health_diagnostics(parent: &fuchsia_inspect::Node, diagnostics: &[HealthDiagnosticItem]) {
    parent.record_child("diagnostics", |diag_node| {
        for item in diagnostics {
            match &item.value {
                HealthDiagnosticValue::StringValue(s) => diag_node.record_string(&item.key, s),
                HealthDiagnosticValue::IntValue(i) => diag_node.record_int(&item.key, *i),
                HealthDiagnosticValue::UintValue(u) => diag_node.record_uint(&item.key, *u),
                HealthDiagnosticValue::BoolValue(b) => diag_node.record_bool(&item.key, *b),
                HealthDiagnosticValue::Timestamp(t) => diag_node.record_int(&item.key, *t),
                _ => {}
            }
        }
    });
}

/// Records the result of a health check under a new [`HEALTH_INSPECT_NODE_NAME`] child of
/// `parent` and returns it. Dropping the returned node removes the record again.
///
/// `level` is the requested level of the check, or `None` when the record does not come from a
/// health check request (i.e. a self-reported fault). Errors are recorded as `status = "ERROR"`
/// with an `error` property. The `diagnostics` and `conditions` children are only created when
/// non-empty, matching the C++ driver SDK mirror.
///
/// Drivers that publish Inspect on demand (for example via
/// [`crate::DriverContext::inspect_content_publisher`]) can call this directly when populating
/// their tree; drivers with a long-lived inspector should prefer [`HealthCheckInspect`].
pub fn record_health_check_to_inspect(
    parent: &fuchsia_inspect::Node,
    level: Option<HealthCheckRequest>,
    result: Result<&HealthCheckResponse, HealthCheckError>,
) -> fuchsia_inspect::Node {
    let node = parent.create_child(HEALTH_INSPECT_NODE_NAME);
    if let Some(level) = level {
        node.record_string("level", health_level_str(level));
    }
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            node.record_string("status", "ERROR");
            node.record_string("error", health_error_str(error));
            return node;
        }
    };
    node.record_string("status", health_status_str(derive_health_status(response)));
    if let Some(diagnostics) = response.diagnostics.as_deref().filter(|d| !d.is_empty()) {
        record_health_diagnostics(&node, diagnostics);
    }
    if let Some(conditions) = response.conditions.as_deref().filter(|c| !c.is_empty()) {
        node.record_child("conditions", |conditions_node| {
            for condition in conditions {
                let reason_code = condition.reason_code.as_deref().unwrap_or("UNKNOWN");
                conditions_node.record_child(reason_code, |condition_node| {
                    let status = condition.status.unwrap_or(HealthStatus::Unhealthy);
                    condition_node.record_string("status", health_status_str(status));
                    if let Some(message) = &condition.message {
                        condition_node.record_string("message", message);
                    }
                    if let Some(first_observed) = condition.first_observed {
                        condition_node.record_int("first_observed", first_observed);
                    }
                    if let Some(last_observed) = condition.last_observed {
                        condition_node.record_int("last_observed", last_observed);
                    }
                    if let Some(occurrence_count) = condition.occurrence_count {
                        condition_node.record_uint("occurrence_count", occurrence_count);
                    }
                    if let Some(diagnostics) =
                        condition.diagnostics.as_deref().filter(|d| !d.is_empty())
                    {
                        record_health_diagnostics(condition_node, diagnostics);
                    }
                });
            }
        });
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fidl_next_fuchsia_driver_framework::HealthCondition;
    use futures::StreamExt;

    #[fuchsia::test]
    async fn record_health_check_to_inspect_mirrors_full_response() {
        let inspector = fuchsia_inspect::Inspector::default();
        let response = HealthCheckResponse {
            diagnostics: Some(vec![
                HealthDiagnosticItem {
                    key: "state".to_string(),
                    value: HealthDiagnosticValue::StringValue("active".to_string()),
                },
                HealthDiagnosticItem {
                    key: "underruns".to_string(),
                    value: HealthDiagnosticValue::UintValue(2),
                },
                HealthDiagnosticItem {
                    key: "temp".to_string(),
                    value: HealthDiagnosticValue::IntValue(-3),
                },
                HealthDiagnosticItem {
                    key: "powered".to_string(),
                    value: HealthDiagnosticValue::BoolValue(true),
                },
                HealthDiagnosticItem {
                    key: "last_irq".to_string(),
                    value: HealthDiagnosticValue::Timestamp(99),
                },
            ]),
            conditions: Some(vec![
                HealthCondition {
                    status: Some(HealthStatus::Unhealthy),
                    reason_code: Some("DSI_UNLOCKED".to_string()),
                    message: Some("DSI PHY lost lock".to_string()),
                    first_observed: Some(10),
                    last_observed: Some(20),
                    occurrence_count: Some(3),
                    diagnostics: Some(vec![HealthDiagnosticItem {
                        key: "phy_status".to_string(),
                        value: HealthDiagnosticValue::UintValue(7),
                    }]),
                },
                HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("TE_JITTER".to_string()),
                    ..Default::default()
                },
            ]),
        };
        let _node = record_health_check_to_inspect(
            inspector.root(),
            Some(HealthCheckRequest::Deep),
            Ok(&response),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "DEEP",
                status: "UNHEALTHY",
                diagnostics: {
                    state: "active",
                    underruns: 2u64,
                    temp: -3i64,
                    powered: true,
                    last_irq: 99i64,
                },
                conditions: {
                    DSI_UNLOCKED: {
                        status: "UNHEALTHY",
                        message: "DSI PHY lost lock",
                        first_observed: 10i64,
                        last_observed: 20i64,
                        occurrence_count: 3u64,
                        diagnostics: { phy_status: 7u64 },
                    },
                    TE_JITTER: { status: "DEGRADED" },
                },
            },
        });
    }

    #[fuchsia::test]
    async fn record_health_check_to_inspect_mirrors_error() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = record_health_check_to_inspect(
            inspector.root(),
            Some(HealthCheckRequest::Ping),
            Err(HealthCheckError::NotSupported),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "PING", status: "ERROR", error: "NOT_SUPPORTED" },
        });

        // Dropping the node removes the record; records without a level (self-reported alerts)
        // omit the level property.
        drop(node);
        let _node =
            record_health_check_to_inspect(inspector.root(), None, Err(HealthCheckError::Timeout));
        assert_data_tree!(inspector, root: {
            driver_health_check: { status: "ERROR", error: "TIMEOUT" },
        });
    }

    #[fuchsia::test]
    async fn derive_status_prefers_unhealthy_over_degraded() {
        let response = HealthCheckResponse {
            conditions: Some(vec![
                HealthCondition { status: Some(HealthStatus::Degraded), ..Default::default() },
                HealthCondition { status: Some(HealthStatus::Unhealthy), ..Default::default() },
            ]),
            ..Default::default()
        };
        assert_eq!(derive_health_status(&response), HealthStatus::Unhealthy);
        assert_eq!(derive_health_status(&HealthCheckResponse::default()), HealthStatus::Ok);
    }

    #[fuchsia::test]
    async fn health_check_inspect_replaces_and_retains_records() {
        let inspector = fuchsia_inspect::Inspector::default();
        let health_inspect = HealthCheckInspect::new(inspector.root());
        assert_data_tree!(inspector, root: {});

        // If the first check is a no-info error, it is still recorded so the node exists.
        health_inspect.record(HealthCheckRequest::Deep, &Err(HealthCheckError::NotSupported));
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "DEEP", status: "ERROR", error: "NOT_SUPPORTED" },
        });

        // A subsequent informative PING replaces the uninformative error.
        health_inspect.record(HealthCheckRequest::Ping, &Ok(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "PING", status: "OK" },
        });

        // A deeper FAST check replaces PING.
        health_inspect.record(
            HealthCheckRequest::Fast,
            &Ok(HealthCheckResponse {
                conditions: Some(vec![HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("FIFO_WARN".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "DEGRADED",
                conditions: {
                    FIFO_WARN: { status: "DEGRADED" },
                },
            },
        });

        // No-information errors (NOT_SUPPORTED, UNAVAILABLE, INVALID_STATE) and a healthy PING
        // must not overwrite the informative FAST record.
        health_inspect.record(HealthCheckRequest::Deep, &Err(HealthCheckError::NotSupported));
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::Unavailable));
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::InvalidState));
        health_inspect.record(HealthCheckRequest::Ping, &Ok(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "DEGRADED",
                conditions: {
                    FIFO_WARN: { status: "DEGRADED" },
                },
            },
        });

        // A real failure error does replace the FAST record.
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::CommunicationError));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "ERROR",
                error: "COMMUNICATION_ERROR",
            },
        });

        // Dropping the recorder removes the record.
        drop(health_inspect);
        assert_data_tree!(inspector, root: {});
    }

    #[fuchsia::test]
    async fn self_fault_alert_replaces_inspect_node() {
        let inspector = fuchsia_inspect::Inspector::default();
        let health_inspect = HealthCheckInspect::new(inspector.root());

        health_inspect.record_alert(&HealthAlert::SelfFault(HealthCheckResponse {
            conditions: Some(vec![HealthCondition {
                status: Some(HealthStatus::Unhealthy),
                reason_code: Some("DMA_STALLED".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                status: "UNHEALTHY",
                conditions: { DMA_STALLED: { status: "UNHEALTHY" } },
            },
        });

        // Neighbour faults are not this driver's own state, so they do not touch the record.
        health_inspect
            .record_alert(&HealthAlert::UpstreamNeighborFault(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                status: "UNHEALTHY",
                conditions: { DMA_STALLED: { status: "UNHEALTHY" } },
            },
        });

        // Recovery is reported with empty conditions and replaces the record with OK.
        health_inspect.record_alert(&HealthAlert::SelfFault(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: { driver_health_check: { status: "OK" } });
    }

    #[fuchsia::test]
    async fn alerts_are_dropped_when_queue_is_full() {
        let (reporter, alerts) = HealthAlertReporter::new_for_testing();
        // Report through clones to check that they all share one bounded queue.
        for _ in 0..(HEALTH_ALERT_QUEUE_CAPACITY + 5) {
            reporter.clone().report_health_alert(HealthAlert::SelfFault(Default::default()));
        }
        match &*reporter.sink.lock() {
            HealthAlertSink::TestQueue { dropped, .. } => assert_eq!(*dropped, 5),
            _ => panic!("expected TestQueue"),
        }
        // Dropping the last reporter closes the queue, so `collect` drains what was accepted.
        drop(reporter);
        let received: Vec<_> = alerts.collect().await;
        assert_eq!(received.len(), HEALTH_ALERT_QUEUE_CAPACITY);
    }
}
