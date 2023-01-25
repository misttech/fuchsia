// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use crate::{
    devicetree::types::PropertyTypeLookup, path::JsonPath, validator::property_type::PropertyType,
};

use super::{utils::PhandleIterator, DevicetreeFixup, PhandleError};

pub struct PhandleFixup {
    node: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
    keys_to_visit: Vec<String>,
}

fn needs_fixup(lookup: &dyn PropertyTypeLookup, key: &str, value: &serde_json::Value) -> bool {
    let types = lookup.get_property_type(key);
    #[allow(clippy::if_same_then_else)]
    if !types.contains(&PropertyType::PhandleArray) {
        false
    } else if lookup
        .get_property_dimensions(key)
        .map(|v| v.is_fixed())
        .unwrap_or(false)
    {
        // Fixed dimensions, so skip.
        false
    } else {
        // If this has already been fixed up, skip it.
        !value
            .as_array()
            .map(|v| if v.len() == 1 { !v[0].is_array() } else { true })
            .unwrap_or(true)
    }
}

impl DevicetreeFixup for PhandleFixup {
    fn new(
        _nodename: &str,
        node: &serde_json::Map<String, serde_json::Value>,
        path: crate::path::JsonPath,
        type_lookup: &dyn PropertyTypeLookup,
    ) -> Result<Option<Self>, super::DevicetreeFixupError> {
        let keys_to_visit: Vec<String> = node
            .iter()
            .filter(|(k, v)| needs_fixup(type_lookup, k, v))
            .map(|(k, _)| k.clone())
            .collect();
        if !keys_to_visit.is_empty() {
            Ok(Some(PhandleFixup {
                node: node.clone(),
                path,
                keys_to_visit,
            }))
        } else {
            Ok(None)
        }
    }

    fn fixup(
        mut self,
        lookup: &dyn super::DevicetreeLookup,
    ) -> Result<serde_json::Map<String, serde_json::Value>, super::DevicetreeFixupError> {
        for key in self.keys_to_visit.into_iter() {
            let value = self.node.get(&key).unwrap().as_array().unwrap()[0]
                .as_array()
                .unwrap();
            let iter = PhandleIterator::new(value, lookup, self.path.extend(&key), &key);

            let translated = iter.collect::<Result<Vec<_>, _>>();
            match translated {
                Ok(value) => self.node.insert(key.clone(), serde_json::to_value(value)?),
                Err(PhandleError::NodeHadNoProperty(..)) => continue,
                Err(e) => return Err(e.into()),
            };
        }
        Ok(self.node)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{devicetree::fixups::utils::for_tests::FakeLookup, path::JsonPath};

    use super::*;
    #[test]
    fn test_simple_phandle_fixup() {
        let node = json!({
            "fudges": [[
                0xcafe, 0x1, 0x2, 0x3,
                0xfeed, 0x1,
                0xd00d, 0x9, 0xa,
            ]]
        });

        let lookup = FakeLookup::new()
            .with_phandle(0xcafe, "#fudge-cells", 3)
            .with_phandle(0xfeed, "#fudge-cells", 1)
            .with_phandle(0xd00d, "#fudge-cells", 2)
            .with_prop_type(
                "fudges",
                &[PropertyType::PhandleArray],
                Some([[1, 0], [1, 0]].into()),
            );

        let result = PhandleFixup::new("", node.as_object().unwrap(), JsonPath::new(), &lookup)
            .expect("node ok")
            .expect("fixup applies")
            .fixup(&lookup)
            .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                            "fudges": [
                [0xcafe, 0x1, 0x2, 0x3],
                [0xfeed, 0x1],
                [0xd00d, 0x9, 0xa],
            ]

            })
        )
    }

    #[test]
    fn test_interconnects_fixup() {
        let node = json!({
            "interconnects": [[
                0xcafe, 0x1, 0x2, 0x3,
                0xfeed, 0x1,
            ]]
        });

        let lookup = FakeLookup::new()
            .with_phandle(0xcafe, "#interconnect-cells", 3)
            .with_phandle(0xfeed, "#interconnect-cells", 1)
            .with_prop_type(
                "interconnects",
                &[PropertyType::PhandleArray],
                Some([[1, 0], [1, 0]].into()),
            );

        let result = PhandleFixup::new("", node.as_object().unwrap(), JsonPath::new(), &lookup)
            .expect("node ok")
            .expect("fixup applies")
            .fixup(&lookup)
            .expect("fixup ok");

        assert_eq!(serde_json::Value::from(result), node);
    }
}
