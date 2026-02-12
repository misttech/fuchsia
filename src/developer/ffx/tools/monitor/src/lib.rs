// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;
use args::{MonitorCommand, SubCommand};

use anyhow::{Context, Result};
use async_trait::async_trait;
use ffx_config::EnvironmentContext;
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{FfxMain, FfxTool};
use hyper::service::service_fn;
use hyper::{Body, Request, Response, StatusCode};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use target_formatter::{JsonTarget, JsonTargetFormatter};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::spawn_blocking;

use crate::args::StartCommand;

// Default value of these can be found in //src/developer/ffx/data/config.json
const CONFIG_PID_FILE: &str = "monitor.pid_file";
const CONFIG_PORT_FILE: &str = "monitor.port_file";

const LOCAL_SERVER_IP_ADDRESS: &str = "127.0.0.1";
const LOCAL_SERVER_IP_ADDRESS_ARRAY: [u8; 4] = [127, 0, 0, 1];
const DEFAULT_MONITOR_PORT: u16 = 8080;
const LOG_VERSION: u16 = 2;
const AGGREGATIONS_VERSION: u16 = 1;

#[derive(FfxTool)]
#[target(None)]
pub struct MonitorTool {
    #[command]
    cmd: MonitorCommand,

    context: EnvironmentContext,
}

type Cache = Arc<Mutex<HashMap<String, serde_json::Value>>>;
type LogSender = mpsc::UnboundedSender<LogMessage>;

struct LogContext {
    sender: LogSender,
    log_path: Option<Arc<PathBuf>>,
    aggregations_path: Option<Arc<PathBuf>>,
}

#[derive(Serialize, Debug)]
struct Aggregations {
    version: u16,
    summary: Summary,
    device_aggregations: Vec<DeviceAggregation>,
}

#[derive(Serialize, Debug, Default)]
struct Summary {
    total_devices: usize,
    start_time: u64,
    duration_ms: u64,
    latency: LatencySummary,
}

#[derive(Serialize, Debug, Default)]
struct LatencySummary {
    min: u64,
    avg: u64,
    p90: u64,
    p95: u64,
    max: u64,
    count: usize,
}

#[derive(Serialize, Debug)]
struct DeviceAggregation {
    id: String,
    nodename: String,
    target_type: String,
    metrics: DeviceMetricsOutput,
}

#[derive(Serialize, Debug)]
struct DeviceMetricsOutput {
    active_ms: u64,
    inactive_ms: u64,
    disconnected_ms: u64,
    rcs_dropouts: u64,
    target_vanished_count: u64,
}

#[derive(Debug, Clone)]
struct DeviceMetrics {
    active_ms: u64,
    inactive_ms: u64,
    disconnected_ms: u64,
    rcs_dropouts: u64,
    target_vanished_count: u64,
    last_seen_state: Option<String>,
    target_type: String,
}
#[derive(Serialize, Debug, Clone)]
struct LogEntry {
    targets: Vec<JsonTarget>,
    timestamp: u64,
    latency: u64,
}

#[derive(Default, Clone)]
struct AggregationsState {
    start_time: Option<u64>,
    last_time: u64,
    latencies: Vec<u64>,
    devices: HashMap<String, DeviceMetrics>,
    prev_time: u64,
}

