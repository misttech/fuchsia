// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::{Result, bail, format_err};
use args::{HealthCheckCommand, HealthCheckLevelArg};
use flex_fuchsia_driver_development as fdd;
use flex_fuchsia_driver_framework as fdf;
#[cfg(not(feature = "fdomain"))]
use fuchsia_driver_dev::get_device_info;
#[cfg(feature = "fdomain")]
use fuchsia_driver_dev_fdomain::get_device_info;
use futures::StreamExt;
use std::io::Write;

/// Upper bound on the number of health checks in flight at once in `--all` mode.
const MAX_CONCURRENT_CHECKS: usize = 8;

impl From<HealthCheckLevelArg> for fdf::HealthCheckRequest {
    fn from(level: HealthCheckLevelArg) -> Self {
        match level {
            HealthCheckLevelArg::Ping => fdf::HealthCheckRequest::Ping,
            HealthCheckLevelArg::Fast => fdf::HealthCheckRequest::Fast,
            HealthCheckLevelArg::Deep => fdf::HealthCheckRequest::Deep,
        }
    }
}

/// How a single health check outcome is tallied, and whether it should fail the command.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Ok,
    Degraded,
    Unhealthy,
    /// `TIMEOUT`, `INTERNAL_ERROR`, or a FIDL transport error: the driver or the framework is in
    /// trouble.
    Error,
    /// `NOT_SUPPORTED`, `UNAVAILABLE`, `COMMUNICATION_ERROR`, `INVALID_STATE`, or `NOT_FOUND`:
    /// the level could not be evaluated, which is not evidence of a problem and therefore does
    /// not fail `--all` (a single explicit target still fails on anything but OK).
    NoInfo,
}

fn derive_health_status(resp: &fdf::HealthCheckResponse) -> fdf::HealthStatus {
    let mut overall = fdf::HealthStatus::Ok;
    if let Some(conditions) = &resp.conditions {
        for condition in conditions {
            match condition.status.unwrap_or(fdf::HealthStatus::Unhealthy) {
                fdf::HealthStatus::Ok => {}
                fdf::HealthStatus::Degraded => overall = fdf::HealthStatus::Degraded,
                fdf::HealthStatus::Unhealthy => return fdf::HealthStatus::Unhealthy,
            }
        }
    }
    overall
}

fn classify(res: &Result<fdf::HealthCheckResponse, fdf::HealthCheckError>) -> Outcome {
    match res {
        Ok(resp) => match derive_health_status(resp) {
            fdf::HealthStatus::Ok => Outcome::Ok,
            fdf::HealthStatus::Degraded => Outcome::Degraded,
            fdf::HealthStatus::Unhealthy => Outcome::Unhealthy,
        },
        Err(fdf::HealthCheckError::Timeout | fdf::HealthCheckError::InternalError) => {
            Outcome::Error
        }
        Err(_) => Outcome::NoInfo,
    }
}

#[derive(Default)]
struct Summary {
    ok: usize,
    degraded: usize,
    unhealthy: usize,
    errors: usize,
    no_info: usize,
}

impl Summary {
    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Ok => self.ok += 1,
            Outcome::Degraded => self.degraded += 1,
            Outcome::Unhealthy => self.unhealthy += 1,
            Outcome::Error => self.errors += 1,
            Outcome::NoInfo => self.no_info += 1,
        }
    }

    fn checked(&self) -> usize {
        self.ok + self.degraded + self.unhealthy + self.errors + self.no_info
    }

    /// The number of nodes that reported a problem. "No information" outcomes never count.
    fn problems(&self) -> usize {
        self.degraded + self.unhealthy + self.errors
    }
}

fn health_status_str(status: Option<fdf::HealthStatus>) -> &'static str {
    match status {
        Some(fdf::HealthStatus::Ok) => "OK",
        Some(fdf::HealthStatus::Degraded) => "DEGRADED",
        Some(fdf::HealthStatus::Unhealthy) => "UNHEALTHY",
        None => "UNKNOWN",
    }
}

fn diagnostic_value_str(value: &fdf::HealthDiagnosticValue) -> String {
    match value {
        fdf::HealthDiagnosticValue::StringValue(s) => s.clone(),
        fdf::HealthDiagnosticValue::IntValue(i) => i.to_string(),
        fdf::HealthDiagnosticValue::UintValue(u) => format!("0x{:x} ({})", u, u),
        fdf::HealthDiagnosticValue::BoolValue(b) => b.to_string(),
        fdf::HealthDiagnosticValue::Timestamp(t) => format!("{} (boot ns)", t.into_nanos()),
        _ => String::from("<unknown>"),
    }
}

