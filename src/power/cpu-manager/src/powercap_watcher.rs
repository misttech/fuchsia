// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::log_if_err;
use crate::message::Message;
use crate::node::Node;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use cpu_manager_node_config::PowercapWatcherDependencies;
use fidl_fuchsia_power_powercap as fpowercap;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_inspect::{self as inspect, Property};
use futures::future::{FutureExt as _, LocalBoxFuture};
use futures::stream::FuturesUnordered;
use serde_derive::Deserialize;
use serde_json as json;
use std::collections::HashMap;
use std::rc::Rc;

/// Node: PowercapWatcher
///
/// Summary: Connects to the powercap client state service to monitor per-cluster OPP ceilings
///          computed by Power Manager's `PowercapController` and relays them to `CpuManagerMain`.
///          `PowercapWatcher` and `PowercapController` are intended to be enabled together on
///          products that configure powercap zones.
///
/// Handles Messages: N/A
///
/// Sends Messages:
///     - UpdatePowercapDomainOpps
///
/// FIDL dependencies:
///     - fuchsia.power.powercap.ClientStateWatcher: monitors per-cluster OPP ceilings computed by
///       Power Manager's `PowercapController` using a hanging-get pattern. The returned
///       `domain_opps` vector is sized to `max(cluster_id) + 1` across configured powercap
///       `domain_mappings` (up to `MAX_DOMAINS`), and any CPU cluster with
///       `cluster_index >= domain_opps.len()` has no powercap ceiling (`None`).

pub struct PowercapWatcherBuilder<'a> {
    cpu_manager_main_node: Rc<dyn Node>,
    powercap_watcher: Option<fpowercap::ClientStateWatcherProxy>,
    inspect_root: Option<&'a inspect::Node>,
}

impl<'a> PowercapWatcherBuilder<'a> {
    pub fn new_from_json(json_data: json::Value, nodes: &HashMap<String, Rc<dyn Node>>) -> Self {
        #[derive(Deserialize)]
        struct JsonData {
            dependencies: PowercapWatcherDependencies,
        }

        let data: JsonData = json::from_value(json_data).unwrap();
        Self {
            cpu_manager_main_node: nodes[&data.dependencies.cpu_manager_main_node].clone(),
            powercap_watcher: None,
            inspect_root: None,
        }
    }

    #[cfg(test)]
    fn new(cpu_manager_main_node: Rc<dyn Node>) -> Self {
        Self { cpu_manager_main_node, powercap_watcher: None, inspect_root: None }
    }

    #[cfg(test)]
    fn with_inspect_root(mut self, root: &'a inspect::Node) -> Self {
        self.inspect_root = Some(root);
        self
    }

    #[cfg(test)]
    fn with_proxy(mut self, watcher: fpowercap::ClientStateWatcherProxy) -> Self {
        self.powercap_watcher = Some(watcher);
        self
    }

    pub async fn build(
        self,
        futures_out: &FuturesUnordered<LocalBoxFuture<'_, ()>>,
    ) -> Result<Rc<PowercapWatcher>> {
        let inspect_root =
            self.inspect_root.unwrap_or_else(|| inspect::component::inspector().root());
        let inspect = InspectData::new(inspect_root, "PowercapWatcher".to_string());

        let powercap_watcher = if let Some(watcher) = self.powercap_watcher {
            watcher
        } else {
            connect_to_protocol::<fpowercap::ClientStateWatcherMarker>()
                .context("Failed to connect to fuchsia.power.powercap.ClientStateWatcher")?
        };

        let node = Rc::new(PowercapWatcher {
            powercap_watcher,
            cpu_manager_main_node: self.cpu_manager_main_node,
            inspect,
        });

        futures_out.push(node.clone().watch()?);

        Ok(node)
    }
}

pub struct PowercapWatcher {
    powercap_watcher: fpowercap::ClientStateWatcherProxy,
    cpu_manager_main_node: Rc<dyn Node>,
    inspect: InspectData,
}

impl PowercapWatcher {
    fn watch<'a>(self: Rc<Self>) -> Result<LocalBoxFuture<'a, ()>> {
        Ok(async move {
            loop {
                match self.powercap_watcher.watch().await {
                    Ok(domain_opps) => {
                        self.inspect.set_domain_opps(&domain_opps);
                        log_if_err!(
                            self.send_message(
                                &self.cpu_manager_main_node,
                                &Message::UpdatePowercapDomainOpps(domain_opps),
                            )
                            .await,
                            "Failed to send UpdatePowercapDomainOpps"
                        );
                    }
                    Err(e) => {
                        log::warn!("Error while waiting for powercap state updates: {:?}", e);
                        break;
                    }
                }
            }
        }
        .boxed_local())
    }
}