impl AggregationsState {
    fn update(&mut self, entry: &LogEntry) {
        if self.start_time.is_none() {
            self.start_time = Some(entry.timestamp);
            self.prev_time = entry.timestamp;
        }
        self.last_time = entry.timestamp;
        self.latencies.push(entry.latency);

        let delta = entry.timestamp.saturating_sub(self.prev_time);

        for dev_metrics in self.devices.values_mut() {
            match dev_metrics.last_seen_state.as_deref() {
                Some("active") => dev_metrics.active_ms += delta,
                Some("inactive") => dev_metrics.inactive_ms += delta,
                Some("disconnected") => dev_metrics.disconnected_ms += delta,
                _ => {}
            }
        }

        let mut current_nodenames = HashSet::new();
        for target in &entry.targets {
            let target_val = serde_json::to_value(target).unwrap();
            let name = target_val["nodename"].as_str().unwrap_or("<unknown>").to_string();
            current_nodenames.insert(name.clone());

            let is_active = target_val["rcs_state"].as_str().unwrap_or("N") == "Y";

            let dev_metrics = self.devices.entry(name).or_insert_with(|| {
                let target_type =
                    target_val["target_type"].as_str().unwrap_or("Unknown").to_string();
                DeviceMetrics {
                    active_ms: 0,
                    inactive_ms: 0,
                    disconnected_ms: 0,
                    rcs_dropouts: 0,
                    target_vanished_count: 0,
                    last_seen_state: None,
                    target_type,
                }
            });

            if dev_metrics.last_seen_state.as_deref() == Some("active") && !is_active {
                dev_metrics.rcs_dropouts += 1;
            }

            dev_metrics.last_seen_state =
                Some(if is_active { "active".to_string() } else { "inactive".to_string() });
        }

        for (name, dev_metrics) in self.devices.iter_mut() {
            if !current_nodenames.contains(name) {
                match dev_metrics.last_seen_state.as_deref() {
                    Some("active") | Some("inactive") => {
                        dev_metrics.target_vanished_count += 1;
                        dev_metrics.last_seen_state = Some("disconnected".to_string());
                    }
                    _ => {}
                }
            }
        }

        self.prev_time = entry.timestamp;
    }

    fn finalize(&self) -> Aggregations {
        let start_time = self.start_time.unwrap_or(0);
        let duration_ms = self.last_time.saturating_sub(start_time);

        let mut sorted_latencies = self.latencies.clone();
        sorted_latencies.sort_unstable();

        let count = sorted_latencies.len();
        let latency_summary = if count > 0 {
            let sum: u64 = sorted_latencies.iter().sum();
            let p90_idx = ((count as f64) * 0.90).floor() as usize;
            let p95_idx = ((count as f64) * 0.95).floor() as usize;
            let p90_idx = p90_idx.min(count.saturating_sub(1));
            let p95_idx = p95_idx.min(count.saturating_sub(1));
            LatencySummary {
                min: sorted_latencies[0],
                max: sorted_latencies[count - 1],
                avg: sum / (count as u64),
                p90: sorted_latencies[p90_idx],
                p95: sorted_latencies[p95_idx],
                count,
            }
        } else {
            LatencySummary::default()
        };

        let mut device_aggregations = Vec::new();
        let mut sorted_devices: Vec<_> = self.devices.iter().collect();
        sorted_devices.sort_by(|a, b| a.0.cmp(b.0));

        for (i, (nodename, metrics)) in sorted_devices.into_iter().enumerate() {
            device_aggregations.push(DeviceAggregation {
                id: format!("target_{}", i),
                nodename: nodename.clone(),
                target_type: metrics.target_type.clone(),
                metrics: DeviceMetricsOutput {
                    active_ms: metrics.active_ms,
                    inactive_ms: metrics.inactive_ms,
                    disconnected_ms: metrics.disconnected_ms,
                    rcs_dropouts: metrics.rcs_dropouts,
                    target_vanished_count: metrics.target_vanished_count,
                },
            });
        }

        Aggregations {
            version: AGGREGATIONS_VERSION,
            summary: Summary {
                total_devices: device_aggregations.len(),
                start_time,
                duration_ms,
                latency: latency_summary,
            },
            device_aggregations,
        }
    }
}

enum LogMessage {
    Log(LogEntry),
    Flush(Option<Arc<PathBuf>>, Option<Arc<PathBuf>>, oneshot::Sender<anyhow::Result<()>>),
}

