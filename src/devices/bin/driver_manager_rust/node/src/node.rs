// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::devfs::ControllerAllowlistPassthrough;
use crate::node_manager::NodeManager;
use crate::serve::{ComponentControllerClientBinding, NodeControllerServerBinding};
use crate::shutdown::NodeBridge;
use crate::types::{DriverState, NodeDictionary, NodeState, NodeTypeVariant};
use driver_manager_devfs::DevfsDevice;
use driver_manager_driver_host::DriverHost;
use driver_manager_shutdown::{NodeRemovalTracker, NodeShutdownCoordinator, RemovalSet};
use driver_manager_types::{Collection, NodeOffer, OfferTransport, StartRequestReceiver};
use fidl_fuchsia_component_sandbox::AggregateSource;
use fuchsia_async::{self as fasync};
use futures::channel::oneshot;
use log::{debug, warn};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use {
    fidl_fuchsia_device_fs as fdevfs, fidl_fuchsia_driver_development as fdd,
    fidl_fuchsia_driver_framework as fdf,
};

pub struct Node {
    pub(crate) name: String,
    pub(crate) node_manager: Box<dyn NodeManager>,
    pub(crate) collection: Cell<Collection>,
    pub(crate) driver_package_type: Cell<fdf::DriverPackageType>,
    pub(crate) node_type: RefCell<NodeTypeVariant>,
    pub(crate) children: RefCell<Vec<Rc<Node>>>,
    pub(crate) properties: RefCell<Vec<fdf::NodePropertyEntry2>>,
    pub(crate) symbols: RefCell<Vec<fdf::NodeSymbol>>,
    pub(crate) offers: RefCell<Vec<NodeOffer>>,
    pub(crate) devfs_device: RefCell<DevfsDevice>,
    pub(crate) protocol_connector: RefCell<Option<fdevfs::ConnectorProxy>>,
    pub(crate) controller_allowlist_passthrough:
        RefCell<Option<Rc<ControllerAllowlistPassthrough>>>,
    pub(crate) node_shutdown_coordinator: RefCell<NodeShutdownCoordinator>,
    pub(crate) state: RefCell<NodeState>,
    pub(crate) driver_host: RefCell<Option<Rc<dyn DriverHost>>>,
    pub(crate) host_restart_on_crash: Cell<bool>,
    pub(crate) remove_complete_callback: RefCell<Option<oneshot::Sender<()>>>,
    pub(crate) bus_info: RefCell<Option<fdf::BusInfo>>,
    pub(crate) composite_rebind_completer: RefCell<Option<oneshot::Sender<Result<(), zx::Status>>>>,
    pub(crate) restart_driver_url_suffix: RefCell<Option<String>>,
    pub(crate) driver_host_name_for_colocation: RefCell<String>,
    pub can_multibind_composites: bool,
    pub(crate) node_controller_server_binding: RefCell<Option<NodeControllerServerBinding>>,
    pub(crate) pending_bind_completer: RefCell<Option<oneshot::Sender<Result<(), zx::Status>>>>,
    pub(crate) bind_error: RefCell<Option<fdf::DriverResult>>,
    pub(crate) unbinding_children_completers: RefCell<Vec<oneshot::Sender<Result<(), zx::Status>>>>,
    pub(crate) weak_self: Weak<Self>,
    pub(crate) dictionary: RefCell<NodeDictionary>,
    pub(crate) wait_for_driver_completer:
        RefCell<Option<oneshot::Sender<Result<fdf::DriverResult, zx::Status>>>>,
    pub(crate) component_controller: RefCell<Option<ComponentControllerClientBinding>>,
    pub(crate) start_request_receiver: RefCell<Option<StartRequestReceiver>>,
    pub(crate) start_handles: RefCell<Option<Vec<fidl_fuchsia_process::HandleInfo>>>,
    pub(crate) should_destroy_driver_component: Cell<bool>,
    pub(crate) scope: fasync::Scope,
}

impl Drop for Node {
    fn drop(&mut self) {
        debug!("Node: '{}' dropped", self.name());
    }
}

