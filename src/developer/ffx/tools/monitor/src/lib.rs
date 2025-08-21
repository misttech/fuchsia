// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;
use args::{MonitorCommand, SubCommand};

use anyhow::{Context, Result};
use async_trait::async_trait;
use ffx_config::EnvironmentContext;
use fho::{FfxMain, FfxTool};
use hyper::service::service_fn;
use hyper::{Body, Request, Response, StatusCode};
use std::convert::Infallible;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::process::Command;
use tokio::net::TcpListener;

use std::path::Path;

// Default value of this can be found in //src/developer/ffx/data/config.json
const CONFIG_PID_FILE: &str = "monitor.pid_file";

#[derive(FfxTool)]
pub struct MonitorTool {
    #[command]
    cmd: MonitorCommand,
    context: EnvironmentContext,
}

async fn start_server(addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await.context("binding to address")?;
    loop {
        let (stream, _) = listener.accept().await.context("accepting connection")?;
        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::Http::new()
                .serve_connection(stream, service_fn(handle_request))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn handle_request(req: Request<Body>) -> std::result::Result<Response<Body>, Infallible> {
    let mut response = Response::new("".into());
    match req.uri().path() {
        "/status" => {
            *response.status_mut() = StatusCode::OK;
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
        match self.cmd.subcommand {
            SubCommand::Start(start_cmd) => {
                let pid = std::process::id();
                if let Some(parent) = Path::new(&pid_file_path).parent() {
                    fs::create_dir_all(parent).context("creating pid file directory")?;
                }
                fs::write(&pid_file_path, pid.to_string()).context("writing pid file")?;

                let addr = SocketAddr::from(([127, 0, 0, 1], start_cmd.port));
                writeln!(writer, "Starting server on http://{} with pid {}", addr, pid)
                    .context("writing start message")?;

                start_server(addr).await.map_err(fho::Error::from)
            }
            SubCommand::Stop(_stop_cmd) => {
                let pid_str = fs::read_to_string(&pid_file_path).context("reading pid file")?;
                let pid: i32 = pid_str.trim().parse().context("parsing pid")?;

                writeln!(writer, "Stopping server with pid {}", pid)
                    .context("writing stop message")?;
                Command::new("kill").arg(pid.to_string()).status().context("killing process")?;
                fs::remove_file(pid_file_path).context("removing pid file")?;
                Ok(())
            }
        }
    }
}
