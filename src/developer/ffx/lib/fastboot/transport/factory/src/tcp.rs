// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::analytics::PointOfFailure;
use crate::helpers::{rediscover_helper, verify_fastboot_live};
use anyhow::{Context as _, Result};
use discovery::{FastbootConnectionState, TargetHandle, TargetState};
use ffx_config::EnvironmentContext;
use ffx_diagnostics_analytics::{ResultExt, mark_point_of_failure};
use ffx_fastboot_interface::interface_factory::{
    InterfaceFactory, InterfaceFactoryBase, InterfaceFactoryError,
};
use ffx_fastboot_transport_interface::tcp::{TcpNetworkInterface, open_once};
use fuchsia_async::Timer;

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
    open_retries: Option<u64>,
    retry_wait_seconds: u64,
    context: EnvironmentContext,
}

impl TcpFactory {
    pub fn new(
        context: &EnvironmentContext,
        target_name: String,
        fastboot_devices_file_path: Option<PathBuf>,
        addr: SocketAddr,
        open_retries: Option<u64>,
        retry_wait_seconds: u64,
    ) -> Self {
        Self {
            target_name,
            fastboot_devices_file_path,
            addr,
            open_retries,
            retry_wait_seconds,
            context: context.clone(),
        }
    }
}

impl Drop for TcpFactory {
    fn drop(&mut self) {
        futures::executor::block_on(async move {
            self.close().await;
        });
    }
}

impl InterfaceFactoryBase<TcpNetworkInterface<netext::MultithreadedTokioAsyncWrapper<TcpStream>>>
    for TcpFactory
{
    async fn open(
        &mut self,
    ) -> Result<
        TcpNetworkInterface<netext::MultithreadedTokioAsyncWrapper<TcpStream>>,
        InterfaceFactoryError,
    > {
        let wait_duration = Duration::from_secs(self.retry_wait_seconds);
        let mut try_count = 1;
        loop {
            if let Some(max_retries) = self.open_retries {
                if try_count > max_retries {
                    let err = InterfaceFactoryError::ConnectionError(
                        "TCP".to_string(),
                        self.addr,
                        max_retries,
                    );
                    mark_point_of_failure(PointOfFailure::FactoryOpenError("tcp".into(), &err))
                        .await;
                    break Err(err);
                }
            }
            try_count += 1;
            match open_once(&self.addr, Duration::from_secs(1)).await.with_context(|| {
                format!("TCPFactory connecting via TCP to Fastboot address: {}", self.addr)
            }) {
                Err(e) => {
                    log::debug!(
                        "Attempt {}. Got error connecting to fastboot address: {}",
                        try_count - 1,
                        e,
                    );

                    Timer::new(wait_duration).await;
                }
                Ok(mut interface) => {
                    if verify_fastboot_live(&mut interface, &self.addr).await {
                        return Ok(interface);
                    }
                    log::debug!(
                        "Attempt {}. Fastboot target at {} completed TCP handshake \
                         but is not yet responsive over Fastboot protocol; retrying",
                        try_count - 1,
                        self.addr,
                    );
                    drop(interface);
                    Timer::new(wait_duration).await;
                    let _ = self.rediscover().await;
                }
            }
        }
    }

    async fn close(&self) {
        log::debug!("Closing Fastboot TCP Factory for: {}", self.addr);
    }

    async fn rediscover(&mut self) -> Result<(), InterfaceFactoryError> {
        rediscover_helper(
            &self.context,
            &self.fastboot_devices_file_path,
            &self.target_name,
            filter_target,
            &mut |connection_state| {
                match connection_state {
                    FastbootConnectionState::Tcp(addrs) => {
                        self.addr = addrs.iter().find_map(|x| x.try_into().ok()).unwrap();
                    }
                    s @ _ => {
                        let err = InterfaceFactoryError::RediscoverTargetNotInCorrectTransport(
                            self.target_name.clone(),
                            "TCP".to_string(),
                            s.to_string(),
                        );
                        return Err(err);
                    }
                }
                Ok(())
            },
        )
        .await
        .map_err(Into::into)
        .or_else_analytics(|e| PointOfFailure::FactoryRediscoveryError("tcp".into(), e).into())
        .await
    }
}

impl InterfaceFactory<TcpNetworkInterface<netext::MultithreadedTokioAsyncWrapper<TcpStream>>>
    for TcpFactory
{
}

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