/// Resolves a log file path.
///
/// If the provided `file_path` is absolute, it's used as is.
/// If it's relative, it's joined with the provided `log_dir`.
/// If `file_path` is relative but `log_dir` is not set, an error is returned.
fn get_log_path(
    file_path: &Option<String>,
    log_dir: &Option<String>,
) -> Result<Option<Arc<PathBuf>>> {
    match (file_path, log_dir) {
        (None, _) => Ok(None),
        (Some(p), _) if Path::new(p).is_absolute() => Ok(Some(Arc::new(PathBuf::from(p)))),
        (Some(_), None) => {
            Err(anyhow::anyhow!("log.dir is not set but is required for relative log file paths"))
        }
        (Some(p), Some(dir)) => Ok(Some(Arc::new(PathBuf::from(dir).join(p)))),
    }
}

async fn start_server(
    context: &EnvironmentContext,
    addr: SocketAddr,
    cmd: StartCommand,
    writer: &mut MachineWriter<serde_json::Value>,
    pid_file_path: &str,
) -> anyhow::Result<()> {
    if cmd.aggregations_file.is_some() && cmd.log_file.is_none() {
        return Err(anyhow::anyhow!(
            "--aggregations-file can only be used when --log-file is also specified"
        ));
    }

    let log_dir: Option<String> = context.get("log.dir").ok();
    let log_path = get_log_path(&cmd.log_file, &log_dir)?;
    let aggregations_path = get_log_path(&cmd.aggregations_file, &log_dir)?;

    let log_context = if log_path.is_some() {
        let ensure_parent = |p: &Option<Arc<PathBuf>>| {
            if let Some(path) = p {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
            }
        };
        ensure_parent(&log_path);
        ensure_parent(&aggregations_path);

        let (tx, rx) = mpsc::unbounded_channel::<LogMessage>();
        tokio::spawn(run_log_manager(rx));
        Some(Arc::new(LogContext { sender: tx, log_path, aggregations_path }))
    } else {
        None
    };

    let target_status_cache = Arc::new(Mutex::new(HashMap::new()));
    let cache_for_task = target_status_cache.clone();
    let log_sender_for_task = log_context.as_ref().map(|c| c.sender.clone());
    let context_clone = context.clone();
    tokio::spawn(async move {
        loop {
            let ctx = context_clone.clone();
            let cmd_clone = cmd.clone();
            let start = std::time::Instant::now();
            let res = spawn_blocking(move || {
                fuchsia_async::LocalExecutor::default()
                    .run_singlethreaded(collect_target_status(&ctx, cmd_clone))
            })
            .await;
            let latency_ms = start.elapsed().as_millis() as u64;

            match res {
                Ok(Ok(statuses)) => {
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    let mut cache_lock = cache_for_task.lock().await;
                    let json_value = serde_json::to_value(&statuses).unwrap();
                    cache_lock.insert("targets".to_owned(), json_value);
                    log::debug!("Successfully updated target status cache {:?}", cache_lock);

                    if let Some(sender) = &log_sender_for_task {
                        let _ = sender.send(LogMessage::Log(LogEntry {
                            targets: statuses,
                            timestamp,
                            latency: latency_ms,
                        }));
                    }
                }
                Ok(Err(e)) => {
                    log::error!("Error collecting target status: {:?}", e);
                }
                Err(e) => {
                    log::error!("Task panicked while collecting target status: {:?}", e);
                }
            }
            sleep(Duration::from_secs(1));
        }
    });

    let log_context_signal = log_context.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use futures::FutureExt;
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
            futures::future::select(
                Box::pin(sigterm.recv().map(|_| ())),
                Box::pin(sigint.recv().map(|_| ())),
            )
            .await;
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await;
        }

        if let Some(ctx) = log_context_signal {
            if let Err(e) = flush_logs(&ctx).await {
                log::error!("Failed to flush logs on stop signal: {:?}", e);
            }
        }
        std::process::exit(0);
    });

    let pid = std::process::id();
    writeln!(writer, "Starting server on http://{} with pid {}", addr, pid)
        .context("writing start message")?;

    let listener = TcpListener::bind(addr).await.context("binding to address")?;

    // Record PID in config file after server successfully started
    if let Some(parent) = Path::new(&pid_file_path).parent() {
        fs::create_dir_all(parent).context("creating pid file directory")?;
    }
    fs::write(&pid_file_path, pid.to_string()).context("writing pid file")?;

    loop {
        let (stream, _) = listener.accept().await.context("accepting connection")?;
        let cache_for_handler = target_status_cache.clone();
        let log_context_for_handler = log_context.clone();

        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::Http::new()
                .serve_connection(
                    stream,
                    service_fn(move |req| {
                        handle_request(
                            req,
                            cache_for_handler.clone(),
                            log_context_for_handler.clone(),
                        )
                    }),
                )
                .await
            {
                log::error!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn collect_target_status(
    context: &EnvironmentContext,
    cmd: StartCommand,
) -> Result<Vec<JsonTarget>> {
    let query = ffx_target::TargetInfoQuery::from(cmd.nodename.clone());
    let infos =
        ffx_target::list_targets(context, query, !cmd.no_usb, !cmd.no_mdns, !cmd.no_probe).await?;
    let formatter = JsonTargetFormatter::try_from(infos)?;
    Ok(formatter.targets)
}

async fn flush_logs(ctx: &Arc<LogContext>) -> anyhow::Result<()> {
    let (tx, finished_signal) = tokio::sync::oneshot::channel();
    let _ =
        ctx.sender.send(LogMessage::Flush(ctx.log_path.clone(), ctx.aggregations_path.clone(), tx));
    match finished_signal.await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("Failed to flush logs: {:?}", e)),
        Err(e) => Err(anyhow::anyhow!("Failed to receive flush confirmation: {:?}", e)),
    }
}

