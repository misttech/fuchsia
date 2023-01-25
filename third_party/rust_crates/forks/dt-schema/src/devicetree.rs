// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{
    collections::HashMap,
    io::{Read, Seek},
    rc::Rc,
};

use byteorder::{BigEndian, ByteOrder};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{devicetree::parser::Parser, path::JsonPath};

use self::{
    property::{DevicetreeJsonError, Property},
    types::PropertyTypeLookup,
    util::NodeAncestry,
};

mod fixups;
mod parser;
mod property;
pub mod types;
mod util;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum NodeOrProperty {
    Node(serde_json::Map<String, serde_json::Value>),
    Property(Vec<u8>),
}

#[derive(Debug, Clone)]
/// Represents a node in a devicetree.
pub struct Node {
    name: String,
    properties: Vec<Property>,
    children: Vec<Rc<Node>>,
}

impl Node {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn properties(&self) -> &Vec<Property> {
        &self.properties
    }

    #[tracing::instrument(level = "info", skip_all, fields(name=self.name))]
    pub fn as_json(
        self: &Rc<Self>,
        type_lookup: &dyn PropertyTypeLookup,
        path: JsonPath,
        ancestry: NodeAncestry,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DevicetreeJsonError> {
        let mut ret = serde_json::Map::new();
        for prop in self.properties.iter() {
            ret.insert(
                prop.key().clone(),
                prop.value_json(&self.name, type_lookup, path.extend(prop.key()))?,
            );
        }

        ret = fixups::fixup_node(&self.name, ret, &path, type_lookup, &ancestry)?;

        for node in self.children.iter() {
            ret.insert(
                node.name().to_owned(),
                json!(node.as_json(
                    type_lookup,
                    path.extend(node.name()),
                    ancestry.visit(self.clone()),
                )?),
            );
        }

        let name = if self.name.is_empty() {
            "/"
        } else {
            &self.name
        };

        ret.insert("$nodename".to_owned(), json!([name]));

        Ok(ret)
    }
}

#[derive(Debug, Clone)]
/// Represents a devicetree.
pub struct Devicetree {
    root_node: Rc<Node>,
    phandle: HashMap<usize, (Rc<Node>, JsonPath)>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    IoError(#[from] std::io::Error),
    #[error("Parse error")]
    ParseError(#[from] parser::ParseError),
}

impl Devicetree {
    pub fn from_reader(reader: impl Read + Seek) -> Result<Devicetree, Error> {
        let mut parser = Parser::new(reader)?;
        let mut ret = Devicetree {
            root_node: parser.parse()?,
            phandle: HashMap::new(),
        };

        let mut map = HashMap::new();
        for (item, path) in ret.iter() {
            if let Some(phandle) = item
                .properties()
                .iter()
                .find(|prop| prop.key() == "phandle")
            {
                map.insert(BigEndian::read_u32(&phandle.value) as usize, (item, path));
            }
        }

        ret.phandle = map;

        Ok(ret)
    }

    pub fn iter(&self) -> Iter {
        Iter::new(self.root_node.clone())
    }

    pub fn as_json(
        &self,
        lookup: &dyn PropertyTypeLookup,
    ) -> Result<serde_json::Value, DevicetreeJsonError> {
        self.root_node
            .as_json(lookup, JsonPath::new(), NodeAncestry::new(self))
            .map(|v| v.into())
    }

    pub fn by_phandle(&self, phandle: usize) -> Option<&(Rc<Node>, JsonPath)> {
        self.phandle.get(&phandle)
    }
}

pub struct Iter {
    node_stack: Vec<(Rc<Node>, JsonPath, isize)>,
}

impl Iter {
    fn new(root_node: Rc<Node>) -> Self {
        Iter {
            node_stack: vec![(root_node, JsonPath::new(), -1)],
        }
    }
}

impl Iterator for Iter {
    type Item = (Rc<Node>, JsonPath);

    fn next(&mut self) -> Option<Self::Item> {
        let (next_node, path, child_index) = self.node_stack.pop()?;
        // Record that we need to visit our next child at some point.
        self.node_stack
            .push((next_node.clone(), path.clone(), child_index + 1));

        // If we haven't started, yield the root node first.
        if child_index == -1 {
            return Some((next_node, JsonPath::new()));
        }

        let child_index: usize = child_index.try_into().unwrap();
        if next_node.children.len() > child_index {
            // Visit the next child of |next_node| next.
            let child = &next_node.children[child_index];
            let child_path = path.extend(child.name());
            self.node_stack.push((child.clone(), child_path.clone(), 0));
            Some((next_node.children[child_index].clone(), child_path))
        } else {
            // Finished with children of |next_node|, so move up the stack.
            self.node_stack.pop();
            self.next()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::validator::{dimension::Dimension, property_type::PropertyType};

    use super::*;

    fn empty_node(name: &str) -> Rc<Node> {
        Rc::new(Node {
            name: name.to_owned(),
            properties: vec![],
            children: vec![],
        })
    }

    fn make_prop(name: &str, data: &[u8]) -> Property {
        Property {
            key: name.to_owned(),
            value: data.into(),
        }
    }

    #[test]
    fn test_iterator() {
        let fake_tree = Devicetree {
            root_node: Rc::new(Node {
                name: "".to_owned(),
                properties: vec![],
                children: vec![
                    empty_node("a"),
                    Rc::new(Node {
                        name: "b".to_owned(),
                        properties: vec![],
                        children: vec![empty_node("c"), empty_node("d")],
                    }),
                    empty_node("e"),
                ],
            }),
            phandle: HashMap::new(),
        };

        let order: Vec<(String, String)> = fake_tree
            .iter()
            .map(|(n, p)| (n.name.clone(), p.to_string()))
            .collect();
        let expected: Vec<(String, String)> = [
            ("", "/"),
            ("a", "/a"),
            ("b", "/b"),
            ("c", "/b/c"),
            ("d", "/b/d"),
            ("e", "/e"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
        assert_eq!(order, expected);
    }

    struct FakeTypeLookup {
        types: HashMap<String, (BTreeSet<PropertyType>, Option<Dimension>)>,
    }

    impl PropertyTypeLookup for FakeTypeLookup {
        fn get_property_type(&self, propname: &str) -> BTreeSet<PropertyType> {
            self.types
                .get(propname)
                .map(|v| v.0.clone())
                .unwrap_or_default()
        }

        fn get_property_dimensions(&self, propname: &str) -> Option<Dimension> {
            self.types.get(propname).and_then(|v| v.1)
        }
    }

    #[test]
    fn test_as_json() {
        let fake_tree = Devicetree {
            root_node: Rc::new(Node {
                name: "".to_owned(),
                properties: vec![
                    make_prop("test,str-prop", &[b'a', b'b', 0]),
                    make_prop("test,int16-array-prop", &[0x10, 0x01, 0x20, 0x02]),
                ],
                children: vec![
                    empty_node("a"),
                    Rc::new(Node {
                        name: "b".to_owned(),
                        properties: vec![
                            make_prop("test,bool-prop", &[]),
                            make_prop("test,int8-matrix-prop", &[0x10, 0x10, 0x20, 0x20]),
                        ],
                        children: vec![empty_node("c"), empty_node("d")],
                    }),
                    empty_node("e"),
                ],
            }),
            phandle: HashMap::new(),
        };
        let types: HashMap<String, (BTreeSet<PropertyType>, Option<Dimension>)> = vec![
            ("test,str-prop", ([PropertyType::String], None)),
            ("test,int16-array-prop", ([PropertyType::Int16Array], None)),
            ("test,bool-prop", ([PropertyType::Flag], None)),
            (
                "test,int8-matrix-prop",
                ([PropertyType::Uint8Matrix], Some([[1, 0], [2, 2]].into())),
            ),
        ]
        .into_iter()
        .map(|(k, (i, d))| (k.to_owned(), (BTreeSet::from(i), d)))
        .collect();
        let lookup = FakeTypeLookup { types };

        assert_eq!(
            fake_tree.as_json(&lookup).expect("as json ok"),
            json!({
                "$nodename": ["/"],
                "test,str-prop": ["ab"],
                "test,int16-array-prop": [[0x1001, 0x2002]],
                "a": {"$nodename": ["a"]},
                "b": {
                    "$nodename": ["b"],
                    "test,bool-prop": true,
                    "test,int8-matrix-prop": [[0x10, 0x10], [0x20, 0x20]],
                    "c": {"$nodename": ["c"]},
                    "d": {"$nodename": ["d"]}
                },
                "e": {
                    "$nodename": ["e"],
                }
            })
        );
    }
}
