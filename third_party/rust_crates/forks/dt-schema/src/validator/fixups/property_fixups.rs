// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use super::{
    do_fixup, empty_items::EmptyItemsRemovalFixup, fixup_201909::Fixup201909,
    int_items_to_matrix::IntItemsToMatrix, int_minmax::IntMinMaxToMatrixFixup,
    items_size::ItemsSizeFixup, reg::RegFixup, single_int::SingleIntFixup,
    single_string::SingleStringFixup, variable_matrix_fixup::VariableIntMatrixFixup, FixupError,
};
use crate::path::JsonPath;

pub fn walk_properties(
    propname: &str,
    mut value: serde_json::Value,
    prop_path: JsonPath,
) -> Result<serde_json::Value, FixupError> {
    if let Some(map) = value.as_object_mut() {
        for cond in ["allOf", "anyOf", "oneOf"] {
            if let Some(serde_json::Value::Array(array)) = map.get_mut(cond) {
                for (i, item) in array.iter_mut().enumerate() {
                    *item = walk_properties(
                        propname,
                        item.clone(),
                        prop_path.extend_array_index(cond, i),
                    )?;
                }
            }
        }

        if let Some(value) = map.get_mut("then") {
            *value = walk_properties(propname, value.clone(), prop_path.extend("then"))?;
        }

        fixup_properties(propname, value, &prop_path)
    } else {
        Ok(value)
    }
}

fn fixup_properties(
    propname: &str,
    mut value: serde_json::Value,
    path: &JsonPath,
) -> Result<serde_json::Value, FixupError> {
    value.as_object_mut().and_then(|v| v.remove("description"));

    let value = do_fixup::<RegFixup>(propname, value, path)?;
    let value = do_fixup::<EmptyItemsRemovalFixup>(propname, value, path)?;
    let value = do_fixup::<VariableIntMatrixFixup>(propname, value, path)?;

    let value = do_fixup::<IntMinMaxToMatrixFixup>(propname, value, path)?;
    let value = do_fixup::<IntItemsToMatrix>(propname, value, path)?;

    let value = do_fixup::<SingleStringFixup>(propname, value, path)?;
    let value = do_fixup::<SingleIntFixup>(propname, value, path)?;
    let value = do_fixup::<ItemsSizeFixup>(propname, value, path)?;
    let value = do_fixup::<Fixup201909>(propname, value, path)?;

    Ok(value)
}