/// Formats a boot-timeline duration in nanoseconds as a short human-readable age.
fn format_age(age_ns: i64) -> String {
    const NS_PER_MS: i64 = 1_000_000;
    const NS_PER_S: i64 = 1_000 * NS_PER_MS;
    const NS_PER_MIN: i64 = 60 * NS_PER_S;
    const NS_PER_HOUR: i64 = 60 * NS_PER_MIN;
    if age_ns < 0 {
        return String::from("in the future");
    }
    if age_ns < NS_PER_S {
        format!("{}ms", age_ns / NS_PER_MS)
    } else if age_ns < NS_PER_MIN {
        format!("{:.1}s", age_ns as f64 / NS_PER_S as f64)
    } else if age_ns < NS_PER_HOUR {
        format!("{}m{}s", age_ns / NS_PER_MIN, (age_ns % NS_PER_MIN) / NS_PER_S)
    } else {
        format!("{}h{}m", age_ns / NS_PER_HOUR, (age_ns % NS_PER_HOUR) / NS_PER_MIN)
    }
}

fn write_diagnostics(
    writer: &mut dyn Write,
    indent: &str,
    diagnostics: &[fdf::HealthDiagnosticItem],
) -> Result<()> {
    for item in diagnostics {
        writeln!(writer, "{}{} = {}", indent, item.key, diagnostic_value_str(&item.value))?;
    }
    Ok(())
}

/// Prints one health check result. `now_boot_ns` is the current boot-timeline instant on the
/// target, used to turn `last_observed` timestamps into ages; when unknown (e.g. on the host) the
/// raw timestamp is printed instead.
fn print_response(
    writer: &mut dyn Write,
    target: &str,
    res: &Result<fdf::HealthCheckResponse, fdf::HealthCheckError>,
    now_boot_ns: Option<i64>,
) -> Result<()> {
    match res {
        Ok(resp) => {
            let status = derive_health_status(resp);
            writeln!(writer, "[{}] Status: {}", target, health_status_str(Some(status)))?;
            if let Some(conditions) = &resp.conditions {
                if !conditions.is_empty() {
                    writeln!(writer, "  Conditions:")?;
                    for condition in conditions {
                        let mut details = Vec::new();
                        if let Some(last_observed) = condition.last_observed.map(|t| t.into_nanos())
                        {
                            match now_boot_ns {
                                Some(now) => details.push(format!(
                                    "last observed {} ago",
                                    format_age(now - last_observed)
                                )),
                                None => details
                                    .push(format!("last observed at {} boot ns", last_observed)),
                            }
                        }
                        if let Some(count) = condition.occurrence_count {
                            details.push(format!("{}x", count));
                        }
                        let suffix = if details.is_empty() {
                            String::new()
                        } else {
                            format!("  ({})", details.join(", "))
                        };
                        writeln!(
                            writer,
                            "    [{}] {}{}",
                            health_status_str(condition.status),
                            condition.reason_code.as_deref().unwrap_or("-"),
                            suffix
                        )?;
                        if let Some(message) = &condition.message {
                            if !message.is_empty() {
                                writeln!(writer, "        {}", message)?;
                            }
                        }
                        if let Some(diagnostics) = &condition.diagnostics {
                            write_diagnostics(writer, "        ", diagnostics)?;
                        }
                    }
                }
            }
            if let Some(diagnostics) = &resp.diagnostics {
                if !diagnostics.is_empty() {
                    writeln!(writer, "  Diagnostics:")?;
                    write_diagnostics(writer, "    ", diagnostics)?;
                }
            }
        }
        Err(err) => {
            writeln!(writer, "[{}] ERROR: {:?}", target, err)?;
        }
    }
    Ok(())
}

/// Returns the current boot-timeline instant when running on a Fuchsia target, where it shares a
/// clock with the drivers' `last_observed` timestamps.
fn current_boot_ns() -> Option<i64> {
    #[cfg(target_os = "fuchsia")]
    {
        Some(zx::BootInstant::get().into_nanos())
    }
    #[cfg(not(target_os = "fuchsia"))]
    {
        None
    }
}

pub async fn health_check(
    cmd: HealthCheckCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    health_check_with_now(cmd, writer, driver_development_proxy, current_boot_ns).await
}

