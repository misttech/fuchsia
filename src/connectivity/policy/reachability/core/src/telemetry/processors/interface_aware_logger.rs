// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{InterfaceIdentifier, InterfaceTimeSeriesGrouping};

use fuchsia_inspect::Node as InspectNode;
use fuchsia_inspect_contrib::nodes::LruCacheNode;
use std::collections::HashMap;
use windowed_stats::experimental::inspect::{InspectSender, InspectedTimeMatrix, TimeMatrixClient};
use windowed_stats::experimental::series::interpolation::ConstantSample;
use windowed_stats::experimental::series::metadata::BitSetNode;
use windowed_stats::experimental::series::statistic::Union;
use windowed_stats::experimental::series::{SamplingProfile, TimeMatrix};

use crate::fetch::{FetchError, fetch_result_short_name};
use crate::ping::{PingError, ping_result_short_name};
use crate::{FetchParameters, IpVersions, PingParameters};

fn bitset_constant_sample_time_matrix(
    client: &TimeMatrixClient,
    time_series_name: &str,
    bitset_node: BitSetNode,
) -> InspectedTimeMatrix<u64> {
    client.inspect_time_matrix_with_metadata(
        time_series_name,
        TimeMatrix::<Union<u64>, ConstantSample>::new(
            SamplingProfile::highly_granular(),
            ConstantSample::default(),
        ),
        bitset_node,
    )
}

struct PerInterfaceTimeSeries {
    gateway_ping_result_time_matrix: IpVersions<InspectedTimeMatrix<u64>>,
    internet_ping_result_time_matrix: IpVersions<InspectedTimeMatrix<u64>>,
    fetch_result_time_matrix: IpVersions<InspectedTimeMatrix<u64>>,
    _interface_node: InspectNode,
    _v4_node: InspectNode,
    _v6_node: InspectNode,
}

impl PerInterfaceTimeSeries {
    pub fn new(
        parent_node: &InspectNode,
        inspect_metadata_path: &str,
        identifier: InterfaceIdentifier,
    ) -> Self {
        let interface_node = parent_node.create_child(format!("{}", identifier));
        let v4_node = interface_node.create_child("v4");
        let v6_node = interface_node.create_child("v6");

        let client_v4 = TimeMatrixClient::new(v4_node.clone_weak());
        let client_v6 = TimeMatrixClient::new(v6_node.clone_weak());

        let bitset_constant_sample_time_matrices = |name: &str, metadata_node_name: &str| {
            let bitset_node =
                BitSetNode::from_path(format!("{}/{}", inspect_metadata_path, metadata_node_name));
            IpVersions {
                ipv4: bitset_constant_sample_time_matrix(&client_v4, name, bitset_node.clone()),
                ipv6: bitset_constant_sample_time_matrix(&client_v6, name, bitset_node),
            }
        };

        Self {
            gateway_ping_result_time_matrix: bitset_constant_sample_time_matrices(
                "gateway_ping_results",
                InspectMetadataNode::PING_RESULTS,
            ),
            internet_ping_result_time_matrix: bitset_constant_sample_time_matrices(
                "internet_ping_results",
                InspectMetadataNode::PING_RESULTS,
            ),
            fetch_result_time_matrix: bitset_constant_sample_time_matrices(
                "fetch_results",
                InspectMetadataNode::FETCH_RESULTS,
            ),
            _interface_node: interface_node,
            _v4_node: v4_node,
            _v6_node: v6_node,
        }
    }

    fn log_gateway_ping_result(&self, ping_parameters: &PingParameters, result_bitmask: u64) {
        if ping_parameters.addr.is_ipv4() {
            self.gateway_ping_result_time_matrix.ipv4.fold_or_log_error(result_bitmask);
        } else {
            self.gateway_ping_result_time_matrix.ipv6.fold_or_log_error(result_bitmask);
        }
    }

    fn log_internet_ping_result(&self, ping_parameters: &PingParameters, result_bitmask: u64) {
        if ping_parameters.addr.is_ipv4() {
            self.internet_ping_result_time_matrix.ipv4.fold_or_log_error(result_bitmask);
        } else {
            self.internet_ping_result_time_matrix.ipv6.fold_or_log_error(result_bitmask);
        }
    }

    fn log_fetch_result(&self, fetch_parameters: &FetchParameters, result_bitmask: u64) {
        if fetch_parameters.ip.is_ipv4() {
            self.fetch_result_time_matrix.ipv4.fold_or_log_error(result_bitmask);
        } else {
            self.fetch_result_time_matrix.ipv6.fold_or_log_error(result_bitmask);
        }
    }
}

// The wrapper for time series reporting.
pub struct InterfaceAwareLogger {
    // Tracks the provided `InterfaceIdentifier`s against the time series for
    // that identifier. Entries are only created during initialization.
    time_series_stats: HashMap<InterfaceIdentifier, PerInterfaceTimeSeries>,
    inspect_metadata_node: InspectMetadataNode,
    _logger_node: InspectNode,
}

