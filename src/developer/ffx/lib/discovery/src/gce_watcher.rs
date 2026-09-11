// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetEvent;
use crate::error::Error;
use crate::events::{TargetHandle, TargetState};
use crate::instance_watcher::{InstanceSource, InstanceWatcher, is_pid_running};
use addr::TargetAddr;
use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GceInstanceData {
    #[serde(alias = "instance")]
    pub instance_name: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub zone: String,
    pub pid: u32,
    #[serde(alias = "port")]
    pub ssh_port: u16,
    #[serde(default)]
    pub reverse_ports: Vec<u16>,
    #[serde(default)]
    pub serial_number: Option<String>,
}

impl GceInstanceData {
    pub fn is_running(&self) -> bool {
        is_pid_running(self.pid)
    }

    pub fn to_target_handle(&self) -> Option<TargetHandle> {
        if !self.is_running() || self.ssh_port == 0 {
            return None;
        }
        let sock_addr = SocketAddr::from(([127, 0, 0, 1], self.ssh_port));
        Some(TargetHandle {
            node_name: Some(self.instance_name.clone()),
            state: TargetState::Product {
                addrs: vec![TargetAddr::Net(sock_addr)],
                serial: self.serial_number.clone(),
            },
            manual: false,
        })
    }
}

pub fn read_instance_file(path: &Path) -> Option<GceInstanceData> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn get_all_gce_targets(instance_root: &Path) -> Vec<TargetHandle> {
    GceSource.get_all_targets(instance_root)
}

pub struct GceSource;

impl InstanceSource for GceSource {
    fn recursive(&self) -> bool {
        false
    }

    fn instance_name_from_path(&self, instance_root: &Path, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(instance_root).ok()?;
        if rel.parent() != Some(Path::new("")) {
            return None;
        }
        if rel.extension() != Some(std::ffi::OsStr::new("json")) {
            return None;
        }
        let name = rel.file_stem()?.to_str()?;
        if name.is_empty() { None } else { Some(name.to_string()) }
    }

    fn read_target_handle(&self, _instance_root: &Path, path: &Path) -> Option<TargetHandle> {
        read_instance_file(path)?.to_target_handle()
    }

    fn get_all_targets(&self, instance_root: &Path) -> Vec<TargetHandle> {
        let mut targets = Vec::new();
        let Ok(entries) = std::fs::read_dir(instance_root) else {
            return targets;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.instance_name_from_path(instance_root, &path).is_some() {
                if let Some(handle) = self.read_target_handle(instance_root, &path) {
                    targets.push(handle);
                }
            }
        }
        targets
    }
}

pub struct GceWatcher {
    _watcher: InstanceWatcher,
}

impl GceWatcher {
    pub fn new(
        instance_root: PathBuf,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self, Error> {
        let watcher = InstanceWatcher::new(instance_root, sender, GceSource, |path, err| {
            Error::GceWatcher { path, err }
        })?;
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn test_read_instance_file_and_to_target_handle() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("my-test-vm.json");
        let data = serde_json::json!({
            "instance_name": "my-test-vm",
            "project": "test-project",
            "zone": "us-central1-a",
            "pid": std::process::id(),
            "ssh_port": 2222,
            "serial_number": "GC-TESTSERIAL",
        });
        std::fs::write(&file_path, serde_json::to_string(&data).unwrap()).unwrap();

        let instance = read_instance_file(&file_path).expect("instance parsed");
        assert_eq!(instance.instance_name, "my-test-vm");
        assert_eq!(instance.ssh_port, 2222);
        assert_eq!(instance.serial_number.as_deref(), Some("GC-TESTSERIAL"));
        assert!(instance.is_running());

        let handle = instance.to_target_handle().expect("target handle created");
        assert_eq!(handle.node_name.as_deref(), Some("my-test-vm"));
        assert!(!handle.manual);
        match handle.state {
            TargetState::Product { addrs, serial } => {
                assert_eq!(serial.as_deref(), Some("GC-TESTSERIAL"));
                assert_eq!(addrs, vec![TargetAddr::Net(SocketAddr::from(([127, 0, 0, 1], 2222)))]);
            }
            _ => panic!("expected Product target state"),
        }
    }

    #[test]
    fn test_stopped_instance_returns_none() {
        let data = GceInstanceData {
            instance_name: "dead-vm".to_string(),
            project: "test-project".to_string(),
            zone: "us-central1-a".to_string(),
            pid: 0,
            ssh_port: 2222,
            reverse_ports: vec![],
            serial_number: None,
        };
        assert!(!data.is_running());
        assert!(data.to_target_handle().is_none());
    }

    #[fuchsia::test]
    async fn test_gce_watcher_drain_and_watch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let file_path = root.join("running-vm.json");
        let data = serde_json::json!({
            "instance_name": "running-vm",
            "pid": std::process::id(),
            "ssh_port": 3333,
            "serial_number": "GC-RUNNING123",
        });
        std::fs::write(&file_path, serde_json::to_string(&data).unwrap()).unwrap();

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _watcher = GceWatcher::new(root.clone(), tx).expect("watcher created");

        // Should get the existing running instance right away
        let initial_event = rx.next().await.expect("received initial event");
        match initial_event {
            TargetEvent::Added(h) => {
                assert_eq!(h.node_name.as_deref(), Some("running-vm"));
                assert_eq!(
                    h.state,
                    TargetState::Product {
                        addrs: vec![TargetAddr::Net(SocketAddr::from(([127, 0, 0, 1], 3333)))],
                        serial: Some("GC-RUNNING123".to_string()),
                    }
                );
            }
            _ => panic!("expected Added event"),
        }

        // Delete the file
        std::fs::remove_file(&file_path).unwrap();

        // Should receive Removed event
        let remove_event = rx.next().await.expect("received remove event");
        match remove_event {
            TargetEvent::Removed(h) => {
                assert_eq!(h.node_name.as_deref(), Some("running-vm"));
            }
            _ => panic!("expected Removed event"),
        }
    }
}
