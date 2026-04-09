// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::telemetry::NetworkEventMetadata;
use fuchsia_inspect::Node as InspectNode;
use fuchsia_inspect_contrib::nodes::LruCacheNode;
use fuchsia_inspect_derive::Unit;
use windowed_stats::experimental::inspect::{InspectSender, InspectedTimeMatrix};
use windowed_stats::experimental::series::interpolation::LastSample;
use windowed_stats::experimental::series::metadata::BitSetNode;
use windowed_stats::experimental::series::statistic::Union;
use windowed_stats::experimental::series::{SamplingProfile, TimeMatrix};

pub struct NetworkPropertiesProcessor {
    default_network_detailed_matrix: InspectedTimeMatrix<u64>,
    default_network_type_matrix: InspectedTimeMatrix<u64>,
    inspect_metadata_node: InspectMetadataNode,
}

const METADATA_NODE_NAME: &str = "metadata";

impl NetworkPropertiesProcessor {
    pub fn new<S: InspectSender>(parent: &InspectNode, parent_path: &str, client: &S) -> Self {
        let inspect_metadata_node = parent.create_child(METADATA_NODE_NAME);
        let inspect_metadata_path = format!("{}/{}", parent_path, METADATA_NODE_NAME);
        let detailed_time_matrix = TimeMatrix::<Union<u64>, LastSample>::new(
            SamplingProfile::granular(),
            LastSample::or(0),
        );
        let default_network_detailed_matrix = client.inspect_time_matrix_with_metadata(
            "default_network_detailed",
            detailed_time_matrix,
            BitSetNode::from_path(format!(
                "{}/{}",
                inspect_metadata_path,
                InspectMetadataNode::NETWORK_REGISTRY
            )),
        );

        let types_time_matrix = TimeMatrix::<Union<u64>, LastSample>::new(
            SamplingProfile::granular(),
            LastSample::or(0),
        );
        let default_network_type_matrix = client.inspect_time_matrix_with_metadata(
            "default_network_type",
            types_time_matrix,
            BitSetNode::from_path(format!(
                "{}/{}",
                inspect_metadata_path,
                InspectMetadataNode::NETWORK_TYPES
            )),
        );

        Self {
            default_network_detailed_matrix,
            default_network_type_matrix,
            inspect_metadata_node: InspectMetadataNode::new(inspect_metadata_node),
        }
    }

    pub fn log_default_network_lost(&mut self) {
        self.default_network_detailed_matrix.fold_or_log_error(0);
        self.default_network_type_matrix.fold_or_log_error(0);
    }

    pub fn log_default_network_changed(&mut self, metadata: NetworkEventMetadata) {
        let data = NetworkData::from(metadata);
        let types_mapped_id =
            self.inspect_metadata_node.network_types.insert(data.transport.clone());
        self.default_network_type_matrix.fold_or_log_error(1u64 << types_mapped_id);

        let detailed_mapped_id = self.inspect_metadata_node.network_registry.insert(data);
        self.default_network_detailed_matrix.fold_or_log_error(1u64 << detailed_mapped_id);
    }
}

#[derive(Unit, PartialEq, Eq, Hash)]
struct NetworkData {
    pub id: u64,
    pub name: String,
    pub transport: String,
    pub is_fuchsia_provisioned: bool,
}

impl From<NetworkEventMetadata> for NetworkData {
    fn from(metadata: NetworkEventMetadata) -> Self {
        let NetworkEventMetadata { id, name, transport, is_fuchsia_provisioned } = metadata;
        Self {
            id: id,
            name: name.unwrap_or_else(|| "unknown".to_string()),
            transport: format!("{:?}", transport),
            is_fuchsia_provisioned,
        }
    }
}

const NETWORKS_METADATA_CACHE_SIZE: usize = 16;
const NETWORK_TYPES_CACHE_SIZE: usize = 8;

// Holds the inspect node children for the metadata that correlates to
// bits in the default network bitsets.
struct InspectMetadataNode {
    _node: InspectNode,
    network_registry: LruCacheNode<NetworkData>,
    network_types: LruCacheNode<String>,
}

impl InspectMetadataNode {
    const NETWORK_REGISTRY: &'static str = "network_registry";
    const NETWORK_TYPES: &'static str = "network_types";

    fn new(inspect_node: InspectNode) -> Self {
        // Record the network registry, which is dynamically updated as networks
        // are added.
        let network_registry = LruCacheNode::new(
            inspect_node.create_child(Self::NETWORK_REGISTRY),
            NETWORKS_METADATA_CACHE_SIZE,
        );

        // Record the observed network types for the default network type time matrix.
        let network_types = LruCacheNode::new(
            inspect_node.create_child(Self::NETWORK_TYPES),
            NETWORK_TYPES_CACHE_SIZE,
        );

        Self { _node: inspect_node, network_registry, network_types }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyBytesProperty, assert_data_tree};
    use fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy;
    use fuchsia_inspect::Inspector;
    use fuchsia_inspect::reader::DiagnosticsHierarchy;
    use futures::task::Poll;
    use std::pin::pin;
    use windowed_stats::experimental::clock::Timed;
    use windowed_stats::experimental::inspect::TimeMatrixClient;
    use windowed_stats::experimental::testing::{MockTimeMatrixClient, TimeMatrixCall};

