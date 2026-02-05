// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use crate::types::{DriverState, NodeState};
use driver_manager_shutdown::{RemovalSet, ShutdownIntent};
use driver_manager_types::{BindResultTracker, ShutdownState};
use futures::channel::oneshot;
use log::{error, warn};
use std::cell::RefCell;
use std::rc::Rc;
use {fidl_fuchsia_driver_development as fdd, fidl_fuchsia_driver_framework as fdf};

impl Node {
    pub async fn complete_bind(self: &Rc<Self>, result: Result<(), zx::Status>) {
        if result.is_err() {
            warn!("Bind failed for node '{}'", self.make_component_moniker());
            if matches!(
                *self.node_shutdown_coordinator.borrow().node_state(),
                ShutdownState::Running
            ) && !matches!(self.inner.borrow().state, NodeState::Unbound)
            {
                warn!("Quarantining node '{}'", self.make_component_moniker());
                self.quarantine_node().await;
            } else {
                self.inner.borrow_mut().state = NodeState::Unbound;
            }
        }

        let mut inner = self.inner.borrow_mut();
        if let NodeState::DriverComponent(ref mut driver_component) = inner.state {
            if driver_component.state == DriverState::Stopped {
                warn!("completed bind but the driver {} is already stopped", self.name());
            } else {
                driver_component.state = DriverState::Running;
                drop(inner);
                self.on_bind();
            }
        } else {
            drop(inner);
        }

        let completer = self.inner.borrow_mut().pending_bind_completer.take();
        if let Some(completer) = completer {
            let _ = completer.send(result);
        }

        if let Err(status) = &result {
            self.on_start_error(*status);
        } else {
            let completer = self.inner.borrow_mut().wait_for_driver_completer.take();
            if let Some(completer) = completer {
                let token = if self.is_composite() {
                    self.inner.borrow().children.iter().find_map(|child| {
                        if let NodeState::DriverComponent(driver_component) =
                            &child.inner.borrow().state
                            && driver_component.state == DriverState::Running
                        {
                            Some(driver_component.duplicate_instance_handle())
                        } else {
                            None
                        }
                    })
                } else if let NodeState::DriverComponent(driver_component) =
                    &self.inner.borrow().state
                {
                    Some(driver_component.duplicate_instance_handle())
                } else {
                    None
                };

                if let Some(token) = token {
                    let _ = completer.send(Ok(fdf::DriverResult::DriverStartedNodeToken(token)));
                } else {
                    let _ = completer.send(Err(zx::Status::INTERNAL));
                }
            }
        }

        self.node_shutdown_coordinator.borrow_mut().check_node_state();
    }

    pub(crate) fn wait_for_driver(
        self: &Rc<Self>,
        completer: oneshot::Sender<Result<fdf::DriverResult, zx::Status>>,
    ) {
        if let NodeState::DriverComponent(driver_component) = &self.inner.borrow().state
            && driver_component.state == DriverState::Running
        {
            let token = driver_component.duplicate_instance_handle();
            let _ = completer.send(Ok(fdf::DriverResult::DriverStartedNodeToken(token)));
            return;
        }

        if self.inner.borrow().wait_for_driver_completer.is_some() {
            let _ = completer.send(Err(zx::Status::ALREADY_EXISTS));
            return;
        }

        self.inner.borrow_mut().wait_for_driver_completer = Some(completer);

        let node_clone = self.clone();
        self.scope.spawn_local(async move {
            node_clone.node_manager.wait_for_bootup().await;
            let completer = node_clone.inner.borrow_mut().wait_for_driver_completer.take();
            if let Some(completer) = completer {
                if let Some(result) = node_clone.inner.borrow().bind_error.as_ref() {
                    let response = match result {
                        fdf::DriverResult::MatchError(s) => Ok(fdf::DriverResult::MatchError(*s)),
                        fdf::DriverResult::StartError(s) => Ok(fdf::DriverResult::StartError(*s)),
                        _ => Err(zx::Status::INTERNAL),
                    };
                    let _ = completer.send(response);
                    return;
                }

                // Re-check running state
                if let NodeState::DriverComponent(driver_component) =
                    &node_clone.inner.borrow().state
                    && driver_component.state == DriverState::Running
                {
                    let token = driver_component.duplicate_instance_handle();
                    let _ = completer.send(Ok(fdf::DriverResult::DriverStartedNodeToken(token)));
                    return;
                }

                let _ = completer.send(Err(zx::Status::INTERNAL));
            }
        });
    }

    pub(crate) async fn bind(self: &Rc<Self>, driver: String) -> Result<(), zx::Status> {
        self.bind_helper(false, Some(driver)).await
    }

    pub(crate) async fn bind_helper(
        self: &Rc<Self>,
        force_rebind: bool,
        driver_url_suffix: Option<String>,
    ) -> Result<(), zx::Status> {
        if !force_rebind && let NodeState::DriverComponent(_) = &self.inner.borrow().state {
            return Err(zx::Status::ALREADY_BOUND);
        }

        if self.inner.borrow().pending_bind_completer.is_some() {
            return Err(zx::Status::ALREADY_EXISTS);
        }

        let (tx, rx) = oneshot::channel();
        if let NodeState::DriverComponent(_) = &self.inner.borrow().state {
            self.restart_node_with_rematch(driver_url_suffix, tx);
        } else {
            self.inner.borrow_mut().pending_bind_completer = Some(tx);
            let tracker = self.create_bind_result_tracker(false);
            if let Some(driver_url_suffix) = driver_url_suffix {
                self.node_manager.bind_to_url(self, &driver_url_suffix, tracker);
            } else {
                self.node_manager.bind(self, tracker);
            }
        }
        rx.await.map_err(|_| zx::Status::INTERNAL)?
    }

