// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parent_set_collector::ParentSetCollector;
use driver_manager_node::{Node, NodeManager};
use futures::channel::oneshot;
use std::rc::{Rc, Weak};
use {fidl_fuchsia_driver_development as fdd, fidl_fuchsia_driver_framework as fdf};

pub struct CompositeNodeSpec {
    #[allow(unused)]
    name: String,
    parent_specs: Vec<fdf::ParentSpec2>,
    parent_nodes: Vec<Option<Weak<Node>>>,
    parent_set_collector: Option<ParentSetCollector>,
    driver_url: String,
    node_manager: Box<dyn NodeManager>,
    composite_info: Option<fdf::CompositeInfo>,
}

impl CompositeNodeSpec {
    pub fn new(
        name: String,
        parent_specs: Vec<fdf::ParentSpec2>,
        node_manager: Box<dyn NodeManager>,
    ) -> Self {
        let parent_nodes = vec![None; parent_specs.len()];
        Self {
            name,
            parent_specs,
            parent_nodes,
            parent_set_collector: None,
            driver_url: String::new(),
            node_manager,
            composite_info: None,
        }
    }

    fn bind_parent_impl(
        &mut self,
        composite_parent: fdf::CompositeParent,
        node_ptr: Weak<Node>,
    ) -> Result<Option<Weak<Node>>, zx::Status> {
        if self.composite_info.is_none() {
            self.composite_info = composite_parent.composite;
        }

        let composite_info = self.composite_info.as_ref().unwrap();
        let spec = composite_info.spec.as_ref().unwrap();
        let matched_driver = composite_info.matched_driver.as_ref().unwrap();
        let spec_name = spec.name.as_ref().unwrap();
        let composite_driver = matched_driver.composite_driver.as_ref().unwrap();
        let driver_info = composite_driver.driver_info.as_ref().unwrap();
        let parent_names = matched_driver.parent_names.as_ref().unwrap();
        let primary_index = matched_driver.primary_parent_index.unwrap_or(0);
        let url = driver_info.url.as_ref().unwrap();

        if self.parent_set_collector.is_none() {
            self.parent_set_collector = Some(ParentSetCollector::new(
                spec_name.clone(),
                parent_names.clone(),
                primary_index,
            ));
            self.driver_url = url.clone();
        }

        let collector = self.parent_set_collector.as_mut().unwrap();
        let index = composite_parent.index.unwrap();
        let properties = self.parent_specs[index as usize].properties.clone();
        collector.add_node(index, properties, node_ptr)?;

        match collector.try_to_assemble(self.node_manager.clone_box()) {
            Ok(node) => Ok(Some(Rc::downgrade(&node))),
            Err(zx::Status::SHOULD_WAIT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn bind_parent(
        &mut self,
        composite_parent: fdf::CompositeParent,
        node_ptr: Weak<Node>,
    ) -> Result<Option<Weak<Node>>, zx::Status> {
        let node_index = composite_parent.index.unwrap() as usize;
        if node_index >= self.parent_nodes.len() {
            return Err(zx::Status::OUT_OF_RANGE);
        }

        if let Some(current_at_index) = &self.parent_nodes[node_index]
            && current_at_index.upgrade().is_some()
        {
            return Err(zx::Status::ALREADY_BOUND);
        }

        let result = self.bind_parent_impl(composite_parent, node_ptr.clone());
        if result.is_ok() {
            self.parent_nodes[node_index] = Some(node_ptr);
        }
        result
    }

    pub fn get_composite_info(&self) -> fdd::CompositeNodeInfo {
        let mut info = fdd::CompositeNodeInfo::default();
        if self.parent_set_collector.is_none() {
            info.parent_topological_paths = Some(vec![None; self.parent_nodes.len()]);
            return info;
        }

        if let Some(composite_info) = &self.composite_info {
            let spec = composite_info.spec.as_ref().map(|s| fdf::CompositeNodeSpec {
                name: s.name.clone(),
                parents: s.parents.clone(),
                ..Default::default()
            });
            let matched_driver =
                composite_info.matched_driver.as_ref().map(|md| fdf::CompositeDriverMatch {
                    composite_driver: md.composite_driver.as_ref().map(|cd| {
                        fdf::CompositeDriverInfo {
                            composite_name: cd.composite_name.clone(),
                            driver_info: cd.driver_info.clone(),
                            ..Default::default()
                        }
                    }),
                    parent_names: md.parent_names.clone(),
                    primary_parent_index: md.primary_parent_index,
                    ..Default::default()
                });

            info.composite = Some(fdd::CompositeInfo::Composite(fdf::CompositeInfo {
                spec,
                matched_driver,
                ..Default::default()
            }));
        }

        let collector = self.parent_set_collector.as_ref().unwrap();
        info.parent_topological_paths = Some(collector.get_parent_topological_paths());

        if let Some(node_weak) = collector.completed_composite_node()
            && let Some(node) = node_weak.upgrade()
        {
            info.topological_path = Some(node.make_topological_path(false));
        }
        info
    }

    pub fn remove(&mut self, callback: oneshot::Sender<Result<(), zx::Status>>) {
        self.parent_nodes = vec![None; self.parent_specs.len()];
        if self.parent_set_collector.is_none() {
            let _ = callback.send(Ok(()));
            return;
        }

        let collector = self.parent_set_collector.as_mut().unwrap();
        collector.release_nodes();

        if let Some(node_weak) = collector.completed_composite_node()
            && let Some(node) = node_weak.upgrade()
        {
            node.remove_composite_node_for_rebind(callback);
            self.parent_set_collector = None;
            self.driver_url.clear();
            self.composite_info = None;
            return;
        }

        self.parent_set_collector = None;
        self.driver_url.clear();
        self.composite_info = None;
        let _ = callback.send(Ok(()));
    }
}
