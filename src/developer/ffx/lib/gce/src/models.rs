// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::{Deserialize, Serialize};

/// Represents a GCE virtual machine instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_interfaces: Vec<NetworkInterface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_link: Option<String>,
}

impl Instance {
    /// Extracts the internal primary IPv4 address, if assigned.
    pub fn internal_ip(&self) -> Option<&str> {
        self.network_interfaces.first().and_then(|nic| nic.network_ip.as_deref())
    }

    /// Extracts the external public IPv4 address, if assigned.
    pub fn external_ip(&self) -> Option<&str> {
        self.network_interfaces
            .first()
            .and_then(|nic| nic.access_configs.first())
            .and_then(|cfg| cfg.nat_ip.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    #[serde(rename = "networkIP", alias = "networkIp", skip_serializing_if = "Option::is_none")]
    pub network_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_configs: Vec<AccessConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessConfig {
    #[serde(rename = "natIP", alias = "natIp", skip_serializing_if = "Option::is_none")]
    pub nat_ip: Option<String>,
}

/// Represents the list response from GET /compute/v1/projects/{project}/zones/{zone}/instances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstanceList {
    #[serde(default)]
    pub items: Vec<Instance>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_instance_list() {
        let json_str = r#"{
            "items": [
                {
                    "name": "fuchsia-vm-1",
                    "machineType": "zones/us-central1-a/machineTypes/n2-standard-4",
                    "status": "RUNNING",
                    "networkInterfaces": [
                        {
                            "networkIP": "10.128.0.2",
                            "accessConfigs": [
                                {
                                    "natIP": "35.200.100.50"
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;

        let list: InstanceList = serde_json::from_str(json_str).expect("parsed instance list");
        assert_eq!(list.items.len(), 1);
        let inst = &list.items[0];
        assert_eq!(inst.name.as_deref(), Some("fuchsia-vm-1"));
        assert_eq!(inst.status.as_deref(), Some("RUNNING"));
        assert_eq!(inst.internal_ip(), Some("10.128.0.2"));
        assert_eq!(inst.external_ip(), Some("35.200.100.50"));
    }

    #[test]
    fn test_empty_instance_list() {
        let json_str = r#"{}"#;
        let list: InstanceList = serde_json::from_str(json_str).expect("parsed empty list");
        assert!(list.items.is_empty());
    }
}
