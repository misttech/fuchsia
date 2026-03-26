// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fetch::{FetchError, fetch_result_short_name};
use crate::ping::{PingError, ping_result_short_name};
use crate::telemetry::processors::InterfaceIdentifier;
use crate::{FetchParameters, LinkState, PingParameters, Proto};
use fuchsia_inspect::Node;
use fuchsia_inspect_contrib::inspect_log;
use fuchsia_inspect_contrib::nodes::{BoundedListNode, DedupeLogNode};
use fuchsia_inspect_derive::Unit;

// Keep only the 50 most recent events.
const INSPECT_LOG_WINDOW_SIZE: usize = 50;

/// Maintains the Inspect information.
pub(crate) struct InspectInfo {
    _node: Node,
    v4: BoundedListNode,
    v6: BoundedListNode,
}

impl InspectInfo {
    /// Create inspect node `id` rooted at `n`.
    pub(crate) fn new(n: &Node, id: &str, name: &str) -> Self {
        let node = n.create_child(id);
        node.record_string("name", name);
        let mut v4 = BoundedListNode::new(node.create_child("IPv4"), INSPECT_LOG_WINDOW_SIZE);
        inspect_log!(v4, state: "None");
        let mut v6 = BoundedListNode::new(node.create_child("IPv6"), INSPECT_LOG_WINDOW_SIZE);
        inspect_log!(v6, state: "None");

        InspectInfo { _node: node, v4, v6 }
    }
    pub(crate) fn log_link_state(&mut self, proto: Proto, link_state: LinkState) {
        match proto {
            Proto::IPv4 => inspect_log!(self.v4, state: format!("{:?}", link_state)),
            Proto::IPv6 => inspect_log!(self.v6, state: format!("{:?}", link_state)),
        }
    }
}

pub(crate) struct PerIfaceIdentifierInspectInfo {
    _node: Node,
    v4: PerIfaceIdentifierInspectEvents,
    v6: PerIfaceIdentifierInspectEvents,
}

impl PerIfaceIdentifierInspectInfo {
    pub(crate) fn new(n: &Node, iface_identifier: &InterfaceIdentifier) -> Self {
        let node = n.create_child(format!("{}", iface_identifier));
        let v4 = PerIfaceIdentifierInspectEvents::new(node.create_child("v4"));
        let v6 = PerIfaceIdentifierInspectEvents::new(node.create_child("v6"));

        PerIfaceIdentifierInspectInfo { _node: node, v4, v6 }
    }

    pub(crate) fn log_gateway_ping_result(
        &mut self,
        ping_parameters: &PingParameters,
        result: &Result<(), PingError>,
    ) {
        let events = match ping_parameters.addr.is_ipv4() {
            true => &mut self.v4,
            false => &mut self.v6,
        };
        events.log_gateway_ping_result(ping_parameters, result);
    }

    pub(crate) fn log_internet_ping_result(
        &mut self,
        ping_parameters: &PingParameters,
        result: &Result<(), PingError>,
    ) {
        let events = match ping_parameters.addr.is_ipv4() {
            true => &mut self.v4,
            false => &mut self.v6,
        };
        events.log_internet_ping_result(ping_parameters, result);
    }

    pub(crate) fn log_fetch_result(
        &mut self,
        fetch_parameters: &FetchParameters,
        result: &Result<u16, FetchError>,
    ) {
        let events = match fetch_parameters.ip.is_ipv4() {
            true => &mut self.v4,
            false => &mut self.v6,
        };
        events.log_fetch_result(fetch_parameters, result);
    }
}

#[derive(PartialEq, Eq, Unit)]
struct PingResult {
    address: String,
    interface_name: String,
    result: String,
}

#[derive(PartialEq, Eq, Unit)]
struct FetchResult {
    host_and_path: String,
    resolved_address: String,
    interface_name: String,
    result: String,
}

pub(crate) struct PerIfaceIdentifierInspectEvents {
    _node: Node,
    gateway_ping_results: DedupeLogNode<PingResult>,
    internet_ping_results: DedupeLogNode<PingResult>,
    fetch_results: DedupeLogNode<FetchResult>,
}

