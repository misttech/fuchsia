// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

mod array;
mod compatible;
pub mod dimension;
mod error;
mod fixups;
pub mod property_type;
mod property_type_info;
mod resolver;
mod schema;
mod util;

use serde::Serialize;
use tracing::Level;
use valico::json_schema::{SchemaVersion, Scope};

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::Write,
    sync::Arc,
};

use fancy_regex::Regex;

use crate::{devicetree::types::PropertyTypeLookup, parallel::parallel, path::JsonPath};

use self::{
    dimension::Dimension, error::ValidatorError, fixups::FixupError, property_type::PropertyType,
    resolver::LocalOnlyResolver, schema::Schema,
};

#[derive(Debug, Serialize)]
/// Note that |PartialEq| is only implemented with respect to the type and dimensions.
pub struct GeneratedPropertyType {
    #[serde(skip)]
    id: HashSet<String>,
    r#type: Option<PropertyType>,
    #[serde(skip)]
    regex: Option<Regex>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dim: Option<Dimension>,
}

impl PartialEq for GeneratedPropertyType {
    fn eq(&self, other: &Self) -> bool {
        self.r#type == other.r#type && self.dim == other.dim
    }
}

impl Eq for GeneratedPropertyType {
    fn assert_receiver_is_total_eq(&self) {}
}

impl PartialOrd for GeneratedPropertyType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.r#type.partial_cmp(&other.r#type) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.dim.partial_cmp(&other.dim)
    }
}

impl Ord for GeneratedPropertyType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum FileValidatorError {
    #[error("validator error in file {1}: {0}")]
    ValidatorError(ValidatorError, String),

    #[error("fixup error in file {1}: {0}")]
    FixupError(FixupError, String),
}

#[derive(Debug)]
pub struct Validator {
    schemas: Vec<Schema>,
    /// Map of |property-name| to |possible-property-types|. Used when decoding the DTB.
    property_types: BTreeMap<String, Vec<GeneratedPropertyType>>,
    /// Map of |property-regex| to |possible-property-types|. Used when decoding the DTB.
    pattern_property_types: BTreeMap<String, (Regex, Vec<GeneratedPropertyType>)>,
    /// List of valid compatibles.
    // TODO(simonshields): generate this and use it for making sure all nodes have schemas.
    #[allow(unused)]
    valid_compatibles: HashSet<String>,
    /// Scope which schemas are compiled in to.
    scope: Scope,
}

impl Validator {
    pub fn new_with_schemas(paths: &[String]) -> Result<Self, FileValidatorError> {
        let mut schemas = parallel(
            Arc::new(|path: &String| -> Result<Schema, FileValidatorError> {
                let file = std::fs::File::open(path)
                    .map_err(|v| FileValidatorError::ValidatorError(v.into(), path.clone()))?;

                let pre_fixup = serde_yaml::from_reader(file)
                    .map_err(|v| FileValidatorError::ValidatorError(v.into(), path.clone()))?;

                // Run fixup passes over schema before we parse it into an actual |Schema| object.
                let value = fixups::schema_fixups::SchemaFixup::fixup(pre_fixup, path.clone())
                    .map_err(|v| FileValidatorError::FixupError(v, path.clone()))?;
                Schema::from_value(value, path.clone())
                    .map_err(|v| FileValidatorError::ValidatorError(v, path.clone()))
            }),
            paths,
            "loading schemas",
        )?;

        // Determine property types.
        let mut property_types: BTreeMap<String, Vec<GeneratedPropertyType>> = BTreeMap::new();
        let mut pattern_property_types: BTreeMap<String, (Regex, Vec<GeneratedPropertyType>)> =
            BTreeMap::new();
        for schema in schemas.iter_mut() {
            let types = schema.generate_property_types(Some(schema), JsonPath::new());

            let types = types
                .map_err(|v| FileValidatorError::ValidatorError(v, schema.source_file().clone()))?;

            // Distil types into the final output.
            for (k, v) in types.into_iter() {
                let ignore_untyped = |v: &GeneratedPropertyType| v.r#type.is_some();
                if let Some(regex_item) = v.iter().find(|e| e.regex.is_some()) {
                    // We skip these overly broad property types because they
                    // cause properties to be wrongly detected as strings.
                    if k == "^[a-z][a-z0-9\\-]*$"
                        || k == "^[a-zA-Z][a-zA-Z0-9\\-_]{0,63}$"
                        || k == "^.*$"
                        || k == ".*"
                    {
                        continue;
                    }

                    match pattern_property_types.get_mut(&k) {
                        Some((_re, list)) => list.extend(v.into_iter().filter(ignore_untyped)),
                        None => {
                            pattern_property_types.insert(
                                k,
                                (
                                    regex_item.regex.clone().unwrap(),
                                    v.into_iter().filter(ignore_untyped).collect(),
                                ),
                            );
                        }
                    }
                } else {
                    match property_types.get_mut(&k) {
                        Some(list) => list.extend(v.into_iter().filter(ignore_untyped)),
                        None => {
                            property_types
                                .insert(k, v.into_iter().filter(ignore_untyped).collect());
                        }
                    }
                };
            }
        }

        let mut scope = Scope::new().set_version(SchemaVersion::Draft2019_09);
        let resolver = LocalOnlyResolver::new(&mut schemas.iter());
        for schema in schemas.iter_mut() {
            schema
                .fixup_select_and_finalise(resolver.clone(), &mut scope)
                .map_err(|e| {
                    FileValidatorError::ValidatorError(e, schema.spec_id().clone().unwrap())
                })?;
        }

        Ok(Validator {
            schemas,
            property_types,
            pattern_property_types,
            valid_compatibles: HashSet::new(),
            scope,
        })
    }

