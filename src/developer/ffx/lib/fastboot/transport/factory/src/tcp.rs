// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::helpers::rediscover_helper;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use discovery::{FastbootConnectionState, TargetHandle, TargetState};
use ffx_fastboot_interface::interface_factory::{
    InterfaceFactory, InterfaceFactoryBase, InterfaceFactoryError,
};
use ffx_fastboot_transport_interface::tcp::{TcpNetworkInterface, open_once};
use fuchsia_async::Timer;
use netext::TokioAsyncWrapper;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpStream;

///////////////////////////////////////////////////////////////////////////////
// TcpFactory
//

#[derive(Debug, Clone)]
pub struct TcpFactory {
    target_name: String,
    fastboot_devices_file_path: Option<PathBuf>,
    addr: SocketAddr,
    open_retries: u64,
    retry_wait_seconds: u64,
}

impl TcpFactory {
    pub fn new(
        target_name: String,
        fastboot_devices_file_path: Option<PathBuf>,
        addr: SocketAddr,
        open_retries: u64,
        retry_wait_seconds: u64,
    ) -> Self {
        Self { target_name, fastboot_devices_file_path, addr, open_retries, retry_wait_seconds }
    }
}

impl Drop for TcpFactory {
    fn drop(&mut self) {
        futures::executor::block_on(async move {
            self.close().await;
        });
    }
}

#[async_trait(?Send)]
impl InterfaceFactoryBase<TcpNetworkInterface<TokioAsyncWrapper<TcpStream>>> for TcpFactory {
    async fn open(
        &mut self,
    ) -> Result<TcpNetworkInterface<TokioAsyncWrapper<TcpStream>>, InterfaceFactoryError> {
        let wait_duration = Duration::from_secs(self.retry_wait_seconds);
        for i in 1..self.open_retries {
            match open_once(&self.addr, Duration::from_secs(1)).await.with_context(|| {
                format!("TCPFactory connecting via TCP to Fastboot address: {}", self.addr)
            }) {
                Err(e) => {
                    log::debug!("Attempt {}. Got error connecting to fastboot address: {}", i, e,);

                    Timer::new(wait_duration).await;
                }
                Ok(interface) => return Ok(interface),
            }
        }
        Err(InterfaceFactoryError::ConnectionError("TCP".to_string(), self.addr, self.open_retries))
    }

    async fn close(&self) {
        log::debug!("Closing Fastboot TCP Factory for: {}", self.addr);
    }

    async fn rediscover(&mut self) -> Result<(), InterfaceFactoryError> {
        rediscover_helper(
            &self.fastboot_devices_file_path,
            &self.target_name,
            filter_target,
            &mut |connection_state| {
                match connection_state {
                    FastbootConnectionState::Tcp(addrs) => {
                        self.addr = addrs.iter().find_map(|x| x.try_into().ok()).unwrap();
                    }
                    s @ _ => {
                        return Err(InterfaceFactoryError::RediscoverTargetNotInCorrectTransport(
                            self.target_name.clone(),
                            "TCP".to_string(),
                            s.to_string(),
                        ));
                    }
                }
                Ok(())
            },
        )
        .await
    }
}

impl InterfaceFactory<TcpNetworkInterface<TokioAsyncWrapper<TcpStream>>> for TcpFactory {}

fn filter_target(handle: &TargetHandle) -> bool {
    match &handle.state {
        TargetState::Fastboot(ts)
            if matches!(ts.connection_state, FastbootConnectionState::Tcp(_)) =>
        {
            log::debug!("Filtered and found target handle: {}", handle);
            true
        }
        state @ _ => {
            log::debug!("Target state {} is not  TCP Fastboot... skipping", state);
            false
        }
    }
}