#[async_trait(?Send)]
impl Node for PowercapWatcher {
    fn name(&self) -> String {
        "PowercapWatcher".to_string()
    }
}

struct InspectData {
    domain_opps: inspect::StringProperty,
}

impl InspectData {
    fn new(parent: &inspect::Node, name: String) -> Self {
        let root = parent.create_child(name);
        let domain_opps = root.create_string("domain_opps", "[]");
        parent.record(root);
        Self { domain_opps }
    }

    fn set_domain_opps(&self, opps: &[u64]) {
        self.domain_opps.set(&format!("{:?}", opps));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::mock_node::{MessageMatcher, MockNodeMaker};
    use crate::{msg_eq, msg_ok_return};
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async as fasync;
    use futures::task::Poll;
    use futures::{StreamExt, TryStreamExt};

    struct FakePowercapStateProvider {
        watcher_stream: fpowercap::ClientStateWatcherRequestStream,
    }

    impl FakePowercapStateProvider {
        fn new() -> (fpowercap::ClientStateWatcherProxy, Self) {
            let (watcher_proxy, watcher_stream) =
                fidl::endpoints::create_proxy_and_stream::<fpowercap::ClientStateWatcherMarker>();

            (watcher_proxy, Self { watcher_stream })
        }

        async fn set_domain_opps(&mut self, opps: &[u64]) {
            match self
                .watcher_stream
                .try_next()
                .await
                .expect("Provider request stream yielded Some(None) (channel closed)")
                .expect("Provider request stream yielded Some(Err)")
            {
                fpowercap::ClientStateWatcherRequest::Watch { responder } => {
                    let _ = responder.send(opps);
                }
                fpowercap::ClientStateWatcherRequest::_UnknownMethod { .. } => {
                    panic!("Unexpected unknown method");
                }
            }
        }
    }

    #[fuchsia::test]
    fn test_state_monitor_and_inspect() {
        let mut mock_maker = MockNodeMaker::new();
        let mut exec = fasync::TestExecutor::new();

        let inspector = inspect::Inspector::default();
        let (watcher_proxy, mut fake_provider) = FakePowercapStateProvider::new();
        let futures_out = FuturesUnordered::new();
        let cpu_manager_main_node = mock_maker.make(
            "CpuManagerMain",
            vec![
                (
                    msg_eq!(UpdatePowercapDomainOpps(vec![0, 0, 1, 2])),
                    msg_ok_return!(UpdatePowercapDomainOpps),
                ),
                (
                    msg_eq!(UpdatePowercapDomainOpps(vec![0, 0, 0, 0])),
                    msg_ok_return!(UpdatePowercapDomainOpps),
                ),
            ],
        );

        let _node = exec.run_until_stalled(
            &mut PowercapWatcherBuilder::new(cpu_manager_main_node)
                .with_inspect_root(inspector.root())
                .with_proxy(watcher_proxy)
                .build(&futures_out)
                .boxed_local(),
        );
        let mut task = fasync::Task::local(futures_out.collect::<()>());
        assert_matches!(exec.run_until_stalled(&mut task), Poll::Pending);

        assert_matches!(
            exec.run_until_stalled(&mut fake_provider.set_domain_opps(&[0, 0, 1, 2]).boxed_local()),
            Poll::Ready(())
        );
        assert_matches!(exec.run_until_stalled(&mut task), Poll::Pending);

        assert_data_tree!(
            @executor exec,
            inspector,
            root: {
                "PowercapWatcher": {
                    "domain_opps": "[0, 0, 1, 2]"
                }
            }
        );

        assert_matches!(
            exec.run_until_stalled(&mut fake_provider.set_domain_opps(&[0, 0, 0, 0]).boxed_local()),
            Poll::Ready(())
        );
        assert_matches!(exec.run_until_stalled(&mut task), Poll::Pending);

        assert_data_tree!(
            @executor exec,
            inspector,
            root: {
                "PowercapWatcher": {
                    "domain_opps": "[0, 0, 0, 0]"
                }
            }
        );
    }
}
