// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//use jsonschema::{Draft, JSONSchema};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use valico::json_schema::{Schema as ValicoSchema, SchemaVersion, ValidationState};

use crate::{
    path::JsonPath,
    validator::{
        property_type::PropertyType, property_type_info::PropertyTypeInfo, util::Mergeable,
    },
};

use super::{
    compatible::extract_node_compatibles, error::ValidatorError, resolver::LocalOnlyResolver,
    GeneratedPropertyType,
};

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum AdditionalProperties {
    Bool(bool),
    Object(serde_json::Value),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    /// The select schema for this schema.
    /// We validate this schema against a node to see if the node should be matched to the rest of this schema.
    select: Option<serde_json::Value>,

    /// Properties with regular expression-matched names.
    #[serde(default)]
    pattern_properties: HashMap<String, serde_json::Value>,
    /// Properties with constant names.
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,

    /// Additional properties.
    additional_properties: Option<AdditionalProperties>,

    #[serde(rename = "$id")]
    spec_id: Option<String>,

    /// Other data. Because properties are able to reference things in the schema,
    /// we manually deserialize the schema object into this hashmap.
    #[serde(skip)]
    etc: serde_json::Map<String, serde_json::Value>,

    #[serde(skip)]
    select_id: Option<url::Url>,

    #[serde(skip)]
    schema_id: Option<url::Url>,

    #[serde(skip)]
    source_file: String,
}

impl Schema {
    pub fn from_value(
        value: serde_json::Value,
        source_file: String,
    ) -> Result<Self, ValidatorError> {
        let mut ret: Schema = serde_json::from_value(value.clone())?;
        ret.etc = serde_json::from_value(value)?;
        ret.source_file = source_file;
        Ok(ret)
    }

    pub fn spec_id(&self) -> &Option<String> {
        &self.spec_id
    }

    pub fn source_file(&self) -> &String {
        &self.source_file
    }

    #[cfg(test)]
    pub fn properties(&self) -> &HashMap<String, serde_json::Value> {
        &self.properties
    }

    pub(super) fn raw_schema(&self) -> serde_json::Value {
        self.etc.clone().into()
    }

    fn generate_select(&mut self) -> Result<Option<serde_json::Value>, ValidatorError> {
        if self.select.is_some() {
            return Ok(None);
        }

        if self.properties.is_empty() {
            return Ok(Some(json!(false)));
        }

        if let Some(compatibles) = self.properties.get("compatible") {
            let mut compatible_list = extract_node_compatibles(compatibles);
            // Remove meaningless compatibles.
            compatible_list.remove("syscon");
            compatible_list.remove("simple-mfd");

            if !compatible_list.is_empty() {
                let mut compatible_list = compatible_list.into_iter().collect::<Vec<_>>();
                compatible_list.sort();
                return Ok(Some(json!({
                    "required": ["compatible"],
                    "properties": {
                        "compatible": {
                            "contains": {
                                "enum": compatible_list,
                            }
                        }
                    }
                })));
            }
        }

        // Find a more interesting nodename schema than just "$nodename: true"
        if let Some(nodename) = self
            .properties
            .get("$nodename")
            .filter(|&v| !v.as_bool().unwrap_or(false))
        {
            return Ok(Some(json!({
                "required": ["$nodename"],
                "properties": {
                    "$nodename": nodename,
                }
            })));
        }

        Ok(Some(json!(false)))
    }

    pub fn fixup_select_and_finalise(
        &mut self,
        resolver: LocalOnlyResolver,
        scope: &mut valico::json_schema::Scope,
    ) -> Result<(), ValidatorError> {
        let select = self.generate_select()?;
        if let Some(new_select) = select {
            self.select = Some(new_select);
        }

        if let Some(ref select) = self.select {
            self.etc.insert("select".to_owned(), select.clone());
            let mut scope_url =
                url::Url::parse(self.spec_id.as_ref().expect("have spec id")).unwrap();
            scope_url.set_fragment(Some("/select"));
            self.select_id = Some(scope_url);
        }

        self.schema_id = Some(scope.compile(self.etc.clone().into(), false)?);
        Ok(())
    }

    pub fn applies(&self, value: &serde_json::Value, scope: &valico::json_schema::Scope) -> bool {
        self.select_id
            .as_ref()
            .and_then(|url| scope.resolve(url))
            .map(|schema| schema.validate(value).is_strictly_valid())
            .unwrap_or(false)
    }