impl InterfaceAwareLogger {
    pub fn new(
        inspect_metadata_node: &InspectNode,
        inspect_metadata_path: &str,
        interface_grouping: InterfaceTimeSeriesGrouping,
        logger_node: InspectNode,
    ) -> Self {
        Self {
            // Create a time series per interface type provided.
            time_series_stats: match interface_grouping {
                InterfaceTimeSeriesGrouping::Type(tys) => tys.into_iter().map(|ty| {
                    let identifier = InterfaceIdentifier::Type(ty);
                    (
                        identifier.clone(),
                        PerInterfaceTimeSeries::new(
                            &logger_node,
                            inspect_metadata_path,
                            identifier,
                        ),
                    )
                }),
            }
            .collect(),
            inspect_metadata_node: InspectMetadataNode::new(inspect_metadata_node),
            _logger_node: logger_node,
        }
    }

    pub fn log_gateway_ping_result(
        &mut self,
        interface_identifiers: Vec<InterfaceIdentifier>,
        ping_parameters: &PingParameters,
        gateway_ping_result: &Result<(), PingError>,
    ) {
        self.log_ping_result(
            interface_identifiers,
            ping_parameters,
            gateway_ping_result,
            PerInterfaceTimeSeries::log_gateway_ping_result,
        );
    }

    pub fn log_internet_ping_result(
        &mut self,
        interface_identifiers: Vec<InterfaceIdentifier>,
        ping_parameters: &PingParameters,
        internet_ping_result: &Result<(), PingError>,
    ) {
        self.log_ping_result(
            interface_identifiers,
            ping_parameters,
            internet_ping_result,
            PerInterfaceTimeSeries::log_internet_ping_result,
        );
    }

    fn log_ping_result(
        &mut self,
        interface_identifiers: Vec<InterfaceIdentifier>,
        ping_parameters: &PingParameters,
        ping_result: &Result<(), PingError>,
        log_fn: fn(&PerInterfaceTimeSeries, &PingParameters, u64),
    ) {
        let result = ping_result_short_name(ping_result);
        let result_id = self.inspect_metadata_node.ping_result.insert(result);
        interface_identifiers.iter().for_each(|identifier| {
            if let Some(time_series) = self.time_series_stats.get(identifier) {
                log_fn(time_series, ping_parameters, 1 << result_id);
            }
        });
    }

    pub fn log_fetch_result(
        &mut self,
        interface_identifiers: Vec<InterfaceIdentifier>,
        fetch_parameters: &FetchParameters,
        fetch_result: &Result<u16, FetchError>,
    ) {
        let result = fetch_result_short_name(fetch_result);
        let result_id = self.inspect_metadata_node.fetch_result.insert(result);
        interface_identifiers.iter().for_each(|identifier| {
            if let Some(time_series) = self.time_series_stats.get(identifier) {
                time_series.log_fetch_result(fetch_parameters, 1 << result_id);
            }
        });
    }
}

const PING_RESULT_METADATA_CACHE_SIZE: usize = 32;
const FETCH_RESULT_METADATA_CACHE_SIZE: usize = 32;

// Holds the inspect node children for the static metadata that correlates to
// bits in each of the corresponding structs / enums.
struct InspectMetadataNode {
    ping_result: LruCacheNode<String>,
    fetch_result: LruCacheNode<String>,
}

impl InspectMetadataNode {
    const PING_RESULTS: &'static str = "ping_results";
    const FETCH_RESULTS: &'static str = "fetch_results";

