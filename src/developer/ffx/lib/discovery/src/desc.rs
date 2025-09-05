// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use addr::TargetAddr;

use crate::{FastbootConnectionState, FastbootTargetState, TargetHandle, TargetState};

#[derive(Debug, Hash, Copy, Clone, PartialEq, Eq)]
pub enum FastbootInterface {
    Usb,
    Udp,
    Tcp,
}

/// Represents a target description, e.g. as produced in events within the daemon
#[derive(Debug, Default, Hash, Clone, PartialEq, Eq)]
pub struct Description {
    pub nodename: Option<String>,
    pub addresses: Vec<TargetAddr>,
    pub serial: Option<String>,
    pub ssh_port: Option<u16>,
    pub fastboot_interface: Option<FastbootInterface>,
    // So far this is only used in testing. It's unclear what the reasoning is
    // for the SSH host address being stored as a string rather than a struct
    // elsewhere in the code, so this is being done for the sake of congruity.
    // TODO(b/327682973): Use a real address here or delete this.
    pub ssh_host_address: Option<String>,
}

impl From<&TargetHandle> for Description {
    fn from(value: &TargetHandle) -> Self {
        let (addresses, serial) = match &value.state {
            TargetState::Product { addrs: target_addr, .. } => (target_addr.clone(), None),
            TargetState::Fastboot(FastbootTargetState { serial_number: sn, connection_state }) => {
                let addresses = match connection_state {
                    FastbootConnectionState::Usb => Vec::<TargetAddr>::new(),
                    FastbootConnectionState::Tcp(addresses)
                    | FastbootConnectionState::Udp(addresses) => {
                        addresses.iter().map(Into::into).collect()
                    }
                };
                (addresses, Some(sn.clone()))
            }
            _ => (vec![], None),
        };
        Self { nodename: value.node_name.clone(), addresses, serial, ..Default::default() }
    }
}
