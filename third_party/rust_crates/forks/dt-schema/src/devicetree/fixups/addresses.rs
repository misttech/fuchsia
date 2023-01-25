// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use crate::{devicetree::types::PropertyTypeLookup, path::JsonPath};

use super::{utils::get_cells_property_from_json, DevicetreeFixup, DevicetreeFixupError};

pub struct AddressFixup {
    node: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
}

impl AddressFixup {
    // Expect something of the format:
    // reg = [[<data>...]]
    // and output something of the format:
    // reg = [[data...], [data...]]
    fn fixup_reg(
        &self,
        value: &serde_json::Value,
        address_cells: usize,
        size_cells: usize,
    ) -> Result<serde_json::Value, DevicetreeFixupError> {
        match value {
            serde_json::Value::Array(array) => {
                if array.len() != 1 || !array[0].is_array() {
                    tracing::error!("reg property should be an array containing an array.");
                    return Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                        self.path.clone(),
                        array.clone().into(),
                    ));
                }

                let array = array[0].as_array().unwrap();
                let new_values: Option<Vec<Vec<u64>>> = array
                    .chunks(size_cells + address_cells)
                    .map(|v| v.iter().map(|i| i.as_u64()).collect::<Option<Vec<u64>>>())
                    .collect();
                if let Some(new_values) = new_values {
                    Ok(serde_json::to_value(new_values)?)
                } else {
                    Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                        self.path.clone(),
                        array.clone().into(),
                    ))
                }
            }
            other => {
                tracing::error!("reg property should be an array.");
                Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                    self.path.clone(),
                    other.clone(),
                ))
            }
        }
    }

    fn handle_ranges(
        &self,
        value: &serde_json::Value,
        address_cells: usize,
        prop: &str,
    ) -> Result<Option<serde_json::Value>, DevicetreeFixupError> {
        let array = match value {
            serde_json::Value::Array(array) => {
                if array.len() != 1 || !array[0].is_array() {
                    tracing::error!("ranges property should be an array containing an array.");
                    return Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                        self.path.extend(prop),
                        array.clone().into(),
                    ));
                }
                array[0].as_array().unwrap()
            }
            serde_json::Value::Bool(true) => {
                return Ok(None);
            }
            other => {
                tracing::error!("ranges property should be an array.");
                return Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                    self.path.extend(prop),
                    other.clone(),
                ));
            }
        };

        let child_address_cells =
            get_cells_property_from_json(&self.node, &self.path, "#address-cells")?;
        let child_size_cells = get_cells_property_from_json(&self.node, &self.path, "#size-cells")?;

        let new_values: Option<Vec<Vec<u64>>> = array
            .chunks(
                (child_size_cells + child_address_cells + address_cells as u64)
                    .try_into()
                    .unwrap(),
            )
            .map(|v| v.iter().map(|i| i.as_u64()).collect::<Option<Vec<u64>>>())
            .collect();
        Ok(Some(serde_json::to_value(new_values.ok_or(
            DevicetreeFixupError::UnexpectedPropertyFormat(
                self.path.extend(prop),
                array.clone().into(),
            ),
        )?)?))
    }
}

impl DevicetreeFixup for AddressFixup {
    fn new(
        _nodename: &str,
        node: &serde_json::Map<String, serde_json::Value>,
        path: crate::path::JsonPath,
        _type_lookup: &dyn PropertyTypeLookup,
    ) -> Result<Option<Self>, super::DevicetreeFixupError> {
        if node.contains_key("reg")
            || node.contains_key("ranges")
            || node.contains_key("dma-ranges")
        {
            Ok(Some(AddressFixup {
                node: node.clone(),
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
        let size_cells = lookup.get_prop_from_parents("#size-cells")?.unwrap_or(1) as usize;
        let address_cells = lookup.get_prop_from_parents("#address-cells")?.unwrap_or(2) as usize;

        if let Some(reg) = self.node.get("reg") {
            self.node.insert(
                "reg".to_owned(),
                self.fixup_reg(reg, address_cells, size_cells)?,
            );
        }

        if let Some(ranges) = self.node.get("ranges") {
            if let Some(new_ranges) = self.handle_ranges(ranges, address_cells, "ranges")? {
                self.node.insert("ranges".to_owned(), new_ranges);
            }
        }

        if let Some(ranges) = self.node.get("dma-ranges") {
            if let Some(new_ranges) = self.handle_ranges(ranges, address_cells, "dma-ranges")? {
                self.node.insert("dma-ranges".to_owned(), new_ranges);
            }
        }

        Ok(self.node)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::devicetree::fixups::utils::for_tests::FakeLookup;

    use super::*;
    #[test]
    fn test_fixup_reg() {
        let test_node = json!({
            "reg": [[0x0, 0xdead_beef_u32, 0x0, 0x1000, 0x0, 0x472_0000, 0x0, 0x1_0000]],
        });

        let test_lookup = FakeLookup::new()
            .with_parent("#address-cells", 2)
            .with_parent("#size-cells", 2);

        let result = AddressFixup::new(
            "",
            test_node.as_object().unwrap(),
            JsonPath::new(),
            &test_lookup,
        )
        .expect("valid node")
        .expect("fixup applies")
        .fixup(&test_lookup)
        .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                "reg": [[0x0, 0xdead_beef_u32, 0x0, 0x1000], [0x0, 0x472_0000, 0x0, 0x1_0000]],
            })
        );
    }

    #[test]
    fn test_reg_implicit_values() {
        let test_node = json!({
            "reg": [[0x0, 0xdead_beef_u32, 0x1000, 0x0, 0x472_0000, 0x1_0000]],
        });
        let test_lookup = FakeLookup::new();

        let result = AddressFixup::new(
            "",
            test_node.as_object().unwrap(),
            JsonPath::new(),
            &test_lookup,
        )
        .expect("valid node")
        .expect("fixup applies")
        .fixup(&test_lookup)
        .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                "reg": [[0x0, 0xdead_beef_u32, 0x1000], [0x0, 0x472_0000, 0x1_0000]],
            })
        );
    }

    #[test]
    fn test_range_conversion() {
        let test_node = json!({
            "#address-cells": [[1]],
            "#size-cells": [[2]],
            "ranges": [[0x1000, 0x0, 0x0, 0x0, 0x1000, 0x5000, 0x0, 0x4000, 0x0, 0x800]],
            "dma-ranges": [[0x10, 0x0, 0x1000, 0x4010, 0x0]],
        });
        let test_lookup = FakeLookup::new();

        let result = AddressFixup::new(
            "",
            test_node.as_object().unwrap(),
            JsonPath::new(),
            &test_lookup,
        )
        .expect("valid node")
        .expect("fixup applies")
        .fixup(&test_lookup)
        .expect("fixup ok");

        assert_eq!(
            serde_json::Value::from(result),
            json!({
                "#address-cells": [[1]],
                "#size-cells": [[2]],
                "ranges": [[0x1000, 0x0, 0x0, 0x0, 0x1000], [0x5000, 0x0, 0x4000, 0x0, 0x800]],
                "dma-ranges": [[0x10, 0x0, 0x1000, 0x4010, 0x0]],
            })
        );
    }
}
