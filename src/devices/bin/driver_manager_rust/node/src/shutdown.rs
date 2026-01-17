// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use crate::types::{DriverState, NodeState, NodeTypeVariant};
use async_trait::async_trait;
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
        self.should_destroy_driver_component.set(val);
    }
}

#[async_trait(?Send)]
impl NodeShutdownBridge for NodeBridge {
    fn get_removal_tracker_info(&self, shutdown_state: ShutdownState) -> NodeInfo {
        if let Some(node) = self.0.upgrade() {
            let state = node.state.borrow();
            let driver_url = match &*state {
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
            }
        } else {
            // Should not happen
            NodeInfo {
                name: "".to_string(),
                driver_url: "".to_string(),
                collection: Collection::None,
                state: ShutdownState::Stopped,
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
        if let Some(node) = self.0.upgrade() { !node.children.borrow().is_empty() } else { false }
    }
    fn has_driver(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            match &*node.state.borrow() {
                NodeState::DriverComponent(component) => component.driver_client_binding.is_some(),
                _ => false,
            }
        } else {
            false
        }
    }
    fn has_driver_component(&self) -> bool {
        if let Some(node) = self.0.upgrade() {
            match &*node.state.borrow() {
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
    fn collection(&self) -> Collection {
        if let Some(node) = self.0.upgrade() { node.collection() } else { Collection::None }
    }
    fn children(&self) -> Vec<Rc<dyn ShutdownNode>> {
        if let Some(node) = self.0.upgrade() {
            node.children.borrow().iter().map(|c| c.clone() as Rc<dyn ShutdownNode>).collect()
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
        self.children.borrow_mut().retain(|c| !Rc::ptr_eq(c, child));

        if !self.unbinding_children_completers.borrow().is_empty()
            && self.children.borrow().is_empty()
        {
            for completer in self.unbinding_children_completers.borrow_mut().drain(..) {
                let _ = completer.send(Ok(()));
            }
        }
        self.node_shutdown_coordinator.borrow_mut().check_node_state();
    }

    async fn finish_shutdown(self: &Rc<Self>) {
        log::debug!("Node: {} finishing shutdown", self.make_component_moniker());

        if let Some(binding) = self.node_controller_server_binding.take() {
            binding.close();
        }

        match *self.state.borrow_mut() {
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

        drop(self.devfs_device.borrow_mut().topological.take());
        drop(self.devfs_device.borrow_mut().protocol.take());

        for parent in self.parents() {
            if let Some(p) = parent.upgrade() {
                p.remove_child(self);
            } else if !self.is_root_node() {
                warn!("Parent freed before child {} could be removed from it", self.name());
            }
        }

        *self.state.borrow_mut() = NodeState::Unbound;

        let mut node_type = self.node_type.borrow_mut();
        match *node_type {
            NodeTypeVariant::Normal { .. } => {
                *node_type = NodeTypeVariant::Normal { parent: Weak::new() };
            }
            NodeTypeVariant::Composite { .. } => {
                *node_type = NodeTypeVariant::Composite {
                    parents: vec![],
                    parents_names: vec![],
                    primary_index: 0,
                };
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
        if intent == ShutdownIntent::Restart {
            log::debug!("Node '{}': finishing restart", self.make_component_moniker());
            self.finish_restart().await;
            return;
        } else if intent == ShutdownIntent::Quarantine {
            log::debug!("Node '{}': finishing quarantine", self.make_component_moniker());
            self.finish_quarantine();
            return;
        }

        if let Some(cb) = self.remove_complete_callback.borrow_mut().take() {
            let _ = cb.send(());
        }

        if intent == ShutdownIntent::RebindComposite
            && let Some(completer) = self.composite_rebind_completer.borrow_mut().take()
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
            let mut state = self.state.borrow_mut();
            match *state {
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
            *state = NodeState::Unbound;
        }

        let suffix = self.restart_driver_url_suffix.borrow_mut().take();
        if let Some(suffix) = suffix {
            let tracker = self.create_bind_result_tracker(false);
            self.node_manager.bind_to_url(self, &suffix, tracker);
            return;
        }

        let package_type = self.driver_package_type.get();
        if let Err(e) = self.node_manager.start_driver(self, &previous_url, package_type) {
            error!("Failed to start driver '{}': {}", self.name(), e);
        }
    }

    fn finish_quarantine(self: &Rc<Self>) {
        self.get_shutdown_coordinator().reset_shutdown();
        assert!(
            matches!(*self.state.borrow(), NodeState::Quarantined { .. }),
            "Node::state_ was not set to Quarantined"
        );
    }

    fn stop_driver(&self) {
        let mut state = self.state.borrow_mut();
        if let NodeState::DriverComponent(driver_component) = &mut *state {
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
                self.clear_driver_host();
            }
        }
    }

    fn stop_driver_component(self: &Rc<Self>) {
        if let NodeState::DriverComponent(ref driver_component) = *self.state.borrow()
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
        if self.should_destroy_driver_component.get()
            || intent != ShutdownIntent::Removal
            || self.remove_complete_callback.borrow().is_some()
            || self.is_root_node()
        {
            let proxy = self
                .component_controller
                .borrow()
                .as_ref()
                .expect("component_controller_proxy")
                .component_controller_proxy
                .clone();

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

                self_rc.should_destroy_driver_component.set(false);
            });

            return true;
        }

        debug!("Node: '{}' not destroying", self.make_component_moniker());
        false
    }
}
