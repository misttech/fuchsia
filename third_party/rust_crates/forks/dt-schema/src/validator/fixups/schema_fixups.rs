// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use serde_json::map::Entry;

use super::{
    common_properties::CommonPropertiesFixup, do_fixup, fixup_201909::Fixup201909,
    interrupts::InterruptsFixup, property_fixups, FixupError,
};
use crate::path::JsonPath;

pub struct SchemaFixup {}

impl SchemaFixup {
    #[tracing::instrument(level = "debug", skip(value))]
    pub fn fixup(
        mut value: serde_json::Value,
        path: String,
    ) -> Result<serde_json::Value, FixupError> {
        let object = match value.as_object_mut() {
            Some(o) => o,
            None => {
                return Err(FixupError::UnexpectedSchemaError(
                    "Schema fixup expected object".to_owned(),
                    JsonPath::new(),
                    value,
                ));
            }
        };
        object.remove("examples");
        object.remove("maintainers");
        object.remove("historical");

        let ret = Self::fixup_subschema(value, true, JsonPath::new())?;
        Ok(ret)
    }

    #[tracing::instrument(level = "debug", skip(subschema), fields(path=path.back()))]
    fn fixup_subschema(
        mut subschema: serde_json::Value,
        is_node_property: bool,
        path: JsonPath,
    ) -> Result<serde_json::Value, FixupError> {
        match subschema.as_object_mut() {
            Some(map) => {
                map.remove("description");
            }
            None => {
                return Ok(subschema);
            }
        }

        subschema = do_fixup::<InterruptsFixup>("", subschema, &path)?;
        if is_node_property {
            subschema = do_fixup::<CommonPropertiesFixup>("", subschema, &path)?;
        }

        let object = subschema.as_object_mut().unwrap();

        // "additionalProperties: true" doesn't work with "unevaluatedProperties", so remove it.
        if let Entry::Occupied(e) = object.entry("additionalProperties") {
            if let Some(true) = e.get().as_bool() {
                e.remove();
            }
        }

        for k in [
            "select",
            "if",
            "then",
            "else",
            "additionalProperties",
            "not",
        ] {
            if let Some(value) = object.remove(k) {
                object.insert(
                    k.to_owned(),
                    Self::fixup_subschema(value, false, path.extend(k))?,
                );
            }
        }

        for k in ["allOf", "anyOf", "oneOf"] {
            if let Some(array) = object.get_mut(k).and_then(|v| v.as_array_mut()) {
                for item in array.iter_mut() {
                    *item = Self::fixup_subschema(item.clone(), true, path.extend(k))?;
                }
            }
        }

        for k in [
            "dependentRequired",
            "dependentSchemas",
            "dependencies",
            "properties",
            "patternProperties",
            "$defs",
        ] {
            if let Some(map) = object.get_mut(k).and_then(|v| v.as_object_mut()) {
                let path = path.extend(k);
                for (propname, v) in map.iter_mut() {
                    let new_value = property_fixups::walk_properties(
                        propname,
                        v.clone(),
                        path.extend(propname),
                    )?;
                    *v = Self::fixup_subschema(new_value, true, path.extend(propname))?;
                }
            }
        }

        let subschema = do_fixup::<Fixup201909>("", subschema, &path)?;

        Ok(subschema)
    }
}
