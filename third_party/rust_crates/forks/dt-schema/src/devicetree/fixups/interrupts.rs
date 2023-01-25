// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::VecDeque;

use crate::{devicetree::types::PropertyTypeLookup, path::JsonPath};

use super::{utils::get_cells_property_from_json, DevicetreeFixup, DevicetreeFixupError};

pub struct InterruptFixup {
    value: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
}

/// Convert a matrix that looks like this:
/// [[<values>]]
/// into
/// [<values>]
fn flatten_matrix_with_one_row(val: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    val.as_array()
        .and_then(|v| if v.len() == 1 { v.get(0) } else { None })
        .and_then(|v| v.as_array())
}

impl DevicetreeFixup for InterruptFixup {
    fn new(
        _nodename: &str,
        node: &serde_json::Map<String, serde_json::Value>,
        path: JsonPath,
        _type_lookup: &dyn PropertyTypeLookup,
    ) -> Result<Option<Self>, super::DevicetreeFixupError> {
        if node.iter().any(|(k, v)| {
            // interrupts and interrupt-map could either be a bytes array (i.e. a 1D array),
            // or a uint32 matrix (an array of arrays).
            // a byte array is OK, but this fixup does not apply to byte arrays so we ensure that
            // the property is a matrix.
            (k == "interrupts" || k == "interrupt-map") && flatten_matrix_with_one_row(v).is_some()
        }) {
            Ok(Some(InterruptFixup {
                value: node.clone(),
                path,
            }))
        } else {
            Ok(None)
        }
    }

    fn fixup(
        mut self,
        lookup: &dyn super::DevicetreeLookup,
    ) -> Result<serde_json::Map<String, serde_json::Value>, super::DevicetreeFixupError> {
        let interrupt_cells = if let Some(serde_json::Value::Array(parent)) = self
            .value
            .get("interrupt-parent")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
        {
            let phandle = parent.first().and_then(|v| v.as_u64()).ok_or(
                DevicetreeFixupError::UnexpectedPropertyFormat(
                    self.path.extend("interrupt-parent"),
                    parent.clone().into(),
                ),
            )?;

            lookup.get_cells_size(
                self.path.extend("interrupt-parent"),
                phandle.try_into().unwrap(),
                "#interrupt-cells",
            )?
        } else {
            match lookup.get_prop_from_parents("interrupt-parent")? {
                Some(phandle) => {
                    lookup.get_cells_size(self.path.clone(), phandle, "#interrupt-cells")?
                }
                None => 1,
            }
        };

        if let Some(interrupts) = self
            .value
            .get("interrupts")
            .and_then(flatten_matrix_with_one_row)
        {
            let new_interrupts = interrupts
                .chunks(interrupt_cells as usize)
                .map(|v| v.to_vec())
                .collect::<Vec<Vec<serde_json::Value>>>();
            self.value.insert(
                "interrupts".to_owned(),
                serde_json::to_value(new_interrupts)?,
            );
        }

        if let Some(map) = self
            .value
            .get("interrupt-map")
            .and_then(flatten_matrix_with_one_row)
        {
            let my_path = self.path.extend("interrupt-map");
            // interrupt-map is of the format
            // <child-unit-address> <child-interrupt-specifier> <interrupt-parent> <parent-unit-address> <parent-interrupt-specifier>
            let mut map = map
                .iter()
                .map(|v| v.as_u64())
                .collect::<Option<VecDeque<u64>>>()
                .ok_or(DevicetreeFixupError::UnexpectedPropertyFormat(
                    my_path.clone(),
                    map.clone().into(),
                ))?;

            let interrupt_cells =
                get_cells_property_from_json(&self.value, &self.path, "#interrupt-cells")? as usize;
            // Note that according to the spec this could actually come from the parent node,
            // but in practice it appears to always come from our node.
            let address_cells =
                get_cells_property_from_json(&self.value, &self.path, "#address-cells")? as usize;
            let mut ret = vec![];
            while !map.is_empty() {
                let phandle = map[interrupt_cells + address_cells].try_into().unwrap();
                let parent_icells =
                    lookup.get_cells_size(my_path.clone(), phandle, "#interrupt-cells")? as usize;
                let parent_acells =
                    match lookup.get_cells_size(my_path.clone(), phandle, "#address-cells") {
                        Ok(v) => v as usize,
                        Err(super::PhandleError::NodeHadNoProperty(..)) => 0,
                        Err(e) => return Err(e.into()),
                    };

                let cells = interrupt_cells + address_cells + 1 + parent_acells + parent_icells;
                ret.push(map.drain(0..cells).collect::<Vec<_>>());
            }

            self.value
                .insert("interrupt-map".to_owned(), serde_json::to_value(ret)?);
        }

        Ok(self.value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::devicetree::fixups::utils::for_tests::FakeLookup;

    use super::*;
    #[test]
    fn test_interrupts() {
        let value = json!({
            "interrupt-parent": [[0xcafe]],
            "interrupts": [[0xa, 0xb, 0xc, 0xd, 0xe, 0xf]],
        });
        let lookup = FakeLookup::new().with_phandle(0xcafe, "#interrupt-cells", 3);

        let result = InterruptFixup::new("", value.as_object().unwrap(), JsonPath::new(), &lookup)
            .expect("node OK")
            .expect("fixup applies")
            .fixup(&lookup)
            .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                "interrupt-parent": [[0xcafe]],
                "interrupts": [[0xa, 0xb, 0xc], [0xd, 0xe, 0xf]],
            })
        )
    }

    #[test]
    fn test_interrupt_map() {
        let value = json!({
            "#interrupt-cells": [[1]],
            "#address-cells": [[1]],
            "interrupt-map": [[
                0x0, 0x0, 0xfeed, 0xabc, 0xdef,
                0x1, 0x0, 0xf00d, 0xaa, 0xbb, 0xcc,
                0x1, 0x2, 0xcafe, 0xaa,
            ]],
        });

        let lookup = FakeLookup::new()
            .with_phandle(0xfeed, "#interrupt-cells", 1)
            .with_phandle(0xfeed, "#address-cells", 1)
            .with_phandle(0xf00d, "#interrupt-cells", 2)
            .with_phandle(0xf00d, "#address-cells", 1)
            .with_phandle(0xcafe, "#interrupt-cells", 1);

        let result = InterruptFixup::new("", value.as_object().unwrap(), JsonPath::new(), &lookup)
            .expect("node OK")
            .expect("fixup applies")
            .fixup(&lookup)
            .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                "#interrupt-cells": [[1]],
                "#address-cells": [[1]],
                "interrupt-map": [
                    [0x0, 0x0, 0xfeed, 0xabc, 0xdef],
                    [0x1, 0x0, 0xf00d, 0xaa, 0xbb, 0xcc],
                    [0x1, 0x2, 0xcafe, 0xaa],
                ],
            })
        );
    }
}