impl Node {
    pub fn new(name: &str, parent: Weak<Node>, node_manager: Box<dyn NodeManager>) -> Rc<Self> {
        let driver_host = if let Some(parent) = parent.upgrade() {
            parent.driver_host.borrow().clone()
        } else {
            None
        };
        Rc::new_cyclic(|weak_self| {
            let bridge = Box::new(NodeBridge::new(weak_self.clone()));
            let enable_test_shutdown_delays = node_manager.is_test_shutdown_delay_enabled();
            let shutdown_test_rng = node_manager.get_shutdown_test_rng();
            Self {
                name: name.to_string(),
                node_manager,
                collection: Cell::new(Collection::None),
                driver_package_type: Cell::new(fdf::DriverPackageType::Base),
                node_type: RefCell::new(NodeTypeVariant::Normal { parent }),
                children: RefCell::new(Vec::new()),
                properties: RefCell::new(Vec::new()),
                symbols: RefCell::new(Vec::new()),
                offers: RefCell::new(Vec::new()),
                devfs_device: RefCell::new(DevfsDevice::new()),
                protocol_connector: RefCell::new(None),
                controller_allowlist_passthrough: RefCell::new(None),
                node_shutdown_coordinator: RefCell::new(NodeShutdownCoordinator::new(
                    bridge,
                    enable_test_shutdown_delays,
                    shutdown_test_rng,
                )),
                state: RefCell::new(NodeState::Unbound),
                driver_host: RefCell::new(driver_host),
                host_restart_on_crash: Cell::new(false),
                remove_complete_callback: RefCell::new(None),
                bus_info: RefCell::new(None),
                composite_rebind_completer: RefCell::new(None),
                restart_driver_url_suffix: RefCell::new(None),
                driver_host_name_for_colocation: RefCell::new(String::new()),
                can_multibind_composites: true,
                node_controller_server_binding: RefCell::new(None),
                pending_bind_completer: RefCell::new(None),
                bind_error: RefCell::new(None),
                unbinding_children_completers: RefCell::new(Vec::new()),
                weak_self: weak_self.clone(),
                dictionary: RefCell::new(NodeDictionary::None),
                wait_for_driver_completer: RefCell::new(None),
                component_controller: RefCell::new(None),
                start_request_receiver: RefCell::new(None),
                start_handles: RefCell::new(None),
                should_destroy_driver_component: Cell::new(false),
                scope: fasync::Scope::new_with_name(format!("node:{name}")),
            }
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn token_koid(&self) -> Option<zx::Koid> {
        match &*self.state.borrow() {
            NodeState::DriverComponent(driver_component) => Some(driver_component.instance_koid()),
            _ => None,
        }
    }

    pub fn make_topological_path(&self, deduplicate: bool) -> String {
        let mut names = std::collections::VecDeque::new();
        let mut current = Some(self.weak_self.upgrade().unwrap());
        let mut prev_name = String::new();
        while let Some(node) = current {
            let name = node.name.clone();
            if !deduplicate || name != prev_name {
                names.push_front(name.clone());
                prev_name = name;
            }
            current = node.get_primary_parent();
        }
        names.into_iter().collect::<Vec<_>>().join("/")
    }

    pub fn on_match_error(&self, error: zx::Status) {
        *self.bind_error.borrow_mut() = Some(fdf::DriverResult::MatchError(error.into_raw()));
    }

    pub fn on_start_error(&self, error: zx::Status) {
        *self.bind_error.borrow_mut() = Some(fdf::DriverResult::StartError(error.into_raw()));
    }

    pub fn mark_as_composite_parent(&self) {
        *self.state.borrow_mut() = NodeState::CompositeParent;
    }

    pub fn unmark_as_composite_parent(&self) {
        *self.state.borrow_mut() = NodeState::Unbound;
    }

    pub(crate) fn is_pending_bind(&self) -> bool {
        match &*self.state.borrow() {
            NodeState::DriverComponent(component) => component.state == DriverState::Binding,
            _ => false,
        }
    }

    pub(crate) fn clear_driver_host(&self) {
        if let NodeState::DriverComponent(ref mut component) = *self.state.borrow_mut() {
            component.driver_client_binding.take();
        }
    }

    pub fn make_component_moniker(&self) -> String {
        let mut topo_path = self.make_topological_path(true);

        let k_prefix = "dev/sys/platform/pt/";
        let k_prefix2 = "dev/sys/platform/";

        if topo_path == "dev" {
            topo_path = "root".to_string();
        } else if topo_path == "dev/sys/platform/pt" {
            topo_path = "board".to_string();
        } else if topo_path.starts_with(k_prefix) {
            topo_path.replace_range(0..k_prefix.len(), "");
        } else if topo_path.starts_with(k_prefix2) {
            topo_path.replace_range(0..k_prefix2.len(), "");
        }

        // The driver's component name is based on the node name, which means that the
        // node name cam only have [a-z0-9-_.] characters. DFv1 composites contain ':'
        // which is not allowed, so replace those characters.
        // TODO(https://fxbug.dev/42062456): Migrate driver names to only use CF valid characters.
        // Since we use '.' to denote topology, replace them with '_'.
        topo_path.replace([':', '.'], "_").replace('/', ".")
    }

    pub fn children(&self) -> Vec<Rc<Node>> {
        self.children.borrow().clone()
    }

    pub fn parents(&self) -> Vec<Weak<Node>> {
        match &*self.node_type.borrow() {
            NodeTypeVariant::Normal { parent } => vec![parent.clone()],
            NodeTypeVariant::Composite { parents, .. } => parents.clone(),
        }
    }

    pub(crate) fn is_root_node(&self) -> bool {
        self.make_topological_path(false) == "dev"
    }

    pub fn driver_url(&self) -> String {
        match &*self.state.borrow() {
            NodeState::Starting { driver_url } => driver_url.clone(),
            NodeState::DriverComponent(c) => c.driver_url.clone(),
            NodeState::Quarantined { driver_url } => driver_url.clone(),
            NodeState::OwnedByParent { .. } => "owned by parent".to_string(),
            NodeState::CompositeParent => "owned by composite(s)".to_string(),
            NodeState::Unbound => "unbound".to_string(),
        }
    }

    pub fn is_quarantined(&self) -> bool {
        matches!(*self.state.borrow(), NodeState::Quarantined { .. })
    }

    pub fn get_primary_parent(&self) -> Option<Rc<Node>> {
        match &*self.node_type.borrow() {
            NodeTypeVariant::Normal { parent } => parent.upgrade(),
            NodeTypeVariant::Composite { parents, primary_index, .. } => {
                parents.get(*primary_index as usize).and_then(|p| p.upgrade())
            }
        }
    }

    pub fn get_bus_topology(&self) -> Vec<fdf::BusInfo> {
        let mut segments = vec![];
        let mut current = self.weak_self.upgrade();
        while let Some(node) = current {
            if let Some(bus_info) = node.bus_info.borrow().as_ref() {
                segments.push(bus_info.clone());
            }
            current = node.get_primary_parent();
        }
        segments.reverse();
        segments
    }

    pub fn get_node_properties(
        &self,
        parent_name: Option<&str>,
    ) -> Option<Vec<fdf::NodeProperty2>> {
        let parent_name = parent_name.unwrap_or("default");
        let properties = self.properties.borrow();
        for entry in properties.iter() {
            if entry.name == parent_name {
                return Some(entry.properties.clone());
            }
        }
        None
    }

    pub(crate) fn add_to_parents(&self) {
        let this_node = self.weak_self.upgrade().unwrap();
        match &*self.node_type.borrow() {
            NodeTypeVariant::Normal { parent } => {
                if let Some(p) = parent.upgrade() {
                    p.children.borrow_mut().push(this_node);
                } else {
                    warn!("Parent freed before child {} could be added to it", self.name());
                }
            }
            NodeTypeVariant::Composite { parents, .. } => {
                for parent in parents {
                    if let Some(p) = parent.upgrade() {
                        p.children.borrow_mut().push(this_node.clone());
                    } else {
                        warn!("Parent freed before child {} could be added to it", self.name());
                    }
                }
            }
        }
    }

    pub fn driver_host(&self) -> Option<Rc<dyn DriverHost>> {
        self.driver_host.borrow().clone()
    }

    pub fn is_composite(&self) -> bool {
        matches!(*self.node_type.borrow(), NodeTypeVariant::Composite { .. })
    }

    pub fn is_bound(&self) -> bool {
        matches!(*self.state.borrow(), NodeState::DriverComponent { .. })
    }

    pub fn evaluate_rematch_flags(
        &self,
        rematch_flags: fdd::RestartRematchFlags,
        url: &str,
    ) -> bool {
        if self.is_composite() && !rematch_flags.contains(fdd::RestartRematchFlags::COMPOSITE_SPEC)
        {
            return false;
        }

        if self.driver_url() == url && !rematch_flags.contains(fdd::RestartRematchFlags::REQUESTED)
        {
            return false;
        }

        if self.driver_url() != url
            && rematch_flags.contains(fdd::RestartRematchFlags::NON_REQUESTED)
        {
            return false;
        }

        true
    }

    pub fn set_subtree_dictionary(&self, dictionary: fidl_fuchsia_component_sandbox::CapabilityId) {
        if matches!(*self.dictionary.borrow(), NodeDictionary::Standard(_)) {
            panic!("Cannot set subtree dictionary on nodes with standard dictionaries");
        }

        *self.dictionary.borrow_mut() = NodeDictionary::Subtree(dictionary);
    }

    pub fn remove_subtree_dictionary(&self) {
        *self.dictionary.borrow_mut() = NodeDictionary::None;
    }

    pub fn has_subtree_dictionary(&self) -> bool {
        matches!(*self.dictionary.borrow(), NodeDictionary::Subtree(_))
    }

    pub fn skip_injected_offers(&self) -> bool {
        self.has_subtree_dictionary()
    }

    pub async fn prepare_dictionary(
        &self,
    ) -> Option<fidl_fuchsia_component_sandbox::DictionaryRef> {
        let dictionary_util = self.node_manager.get_dictionary_util().ok()?;

        let to_export = match *self.dictionary.borrow() {
            NodeDictionary::None => None,
            NodeDictionary::Standard(d) => Some(d),
            NodeDictionary::Subtree(d) => Some(d),
        };

        if let Some(d) = to_export {
            return dictionary_util.copy_export_dictionary(d).await.ok();
        }

        // map service to vec of aggregate sources.
        let mut sources = HashMap::<String, Vec<AggregateSource>>::new();
        for dictionary_offer in self.offers.borrow().iter() {
            if !matches!(dictionary_offer.transport, OfferTransport::Dictionary) {
                continue;
            }

            if let Some(connector) = dictionary_offer.dir_connector.take() {
                sources.entry(dictionary_offer.service_name.clone()).or_default().push(
                    AggregateSource {
                        dir_connector: Some(connector),
                        source_instance_filter: Some(
                            dictionary_offer.source_instance_filter.clone(),
                        ),
                        renamed_instances: Some(dictionary_offer.renamed_instances.clone()),
                        ..Default::default()
                    },
                );
            }
        }

        let aggregate_dictionary =
            dictionary_util.create_aggregate_dictionary(sources).await.ok()?;

        // Next time we prepare_dictionary, we can just use this
        *self.dictionary.borrow_mut() = NodeDictionary::Standard(aggregate_dictionary);

        dictionary_util.copy_export_dictionary(aggregate_dictionary).await.ok()
    }

    pub fn remove(
        self: &Rc<Self>,
        removal_set: RemovalSet,
        removal_tracker: Option<Weak<RefCell<NodeRemovalTracker>>>,
    ) {
        NodeShutdownCoordinator::remove(self.clone(), removal_set, removal_tracker);
    }

    pub(crate) fn get_shutdown_coordinator(
        &self,
    ) -> std::cell::RefMut<'_, NodeShutdownCoordinator> {
        self.node_shutdown_coordinator.borrow_mut()
    }

    pub fn weak_from_this(&self) -> Weak<Self> {
        self.weak_self.clone()
    }

    pub fn collection(&self) -> Collection {
        self.collection.get()
    }

    pub fn set_collection(&self, collection: Collection) {
        self.collection.set(collection);
    }

    pub(crate) fn set_should_destroy_driver_component(&self, value: bool) {
        self.should_destroy_driver_component.set(value);
    }

    pub fn set_driver_package_type(&self, package_type: fdf::DriverPackageType) {
        self.driver_package_type.set(package_type);
    }

    pub fn node_type(&self) -> std::cell::Ref<'_, NodeTypeVariant> {
        self.node_type.borrow()
    }

    pub fn offers(&self) -> std::cell::RefMut<'_, Vec<NodeOffer>> {
        self.offers.borrow_mut()
    }

    pub fn has_component_controller_proxy(&self) -> bool {
        self.component_controller.borrow().is_some()
    }
}
