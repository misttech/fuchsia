// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;
use args::{MonitorCommand, SubCommand};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ffx_config::EnvironmentContext;
use fho::{FfxMain, FfxTool};
use fidl_fuchsia_developer_ffx::{RemoteControlState, TargetInfo, TargetState};
use hyper::service::service_fn;
use hyper::{Body, Request, Response, StatusCode};
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::spawn_blocking;

use crate::args::StartCommand;

// Default value of these can be found in //src/developer/ffx/data/config.json
const CONFIG_PID_FILE: &str = "monitor.pid_file";
const CONFIG_PORT: &str = "monitor.port";

const LOCAL_SERVER_IP_ADDRESS: &str = "127.0.0.1";
const LOCAL_SERVER_IP_ADDRESS_ARRAY: [u8; 4] = [127, 0, 0, 1];

#[derive(FfxTool)]
pub struct MonitorTool {
    #[command]
    cmd: MonitorCommand,

    context: EnvironmentContext,
}

#[derive(Debug, PartialEq)]
struct TargetStatus {
    name: Option<String>,
    status: Option<TargetState>,
    timestamp: DateTime<Utc>,
    rcs_state: Option<RemoteControlState>,
    product_config: Option<String>,
    board_config: Option<String>,
    serial_number: Option<String>,
    ssh_address: Option<String>,
    ssh_host_address: Option<String>,
}

impl Serialize for TargetStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("TargetStatus", 9)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("status", &self.status.as_ref().map(|val| format!("{:?}", val)))?;
        s.serialize_field("timestamp", &self.timestamp.to_rfc3339())?;
        s.serialize_field("rcs_state", &self.rcs_state.as_ref().map(|val| format!("{:?}", val)))?;
        s.serialize_field("product_config", &self.product_config)?;
        s.serialize_field("board_config", &self.board_config)?;
        s.serialize_field("serial_number", &self.serial_number)?;
        s.serialize_field("ssh_address", &self.ssh_address)?;
        s.serialize_field("ssh_host_address", &self.ssh_host_address)?;
        s.end()
    }
}

type Cache = Arc<Mutex<HashMap<String, serde_json::Value>>>;