    pub(crate) fn create_bind_result_tracker(
        self: &Rc<Self>,
        silent: bool,
    ) -> Rc<RefCell<BindResultTracker>> {
        let weak_self = Rc::downgrade(self);
        let (tx, rx) = oneshot::channel::<Vec<fdd::NodeBindingInfo>>();
        self.scope.spawn_local(async move {
            let Ok(info) = rx.await else {
                return;
            };
            let Some(self_ptr) = weak_self.upgrade() else {
                return;
            };
            if info.is_empty() {
                self_ptr.inner.borrow_mut().state = NodeState::Unbound;
                self_ptr.on_match_error(zx::Status::NOT_FOUND);
                if !silent {
                    // We need to call a method on Node here, or replicate logic.
                    // Node::complete_bind is seemingly not public or not seen yet.
                    // Let's check Node::complete_bind visibility in node.rs, assuming it exists.
                    // If it doesn't exist, I might need to implement it or check what calls it.
                    // Wait, I saw self_ptr.complete_bind in node.rs:332.
                    // It is likely private. I should make it pub(crate) or move it here if possible.
                    // For now assuming I can call it if I make it pub(crate).
                    self_ptr.complete_bind(Err(zx::Status::NOT_FOUND)).await;
                }
            } else if info.len() > 1 {
                error!("Unexpectedly bound multiple drivers to a single node");
                self_ptr.on_match_error(zx::Status::BAD_STATE);
                if !silent {
                    self_ptr.complete_bind(Err(zx::Status::BAD_STATE)).await;
                }
            }
        });

        Rc::new(RefCell::new(BindResultTracker::new(1, tx)))
    }

    pub(crate) async fn rebind(self: &Rc<Self>, driver: Option<String>) -> Result<(), zx::Status> {
        let (tx, rx) = oneshot::channel();
        self.restart_node_with_rematch(driver, tx);
        rx.await.map_err(|_| zx::Status::INTERNAL)?
    }

    pub(crate) async fn unbind_children(self: &Rc<Self>) -> Result<(), zx::Status> {
        if self.inner.borrow().children.is_empty() {
            return Ok(());
        }

        let rx = {
            let (tx, rx) = oneshot::channel();
            let mut inner = self.inner.borrow_mut();
            inner.unbinding_children_completers.push(tx);
            if inner.unbinding_children_completers.len() == 1 {
                let children = inner.children.clone();
                drop(inner);
                for child in children {
                    child.remove(RemovalSet::All, None);
                }
            }
            rx
        };
        rx.await.map_err(|_| zx::Status::INTERNAL)?
    }

    pub(crate) fn schedule_unbind(self: &Rc<Self>) {
        self.remove(RemovalSet::All, None);
    }

    pub fn restart_node_with_rematch(
        self: &Rc<Self>,
        restart_driver_url_suffix: Option<String>,
        completer: oneshot::Sender<Result<(), zx::Status>>,
    ) {
        if self.inner.borrow().pending_bind_completer.is_some() {
            let _ = completer.send(Err(zx::Status::ALREADY_EXISTS));
            return;
        }

        let mut inner = self.inner.borrow_mut();
        inner.pending_bind_completer = Some(completer);
        inner.restart_driver_url_suffix = restart_driver_url_suffix;
        drop(inner);
        self.restart_node();
    }

    pub fn restart_node(self: &Rc<Self>) {
        self.node_shutdown_coordinator.borrow_mut().set_shutdown_intent(ShutdownIntent::Restart);
        self.remove(RemovalSet::All, None);
    }

    pub(crate) async fn quarantine_node(self: &Rc<Self>) {
        let driver_url = self.driver_url();

        {
            let mut inner = self.inner.borrow_mut();
            match inner.state {
                NodeState::DriverComponent(ref mut driver_component) => {
                    driver_component.close_node();
                    driver_component.driver_client_binding.take();
                }
                NodeState::Starting { .. } => {}
                _ => {
                    panic!("QuarantineNode called from unexpected state");
                }
            }

            // TODO(novinc): consider keeping the DriverComponent and going through shutdown flow
            // with all of that state. This just drops all the connections currently.
            inner.state = NodeState::Quarantined { driver_url };
        }

        self.node_shutdown_coordinator.borrow_mut().set_shutdown_intent(ShutdownIntent::Quarantine);
        self.remove(RemovalSet::All, None);
    }

    fn on_bind(&self) {
        if let Some(controller_ref) = self.inner.borrow().node_controller_server_binding.as_ref() {
            let inner = self.inner.borrow();
            if let NodeState::DriverComponent(driver_component) = &inner.state {
                let node_token = Some(driver_component.duplicate_instance_handle());
                let event = fdf::NodeControllerOnBindRequest { node_token, ..Default::default() };
                if let Err(e) = controller_ref.node_controller_ref.send_on_bind(event) {
                    error!("Failed to send OnBind event: {}", e);
                }
            }
        }

        if let NodeState::DriverComponent(driver_component) = &self.inner.borrow().state {
            let node_token = driver_component.duplicate_instance_handle();
            let koid = driver_component.instance_koid().raw_koid();
            if let Some(driver_host) = self.driver_host() {
                let node_manager = self.node_manager.clone_box();
                self.scope.spawn_local(async move {
                    if let Ok(process_koid) = driver_host.get_process_koid().await
                        && let Some(attributor) = node_manager.memory_attributor()
                    {
                        attributor.add_driver(node_token, koid, process_koid);
                    }
                });
            }
        }
    }
}
