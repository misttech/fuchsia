// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetEvent;
use crate::error::Error;
use crate::events::{FastbootConnectionState, FastbootTargetState, TargetHandle, TargetState};
use addr::TargetIpAddr;
use fastboot::command::{ClientVariable, Command};
use fastboot_file_discovery::{
    FastbootEvent, FastbootEventHandler, FastbootFileWatcher, FastbootMode, get_fastboot_devices,
};
use ffx_config::EnvironmentContext;
use ffx_fastboot_transport_interface::{tcp, udp};
use futures::channel::mpsc::UnboundedSender;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn get_serial_number(
    context: &EnvironmentContext,
    mode: FastbootMode,
    addr: SocketAddr,
) -> Option<String> {
    let timeout = Duration::from_millis(context.get("discovery.fastboot.timeout").unwrap_or(500));
    let chrono_timeout = chrono::Duration::from_std(timeout).unwrap();
    let ctx = fastboot::FastbootContext::new();
    let command = Command::GetVar(ClientVariable::SerialNumber);

    let res = match mode {
        FastbootMode::TCP => {
            let mut interface = match tcp::open_once(&addr, timeout).await {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("Failed to open TCP Fastboot interface for {}: {}", addr, e);
                    return None;
                }
            };
            match fastboot::send_with_timeout(ctx, command.clone(), &mut interface, chrono_timeout)
                .await
            {
                Ok(res) => Some(res),
                Err(e) => {
                    log::warn!("Fastboot TCP command failed for {}: {}", addr, e);
                    None
                }
            }
        }
        FastbootMode::UDP => {
            let mut interface = match udp::open(addr).await {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("Failed to open UDP Fastboot interface for {}: {}", addr, e);
                    return None;
                }
            };
            match fastboot::send_with_timeout(ctx, command.clone(), &mut interface, chrono_timeout)
                .await
            {
                Ok(res) => Some(res),
                Err(e) => {
                    log::warn!("Fastboot UDP command failed for {}: {}", addr, e);
                    None
                }
            }
        }
    }?;

    match res {
        fastboot::reply::Reply::Okay(serial) => Some(serial),
        _ => None,
    }
}

#[derive(Clone)]
struct FastbootFileHandler {
    context: EnvironmentContext,
    sender: UnboundedSender<TargetEvent>,
    discovered_serials: Arc<Mutex<HashMap<SocketAddr, String>>>,
}

impl FastbootEventHandler for FastbootFileHandler {
    async fn handle_event(&mut self, event: FastbootEvent) {
        match event {
            FastbootEvent::Discovered(device) => {
                let serial_number =
                    get_serial_number(&self.context, device.mode(), device.socket_addr())
                        .await
                        .unwrap_or_else(String::new);
                if let Ok(mut serials) = self.discovered_serials.lock() {
                    serials.insert(device.socket_addr(), serial_number.clone());
                } else {
                    log::error!("Failed to lock discovered_serials map");
                }
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    FastbootMode::UDP => FastbootConnectionState::Udp(vec![address]),
                    FastbootMode::TCP => FastbootConnectionState::Tcp(vec![address]),
                };
                let handle = TargetHandle {
                    node_name: None,
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number,
                        connection_state,
                    }),
                    manual: false,
                };
                let _ = self.sender.unbounded_send(TargetEvent::Added(handle));
            }
            FastbootEvent::Lost(device) => {
                let serial_number = if let Ok(mut serials) = self.discovered_serials.lock() {
                    serials.remove(&device.socket_addr()).unwrap_or_else(String::new)
                } else {
                    log::error!("Failed to lock discovered_serials map");
                    String::new()
                };
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    FastbootMode::UDP => FastbootConnectionState::Udp(vec![address]),
                    FastbootMode::TCP => FastbootConnectionState::Tcp(vec![address]),
                };
                let handle = TargetHandle {
                    node_name: None,
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number,
                        connection_state,
                    }),
                    manual: false,
                };
                let _ = self.sender.unbounded_send(TargetEvent::Removed(handle));
            }
        }
    }
}

pub struct FastbootWatcher {
    _watcher: FastbootFileWatcher,
    _task: fuchsia_async::Task<()>,
}

impl FastbootWatcher {
    pub fn new(
        context: EnvironmentContext,
        instance_root: PathBuf,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self, Error> {
        let existing = get_fastboot_devices(&instance_root)
            .map_err(|err| Error::FastbootDiscovery { path: instance_root.clone(), err })?;

        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));

        // Spawn a task scoped to the struct's lifetime for pre-existing devices.
        let initial_handler = FastbootFileHandler {
            context: context.clone(),
            sender: sender.clone(),
            discovered_serials: discovered_serials.clone(),
        };
        let task = fuchsia_async::Task::local(async move {
            let futures = existing.into_iter().map(|device| {
                let mut handler = initial_handler.clone();
                async move {
                    let event = FastbootEvent::Discovered(device);
                    handler.handle_event(event).await;
                }
            });
            futures::future::join_all(futures).await;
        });

        // The async FastbootFileHandler processes subsequent file events natively.
        let handler = FastbootFileHandler { context, sender, discovered_serials };
        let watcher = fastboot_file_discovery::recommended_watcher(handler, instance_root.clone())
            .map_err(|e| Error::FastbootWatcher { path: instance_root, err: e.to_string() })?;

        Ok(Self { _watcher: watcher, _task: task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastboot_file_discovery::FastbootEntry;
    use fuchsia_async as fasync;
    use futures::StreamExt;

    #[fasync::run_singlethreaded(test)]
    async fn test_fastboot_file_handler_discovered() {
        let test_env = ffx_config::test_init().unwrap();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        let device: FastbootEntry = "udp:127.0.0.1:5555".parse().unwrap();
        let event = FastbootEvent::Discovered(device.clone());
        handler.handle_event(event).await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Added(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, ""); // get_serial_number returns None/"" since there is no test endpoint
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Udp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }

        assert_eq!(
            discovered_serials.lock().unwrap().get(&device.socket_addr()),
            Some(&"".to_string())
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_fastboot_file_handler_lost() {
        let test_env = ffx_config::test_init().unwrap();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        let device: FastbootEntry = "udp:127.0.0.1:5555".parse().unwrap();
        discovered_serials.lock().unwrap().insert(device.socket_addr(), "test_serial".to_string());

        let event = FastbootEvent::Lost(device.clone());
        handler.handle_event(event).await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Removed(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, "test_serial");
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Udp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }
    }
}