async fn start_server(addr: SocketAddr, cmd: StartCommand) -> anyhow::Result<()> {
    let cache = Arc::new(Mutex::new(HashMap::new()));

    let cache_for_task = cache.clone();
    tokio::spawn(async move {
        loop {
            let cmd_clone = cmd.clone();
            let res = spawn_blocking(|| {
                let context = ffx_config::global_env_context()
                    .context("loading global environment context")
                    .unwrap();
                fuchsia_async::LocalExecutor::new()
                    .run_singlethreaded(collect_target_status(&context, cmd_clone))
            })
            .await;

            match res {
                Ok(Ok(statuses)) => {
                    let mut cache_lock = cache_for_task.lock().await;
                    let json_value = serde_json::to_value(&statuses).unwrap();
                    cache_lock.insert("targets".to_owned(), json_value);
                    log::debug!("Successfully updated target status cache {:?}", cache_lock);
                }
                Ok(Err(e)) => {
                    eprintln!("Error collecting target status: {:?}", e);
                }
                Err(e) => {
                    eprintln!("Task panicked while collecting target status: {:?}", e);
                }
            }
        }
    });

    let listener = TcpListener::bind(addr).await.context("binding to address")?;
    loop {
        let (stream, _) = listener.accept().await.context("accepting connection")?;
        let cache_for_handler = cache.clone();
        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::Http::new()
                .serve_connection(
                    stream,
                    service_fn(move |req| handle_request(req, cache_for_handler.clone())),
                )
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// Converts a vector of TargetInfo into a vector of TargetStatus.
fn infos_to_statuses(infos: Vec<TargetInfo>) -> Vec<TargetStatus> {
    let now = Utc::now();
    infos
        .into_iter()
        .map(|info| TargetStatus {
            name: info.nodename,
            status: info.target_state,
            timestamp: now,
            rcs_state: info.rcs_state,
            product_config: info.product_config,
            board_config: info.board_config,
            serial_number: info.serial_number,
            ssh_address: info.ssh_address.map(|addr| format!("{:?}", addr)),
            ssh_host_address: info.ssh_host_address.map(|addr| format!("{:?}", addr)),
        })
        .collect()
}

async fn collect_target_status(
    context: &EnvironmentContext,
    cmd: StartCommand,
) -> Result<Vec<TargetStatus>> {
    let infos = ffx_target::list_targets(
        context,
        cmd.nodename.clone(),
        !cmd.no_usb,
        !cmd.no_mdns,
        !cmd.no_probe,
        true,
    )
    .await?;
    Ok(infos_to_statuses(infos))
}

async fn handle_request(
    req: Request<Body>,
    cache: Cache,
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
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    };
    Ok(response)
}

#[async_trait(?Send)]
impl FfxMain for MonitorTool {
    type Writer = ffx_writer::SimpleWriter;
    async fn main(self, mut writer: <Self as FfxMain>::Writer) -> fho::Result<()> {
        let pid_file_path: String = self
            .context
            .get(CONFIG_PID_FILE)
            .map_err(|e| fho::Error::from(anyhow::anyhow!("Failed to get pid file path: {}", e)))?;
        let port: u16 = self
            .context
            .get(CONFIG_PORT)
            .map_err(|e| fho::Error::from(anyhow::anyhow!("Failed to get port: {}", e)))?;
        match self.cmd.subcommand {
            SubCommand::Start(cmd) => {
                let pid = std::process::id();
                if let Some(parent) = Path::new(&pid_file_path).parent() {
                    fs::create_dir_all(parent).context("creating pid file directory")?;
                }
                fs::write(&pid_file_path, pid.to_string()).context("writing pid file")?;

                let addr = SocketAddr::from((LOCAL_SERVER_IP_ADDRESS_ARRAY, port));
                writeln!(writer, "Starting server on http://{} with pid {}", addr, pid)
                    .context("writing start message")?;

                start_server(addr, cmd).await.map_err(fho::Error::from)
            }
            SubCommand::Stop(_) => {
                let pid_str = fs::read_to_string(&pid_file_path).context("reading pid file")?;
                let pid: i32 = pid_str.trim().parse().context("parsing pid")?;

                writeln!(writer, "Stopping server with pid {}", pid)
                    .context("writing stop message")?;
                Command::new("kill").arg(pid.to_string()).status().context("killing process")?;
                fs::remove_file(pid_file_path).context("removing pid file")?;
                Ok(())
            }
            SubCommand::Status(_) => {
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
                let pretty_json = serde_json::to_string_pretty(&json).context("formatting json")?;
                writeln!(writer, "{}", pretty_json).context("writing response to writer")?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_infos_to_statuses() {
        let infos = vec![
            TargetInfo {
                nodename: Some("fuchsia-one".to_string()),
                target_state: Some(TargetState::Product),
                rcs_state: Some(RemoteControlState::Up),
                ..Default::default()
            },
            TargetInfo {
                nodename: Some("fuchsia-two".to_string()),
                target_state: Some(TargetState::Fastboot),
                rcs_state: Some(RemoteControlState::Down),
                ..Default::default()
            },
        ];

        let statuses = infos_to_statuses(infos);

        let expected = vec![
            TargetStatus {
                name: Some("fuchsia-one".to_string()),
                status: Some(TargetState::Product),
                timestamp: statuses[0].timestamp,
                rcs_state: Some(RemoteControlState::Up),
                product_config: None,
                board_config: None,
                serial_number: None,
                ssh_address: None,
                ssh_host_address: None,
            },
            TargetStatus {
                name: Some("fuchsia-two".to_string()),
                status: Some(TargetState::Fastboot),
                timestamp: statuses[1].timestamp,
                rcs_state: Some(RemoteControlState::Down),
                product_config: None,
                board_config: None,
                serial_number: None,
                ssh_address: None,
                ssh_host_address: None,
            },
        ];

        assert_eq!(statuses, expected);
    }
}