    pub struct TestHelper {
        pub inspector: Inspector,
        pub inspect_node: InspectNode,
        pub parent_path: String,
        pub mock_time_matrix_client: MockTimeMatrixClient,

        // Note: keep the executor field last in the struct so it gets dropped last.
        pub exec: fuchsia_async::TestExecutor,
    }

    impl TestHelper {
        pub fn get_inspect_data_tree(&mut self) -> DiagnosticsHierarchy {
            let read_fut = fuchsia_inspect::reader::read(&self.inspector);
            let mut read_fut = pin!(read_fut);
            match self.exec.run_until_stalled(&mut read_fut) {
                Poll::Pending => {
                    panic!("Unexpected pending state");
                }
                Poll::Ready(result) => result.expect("failed to get hierarchy"),
            }
        }
    }

    pub fn setup_test() -> TestHelper {
        let exec = fuchsia_async::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fuchsia_async::MonotonicInstant::from_nanos(0));

        let inspector = Inspector::default();
        let inspect_node = inspector.root().create_child("test_stats");
        let parent_path = "root/test_stats".to_string();

        TestHelper {
            inspector,
            inspect_node,
            parent_path,
            mock_time_matrix_client: MockTimeMatrixClient::new(),
            exec,
        }
    }

    fn log_network_events(processor: &mut NetworkPropertiesProcessor) {
        let eth_metadata = NetworkEventMetadata {
            id: 0,
            name: Some("eth0".to_string()),
            transport: fnp_socketproxy::NetworkType::Ethernet,
            is_fuchsia_provisioned: true,
        };

        let wlan_metadata = NetworkEventMetadata {
            id: 1,
            name: Some("wlan0".to_string()),
            transport: fnp_socketproxy::NetworkType::Wifi,
            is_fuchsia_provisioned: false,
        };

        processor.log_default_network_changed(eth_metadata);
        processor.log_default_network_lost();
        processor.log_default_network_changed(wlan_metadata);
    }

    #[fuchsia::test]
    fn log_default_network_time_series_calls() {
        let harness = setup_test();
        let mut processor = NetworkPropertiesProcessor::new(
            &harness.inspect_node,
            &harness.parent_path,
            &harness.mock_time_matrix_client,
        );
        log_network_events(&mut processor);

        let mut time_matrix_calls = harness.mock_time_matrix_client.drain_calls();
        assert_eq!(
            &time_matrix_calls.drain::<u64>("default_network_detailed")[..],
            &[
                TimeMatrixCall::Fold(Timed::now(1 << 0)),
                TimeMatrixCall::Fold(Timed::now(0)),
                TimeMatrixCall::Fold(Timed::now(1 << 1)),
            ]
        );
        assert_eq!(
            &time_matrix_calls.drain::<u64>("default_network_type")[..],
            &[
                TimeMatrixCall::Fold(Timed::now(1 << 0)),
                TimeMatrixCall::Fold(Timed::now(0)),
                TimeMatrixCall::Fold(Timed::now(1 << 1)),
            ]
        );
    }

    #[fuchsia::test]
    fn log_default_network_inspect_tree() {
        let mut harness = setup_test();
        let time_matrix_client =
            TimeMatrixClient::new(harness.inspect_node.create_child("time_series"));
        let mut processor = NetworkPropertiesProcessor::new(
            &harness.inspect_node,
            &harness.parent_path,
            &time_matrix_client,
        );
        log_network_events(&mut processor);

        let hierarchy = harness.get_inspect_data_tree();

        assert_data_tree!(
            @executor harness.exec,
            hierarchy,
            root: contains {
                test_stats: contains {
                    metadata: contains {
                        network_registry: contains {
                            "0": contains {
                                data: {
                                    id: 0u64,
                                    name: "eth0",
                                    transport: "Ethernet",
                                    is_fuchsia_provisioned: true,
                                }
                            },
                            "1": contains {
                                data: {
                                    id: 1u64,
                                    name: "wlan0",
                                    transport: "Wifi",
                                    is_fuchsia_provisioned: false,
                                }
                            }
                        },
                        network_types: contains {
                            "0": contains {
                                data: "Ethernet",
                            },
                            "1": contains {
                                data: "Wifi",
                            }
                        }
                    },
                    time_series: contains {
                        default_network_detailed: {
                            "type": "bitset",
                            "data": AnyBytesProperty,
                            metadata: {
                                index_node_path: "root/test_stats/metadata/network_registry",
                            }
                        },
                        default_network_type: {
                            "type": "bitset",
                            "data": AnyBytesProperty,
                            metadata: {
                                index_node_path: "root/test_stats/metadata/network_types",
                            }
                        },
                    }
                }
            }
        );
    }
}