async fn handle_request(
    req: Request<Body>,
    cache: Cache,
    log_context: Option<Arc<LogContext>>,
) -> std::result::Result<Response<Body>, Infallible> {
    let mut response = Response::new("".into());
    match req.uri().path() {
        "/status" => {
            let statuses = cache.lock().await;
            match serde_json::to_string(&*statuses) {
                Ok(body) => {
                    *response.body_mut() = body.into();
                    response
                        .headers_mut()
                        .insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
                }
                Err(e) => {
                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    *response.body_mut() = format!("Internal Server Error: {}", e).into();
                }
            }
        }
        "/stop" => {
            if let Some(ctx) = log_context {
                match flush_logs(&ctx).await {
                    Ok(_) => {
                        *response.body_mut() = "OK".into();
                    }
                    Err(e) => {
                        log::error!("{}", e);
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        *response.body_mut() = format!("{}", e).into();
                    }
                }
            } else {
                *response.body_mut() = "OK".into();
            }
        }
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    };
    Ok(response)
}

#[async_trait(?Send)]
impl FfxMain for MonitorTool {
    type Writer = MachineWriter<serde_json::Value>;
    async fn main(self, mut writer: <Self as FfxMain>::Writer) -> fho::Result<()> {
        let pid_file_path: String = self
            .context
            .get(CONFIG_PID_FILE)
            .map_err(|e| fho::Error::from(anyhow::anyhow!("Failed to get pid file path: {}", e)))?;
        let port_file_path: String = self.context.get(CONFIG_PORT_FILE).map_err(|e| {
            fho::Error::from(anyhow::anyhow!("Failed to get port file path: {}", e))
        })?;
        match self.cmd.subcommand {
            SubCommand::Start(cmd) => {
                let port = match cmd.port {
                    Some(port) => port,
                    None => DEFAULT_MONITOR_PORT,
                };

                let addr = SocketAddr::from((LOCAL_SERVER_IP_ADDRESS_ARRAY, port));

                // Record port in config file
                if let Some(parent) = Path::new(&port_file_path).parent() {
                    fs::create_dir_all(parent).context("creating pid file directory")?;
                }
                fs::write(&port_file_path, port.to_string()).context("writing port file")?;
                start_server(&self.context, addr, cmd, &mut writer, &pid_file_path)
                    .await
                    .map_err(fho::Error::from)
            }
            SubCommand::Stop(_) => {
                let port_str = fs::read_to_string(&port_file_path).context("reading port file")?;
                let port: u16 = port_str.trim().parse().context("parsing port")?;

                let url = format!("http://{}:{}/stop", LOCAL_SERVER_IP_ADDRESS, port);
                let client = fuchsia_hyper::new_client();
                let uri = url.parse::<hyper::Uri>().context("parsing uri")?;
                let req = Request::builder()
                    .method(hyper::Method::POST)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap();

                if let Err(e) = client.request(req).await {
                    log::warn!("Failed to send stop request to server: {}", e);
                } else {
                    writeln!(writer, "Successfully sent stop request to server")
                        .context("send stop request")?;
                }

                let pid_str = fs::read_to_string(&pid_file_path).context("reading pid file")?;
                let pid: i32 = pid_str.trim().parse().context("parsing pid")?;

                writeln!(writer, "Stopping server with pid {}", pid)
                    .context("writing stop message")?;
                Command::new("kill").arg(pid.to_string()).status().context("killing process")?;
                fs::remove_file(pid_file_path).context("removing pid file")?;
                Ok(())
            }
            SubCommand::Status(_) => {
                let port_str = fs::read_to_string(&port_file_path).context("reading port file")?;
                let port: u16 = port_str.trim().parse().context("parsing port")?;

                let url = format!("http://{}:{}/status", LOCAL_SERVER_IP_ADDRESS, port);
                let client = fuchsia_hyper::new_client();
                let response = client
                    .get(
                        url.parse()
                            .map_err(|e: hyper::http::uri::InvalidUri| anyhow::anyhow!(e))?,
                    )
                    .await
                    .context("sending request")?;

                let body = hyper::body::to_bytes(response.into_body())
                    .await
                    .context("reading response body")?;
                let json: serde_json::Value =
                    serde_json::from_slice(&body).context("parsing json")?;

                if writer.is_machine() {
                    writer.machine(&json)?;
                } else {
                    let pretty_json =
                        serde_json::to_string_pretty(&json).context("formatting json")?;
                    writeln!(writer, "{}", pretty_json).context("writing response to writer")?;
                }
                Ok(())
            }
        }
    }
}

