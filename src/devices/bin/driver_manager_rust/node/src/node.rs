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
use flyweights::FlyStr;
use fuchsia_async::{self as fasync};
use futures::channel::oneshot;
use log::{debug, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use {
    fidl_fuchsia_device_fs as fdevfs, fidl_fuchsia_driver_development as fdd,
    fidl_fuchsia_driver_framework as fdf,
};

#[derive(Clone)]
pub enum NodePropertyValue {
    IntValue(u32),
    StringValue(FlyStr),
    BoolValue(bool),
    EnumValue(FlyStr),
}

impl std::convert::From<fdf::NodePropertyValue> for NodePropertyValue {
    fn from(source: fdf::NodePropertyValue) -> Self {
        match source {
            fdf::NodePropertyValue::IntValue(i) => Self::IntValue(i),
            fdf::NodePropertyValue::StringValue(s) => Self::StringValue(FlyStr::new(s)),
            fdf::NodePropertyValue::BoolValue(b) => Self::BoolValue(b),
            fdf::NodePropertyValue::EnumValue(e) => Self::EnumValue(FlyStr::new(e)),
            _ => unimplemented!(),
        }
    }
}

impl std::convert::From<NodePropertyValue> for fdf::NodePropertyValue {
    fn from(source: NodePropertyValue) -> fdf::NodePropertyValue {
        match source {
            NodePropertyValue::IntValue(i) => fdf::NodePropertyValue::IntValue(i),
            NodePropertyValue::StringValue(s) => fdf::NodePropertyValue::StringValue(s.to_string()),
            NodePropertyValue::BoolValue(b) => fdf::NodePropertyValue::BoolValue(b),
            NodePropertyValue::EnumValue(e) => fdf::NodePropertyValue::EnumValue(e.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct NodeProperty {
    pub key: FlyStr,
    pub value: fdf::NodePropertyValue,
}

impl std::convert::From<fdf::NodeProperty2> for NodeProperty {
    fn from(source: fdf::NodeProperty2) -> Self {
        Self { key: FlyStr::new(source.key), value: source.value }
    }
}

impl std::convert::From<NodeProperty> for fdf::NodeProperty2 {
    fn from(source: NodeProperty) -> fdf::NodeProperty2 {
        fdf::NodeProperty2 { key: source.key.to_string(), value: source.value }
    }
}

#[derive(Clone)]
pub struct NodePropertyEntry {
    pub name: String,
    pub properties: Vec<NodeProperty>,
}

impl std::convert::From<fdf::NodePropertyEntry2> for NodePropertyEntry {
    fn from(source: fdf::NodePropertyEntry2) -> Self {
        Self {
            name: source.name,
            properties: source.properties.into_iter().map(|p| p.into()).collect(),
        }
    }
}

impl std::convert::From<NodePropertyEntry> for fdf::NodePropertyEntry2 {
    fn from(source: NodePropertyEntry) -> fdf::NodePropertyEntry2 {
        fdf::NodePropertyEntry2 {
            name: source.name,
            properties: source.properties.into_iter().map(|p| p.into()).collect(),
        }
    }
}

pub(crate) struct NodeInner {
    pub(crate) collection: Collection,
    pub(crate) driver_package_type: fdf::DriverPackageType,
    pub(crate) node_type: NodeTypeVariant,
    pub(crate) children: Vec<Rc<Node>>,
    pub(crate) properties: Vec<NodePropertyEntry>,
    pub(crate) symbols: Vec<fdf::NodeSymbol>,
    pub(crate) offers: Vec<NodeOffer>,
    pub(crate) devfs_device: DevfsDevice,
    pub(crate) protocol_connector: Option<fdevfs::ConnectorProxy>,
    pub(crate) controller_allowlist_passthrough: Option<Rc<ControllerAllowlistPassthrough>>,
    pub(crate) state: NodeState,
    pub(crate) driver_host: Option<Rc<dyn DriverHost>>,
    pub(crate) host_restart_on_crash: bool,
    pub(crate) remove_complete_callback: Option<oneshot::Sender<()>>,
    pub(crate) bus_info: Option<fdf::BusInfo>,
    pub(crate) composite_rebind_completer: Option<oneshot::Sender<Result<(), zx::Status>>>,
    pub(crate) restart_driver_url_suffix: Option<String>,
    pub(crate) driver_host_name_for_colocation: String,
    pub(crate) node_controller_server_binding: Option<NodeControllerServerBinding>,
    pub(crate) pending_bind_completer: Option<oneshot::Sender<Result<(), zx::Status>>>,
    pub(crate) bind_error: Option<fdf::DriverResult>,
    pub(crate) unbinding_children_completers: Vec<oneshot::Sender<Result<(), zx::Status>>>,
    pub(crate) dictionary: NodeDictionary,
    pub(crate) wait_for_driver_completer:
        Option<oneshot::Sender<Result<fdf::DriverResult, zx::Status>>>,
    pub(crate) component_controller: Option<ComponentControllerClientBinding>,
    pub(crate) start_request_receiver: Option<StartRequestReceiver>,
    pub(crate) start_handles: Option<Vec<fidl_fuchsia_process::HandleInfo>>,
    pub(crate) should_destroy_driver_component: bool,
}

pub struct Node {
    pub(crate) name: String,
    pub(crate) node_manager: Box<dyn NodeManager>,
    pub(crate) inner: RefCell<NodeInner>,
    pub(crate) node_shutdown_coordinator: RefCell<NodeShutdownCoordinator>,
    pub can_multibind_composites: bool,
    pub(crate) weak_self: Weak<Self>,
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
            parent.inner.borrow().driver_host.clone()
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
                inner: RefCell::new(NodeInner {
                    collection: Collection::None,
                    driver_package_type: fdf::DriverPackageType::Base,
                    node_type: NodeTypeVariant::Normal { parent },
                    children: Vec::new(),
                    properties: Vec::new(),
                    symbols: Vec::new(),
                    offers: Vec::new(),
                    devfs_device: DevfsDevice::new(),
                    protocol_connector: None,
                    controller_allowlist_passthrough: None,
                    state: NodeState::Unbound,
                    driver_host,
                    host_restart_on_crash: false,
                    remove_complete_callback: None,
                    bus_info: None,
                    composite_rebind_completer: None,
                    restart_driver_url_suffix: None,
                    driver_host_name_for_colocation: String::new(),
                    node_controller_server_binding: None,
                    pending_bind_completer: None,
                    bind_error: None,
                    unbinding_children_completers: Vec::new(),
                    dictionary: NodeDictionary::None,
                    wait_for_driver_completer: None,
                    component_controller: None,
                    start_request_receiver: None,
                    start_handles: None,
                    should_destroy_driver_component: false,
                }),
                node_shutdown_coordinator: RefCell::new(NodeShutdownCoordinator::new(
                    bridge,
                    enable_test_shutdown_delays,
                    shutdown_test_rng,
                )),
                can_multibind_composites: true,
                weak_self: weak_self.clone(),
                scope: fasync::Scope::new_with_name(format!("node:{name}")),
            }
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn token_koid(&self) -> Option<zx::Koid> {
        match &self.inner.borrow().state {
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
        self.inner.borrow_mut().bind_error = Some(fdf::DriverResult::MatchError(error.into_raw()));
    }

    pub fn on_start_error(&self, error: zx::Status) {
        self.inner.borrow_mut().bind_error = Some(fdf::DriverResult::StartError(error.into_raw()));
    }

    pub fn mark_as_composite_parent(&self) {
        self.inner.borrow_mut().state = NodeState::CompositeParent;
    }

    pub fn unmark_as_composite_parent(&self) {
        self.inner.borrow_mut().state = NodeState::Unbound;
    }

    pub(crate) fn is_pending_bind(&self) -> bool {
        match &self.inner.borrow().state {
            NodeState::DriverComponent(component) => component.state == DriverState::Binding,
            _ => false,
        }
    }

    pub(crate) fn clear_driver_host(&self) {
        if let NodeState::DriverComponent(ref mut component) = self.inner.borrow_mut().state {
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
        self.inner.borrow().children.clone()
    }

    pub fn parents(&self) -> Vec<Weak<Node>> {
        match &self.inner.borrow().node_type {
            NodeTypeVariant::Normal { parent } => vec![parent.clone()],
            NodeTypeVariant::Composite { parents, .. } => parents.clone(),
        }
    }

    pub(crate) fn is_root_node(&self) -> bool {
        self.make_topological_path(false) == "dev"
    }

    pub fn driver_url(&self) -> String {
        match &self.inner.borrow().state {
            NodeState::Starting { driver_url } => driver_url.clone(),
            NodeState::DriverComponent(c) => c.driver_url.clone(),
            NodeState::Quarantined { driver_url } => driver_url.clone(),
            NodeState::OwnedByParent { .. } => "owned by parent".to_string(),
            NodeState::CompositeParent => "owned by composite(s)".to_string(),
            NodeState::Unbound => "unbound".to_string(),
        }
    }

    pub fn is_quarantined(&self) -> bool {
        matches!(self.inner.borrow().state, NodeState::Quarantined { .. })
    }

    pub fn get_primary_parent(&self) -> Option<Rc<Node>> {
        match &self.inner.borrow().node_type {
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
            if let Some(bus_info) = node.inner.borrow().bus_info.as_ref() {
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
        let inner = self.inner.borrow();
        for entry in inner.properties.iter() {
            if entry.name == parent_name {
                return Some(entry.properties.clone().into_iter().map(|p| p.into()).collect());
            }
        }
        None
    }

    pub(crate) fn add_to_parents(&self) {
        let this_node = self.weak_self.upgrade().unwrap();
        match &self.inner.borrow().node_type {
            NodeTypeVariant::Normal { parent } => {
                if let Some(p) = parent.upgrade() {
                    p.inner.borrow_mut().children.push(this_node);
                } else {
                    warn!("Parent freed before child {} could be added to it", self.name());
                }
            }
            NodeTypeVariant::Composite { parents, .. } => {
                for parent in parents {
                    if let Some(p) = parent.upgrade() {
                        p.inner.borrow_mut().children.push(this_node.clone());
                    } else {
                        warn!("Parent freed before child {} could be added to it", self.name());
                    }
                }
            }
        }
    }

    pub fn driver_host(&self) -> Option<Rc<dyn DriverHost>> {
        self.inner.borrow().driver_host.clone()
    }

    pub fn is_composite(&self) -> bool {
        matches!(self.inner.borrow().node_type, NodeTypeVariant::Composite { .. })
    }

    pub fn is_bound(&self) -> bool {
        matches!(self.inner.borrow().state, NodeState::DriverComponent { .. })
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
        if matches!(self.inner.borrow().dictionary, NodeDictionary::Standard(_)) {
            panic!("Cannot set subtree dictionary on nodes with standard dictionaries");
        }

        self.inner.borrow_mut().dictionary = NodeDictionary::Subtree(dictionary);
    }

    pub fn remove_subtree_dictionary(&self) {
        self.inner.borrow_mut().dictionary = NodeDictionary::None;
    }

    pub fn has_subtree_dictionary(&self) -> bool {
        matches!(self.inner.borrow().dictionary, NodeDictionary::Subtree(_))
    }

    pub fn skip_injected_offers(&self) -> bool {
        self.has_subtree_dictionary()
    }

    pub async fn prepare_dictionary(
        &self,
    ) -> Option<fidl_fuchsia_component_sandbox::DictionaryRef> {
        let dictionary_util = self.node_manager.get_dictionary_util().ok()?;

        let to_export = match self.inner.borrow().dictionary {
            NodeDictionary::None => None,
            NodeDictionary::Standard(d) => Some(d),
            NodeDictionary::Subtree(d) => Some(d),
        };

        if let Some(d) = to_export {
            return dictionary_util.copy_export_dictionary(d).await.ok();
        }

        // map service to vec of aggregate sources.
        let mut sources = HashMap::<String, Vec<AggregateSource>>::new();
        {
            let mut inner = self.inner.borrow_mut();
            for dictionary_offer in inner.offers.iter_mut() {
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
        }

        let aggregate_dictionary =
            dictionary_util.create_aggregate_dictionary(sources).await.ok()?;

        // Next time we prepare_dictionary, we can just use this
        self.inner.borrow_mut().dictionary = NodeDictionary::Standard(aggregate_dictionary);

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
        self.inner.borrow().collection
    }

    pub fn set_collection(&self, collection: Collection) {
        self.inner.borrow_mut().collection = collection;
    }

    pub(crate) fn set_should_destroy_driver_component(&self, value: bool) {
        self.inner.borrow_mut().should_destroy_driver_component = value;
    }

    pub fn set_driver_package_type(&self, package_type: fdf::DriverPackageType) {
        self.inner.borrow_mut().driver_package_type = package_type;
    }

    pub fn set_driver_host_name_for_colocation(&self, name: &str) {
        self.inner.borrow_mut().driver_host_name_for_colocation = name.to_string();
    }

    pub fn node_type(&self) -> std::cell::Ref<'_, NodeTypeVariant> {
        std::cell::Ref::map(self.inner.borrow(), |i| &i.node_type)
    }

    pub fn symbols(&self) -> std::cell::Ref<'_, Vec<fdf::NodeSymbol>> {
        std::cell::Ref::map(self.inner.borrow(), |i| &i.symbols)
    }

    pub fn offers(&self) -> std::cell::Ref<'_, Vec<NodeOffer>> {
        std::cell::Ref::map(self.inner.borrow(), |i| &i.offers)
    }

    pub fn has_component_controller_proxy(&self) -> bool {
        self.inner.borrow().component_controller.is_some()
    }
}