impl PerIfaceIdentifierInspectEvents {
    pub(crate) fn new(node: Node) -> Self {
        // Gateway and internet ping results are separated to allow for better deduping.
        let gateway_ping_results =
            DedupeLogNode::new(node.create_child("gateway_ping_results"), INSPECT_LOG_WINDOW_SIZE);
        let internet_ping_results =
            DedupeLogNode::new(node.create_child("internet_ping_results"), INSPECT_LOG_WINDOW_SIZE);
        let fetch_results =
            DedupeLogNode::new(node.create_child("fetch_results"), INSPECT_LOG_WINDOW_SIZE);

        PerIfaceIdentifierInspectEvents {
            _node: node,
            gateway_ping_results,
            internet_ping_results,
            fetch_results,
        }
    }

    /// Example of gateway ping result logged:
    /// ```
    /// gateway_ping_results:
    ///   0:
    ///     Created@time = 98528289843
    ///     LastSeen@time = 487674877812
    ///     count = 8
    ///     log:
    ///       address = 192.168.1.1:0
    ///       interface_name = wlan
    ///       result = Success
    /// ```
    pub(crate) fn log_gateway_ping_result(
        &mut self,
        ping_parameters: &PingParameters,
        result: &Result<(), PingError>,
    ) {
        Self::log_ping_result(&mut self.gateway_ping_results, ping_parameters, result);
    }

    /// Example of internet ping result logged:
    /// ```
    /// internet_ping_results:
    ///   0:
    ///     Created@time = 98528289843
    ///     LastSeen@time = 487674877812
    ///     count = 8
    ///     log:
    ///       address = 8.8.8.8:0
    ///       interface_name = wlan
    ///       result = Success
    /// ```
    pub(crate) fn log_internet_ping_result(
        &mut self,
        ping_parameters: &PingParameters,
        result: &Result<(), PingError>,
    ) {
        Self::log_ping_result(&mut self.internet_ping_results, ping_parameters, result);
    }

    fn log_ping_result(
        ping_results_node: &mut DedupeLogNode<PingResult>,
        ping_parameters: &PingParameters,
        result: &Result<(), PingError>,
    ) {
        let ping_result = PingResult {
            address: format!("{}", ping_parameters.addr),
            interface_name: ping_parameters.interface_name.clone(),
            result: ping_result_short_name(result),
        };
        ping_results_node.insert(ping_result);
    }