/// Runs the health check(s) described by `cmd`. `now_boot_ns` returns the current boot-timeline
/// instant on the target (see `print_response`) and is sampled after each health check completes
/// so conditions recorded during the check are never reported as in the future; condition ages are
/// only computed when the tool runs on the target, a host `ffx` prints the raw boot-timeline
/// nanoseconds instead.
///
/// Returns an error (after printing the results) when the single target did not report `OK`, or
/// when any node checked by `--all` is `DEGRADED`, `UNHEALTHY`, or failed with `TIMEOUT`,
/// `INTERNAL_ERROR`, or a transport error. "No information" errors (`NOT_SUPPORTED`,
/// `UNAVAILABLE`, `COMMUNICATION_ERROR`, `INVALID_STATE`, `NOT_FOUND`) never fail `--all`.
async fn health_check_with_now(
    cmd: HealthCheckCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
    now_boot_ns: impl Fn() -> Option<i64>,
) -> Result<()> {
    let request: fdf::HealthCheckRequest = cmd.level.into();

    let (monikers, is_all) = match (cmd.moniker, cmd.all) {
        (Some(_), true) => {
            bail!("`--all` cannot be combined with a moniker. Please specify one or the other.");
        }
        (Some(moniker), false) => (vec![moniker], false),
        (None, true) => {
            let nodes = get_device_info(&driver_development_proxy, &[], false).await?;
            let monikers = nodes
                .into_iter()
                .filter(|node| {
                    // Quarantined nodes failed to bind and have no running driver instance
                    // (even though `bound_driver_url` retains the failed driver's URL), so skip
                    // them alongside the placeholder URLs that driver_manager publishes for nodes
                    // without a driver of their own (kUnboundUrl, kOwnedByParentUrl and
                    // kCompositeParent in //src/devices/bin/driver_manager/node.cc).
                    if node.quarantined == Some(true) {
                        return false;
                    }
                    !matches!(
                        node.bound_driver_url.as_deref(),
                        None | Some("" | "unbound" | "owned by parent" | "owned by composite(s)")
                    )
                })
                .filter_map(|node| node.moniker)
                .collect();
            (monikers, true)
        }
        (None, false) => {
            bail!(
                "Please specify the exact component moniker or topological path of a node, or pass `--all` to check all bound drivers."
            );
        }
    };

    // `buffered` bounds the number of in-flight checks while keeping the results in topology
    // order. A transport error on one node is reported and counted rather than aborting the sweep.
    let mut results = futures::stream::iter(monikers)
        .map(|moniker| {
            let proxy = &driver_development_proxy;
            let now = &now_boot_ns;
            async move {
                let res = proxy.run_health_check(&moniker, request).await;
                (moniker, res, now())
            }
        })
        .buffered(MAX_CONCURRENT_CHECKS);

    let mut summary = Summary::default();
    while let Some((moniker, res, checked_at_boot_ns)) = results.next().await {
        match res {
            Ok(res) => {
                print_response(writer, &moniker, &res, checked_at_boot_ns)?;
                let outcome = classify(&res);
                if !is_all {
                    return match outcome {
                        Outcome::Ok => Ok(()),
                        _ => Err(format_err!("The health check of {} did not return OK.", moniker)),
                    };
                }
                summary.record(outcome);
            }
            Err(e) => {
                if !is_all {
                    return Err(e.into());
                }
                writeln!(writer, "[{}] ERROR: {}", moniker, e)?;
                summary.record(Outcome::Error);
            }
        }
    }

    writeln!(
        writer,
        "{} checked: ok {}, degraded {}, unhealthy {}, errors {}, no-info {}",
        summary.checked(),
        summary.ok,
        summary.degraded,
        summary.unhealthy,
        summary.errors,
        summary.no_info
    )?;
    if summary.problems() > 0 {
        bail!("{} of {} drivers reported a problem.", summary.problems(), summary.checked());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use argh::FromArgs;
    use flex_client::fidl::ServerEnd;
    use fuchsia_async as fasync;
    use futures::future::{Future, FutureExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Runs the command against a fake driver development service and returns the command's
    /// result together with everything it printed.
    async fn run_health_check_test<F, Fut>(
        #[cfg(feature = "fdomain")] client: Arc<flex_client::Client>,
        cmd: HealthCheckCommand,
        on_driver_development_request: F,
    ) -> (Result<()>, String)
    where
        F: Fn(fdd::ManagerRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + Sync,
    {
        #[cfg(not(feature = "fdomain"))]
        let client = flex_client::fidl::ZirconClient;
        let (driver_development_proxy, mut driver_development_requests) =
            client.create_proxy_and_stream::<fdd::ManagerMarker>();

        let mut writer = Vec::new();
        let clock_boot_ns = Arc::new(AtomicI64::new(5_000_000_000));
        let request_clock = Arc::clone(&clock_boot_ns);
        let request_handler_task = fasync::Task::spawn(async move {
            while let Some(res) = driver_development_requests.next().await {
                let request = res.context("Failed to get next request")?;
                // Advance the clock while handling `RunHealthCheck` so any `last_observed`
                // timestamp recorded during the check (e.g. 8.5s) would appear "in the future"
                // if `now` had been captured before the RPC started (at 5s).
                if matches!(request, fdd::ManagerRequest::RunHealthCheck { .. }) {
                    request_clock.store(10_000_000_000, Ordering::SeqCst);
                }
                on_driver_development_request(request).await.context("Failed to handle request")?;
            }
            anyhow::bail!("Driver development request stream unexpectedly closed");
        });
        let result = futures::select! {
            res = request_handler_task.fuse() => {
                res.and_then(|()| Err::<(), _>(format_err!("Request handler task unexpectedly finished")))
            }
            res = health_check_with_now(
                cmd,
                &mut writer,
                driver_development_proxy,
                || Some(clock_boot_ns.load(Ordering::SeqCst)),
            ).fuse() => {
                res
            }
        };

        let output = String::from_utf8(writer).expect("health_check output is valid UTF-8");
        (result, output)
    }

    #[fuchsia::test]
    fn test_format_age() {
        assert_eq!(format_age(-1), "in the future");
        assert_eq!(format_age(250_000_000), "250ms");
        assert_eq!(format_age(1_500_000_000), "1.5s");
        assert_eq!(format_age(90_000_000_000), "1m30s");
        assert_eq!(format_age(3_660_000_000_000), "1h1m");
    }

    #[fuchsia::test]
    async fn test_health_check_single_driver_and_missing_target() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();

        // 1. Missing moniker without --all fails immediately.
        let empty_cmd = HealthCheckCommand::from_args(&["health-check"], &[]).unwrap();
        let (result, _) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            empty_cmd,
            |_request: fdd::ManagerRequest| async move { Ok(()) },
        )
        .await;
        let err = result.expect_err("a missing moniker without --all must fail");
        assert!(err.to_string().contains("--all"), "unexpected error: {err}");

        // 2. A moniker together with --all is rejected rather than silently ignoring --all.
        let both_cmd =
            HealthCheckCommand::from_args(&["health-check"], &["vivante-display", "--all"])
                .unwrap();
        let (result, _) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            both_cmd,
            |_request: fdd::ManagerRequest| async move { anyhow::bail!("Unexpected request") },
        )
        .await;
        let err = result.expect_err("a moniker combined with --all must fail");
        assert!(err.to_string().contains("cannot be combined"), "unexpected error: {err}");

        // 3. Single driver health check formats status, reason code, message, and diagnostics,
        //    and a non-OK status makes the command fail.
        let cmd = HealthCheckCommand::from_args(
            &["health-check"],
            &["vivante-display", "--level", "deep"],
        )
        .unwrap();
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::RunHealthCheck { moniker, request, responder } => {
                        assert_eq!(moniker, "vivante-display");
                        assert_eq!(request, fdf::HealthCheckRequest::Deep);
                        let resp = fdf::HealthCheckResponse {
                            conditions: Some(vec![
                                fdf::HealthCondition {
                                    status: Some(fdf::HealthStatus::Degraded),
                                    reason_code: Some(String::from("FIFO_UNDERRUN")),
                                    message: Some(String::from("Recovered from underrun")),
                                    diagnostics: Some(vec![fdf::HealthDiagnosticItem {
                                        key: String::from("underruns"),
                                        value: fdf::HealthDiagnosticValue::UintValue(2),
                                    }]),
                                    last_observed: Some(fidl::BootInstant::from_nanos(
                                        8_500_000_000,
                                    )),
                                    occurrence_count: Some(2),
                                    ..Default::default()
                                },
                                fdf::HealthCondition {
                                    status: Some(fdf::HealthStatus::Ok),
                                    reason_code: Some(String::from("TE_JITTER")),
                                    ..Default::default()
                                },
                            ]),
                            diagnostics: Some(vec![
                                fdf::HealthDiagnosticItem {
                                    key: String::from("pipe_state"),
                                    value: fdf::HealthDiagnosticValue::StringValue(String::from(
                                        "active",
                                    )),
                                },
                                fdf::HealthDiagnosticItem {
                                    key: String::from("drift_ms"),
                                    value: fdf::HealthDiagnosticValue::IntValue(-4),
                                },
                                fdf::HealthDiagnosticItem {
                                    key: String::from("status_reg"),
                                    value: fdf::HealthDiagnosticValue::UintValue(0x1a),
                                },
                                fdf::HealthDiagnosticItem {
                                    key: String::from("phy_locked"),
                                    value: fdf::HealthDiagnosticValue::BoolValue(true),
                                },
                                fdf::HealthDiagnosticItem {
                                    key: String::from("last_vsync"),
                                    value: fdf::HealthDiagnosticValue::Timestamp(
                                        fidl::BootInstant::from_nanos(9_000_000_000),
                                    ),
                                },
                            ]),
                            ..Default::default()
                        };
                        responder.send(Ok(&resp)).context("Failed to send response")?;
                    }
                    _ => anyhow::bail!("Unexpected request"),
                }
                Ok(())
            },
        )
        .await;

        assert!(result.is_err(), "a DEGRADED status must fail the command");
        assert_eq!(
            output,
            concat!(
                "[vivante-display] Status: DEGRADED\n",
                "  Conditions:\n",
                "    [DEGRADED] FIFO_UNDERRUN  (last observed 1.5s ago, 2x)\n",
                "        Recovered from underrun\n",
                "        underruns = 0x2 (2)\n",
                "    [OK] TE_JITTER\n",
                "  Diagnostics:\n",
                "    pipe_state = active\n",
                "    drift_ms = -4\n",
                "    status_reg = 0x1a (26)\n",
                "    phy_locked = true\n",
                "    last_vsync = 9000000000 (boot ns)\n",
            )
        );

        // 4. An OK status (empty conditions) succeeds.
        let cmd = HealthCheckCommand::from_args(&["health-check"], &["vivante-display"]).unwrap();
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::RunHealthCheck { request, responder, .. } => {
                        // The level defaults to FAST.
                        assert_eq!(request, fdf::HealthCheckRequest::Fast);
                        let resp = fdf::HealthCheckResponse::default();
                        responder.send(Ok(&resp)).context("Failed to send response")?;
                    }
                    _ => anyhow::bail!("Unexpected request"),
                }
                Ok(())
            },
        )
        .await;
        result.unwrap();
        assert_eq!(output, "[vivante-display] Status: OK\n");

        // 5. A health check error is printed and fails the command.
        let cmd = HealthCheckCommand::from_args(&["health-check"], &["vivante-display"]).unwrap();
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::RunHealthCheck { responder, .. } => {
                        responder
                            .send(Err(fdf::HealthCheckError::Timeout))
                            .context("Failed to send response")?;
                    }
                    _ => anyhow::bail!("Unexpected request"),
                }
                Ok(())
            },
        )
        .await;
        assert!(result.is_err(), "a TIMEOUT error must fail the command");
        assert_eq!(output, "[vivante-display] ERROR: Timeout\n");

        // 6. A NOT_FOUND error is printed and fails a single-target command.
        let cmd = HealthCheckCommand::from_args(&["health-check"], &["nonexistent"]).unwrap();
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::RunHealthCheck { responder, .. } => {
                        responder
                            .send(Err(fdf::HealthCheckError::NotFound))
                            .context("Failed to send response")?;
                    }
                    _ => anyhow::bail!("Unexpected request"),
                }
                Ok(())
            },
        )
        .await;
        assert!(result.is_err(), "a NOT_FOUND error must fail the command");
        assert_eq!(output, "[nonexistent] ERROR: NotFound\n");
    }

    fn node_info(id: u64, moniker: &str, bound_driver_url: Option<&str>) -> fdd::NodeInfo {
        fdd::NodeInfo {
            id: Some(id),
            moniker: Some(String::from(moniker)),
            bound_driver_url: bound_driver_url.map(String::from),
            ..Default::default()
        }
    }

    /// Serves `nodes` over the node info iterator, then responds to each health check with the
    /// result returned by `respond`.
    async fn handle_all_request<R>(
        request: fdd::ManagerRequest,
        nodes: Vec<fdd::NodeInfo>,
        respond: R,
    ) -> Result<()>
    where
        R: Fn(&str) -> Result<fdf::HealthCheckResponse, fdf::HealthCheckError>,
    {
        match request {
            fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                let mut device_infos = nodes;
                let mut stream =
                    ServerEnd::<fdd::NodeInfoIteratorMarker>::from(iterator).into_stream();
                while let Some(Ok(fdd::NodeInfoIteratorRequest::GetNext { responder })) =
                    stream.next().await
                {
                    responder.send(&device_infos)?;
                    device_infos.clear();
                }
            }
            fdd::ManagerRequest::RunHealthCheck { moniker, request, responder } => {
                assert_eq!(request, fdf::HealthCheckRequest::Ping);
                responder.send(respond(&moniker).as_ref().map_err(|e| *e))?;
            }
            _ => anyhow::bail!("Unexpected request"),
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_health_check_all_bound_drivers() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd =
            HealthCheckCommand::from_args(&["health-check"], &["--all", "-l", "ping"]).unwrap();

        // Only nodes with a real bound driver are checked; quarantined nodes and placeholder URLs
        // are skipped.
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                let mut quarantined_node =
                    node_info(5, "dev.quarantined", Some("fuchsia-boot:///#meta/broken.cm"));
                quarantined_node.quarantined = Some(true);
                let nodes = vec![
                    node_info(1, "dev.none", None),
                    node_info(2, "dev.unbound", Some("unbound")),
                    node_info(3, "dev.owned", Some("owned by parent")),
                    node_info(4, "dev.composite_parent", Some("owned by composite(s)")),
                    quarantined_node,
                    node_info(6, "dev.display", Some("fuchsia-boot:///#meta/display.cm")),
                ];
                handle_all_request(request, nodes, |moniker| {
                    assert_eq!(moniker, "dev.display");
                    Ok(fdf::HealthCheckResponse::default())
                })
                .await
            },
        )
        .await;
        result.unwrap();
        assert_eq!(
            output,
            concat!(
                "[dev.display] Status: OK\n",
                "1 checked: ok 1, degraded 0, unhealthy 0, errors 0, no-info 0\n",
            )
        );

        // Results are printed in topology order. "No information" errors do not fail the sweep,
        // UNHEALTHY and TIMEOUT do.
        let cmd =
            HealthCheckCommand::from_args(&["health-check"], &["--all", "-l", "ping"]).unwrap();
        let (result, output) = run_health_check_test(
            #[cfg(feature = "fdomain")]
            Arc::clone(&client),
            cmd,
            |request: fdd::ManagerRequest| async move {
                let nodes = vec![
                    node_info(1, "dev.display", Some("fuchsia-boot:///#meta/display.cm")),
                    node_info(2, "dev.touch", Some("fuchsia-boot:///#meta/touch.cm")),
                    node_info(3, "dev.i2c", Some("fuchsia-boot:///#meta/i2c.cm")),
                    node_info(4, "dev.spi", Some("fuchsia-boot:///#meta/spi.cm")),
                ];
                handle_all_request(request, nodes, |moniker| match moniker {
                    "dev.display" => Ok(fdf::HealthCheckResponse::default()),
                    "dev.touch" => Err(fdf::HealthCheckError::NotSupported),
                    "dev.i2c" => Ok(fdf::HealthCheckResponse {
                        conditions: Some(vec![fdf::HealthCondition {
                            status: Some(fdf::HealthStatus::Unhealthy),
                            reason_code: Some(String::from("BUS_LOCKUP")),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    "dev.spi" => Err(fdf::HealthCheckError::Timeout),
                    other => panic!("Unexpected moniker {other}"),
                })
                .await
            },
        )
        .await;
        assert!(result.is_err(), "UNHEALTHY and TIMEOUT outcomes must fail the sweep");
        assert_eq!(
            output,
            concat!(
                "[dev.display] Status: OK\n",
                "[dev.touch] ERROR: NotSupported\n",
                "[dev.i2c] Status: UNHEALTHY\n",
                "  Conditions:\n",
                "    [UNHEALTHY] BUS_LOCKUP\n",
                "[dev.spi] ERROR: Timeout\n",
                "4 checked: ok 1, degraded 0, unhealthy 1, errors 1, no-info 1\n",
            )
        );
    }
}