#[cfg(test)]
mod test {
    use super::*;
    use fastboot::command::{ClientVariable, Command};
    use fastboot::reply::Reply;
    use fastboot::{FastbootContext, send};
    use tempfile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[fuchsia::test]
    async fn test_tcp_factory_open_retries_when_pre_reboot_proxy_closes_after_handshake() {
        let env = ffx_config::test_env().build().expect("test env");
        let dir = tempfile::tempdir().expect("tempdir");
        let devices_path = dir.path().join("devices");

        // Pre-reboot listener (simulating pontisd before USB disconnect is noticed)
        let old_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind old listener");
        let old_addr = old_listener.local_addr().expect("old local addr");

        // Post-reboot listener (simulating pontisd after USB re-enumeration)
        let new_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind new listener");
        let new_addr = new_listener.local_addr().expect("new local addr");

        std::fs::write(&devices_path, format!("tcp:{old_addr}\n")).expect("write initial devices");
        let devices_path_for_server = devices_path.clone();

        let server_task = tokio::spawn(async move {
            // 1. Accept connection on old_listener, complete local FB01 handshake,
            //    read getvar:version, then update devices file and close socket (EOF)
            //    to simulate the remote USB device resetting.
            let (mut old_stream, _) = old_listener.accept().await.expect("accept old");
            let mut hs = [0u8; 4];
            old_stream.read_exact(&mut hs).await.expect("read old handshake");
            assert_eq!(&hs, b"FB01");
            old_stream.write_all(b"FB01").await.expect("write old handshake");

            let mut hdr = [0u8; 8];
            old_stream.read_exact(&mut hdr).await.expect("read old getvar header");
            let len = u64::from_be_bytes(hdr) as usize;
            let mut cmd = vec![0u8; len];
            old_stream.read_exact(&mut cmd).await.expect("read old getvar cmd");
            assert_eq!(&cmd, b"getvar:version");

            // Device finishes rebooting and re-registers at new_addr while old stream closes.
            std::fs::write(&devices_path_for_server, format!("tcp:{new_addr}\n"))
                .expect("update devices file");
            drop(old_stream);

            // 2. Accept connection(s) on new_listener, complete FB01 handshake,
            //    answer any discovery serialno probe, respond OKAY0.4 to getvar:version,
            //    and serve a follow-up command.
            loop {
                let (mut new_stream, _) = new_listener.accept().await.expect("accept new");
                new_stream.read_exact(&mut hs).await.expect("read new handshake");
                assert_eq!(&hs, b"FB01");
                new_stream.write_all(b"FB01").await.expect("write new handshake");

                new_stream.read_exact(&mut hdr).await.expect("read new command header");
                let len = u64::from_be_bytes(hdr) as usize;
                let mut cmd = vec![0u8; len];
                new_stream.read_exact(&mut cmd).await.expect("read new command");
                if &cmd == b"getvar:serialno" {
                    new_stream
                        .write_all(b"\x00\x00\x00\x00\x00\x00\x00\x0cOKAYtest-ser")
                        .await
                        .expect("write serialno reply");
                    continue;
                }
                assert_eq!(&cmd, b"getvar:version");
                new_stream
                    .write_all(b"\x00\x00\x00\x00\x00\x00\x00\x07OKAY0.4")
                    .await
                    .expect("write version reply");

                // Follow-up command on the opened interface
                new_stream.read_exact(&mut hdr).await.expect("read follow-up header");
                let len = u64::from_be_bytes(hdr) as usize;
                let mut cmd = vec![0u8; len];
                new_stream.read_exact(&mut cmd).await.expect("read follow-up cmd");
                assert_eq!(&cmd, b"getvar:product");
                new_stream
                    .write_all(b"\x00\x00\x00\x00\x00\x00\x00\x08OKAYiris")
                    .await
                    .expect("write product reply");
                break;
            }
        });

        let mut factory = TcpFactory::new(
            &env.context,
            old_addr.to_string(),
            Some(devices_path),
            old_addr,
            Some(3),
            0,
        );

        let mut interface = factory.open().await.expect("open should succeed on second attempt");
        assert_eq!(factory.addr, new_addr);

        let reply =
            send(FastbootContext::new(), Command::GetVar(ClientVariable::Product), &mut interface)
                .await
                .expect("follow-up command should succeed");
        assert_eq!(reply, Reply::Okay("iris".to_string()));

        server_task.await.expect("server task");
    }
}
