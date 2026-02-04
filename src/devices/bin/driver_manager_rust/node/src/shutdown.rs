// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use crate::types::{DriverState, NodeState, NodeTypeVariant};
use async_trait::async_trait;
use driver_manager_driver_host::DriverHost;
use driver_manager_shutdown::{
    NodeInfo, NodeShutdownBridge, NodeShutdownCoordinator, ShutdownIntent, ShutdownNode,
};
use driver_manager_types::{Collection, ShutdownState};
use log::{debug, error, warn};
use std::rc::{Rc, Weak};

pub struct NodeBridge(Weak<Node>);

impl NodeBridge {
    pub fn new(node: Weak<Node>) -> Self {
        Self(node)
    }
}

#[async_trait(?Send)]
impl ShutdownNode for Node {
    fn get_shutdown_coordinator(&self) -> std::cell::RefMut<'_, NodeShutdownCoordinator> {
        self.node_shutdown_coordinator.borrow_mut()
    }
    fn name(&self) -> &str {
        &self.name
    }
    async fn finish_shutdown(&self) {
        if let Some(this) = self.weak_self.upgrade() {
            this.finish_shutdown().await;
        }
    }
    fn schedule_post_shutdown(&self, intent: ShutdownIntent) {
        if let Some(this) = self.weak_self.upgrade() {
            this.schedule_post_shutdown(intent);
        }
    }
    fn set_should_destroy_driver_component(&self, val: bool) {
        self.inner.borrow_mut().should_destroy_driver_component = val;
    }
}

#[async_trait(?Send)]
impl NodeShutdownBridge for NodeBridge {
    fn get_removal_tracker_info(&self, shutdown_state: ShutdownState) -> NodeInfo {
        if let Some(node) = self.0.upgrade() {
            let inner = node.inner.borrow();
            let driver_url = match &inner.state {
                NodeState::Starting { driver_url } => driver_url.clone(),
                NodeState::DriverComponent(c) => c.driver_url.clone(),
                NodeState::Quarantined { driver_url } => driver_url.clone(),
                _ => "".to_string(),
            };
            NodeInfo {
                name: node.make_component_moniker(),
                driver_url,
                collection: node.collection(),
                state: shutdown_state,
                host: node.driver_host(),
            }
        } else {
            // Should not happen
            NodeInfo {
                name: "".to_string(),
                driver_url: "".to_string(),
                collection: Collection::None,
                state: ShutdownState::Stopped,
                host: None,
            }
        }
    }
    fn stop_driver(&self) {
        if let Some(node) = self.0.upgrade() {
            node.stop_driver();
        }
    }
    fn stop_driver_component(&self) {
        if let Some(node) = self.0.upgrade() {
            node.stop_driver_component();
        }
    }
    fn is_pending_bind(&self) -> bool {
        if let Some(node) = self.0.upgrade() { node.is_pending_bind() } else { false }
    }
    fn has_children(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            !node.inner.borrow().children.is_empty()
        } else {
            false
        }
    }
    fn has_driver(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            match &node.inner.borrow().state {
                NodeState::DriverComponent(component) => component.driver_client_binding.is_some(),
                _ => false,
            }
        } else {
            false
        }
    }
    fn has_driver_component(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            match &node.inner.borrow().state {
                NodeState::DriverComponent(component) => component.state != DriverState::Stopped,
                _ => false,
            }
        } else {
            false
        }
    }
    fn has_driver_component_controller(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            node.has_component_controller_proxy()
        } else {
            false
        }
    }
    fn maybe_destroy_driver_component(&self, intent: ShutdownIntent) -> bool {
        if let Some(node) = self.0.upgrade() {
            node.maybe_destroy_driver_component(intent)
        } else {
            false
        }
    }
    fn get_driver_host(&self) -> Option<Rc<dyn DriverHost>> {
        let node = self.0.upgrade()?;
        node.driver_host()
    }
    fn collection(&self) -> Collection {
        if let Some(node) = self.0.upgrade() { node.collection() } else { Collection::None }
    }
    fn children(&self) -> Vec<Rc<dyn ShutdownNode>> {
        if let Some(node) = self.0.upgrade() {
            node.inner.borrow().children.iter().map(|c| c.clone() as Rc<dyn ShutdownNode>).collect()
        } else {
            vec![]
        }
    }
    fn get_weak_node(&self) -> Weak<dyn ShutdownNode> {
        self.0.clone() as Weak<dyn ShutdownNode>
    }
}