    /// Example of fetch result logged:
    /// ```
    /// fetch_results:
    ///   0:
    ///     Created@time = 128089098645
    ///     LastSeen@time = 487719914947
    ///     count = 7
    ///     log:
    ///       host_and_path = www.gstatic.com/generate_204
    ///       resolved_address = [IP_ADDRESS]
    ///       interface_name = wlan
    ///       result = Completed_204
    /// ```
    pub(crate) fn log_fetch_result(
        &mut self,
        fetch_parameters: &FetchParameters,
        result: &Result<u16, FetchError>,
    ) {
        let fetch_result = FetchResult {
            host_and_path: format!("{}{}", fetch_parameters.domain, fetch_parameters.path),
            resolved_address: format!("{}", fetch_parameters.ip),
            interface_name: fetch_parameters.interface_name.clone(),
            result: fetch_result_short_name(result),
        };
        self.fetch_results.insert(fetch_result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::processors::InterfaceType;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use fuchsia_inspect::Inspector;

    const IPV4_ADDR: std::net::IpAddr =
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 1));
    const IPV6_ADDR: std::net::IpAddr =
        std::net::IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0));

    #[test]
    fn test_log_state() {
        let mut executor = fuchsia_async::TestExecutor::new();
        let inspector = Inspector::default();
        let mut i = InspectInfo::new(inspector.root(), "id", "myname");
        assert_data_tree!(@executor executor, inspector, root: contains {
            id: {
                name:"myname",
                IPv4:{"0": contains {
                    state: "None"
                }
                },
                IPv6:{"0": contains {
                    state: "None"
                }
                }
            }
        });

        i.log_link_state(Proto::IPv4, LinkState::Internet);
        assert_data_tree!(@executor executor, inspector, root: contains {
            id: {
                name:"myname",
                IPv4:{"0": contains {
                    state: "None"
                },
                "1": contains {
                    state: "Internet"
                }
                },
                IPv6:{"0": contains {
                    state: "None"
                }
                }
            }
        });
        i.log_link_state(Proto::IPv4, LinkState::Gateway);
        i.log_link_state(Proto::IPv6, LinkState::Local);
        assert_data_tree!(@executor executor, inspector, root: contains {
            id: {
                name:"myname",
                IPv4:{"0": contains {
                    state: "None"
                },
                "1": contains {
                    state: "Internet"
                },
                "2": contains {
                    state: "Gateway"
                }
                },
                IPv6:{"0": contains {
                    state: "None"
                },
                "1": contains {
                    state: "Local"
                }
                }
            }
        });
    }

    #[test]
    fn test_per_iface_identifier_inspect_info_log_ping_results() {
        let mut executor = fuchsia_async::TestExecutor::new();
        let inspector = Inspector::default();
        let iface_identifier = InterfaceIdentifier::Type(InterfaceType::WlanClient);
        let mut i = PerIfaceIdentifierInspectInfo::new(inspector.root(), &iface_identifier);

        let ping_parameters_v4 = PingParameters {
            interface_name: "test_if".to_string(),
            addr: std::net::SocketAddr::new(IPV4_ADDR.into(), 80),
        };
        let ping_parameters_v6 = PingParameters {
            interface_name: "test_if".to_string(),
            addr: std::net::SocketAddr::new(IPV6_ADDR.into(), 80),
        };

        i.log_gateway_ping_result(&ping_parameters_v4, &Ok(()));
        i.log_internet_ping_result(&ping_parameters_v6, &Err(PingError::NoReply));

        assert_data_tree!(@executor executor, inspector, root: contains {
            "TYPE_wlanclient": {
                v4: {
                    gateway_ping_results: {
                        "0": contains {
                            "Created@time": AnyProperty,
                            count: 1u64,
                            log: {
                                address: "192.168.0.1:80",
                                interface_name: "test_if",
                                result: "Success",
                            }
                        }
                    },
                    internet_ping_results: {},
                    fetch_results: {}
                },
                v6: {
                    gateway_ping_results: {},
                    internet_ping_results: {
                        "0": contains {
                            "Created@time": AnyProperty,
                            count: 1u64,
                            log: {
                                address: "[2001:db8::]:80",
                                interface_name: "test_if",
                                result: "e_NoReply",
                            }
                        }
                    },
                    fetch_results: {}
                }
            }
        });
    }

    #[test]
    fn test_per_iface_identifier_inspect_info_log_fetch_results() {
        let mut executor = fuchsia_async::TestExecutor::new();
        let inspector = Inspector::default();
        let iface_identifier = InterfaceIdentifier::Type(InterfaceType::WlanClient);
        let mut i = PerIfaceIdentifierInspectInfo::new(inspector.root(), &iface_identifier);

        let fetch_parameters_v4 = FetchParameters {
            interface_name: "test_if".to_string(),
            domain: "example.com".to_string(),
            ip: IPV4_ADDR.into(),
            path: "/".to_string(),
            expected_statuses: vec![204],
        };

        // test log_fetch_result for v4
        i.log_fetch_result(&fetch_parameters_v4, &Ok(204));

        assert_data_tree!(@executor executor, inspector, root: contains {
            "TYPE_wlanclient": {
                v4: {
                    gateway_ping_results: {},
                    internet_ping_results: {},
                    fetch_results: {
                        "0": contains {
                            "Created@time": AnyProperty,
                            count: 1u64,
                            log: {
                                host_and_path: "example.com/",
                                interface_name: "test_if",
                                resolved_address: "192.168.0.1",
                                result: "Completed_204",
                            }
                        }
                    }
                },
                v6: {
                    gateway_ping_results: {},
                    internet_ping_results: {},
                    fetch_results: {}
                }
            }
        });
    }
}
