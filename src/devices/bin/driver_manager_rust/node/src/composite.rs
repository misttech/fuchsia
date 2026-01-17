// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use crate::node_manager::NodeManager;
use crate::shutdown::NodeBridge;
use crate::types::{NodeDictionary, NodeState, NodeTypeVariant};
use driver_manager_devfs::DevfsDevice;
use driver_manager_shutdown::{NodeShutdownCoordinator, ShutdownIntent};
use driver_manager_types::{Collection, NodeOffer, OfferTransport};
use futures::channel::oneshot;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use {
    fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_driver_framework as fdf,
    fuchsia_async as fasync,
};

impl Node {
    pub fn new_composite(
        name: &str,
        parents: Vec<Weak<Node>>,
        parents_names: Vec<String>,
        primary_index: u32,
        node_manager: Box<dyn NodeManager>,
    ) -> Rc<Self> {
        let driver_host = if let Some(parent) = parents[primary_index as usize].upgrade() {
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
                node_type: RefCell::new(NodeTypeVariant::Composite {
                    parents,
                    parents_names,
                    primary_index,
                }),
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

    pub fn create_composite_offer(
        offer: &NodeOffer,
        parents_name: &str,
        primary_parent: bool,
    ) -> NodeOffer {
        let is_default_offer = |name: &str| name == "default";

        let mut new_instance_count = offer.renamed_instances.len();
        if primary_parent {
            new_instance_count += offer
                .renamed_instances
                .iter()
                .filter(|instance| is_default_offer(&instance.target_name))
                .count();
        }

        let mut new_mappings = Vec::with_capacity(new_instance_count);
        for instance in &offer.renamed_instances {
            let target_name = &instance.target_name;
            if !is_default_offer(target_name) {
                new_mappings.push(instance.clone());
                continue;
            }

            if primary_parent {
                new_mappings.push(instance.clone());
            }

            new_mappings.push(fdecl::NameMapping {
                source_name: instance.source_name.clone(),
                target_name: parents_name.to_string(),
            });
        }

        let mut new_filter_count = offer.source_instance_filter.len();
        if primary_parent {
            new_filter_count += offer
                .source_instance_filter
                .iter()
                .filter(|filter| is_default_offer(filter))
                .count();
        }

        let mut new_filters = Vec::with_capacity(new_filter_count);
        for filter in &offer.source_instance_filter {
            if !is_default_offer(filter) {
                new_filters.push(filter.clone());
                continue;
            }

            if primary_parent {
                new_filters.push("default".to_string());
            }

            new_filters.push(parents_name.to_string());
        }

        NodeOffer {
            source_name: offer.source_name.clone(),
            source_collection: offer.source_collection,
            transport: offer.transport.clone(),
            service_name: offer.service_name.clone(),
            source_instance_filter: new_filters,
            renamed_instances: new_mappings,
            dir_connector: offer.dir_connector.clone(),
        }
    }

    pub fn create_composite_node(
        node_name: &str,
        parents: Vec<Weak<Node>>,
        parents_names: Vec<String>,
        parent_properties: &[fdf::NodePropertyEntry2],
        node_manager: Box<dyn NodeManager>,
        primary_index: u32,
    ) -> Result<Rc<Self>, zx::Status> {
        if parents.is_empty() {
            return Err(zx::Status::INVALID_ARGS);
        }
        if parents.len() != parent_properties.len() {
            return Err(zx::Status::INVALID_ARGS);
        }
        if primary_index as usize >= parents.len() {
            return Err(zx::Status::OUT_OF_RANGE);
        }

        let mut has_dictionary_offer = false;

        let mut offers = vec![];
        for (i, parent) in parents.iter().enumerate() {
            let parent = parent.upgrade().expect("parent should be alive");
            for offer in parent.offers.borrow().iter() {
                let new_offer = Self::create_composite_offer(
                    offer,
                    &parents_names[i],
                    i == primary_index as usize,
                );

                if matches!(new_offer.transport, OfferTransport::Dictionary) {
                    has_dictionary_offer = true;
                }

                offers.push(new_offer);
            }
        }

        let composite =
            Self::new_composite(node_name, parents, parents_names, primary_index, node_manager);

        Self::set_composite_parent_properties(&composite, parent_properties);

        let primary_parent = composite.get_primary_parent().expect("primary parent should exist");
        let symbols = primary_parent.symbols.borrow().clone();
        *composite.symbols.borrow_mut() = symbols;

        // Copy the subtree dictionary of the primary parent node down to the composite.
        if let NodeDictionary::Subtree(d) = *primary_parent.dictionary.borrow() {
            if has_dictionary_offer {
                panic!("Cannot use dictionary offers on node");
            }

            *composite.dictionary.borrow_mut() = NodeDictionary::Subtree(d);
        }

        *composite.offers.borrow_mut() = offers;

        composite.add_to_parents();

        let Some(ref topological) = primary_parent.devfs_device.borrow().topological else {
            panic!(
                "Missing topological devfs node for primary parent: {}",
                composite.make_topological_path(false)
            );
        };

        // TODO(https://fxbug.dev/331779666): disable controller access for composite nodes
        let devfs_device = topological.add_child(
            &composite.name,
            None,
            composite.create_devfs_passthrough(None, None, true, "".to_string()),
        )?;
        *composite.devfs_device.borrow_mut() = devfs_device;

        Ok(composite)
    }

    pub fn set_composite_parent_properties(&self, parent_properties: &[fdf::NodePropertyEntry2]) {
        let mut properties = self.properties.borrow_mut();
        properties.clear();
        *properties = parent_properties.to_vec();
        if let NodeTypeVariant::Composite { primary_index, .. } = &*self.node_type.borrow() {
            let default_properties = &parent_properties[*primary_index as usize].properties;
            properties.push(fdf::NodePropertyEntry2 {
                name: "default".to_string(),
                properties: default_properties.to_vec(),
            });
        }
    }

    pub fn remove_composite_node_for_rebind(
        self: &Rc<Self>,
        completer: oneshot::Sender<Result<(), zx::Status>>,
    ) {
        let mut rebind_completer = self.composite_rebind_completer.borrow_mut();
        if rebind_completer.is_some() {
            let _ = completer.send(Err(zx::Status::ALREADY_EXISTS));
            return;
        }

        if !matches!(&*self.node_type.borrow(), NodeTypeVariant::Composite { .. }) {
            let _ = completer.send(Err(zx::Status::NOT_SUPPORTED));
            return;
        }

        *rebind_completer = Some(completer);
        self.node_shutdown_coordinator
            .borrow_mut()
            .set_shutdown_intent(ShutdownIntent::RebindComposite);
        self.remove(driver_manager_shutdown::RemovalSet::All, None);
    }
}