    pub fn dump_properties(&self, out: impl Write) -> Result<(), ValidatorError> {
        let props_for_dump: BTreeMap<_, _> =
            self.property_types
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        BTreeSet::from_iter(v.iter().filter(|e| {
                            e.r#type.unwrap_or(PropertyType::Node) != PropertyType::Node
                        })),
                    )
                })
                .filter(|(_, v)| !v.is_empty())
                .collect();

        serde_yaml::to_writer(out, &props_for_dump)?;
        Ok(())
    }

    pub fn validate(&self, value: &serde_json::Value, path: JsonPath) -> bool {
        let schemas: Vec<_> = self
            .schemas
            .iter()
            .filter(|s| s.applies(value, &self.scope))
            .map(|s| (s, s.validate(value, &path.to_string(), &self.scope)))
            .collect();

        let disabled = value
            .as_object()
            .and_then(|o| o.get("status"))
            .and_then(|v| v.as_array())
            .map(|v| v.len() == 1 && v[0] == "disabled")
            .unwrap_or(false);
        let span = tracing::error_span!("validate", path = %path);
        let _entered = span.enter();
        let mut ok = true;
        for (schema, status) in schemas {
            let schema_span = tracing::error_span!("schema", file=%schema.source_file());
            let _entered = schema_span.enter();
            if let Some(e) = status.filter(|v| !v.is_strictly_valid()) {
                let level = if !disabled {
                    ok = false;
                    tracing::error!("Validation failed:");
                    Level::ERROR
                } else {
                    tracing::info!("Validation of disabled node failed (this may be expected):");
                    Level::INFO
                };
                for error in e.errors {
                    if level == Level::INFO {
                        tracing::event!(
                            Level::INFO,
                            "at {}: {} {}",
                            error.get_path(),
                            error.get_title(),
                            error.get_detail().unwrap_or(""),
                        );
                    } else {
                        tracing::event!(
                            Level::ERROR,
                            "at {}: {} {}",
                            error.get_path(),
                            error.get_title(),
                            error.get_detail().unwrap_or(""),
                        );
                    }
                }
            }
        }

        ok
    }
}

impl PropertyTypeLookup for Validator {
    fn get_property_type(&self, propname: &str) -> BTreeSet<PropertyType> {
        let mut types: BTreeSet<PropertyType> = self
            .property_types
            .get(propname)
            .map(|v| v.iter().filter_map(|e| e.r#type).collect())
            .unwrap_or_default();

        if types.is_empty() {
            types.extend(
                self.pattern_property_types
                    .iter()
                    .filter(|(_, (regex, _))| regex.is_match(propname).unwrap_or(false))
                    .flat_map(|(_, (_, ty))| ty.iter().filter_map(|e| e.r#type)),
            );
        }

        types
    }

    fn get_property_dimensions(&self, propname: &str) -> Option<Dimension> {
        if let Some(types) = self.property_types.get(propname) {
            if let Some(dim) = types.iter().filter_map(|v| v.dim).next() {
                return Some(dim);
            }
        }

        self.pattern_property_types
            .iter()
            .filter_map(|(_, (regex, ty))| {
                if regex.is_match(propname).unwrap_or(false) {
                    ty.iter().filter_map(|e| e.dim).next()
                } else {
                    None
                }
            })
            .next()
    }
}