impl Node {
    fn remove_child(&self, child: &Rc<Node>) {
        log::debug!("RemoveChild {} from parent {}", child.name(), self.name());
        let mut inner = self.inner.borrow_mut();
        inner.children.retain(|c| !Rc::ptr_eq(c, child));

        if !inner.unbinding_children_completers.is_empty() && inner.children.is_empty() {
            for completer in inner.unbinding_children_completers.drain(..) {
                let _ = completer.send(Ok(()));
            }
        }
        drop(inner);
        self.node_shutdown_coordinator.borrow_mut().check_node_state();
    }

    async fn finish_shutdown(self: &Rc<Self>) {
        log::debug!("Node: {} finishing shutdown", self.make_component_moniker());

        if let Some(koid) = self.token_koid()
            && let Some(attributor) = self.node_manager.memory_attributor()
        {
            attributor.remove_driver(koid.raw_koid());
        }

        if let Some(binding) = self.inner.borrow_mut().node_controller_server_binding.take() {
            binding.close();
        }

        {
            let mut inner = self.inner.borrow_mut();
            match inner.state {
                NodeState::DriverComponent(ref mut driver_component) => {
                    driver_component.close_node();
                    driver_component.driver_client_binding.take();
                }
                NodeState::OwnedByParent { ref mut node_server_binding } => {
                    if let Some(binding) = node_server_binding.take() {
                        binding.close()
                    }
                }
                _ => {}
            }

            inner.devfs_device.topological.take();
            inner.devfs_device.protocol.take();
        }

        for parent in self.parents() {
            if let Some(p) = parent.upgrade() {
                p.remove_child(self);
            } else if !self.is_root_node() {
                warn!("Parent freed before child {} could be removed from it", self.name());
            }
        }

        {
            let mut inner = self.inner.borrow_mut();
            inner.state = NodeState::Unbound;

            match inner.node_type {
                NodeTypeVariant::Normal { .. } => {
                    inner.node_type = NodeTypeVariant::Normal { parent: Weak::new() };
                }
                NodeTypeVariant::Composite { .. } => {
                    inner.node_type = NodeTypeVariant::Composite {
                        parents: vec![],
                        parents_names: vec![],
                        primary_index: 0,
                    };
                }
            }
        }
    }

    fn schedule_post_shutdown(self: &Rc<Self>, intent: ShutdownIntent) {
        let self_clone = self.clone();
        self.scope.spawn_local(async move {
            self_clone.post_shutdown(intent).await;
        });
    }

    async fn post_shutdown(self: &Rc<Self>, intent: ShutdownIntent) {
        if let Some(koid) = self.token_koid()
            && let Some(attributor) = self.node_manager.memory_attributor()
        {
            attributor.remove_driver(koid.raw_koid());
        }

        if intent == ShutdownIntent::Restart {
            log::debug!("Node '{}': finishing restart", self.make_component_moniker());
            self.finish_restart().await;
            return;
        } else if intent == ShutdownIntent::Quarantine {
            log::debug!("Node '{}': finishing quarantine", self.make_component_moniker());
            self.finish_quarantine();
            return;
        }

        if let Some(cb) = self.inner.borrow_mut().remove_complete_callback.take() {
            let _ = cb.send(());
        }

        if intent == ShutdownIntent::RebindComposite
            && let Some(completer) = self.inner.borrow_mut().composite_rebind_completer.take()
        {
            let _ = completer.send(Ok(()));
        }
    }