/// Runs the log manager task which accumulates logs in memory and flushes them to disk upon request.
///
/// This task listens for `LogMessage`s on the provided receiver.
/// - `LogMessage::Log(entry)`: Appends the log entry to the in-memory buffer.
/// - `LogMessage::Flush(path, reply)`: Flushes the accumulated logs to the specified path as a JSON file,
///   overwriting any existing file. The in-memory buffer is cleared after flushing.
async fn run_log_manager(mut log_receiver: mpsc::UnboundedReceiver<LogMessage>) {
    let mut logs: Vec<LogEntry> = Vec::new();
    let mut state = AggregationsState::default();
    while let Some(msg) = log_receiver.recv().await {
        match msg {
            LogMessage::Log(entry) => {
                state.update(&entry);
                logs.push(entry);
            }
            LogMessage::Flush(log_path, aggregations_path, reply) => {
                let logs_to_write = logs.clone();
                let current_state = state.clone();
                let res = spawn_blocking(move || {
                    if let Some(path) = log_path {
                        let json = serde_json::json!({
                            "data": logs_to_write,
                            "version": LOG_VERSION,
                        });
                        let json_str = serde_json::to_string_pretty(&json)?;
                        fs::write(&*path, json_str)?;
                    }

                    if let Some(path) = aggregations_path {
                        let aggs = current_state.finalize();
                        let json_str = serde_json::to_string_pretty(&aggs)?;
                        fs::write(&*path, json_str)?;
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await;
                let _ = reply.send(match res {
                    Ok(inner_res) => inner_res,
                    Err(e) => Err(anyhow::anyhow!("JoinError: {}", e)),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_target(nodename: &str, rcs_state: &str) -> JsonTarget {
        serde_json::from_value(serde_json::json!({
            "nodename": nodename,
            "rcs_state": rcs_state,
            "serial": "serial",
            "target_state": "Product",
            "target_type": "smart_display_m3_eng.nelson",
            "addresses": [
                {
                    "type": "Ip",
                    "ip": "1.2.3.4",
                    "ssh_port": 0
                }
            ],
            "is_default": true,
            "is_manual": false,
        }))
        .unwrap()
    }

    fn make_entry(timestamp: u64, latency: u64, targets: Vec<JsonTarget>) -> LogEntry {
        LogEntry { targets, timestamp, latency }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_log_manager() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let log_path = dir.path().join("log.json");
        let (tx, rx) = mpsc::unbounded_channel();

        fuchsia_async::Task::local(run_log_manager(rx)).detach();

        tx.send(LogMessage::Log(make_entry(
            1000,
            500,
            vec![make_target("fuchsia-1234-5678-abcd", "Y")],
        )))?;
        tx.send(LogMessage::Log(make_entry(
            2000,
            500,
            vec![make_target("fuchsia-1234-5678-abcd", "Y")],
        )))?;

        let (flush_tx, flush_rx) = oneshot::channel();
        let aggregations_path = dir.path().join("aggregations.json");
        tx.send(LogMessage::Flush(
            Some(Arc::new(log_path.clone())),
            Some(Arc::new(aggregations_path.clone())),
            flush_tx,
        ))?;
        flush_rx.await??;

        // Verify first flush
        let content = fs::read_to_string(&log_path)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(json["version"], LOG_VERSION);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);

        assert_eq!(data[0]["timestamp"], 1000);
        assert_eq!(data[0]["targets"][0]["nodename"], "fuchsia-1234-5678-abcd");

        assert_eq!(data[1]["timestamp"], 2000);
        assert_eq!(data[1]["targets"][0]["nodename"], "fuchsia-1234-5678-abcd");

        // Verify aggregations
        let aggs_content = fs::read_to_string(&aggregations_path)?;
        let aggs_json: serde_json::Value = serde_json::from_str(&aggs_content)?;
        assert_eq!(aggs_json["version"], AGGREGATIONS_VERSION);
        let aggs_summary = aggs_json["summary"].as_object().unwrap();
        assert_eq!(aggs_summary["duration_ms"], 1000);

        let target_aggs = aggs_json["device_aggregations"].as_array().unwrap();
        assert_eq!(target_aggs.len(), 1);
        assert_eq!(target_aggs[0]["nodename"], "fuchsia-1234-5678-abcd");
        assert_eq!(target_aggs[0]["metrics"]["active_ms"], 1000);
        assert_eq!(target_aggs[0]["metrics"]["inactive_ms"], 0);

        // Flush again with new data, verifying overwrite
        tx.send(LogMessage::Log(make_entry(
            3000,
            500,
            vec![make_target("fuchsia-1234-5678-abcd", "Y")],
        )))?;
        let (flush_tx, flush_rx) = oneshot::channel();
        tx.send(LogMessage::Flush(
            Some(Arc::new(log_path.clone())),
            Some(Arc::new(aggregations_path.clone())),
            flush_tx,
        ))?;
        flush_rx.await??;

        let content = fs::read_to_string(&log_path)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(json["version"], LOG_VERSION);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 3); // Should have all entries
        assert_eq!(data[2]["timestamp"], 3000);

        let aggs_content = fs::read_to_string(&aggregations_path)?;
        let aggs_json: serde_json::Value = serde_json::from_str(&aggs_content)?;
        assert_eq!(aggs_json["version"], AGGREGATIONS_VERSION);

        Ok(())
    }

    #[test]
    fn test_get_log_path() {
        let default_dir = Some("/default/dir".to_string());

        // Absolute path
        let abs_path = "/tmp/log.json";
        let res = get_log_path(&Some(abs_path.to_string()), &default_dir).unwrap();
        assert_eq!(res.unwrap().as_path(), Path::new(abs_path));

        // Relative path with log_dir
        let res = get_log_path(&Some("log.json".to_string()), &default_dir).unwrap();
        assert_eq!(res.unwrap().as_path(), Path::new("/default/dir/log.json"));

        // Relative path without log_dir should fail
        let res = get_log_path(&Some("log.json".to_string()), &None);
        assert!(res.is_err());

        // None path
        let res = get_log_path(&None, &default_dir).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn test_compute_aggregations_reappearing_device() {
        let mut state = AggregationsState::default();
        state.update(&make_entry(0, 100, vec![make_target("A", "Y")]));
        state.update(&make_entry(1000, 100, vec![]));
        state.update(&make_entry(2000, 100, vec![make_target("A", "Y")]));

        let result = state.finalize();
        let dev_a = &result.device_aggregations[0];
        assert_eq!(dev_a.nodename, "A");
        assert_eq!(dev_a.metrics.target_vanished_count, 1);
        assert_eq!(dev_a.metrics.disconnected_ms, 1000);
        assert_eq!(dev_a.metrics.active_ms, 1000);
    }

    #[test]
    fn test_compute_aggregations() {
        let mut state = AggregationsState::default();
        state.update(&make_entry(0, 100, vec![make_target("A", "Y"), make_target("B", "Y")]));
        state.update(&make_entry(1000, 150, vec![make_target("A", "Y"), make_target("B", "N")]));
        state.update(&make_entry(2000, 120, vec![make_target("A", "N")]));
        state.update(&make_entry(3000, 110, vec![make_target("A", "Y"), make_target("C", "Y")]));

        let result = state.finalize();
        assert_eq!(result.version, AGGREGATIONS_VERSION);

        assert_eq!(result.summary.start_time, 0);
        assert_eq!(result.summary.duration_ms, 3000);
        assert_eq!(result.summary.total_devices, 3);

        let latency = &result.summary.latency;
        assert_eq!(latency.count, 4);
        assert_eq!(latency.min, 100);
        assert_eq!(latency.max, 150);
        assert_eq!(latency.avg, (100 + 150 + 120 + 110) / 4);
        assert_eq!(latency.p90, 150);
        assert_eq!(latency.p95, 150);

        let dev_a = &result.device_aggregations[0];
        assert_eq!(dev_a.nodename, "A");
        assert_eq!(dev_a.metrics.active_ms, 2000);
        assert_eq!(dev_a.metrics.inactive_ms, 1000);
        assert_eq!(dev_a.metrics.rcs_dropouts, 1);
        assert_eq!(dev_a.metrics.target_vanished_count, 0);
        assert_eq!(dev_a.metrics.disconnected_ms, 0);

        let dev_b = &result.device_aggregations[1];
        assert_eq!(dev_b.nodename, "B");
        assert_eq!(dev_b.metrics.active_ms, 1000);
        assert_eq!(dev_b.metrics.inactive_ms, 1000);
        assert_eq!(dev_b.metrics.rcs_dropouts, 1);
        assert_eq!(dev_b.metrics.target_vanished_count, 1);
        assert_eq!(dev_b.metrics.disconnected_ms, 1000);

        let dev_c = &result.device_aggregations[2];
        assert_eq!(dev_c.nodename, "C");
        assert_eq!(dev_c.metrics.active_ms, 0);
        assert_eq!(dev_c.metrics.rcs_dropouts, 0);
    }

    #[test]
    fn test_compute_aggregations_empty() {
        let state = AggregationsState::default();
        let result = state.finalize();
        assert_eq!(result.version, AGGREGATIONS_VERSION);
        assert_eq!(result.summary.total_devices, 0);
        assert_eq!(result.summary.duration_ms, 0);
        assert_eq!(result.device_aggregations.len(), 0);
    }

    #[test]
    fn test_compute_aggregations_p90_p95() {
        let mut state = AggregationsState::default();
        for i in 1..=100 {
            state.update(&make_entry(i * 1000, i, vec![]));
        }

        let result = state.finalize();
        let latency = &result.summary.latency;
        assert_eq!(latency.count, 100);
        assert_eq!(latency.min, 1);
        assert_eq!(latency.max, 100);
        assert_eq!(latency.p90, 91);
        assert_eq!(latency.p95, 96);
    }
}
