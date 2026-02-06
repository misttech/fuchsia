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
use std::collections::HashMap;
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
const LOG_VERSION: u16 = 1;

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
    path: Arc<PathBuf>,
}

enum LogMessage {
    Log(serde_json::Value),
    Flush(Arc<PathBuf>, oneshot::Sender<anyhow::Result<()>>),
}

async fn start_server(
    context: &EnvironmentContext,
    addr: SocketAddr,
    cmd: StartCommand,
    writer: &mut MachineWriter<serde_json::Value>,
    pid_file_path: &str,
) -> anyhow::Result<()> {
    let cache = Arc::new(Mutex::new(HashMap::new()));

    let log_context = if let Some(file_path) = &cmd.log_file {
        let path = PathBuf::from(file_path);
        let log_path = if path.is_absolute() {
            Some(path)
        } else {
            let log_dir: String = context.get("log.dir").unwrap_or_else(|_| "".to_string());
            if log_dir.is_empty() {
                return Err(anyhow::anyhow!(
                    "log.dir is not set but is required for relative log file paths"
                ));
            } else {
                Some(PathBuf::from(log_dir).join(path))
            }
        };

        if let Some(path) = log_path {
            if let Some(parent) = path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    log::warn!(
                        "Failed to create log directory: {}. Logging to file will be disabled.",
                        e
                    );
                    None
                } else {
                    let (tx, rx) = mpsc::unbounded_channel::<LogMessage>();
                    tokio::spawn(run_log_manager(rx));
                    Some(Arc::new(LogContext { sender: tx, path: Arc::new(path) }))
                }
            } else {
                let (tx, rx) = mpsc::unbounded_channel::<LogMessage>();
                tokio::spawn(run_log_manager(rx));
                Some(Arc::new(LogContext { sender: tx, path: Arc::new(path) }))
            }
        } else {
            None
        }
    } else {
        None
    };

    let cache_for_task = cache.clone();
    let log_sender_for_task = log_context.as_ref().map(|c| c.sender.clone());
    let context_clone = context.clone();
    tokio::spawn(async move {
        loop {
            let ctx = context_clone.clone();
            let cmd_clone = cmd.clone();
            let res = spawn_blocking(move || {
                fuchsia_async::LocalExecutor::default()
                    .run_singlethreaded(collect_target_status(&ctx, cmd_clone))
            })
            .await;

            match res {
                Ok(Ok(statuses)) => {
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let entry = serde_json::json!({
                        "targets": statuses,
                        "timestamp": timestamp,
                    });

                    let mut cache_lock = cache_for_task.lock().await;
                    let json_value = serde_json::to_value(&statuses).unwrap();
                    cache_lock.insert("targets".to_owned(), json_value);
                    log::debug!("Successfully updated target status cache {:?}", cache_lock);

                    if let Some(sender) = &log_sender_for_task {
                        let _ = sender.send(LogMessage::Log(entry));
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
        let cache_for_handler = cache.clone();
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
                let (tx, rx) = oneshot::channel();
                let _ = ctx.sender.send(LogMessage::Flush(ctx.path.clone(), tx));
                match rx.await {
                    Ok(Ok(_)) => {
                        *response.body_mut() = "OK".into();
                    }
                    Ok(Err(e)) => {
                        log::error!("Failed to flush logs: {:?}", e);
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        *response.body_mut() = format!("Failed to flush logs: {}", e).into();
                    }
                    Err(e) => {
                        log::error!("Failed to receive flush confirmation: {:?}", e);
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        *response.body_mut() = "Failed to receive flush confirmation".into();
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
    let mut logs: Vec<serde_json::Value> = Vec::new();
    while let Some(msg) = log_receiver.recv().await {
        match msg {
            LogMessage::Log(entry) => logs.push(entry),
            LogMessage::Flush(path, reply) => {
                // Flush accumulated logs, overwriting the file (intended for flush-on-stop).
                let logs_to_write = std::mem::take(&mut logs);
                let res = spawn_blocking(move || {
                    let json = serde_json::json!({
                        "data": logs_to_write,
                        "version": LOG_VERSION,
                    });
                    let json_str = serde_json::to_string_pretty(&json)?;
                    fs::write(&*path, json_str)?;
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

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_log_manager() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let log_path = dir.path().join("log.json");
        let (tx, rx) = mpsc::unbounded_channel();

        fuchsia_async::Task::local(run_log_manager(rx)).detach();

        let make_entry = |timestamp: u64| {
            serde_json::json!({
                "timestamp": timestamp,
                "targets": [
                    {
                        "addresses": [
                            {
                                "ip": "1.2.3.4",
                                "ssh_port": 0,
                                "type": "Ip"
                            }
                        ],
                        "is_default": true,
                        "is_manual": false,
                        "nodename": "fuchsia-1234-5678-abcd",
                        "rcs_state": "Y",
                    }
                ]
            })
        };

        tx.send(LogMessage::Log(make_entry(1000)))?;
        tx.send(LogMessage::Log(make_entry(2000)))?;

        let (flush_tx, flush_rx) = oneshot::channel();
        tx.send(LogMessage::Flush(Arc::new(log_path.clone()), flush_tx))?;
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

        // Flush again with new data, verifying overwrite
        tx.send(LogMessage::Log(make_entry(3000)))?;
        let (flush_tx, flush_rx) = oneshot::channel();
        tx.send(LogMessage::Flush(Arc::new(log_path.clone()), flush_tx))?;
        flush_rx.await??;

        let content = fs::read_to_string(&log_path)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(json["version"], LOG_VERSION);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 1); // Should only have the new entry
        assert_eq!(data[0]["timestamp"], 3000);

        Ok(())
    }
}