    async fn finish_restart(self: &Rc<Self>) {
        self.get_shutdown_coordinator().reset_shutdown();
        // Store previous url before we reset the state_.
        let previous_url = self.driver_url();

        // Perform cleanups for previous driver before we try to start the next driver.
        {
            let mut inner = self.inner.borrow_mut();
            match inner.state {
                NodeState::DriverComponent(ref mut driver_component) => {
                    driver_component.close_node();
                    driver_component.driver_client_binding.take();
                }
                NodeState::OwnedByParent { ref mut node_server_binding } => {
                    if let Some(binding) = node_server_binding.take() {
                        binding.close()
                    }
                }
                _ => {}
            }
            inner.state = NodeState::Unbound;
        }

        if self.inner.borrow().host_restart_on_crash {
            self.inner.borrow_mut().driver_host.take();
        }

        let suffix = self.inner.borrow_mut().restart_driver_url_suffix.take();
        if let Some(suffix) = suffix {
            let tracker = self.create_bind_result_tracker(false);
            self.node_manager.bind_to_url(self, &suffix, tracker);
            return;
        }

        let package_type = self.inner.borrow().driver_package_type;
        if let Err(e) = self.node_manager.start_driver(self, &previous_url, package_type) {
            error!("Failed to start driver '{}': {}", self.name(), e);
        }
    }

    fn finish_quarantine(self: &Rc<Self>) {
        self.get_shutdown_coordinator().reset_shutdown();
        assert!(
            matches!(self.inner.borrow().state, NodeState::Quarantined { .. }),
            "Node::state_ was not set to Quarantined"
        );
    }

    fn stop_driver(&self) {
        let mut inner = self.inner.borrow_mut();
        if let NodeState::DriverComponent(ref mut driver_component) = inner.state {
            if driver_component.state == DriverState::Binding {
                warn!(
                    "Stopping driver '{}' for node '{}' while bind is in process",
                    driver_component.driver_url,
                    self.make_component_moniker()
                );
                return;
            }

            if let Some(driver) = &driver_component.driver_client_binding
                && let Err(e) = driver.driver_host_proxy.stop()
            {
                error!("Node: {} failed to stop driver: {}", self.name(), e);
                drop(inner);
                self.clear_driver_host();
            }
        }
    }

    fn stop_driver_component(self: &Rc<Self>) {
        if let NodeState::DriverComponent(ref driver_component) = self.inner.borrow().state
            && driver_component.state != DriverState::Stopped
        {
            debug!(
                "Node '{}' sending stop through component runner",
                self.make_component_moniker()
            );
            driver_component.send_on_stop();
        }
    }

    fn maybe_destroy_driver_component(self: &Rc<Self>, intent: ShutdownIntent) -> bool {
        let inner = self.inner.borrow();
        if inner.should_destroy_driver_component
            || intent != ShutdownIntent::Removal
            || inner.remove_complete_callback.is_some()
            || self.is_root_node()
        {
            let proxy = inner
                .component_controller
                .as_ref()
                .expect("component_controller_proxy")
                .component_controller_proxy
                .clone();
            drop(inner);

            let name = self.make_component_moniker();

            let self_rc = self.clone();
            self.scope.spawn_local(async move {
                let e = proxy.destroy().await;
                match e {
                    Ok(inner) => match inner {
                        Ok(()) => (),
                        Err(e) => {
                            error!("Node: '{}' destroy failed: {:?}", name, e);
                        }
                    },
                    Err(e) => {
                        error!("Node: '{}' failed to send destroy: {:?}", name, e);
                    }
                }

                self_rc.inner.borrow_mut().should_destroy_driver_component = false;
            });

            return true;
        }

        debug!("Node: '{}' not destroying", self.make_component_moniker());
        false
    }
}
