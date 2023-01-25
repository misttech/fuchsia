// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{collections::VecDeque, rc::Rc};

use byteorder::{BigEndian, ByteOrder};

use crate::path::JsonPath;

use super::{
    fixups::{DevicetreeLookup, PhandleError},
    Devicetree, Node,
};

#[derive(Clone)]
pub struct NodeAncestry<'a> {
    devicetree: &'a Devicetree,
    nodes: VecDeque<Rc<Node>>,
}

impl<'a> NodeAncestry<'a> {
    pub fn new(devicetree: &'a Devicetree) -> Self {
        NodeAncestry {
            devicetree,
            nodes: VecDeque::new(),
        }
    }

    pub fn visit(&self, node: Rc<Node>) -> Self {
        let mut new = self.clone();
        new.nodes.push_back(node);
        new
    }

    pub fn history(&self) -> impl Iterator<Item = &Rc<Node>> {
        self.nodes.iter().rev()
    }
}

impl DevicetreeLookup for NodeAncestry<'_> {
    fn get_cells_size(
        &self,
        phandle_path: JsonPath,
        phandle: u32,
        cell_prop: &str,
    ) -> Result<u32, PhandleError> {
        if phandle == 0 {
            // :(
            return Ok(1);
        }
        // Resolve the phandle
        let (node, node_path) = self
            .devicetree
            .by_phandle(phandle as usize)
            .ok_or(PhandleError::InvalidPhandle(phandle, phandle_path))?;

        let prop = node
            .properties()
            .iter()
            .find(|v| v.key() == cell_prop)
            .ok_or_else(|| {
                PhandleError::NodeHadNoProperty(cell_prop.to_owned(), node_path.clone())
            })?;

        let value = BigEndian::read_u32(&prop.value);
        Ok(value)
    }

    fn get_prop_from_parents(&self, prop: &str) -> Result<Option<u32>, PhandleError> {
        for node in self.history() {
            if let Some(value) = node.properties.iter().find(|k| k.key == prop) {
                return Ok(Some(BigEndian::read_u32(&value.value)));
            }
        }

        Ok(None)
    }
}