    pub fn validate<'a>(
        &'a self,
        value: &'a serde_json::Value,
        path: &str,
        scope: &valico::json_schema::Scope,
    ) -> Option<ValidationState> {
        scope
            .resolve(self.schema_id.as_ref().unwrap())
            .map(|v| v.validate_in(value, path))
    }

    fn resolve(&self, path: &str) -> Result<Option<&serde_json::Value>, ValidatorError> {
        tracing::debug!("resolve {}", path);
        if path.starts_with("#/") {
            let mut iter = path.split('/').skip(1);
            let path = iter
                .next()
                .ok_or(ValidatorError::InvalidReference(path.to_owned()))?;
            let mut s = self
                .etc
                .get(path)
                .ok_or(ValidatorError::InvalidReference(path.to_owned()))?;
            for p in iter {
                s = s
                    .as_object()
                    .and_then(|obj| obj.get(p))
                    .ok_or(ValidatorError::InvalidReference(path.to_owned()))?;
            }
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }

    /// Determine type of a single property.
    /// |k|: name of property
    /// |v|: value of property
    /// |prop_types|: map of prop-name to list of types for that property so far.
    /// |is_pattern|: true if this is in `patternProperties`.
    fn handle_one_property<'a>(
        &self,
        root_schema: &'a Schema,
        k: &String,
        mut v: &'a serde_json::Value,
        prop_types: &mut HashMap<String, Vec<GeneratedPropertyType>>,
        is_pattern: bool,
        path: JsonPath,
    ) -> Result<(), ValidatorError> {
        // If there's a reference to some other type in the schema, pull the information from there.
        // Note that we only support references within the same file.
        // There's logic in |PropertyTypeInfo::extract_type| to "guess" what type a reference to another file refers to.
        while let Some(reffed) = v
            .as_object()
            .and_then(|e| e.get("$ref"))
            .and_then(|v| v.as_str())
            .map(|v| root_schema.resolve(v))
        {
            let reffed = match reffed? {
                Some(r) => r,
                None => break,
            };

            v = reffed;
        }
        if !v.is_object() {
            return Ok(());
        }
        let info: PropertyTypeInfo = serde_json::from_value(v.clone())?;
        // Recurse into all/any/one of:
        for (key, item) in [
            ("allOf", &info.all_of),
            ("anyOf", &info.any_of),
            ("oneOf", &info.one_of),
        ]
        .into_iter()
        .filter(|(_, v)| v.is_some())
        {
            for (index, value) in item.as_ref().unwrap().iter().enumerate() {
                self.handle_one_property(
                    root_schema,
                    k,
                    value,
                    prop_types,
                    is_pattern,
                    path.extend_array_index(key, index),
                )?;
            }
        }

        // and look for a type definition for this value.
        let mut type_info = match info.extract_type(root_schema, k, is_pattern, path.clone())? {
            Some(info) => info,
            None => {
                tracing::debug!("no type for {}", k);
                return Ok(());
            }
        };
        tracing::debug!("type for {}: {:?}", k, type_info);

        // Grab the property type that corresponds to the final value.
        let vec = match prop_types.get_mut(k) {
            Some(v) => v,
            None => {
                prop_types.insert(k.clone(), vec![]);
                prop_types.get_mut(k).unwrap()
            }
        };

        // If we failed to infer a type, only store this value if we have no other information.
        if type_info.r#type.is_none() {
            if vec.is_empty() {
                vec.push(type_info);
            }
            return Ok(());
        }

        // We know what the type is. Now we reconcile our "new" type with what we already know.
        let new_type = type_info.r#type.unwrap();
        let mut index_to_remove = None;
        for (index, item) in vec.iter_mut().enumerate() {
            // This has no known type, remove it.
            if item.r#type.is_none() {
                // remove a value with |type == None|.
                index_to_remove = Some(index);
                break;
            }
            let item_type = item.r#type.unwrap();

            // Merge the two dimensions, if |item| is a matrix type.
            if let Some(dim) = type_info.dim {
                if item.r#type.map(|v| v.is_matrix()).unwrap_or(false) {
                    match item.dim {
                        None => item.dim = type_info.dim,
                        Some(existing) => {
                            if existing != dim {
                                item.dim = Some(existing.merge(dim));
                            }
                        }
                    }
                }
            }

            // Already have the same or looser type, so just add our id.
            if item_type.is_looser(&new_type) {
                item.id.insert(self.spec_id.clone().unwrap());

                if new_type == PropertyType::Node {
                    // Descend into child schemas if we haven't seen this node already.
                    break;
                }
            } else if new_type.is_looser(&item_type) {
                // Replace the scalar type with looser type.
                type_info.id.extend(item.id.iter().cloned());
                index_to_remove = Some(index);
                break;
            }
        }

        // If we found an existing item, remove it.
        if let Some(remove) = index_to_remove {
            vec.swap_remove(remove);
        }

        // Add our new type info.
        vec.push(type_info);

        // Descend and merge the subschemas in to our discovered types.
        for (prop_name, value) in info.etc.iter().filter(|(k, _)| {
            k == &"properties" || k == &"additionalProperties" || k == &"patternProperties"
        }) {
            if let Ok(mut schema) =
                Schema::from_value(json!({prop_name: value.clone()}), self.source_file.clone())
            {
                tracing::debug!("descending to {}/{}", k, prop_name);
                if schema.spec_id.is_none() {
                    schema.spec_id = self.spec_id.clone();
                }
                let new_types =
                    schema.generate_property_types(Some(root_schema), path.extend(prop_name))?;
                prop_types.merge(new_types);
            }
        }

        Ok(())
    }

    /// Extract property types from this schema.
    /// Returns a mapping of String (the property or pattern for the property) to a vector of possible property types.
    pub fn generate_property_types(
        &self,
        root_schema: Option<&Schema>,
        path: JsonPath,
    ) -> Result<HashMap<String, Vec<GeneratedPropertyType>>, ValidatorError> {
        let root_schema = root_schema.unwrap_or(self);
        let mut prop_types = HashMap::new();
        if let Some(AdditionalProperties::Object(ref extras)) = self.additional_properties {
            let mut extras = Schema::from_value(extras.clone(), self.source_file.clone())?;
            if extras.spec_id.is_none() {
                extras.spec_id = self.spec_id.clone();
            }
            prop_types = extras
                .generate_property_types(Some(root_schema), path.extend("additionalProperties"))?;
        }

        let prop_path = path.extend("properties");
        for (k, v) in self.properties.iter() {
            self.handle_one_property(
                root_schema,
                k,
                v,
                &mut prop_types,
                false,
                prop_path.extend(k),
            )?;
        }

        let pat_path = path.extend("patternProperties");
        for (k, v) in self.pattern_properties.iter() {
            self.handle_one_property(root_schema, k, v, &mut prop_types, true, pat_path.extend(k))?;
        }

        Ok(prop_types)
    }
}