    fn new(inspect_node: &InspectNode) -> Self {
        let ping_result = LruCacheNode::new(
            inspect_node.create_child(Self::PING_RESULTS),
            PING_RESULT_METADATA_CACHE_SIZE,
        );
        let fetch_result = LruCacheNode::new(
            inspect_node.create_child(Self::FETCH_RESULTS),
            FETCH_RESULT_METADATA_CACHE_SIZE,
        );

        Self { ping_result, fetch_result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyBytesProperty, AnyNumericProperty, assert_data_tree};
    use net_declare::{std_socket_addr_v4, std_socket_addr_v6};

    use crate::telemetry::processors::InterfaceType;
    use crate::telemetry::testing::setup_test;

    #[fuchsia::test]
    fn test_log_time_series_metadata_to_inspect() {
        let mut harness = setup_test();
        let logger_node = harness.inspect_node.create_child("interfaces");

        let _interface_aware_logger = InterfaceAwareLogger::new(
            &harness.inspect_metadata_node,
            &harness.inspect_metadata_path,
            InterfaceTimeSeriesGrouping::Type(vec![InterfaceType::Ethernet]),
            logger_node,
        );

        let tree = harness.get_inspect_data_tree();
        assert_data_tree!(
            @executor harness.exec,
            tree,
            root: contains {
                test_stats: contains {
                    metadata: {
                        ping_results: contains {},
                        fetch_results: contains {},
                    },
                    interfaces: contains {
                        TYPE_ethernet: {
                            v4: {
                                gateway_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                                internet_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                                fetch_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/fetch_results",
                                    }
                                }
                            },
                            v6: {
                                gateway_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                                internet_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                                fetch_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/fetch_results",
                                    }
                                }
                            }
                        }
                    }
                }
            }
        )
    }

    #[fuchsia::test]
    fn test_log_gateway_ping_result() {
        let mut harness = setup_test();
        let logger_node = harness.inspect_node.create_child("interfaces");

        let mut interface_aware_logger = InterfaceAwareLogger::new(
            &harness.inspect_metadata_node,
            &harness.inspect_metadata_path,
            InterfaceTimeSeriesGrouping::Type(vec![InterfaceType::Ethernet]),
            logger_node,
        );

        let ping_parameters = crate::PingParameters {
            addr: std_socket_addr_v4!("8.8.8.8:80").into(),
            interface_name: "eth0".to_string(),
        };

        interface_aware_logger.log_gateway_ping_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &ping_parameters,
            &Ok(()),
        );
        interface_aware_logger.log_gateway_ping_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &ping_parameters,
            &Err(PingError::NoReply),
        );

        let tree = harness.get_inspect_data_tree();
        assert_data_tree!(
            @executor harness.exec,
            tree,
            root: contains {
                test_stats: contains {
                    metadata: contains {
                        ping_results: {
                            "0": {
                                "@time": AnyNumericProperty,
                                data: "Success",
                            },
                            "1": {
                                "@time": AnyNumericProperty,
                                data: "e_NoReply",
                            },
                        },
                    },
                    interfaces: contains {
                        TYPE_ethernet: contains {
                            v4: contains {
                                // TODO(https://fxbug.dev/496641843): Verify time series data once
                                // there is library support.
                                gateway_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                            }
                        }
                    }
                }
            }
        )
    }

    #[fuchsia::test]
    fn test_log_internet_ping_result() {
        let mut harness = setup_test();
        let logger_node = harness.inspect_node.create_child("interfaces");

        let mut interface_aware_logger = InterfaceAwareLogger::new(
            &harness.inspect_metadata_node,
            &harness.inspect_metadata_path,
            InterfaceTimeSeriesGrouping::Type(vec![InterfaceType::Ethernet]),
            logger_node,
        );

        let ping_parameters = crate::PingParameters {
            addr: std_socket_addr_v6!("[2001:db8::]:80").into(),
            interface_name: "eth0".to_string(),
        };

        interface_aware_logger.log_internet_ping_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &ping_parameters,
            &Ok(()),
        );
        interface_aware_logger.log_internet_ping_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &ping_parameters,
            &Err(PingError::NoReply),
        );

        let tree = harness.get_inspect_data_tree();
        assert_data_tree!(
            @executor harness.exec,
            tree,
            root: contains {
                test_stats: contains {
                    metadata: contains {
                        ping_results: {
                            "0": {
                                "@time": AnyNumericProperty,
                                data: "Success",
                            },
                            "1": {
                                "@time": AnyNumericProperty,
                                data: "e_NoReply",
                            },
                        },
                    },
                    interfaces: contains {
                        TYPE_ethernet: contains {
                            v6: contains {
                                // TODO(https://fxbug.dev/496641843): Verify time series data once
                                // there is library support.
                                internet_ping_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/ping_results",
                                    }
                                },
                            }
                        }
                    }
                }
            }
        )
    }

    #[fuchsia::test]
    fn test_log_fetch_result() {
        let mut harness = setup_test();
        let logger_node = harness.inspect_node.create_child("interfaces");

        let mut interface_aware_logger = InterfaceAwareLogger::new(
            &harness.inspect_metadata_node,
            &harness.inspect_metadata_path,
            InterfaceTimeSeriesGrouping::Type(vec![InterfaceType::Ethernet]),
            logger_node,
        );

        let fetch_parameters = crate::FetchParameters {
            interface_name: "eth0".to_string(),
            domain: "example.com".to_string(),
            ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)),
            path: "".to_string(),
            expected_statuses: vec![204],
        };

        interface_aware_logger.log_fetch_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &fetch_parameters,
            &Ok(204),
        );
        interface_aware_logger.log_fetch_result(
            vec![InterfaceIdentifier::Type(InterfaceType::Ethernet)],
            &fetch_parameters,
            &Err(FetchError::ReadTcpStreamTimeout),
        );

        let tree = harness.get_inspect_data_tree();
        assert_data_tree!(
            @executor harness.exec,
            tree,
            root: contains {
                test_stats: contains {
                    metadata: contains {
                        fetch_results: {
                            "0": {
                                "@time": AnyNumericProperty,
                                data: "Completed_204",
                            },
                            "1": {
                                "@time": AnyNumericProperty,
                                data: "e_ReadTcpTimeout",
                            },
                        },
                    },
                    interfaces: contains {
                        TYPE_ethernet: contains {
                            v4: contains {
                                // TODO(https://fxbug.dev/496641843): Verify time series data once
                                // there is library support.
                                fetch_results: {
                                    "type": "bitset",
                                    "data": AnyBytesProperty,
                                    metadata: {
                                        index_node_path: "root/test_stats/metadata/fetch_results",
                                    }
                                },
                            }
                        }
                    }
                }
            }
        )
    }
}
