// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use indexmap::IndexMap;
use itertools::Itertools;

use crate::types::capability::{ContextCapability, ParsedCapability};
use crate::types::child::{ContextChild, ParsedChild};
use crate::types::collection::{ContextCollection, ParsedCollection};
use crate::types::common::*;
use crate::types::expose::{ContextExpose, ParsedExpose};
use crate::types::offer::{ContextOffer, ParsedOffer};
use crate::types::r#use::{ContextUse, ParsedUse};
use crate::{
    Canonicalize, Capability, CapabilityClause, CapabilityFromRef, Child, Collection, ConfigKey,
    ConfigValueType, Environment, Error, Expose, Offer, Program, Use,
};

pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::{cmp, path};

/// # Component manifest (`.cml`) reference
///
/// A `.cml` file contains a single json5 object literal with the keys below.
///
/// Where string values are expected, a list of valid values is generally documented.
/// The following string value types are reused and must follow specific rules.
///
/// The `.cml` file is compiled into a FIDL wire format (`.cm`) file.
///
/// ## String types
///
/// ### Names {#names}
///
/// Both capabilities and a component's children are named. A name string may
/// consist of one or more of the following characters: `A-Z`, `a-z`, `0-9`,
/// `_`, `.`, `-`. It must not exceed 255 characters in length and may not start
/// with `.` or `-`.
///
/// ### Paths {#paths}
///
/// Paths are sequences of [names](#names) delimited by the `/` character. A path
/// must not exceed 4095 characters in length. Throughout the document,
///
/// - Relative paths cannot start with the `/` character.
/// - Namespace and outgoing directory paths must start with the `/` character.
///
/// ### References {#references}
///
/// A reference string takes the form of `#<name>`, where `<name>` refers to the name of a child:
///
/// - A [static child instance][doc-static-children] whose name is
///     `<name>`, or
/// - A [collection][doc-collections] whose name is `<name>`.
///
/// [doc-static-children]: /docs/concepts/components/v2/realms.md#static-children
/// [doc-collections]: /docs/concepts/components/v2/realms.md#collections
/// [doc-protocol]: /docs/concepts/components/v2/capabilities/protocol.md
/// [doc-dictionaries]: /reference/fidl/fuchsia.component.decl#Dictionary
/// [doc-directory]: /docs/concepts/components/v2/capabilities/directory.md
/// [doc-storage]: /docs/concepts/components/v2/capabilities/storage.md
/// [doc-resolvers]: /docs/concepts/components/v2/capabilities/resolver.md
/// [doc-runners]: /docs/concepts/components/v2/capabilities/runner.md
/// [doc-event]: /docs/concepts/components/v2/capabilities/event.md
/// [doc-service]: /docs/concepts/components/v2/capabilities/service.md
/// [doc-directory-rights]: /docs/concepts/components/v2/capabilities/directory.md#directory-capability-rights
///
/// ## Top-level keys {#document}
#[derive(ReferenceDoc, Deserialize, Debug, Default, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// The optional `include` property describes zero or more other component manifest
    /// files to be merged into this component manifest. For example:
    ///
    /// ```json5
    /// include: [ "syslog/client.shard.cml" ]
    /// ```
    ///
    /// In the example given above, the component manifest is including contents from a
    /// manifest shard provided by the `syslog` library, thus ensuring that the
    /// component functions correctly at runtime if it attempts to write to `syslog`. By
    /// convention such files are called "manifest shards" and end with `.shard.cml`.
    ///
    /// Include paths prepended with `//` are relative to the source root of the Fuchsia
    /// checkout. However, include paths not prepended with `//`, as in the example
    /// above, are resolved from Fuchsia SDK libraries (`//sdk/lib`) that export
    /// component manifest shards.
    ///
    /// For reference, inside the Fuchsia checkout these two include paths are
    /// equivalent:
    ///
    /// * `syslog/client.shard.cml`
    /// * `//sdk/lib/syslog/client.shard.cml`
    ///
    /// You can review the outcome of merging any and all includes into a component
    /// manifest file by invoking the following command:
    ///
    /// Note: The `fx` command below is for developers working in a Fuchsia source
    /// checkout environment.
    ///
    /// ```sh
    /// fx cmc include {{ "<var>" }}cml_file{{ "</var>" }} --includeroot $FUCHSIA_DIR --includepath $FUCHSIA_DIR/sdk/lib
    /// ```
    ///
    /// Includes can cope with duplicate [`use`], [`offer`], [`expose`], or [`capabilities`]
    /// declarations referencing the same capability, as long as the properties are the same. For
    /// example:
    ///
    /// ```json5
    /// // my_component.cml
    /// include: [ "syslog.client.shard.cml" ]
    /// use: [
    ///     {
    ///         protocol: [
    ///             "fuchsia.posix.socket.Provider",
    ///             "fuchsia.logger.LogSink",
    ///         ],
    ///     },
    /// ],
    ///
    /// // syslog.client.shard.cml
    /// use: [
    ///     { protocol: "fuchsia.logger.LogSink" },
    /// ],
    /// ```
    ///
    /// In this example, the contents of the merged file will be the same as my_component.cml --
    /// `fuchsia.logger.LogSink` is deduped.
    ///
    /// However, this would fail to compile:
    ///
    /// ```json5
    /// // my_component.cml
    /// include: [ "syslog.client.shard.cml" ]
    /// use: [
    ///     {
    ///         protocol: "fuchsia.logger.LogSink",
    ///         // properties for fuchsia.logger.LogSink don't match
    ///         from: "#archivist",
    ///     },
    /// ],
    ///
    /// // syslog.client.shard.cml
    /// use: [
    ///     { protocol: "fuchsia.logger.LogSink" },
    /// ],
    /// ```
    ///
    /// An exception to this constraint is the `availability` property. If two routing declarations
    /// are identical, and one availability is stronger than the other, the availability will be
    /// "promoted" to the stronger value (if `availability` is missing, it defaults to `required`).
    /// For example:
    ///
    /// ```json5
    /// // my_component.cml
    /// include: [ "syslog.client.shard.cml" ]
    /// use: [
    ///     {
    ///         protocol: [
    ///             "fuchsia.posix.socket.Provider",
    ///             "fuchsia.logger.LogSink",
    ///         ],
    ///         availability: "optional",
    ///     },
    /// ],
    ///
    /// // syslog.client.shard.cml
    /// use: [
    ///     {
    ///         protocol: "fuchsia.logger.LogSink
    ///         availability: "required",  // This is the default
    ///     },
    /// ],
    /// ```
    ///
    /// Becomes:
    ///
    /// ```json5
    /// use: [
    ///     {
    ///         protocol: "fuchsia.posix.socket.Provider",
    ///         availability: "optional",
    ///     },
    ///     {
    ///         protocol: "fuchsia.logger.LogSink",
    ///         availability: "required",
    ///     },
    /// ],
    /// ```
    ///
    /// Includes are transitive, meaning that shards can have their own includes.
    ///
    /// Include paths can have diamond dependencies. For instance this is valid:
    /// A includes B, A includes C, B includes D, C includes D.
    /// In this case A will transitively include B, C, D.
    ///
    /// Include paths cannot have cycles. For instance this is invalid:
    /// A includes B, B includes A.
    /// A cycle such as the above will result in a compile-time error.
    ///
    /// [`use`]: #use
    /// [`offer`]: #offer
    /// [`expose`]: #expose
    /// [`capabilities`]: #capabilities
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,

    /// Components that are executable include a `program` section. The `program`
    /// section must set the `runner` property to select a [runner][doc-runners] to run
    /// the component. The format of the rest of the `program` section is determined by
    /// that particular runner.
    ///
    /// # ELF runners {#elf-runners}
    ///
    /// If the component uses the ELF runner, `program` must include the following
    /// properties, at a minimum:
    ///
    /// - `runner`: must be set to `"elf"`
    /// - `binary`: Package-relative path to the executable binary
    /// - `args` _(optional)_: List of arguments
    ///
    /// Example:
    ///
    /// ```json5
    /// program: {
    ///     runner: "elf",
    ///     binary: "bin/hippo",
    ///     args: [ "Hello", "hippos!" ],
    /// },
    /// ```
    ///
    /// For a complete list of properties, see: [ELF Runner](/docs/concepts/components/v2/elf_runner.md)
    ///
    /// # Other runners {#other-runners}
    ///
    /// If a component uses a custom runner, values inside the `program` stanza other
    /// than `runner` are specific to the runner. The runner receives the arguments as a
    /// dictionary of key and value pairs. Refer to the specific runner being used to
    /// determine what keys it expects to receive, and how it interprets them.
    ///
    /// [doc-runners]: /docs/concepts/components/v2/capabilities/runner.md
    #[reference_doc(json_type = "object")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<Program>,

    /// The `children` section declares child component instances as described in
    /// [Child component instances][doc-children].
    ///
    /// [doc-children]: /docs/concepts/components/v2/realms.md#child-component-instances
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Child>>,

    /// The `collections` section declares collections as described in
    /// [Component collections][doc-collections].
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collections: Option<Vec<Collection>>,

    /// The `environments` section declares environments as described in
    /// [Environments][doc-environments].
    ///
    /// [doc-environments]: /docs/concepts/components/v2/environments.md
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environments: Option<Vec<Environment>>,

    /// The `capabilities` section defines capabilities that are provided by this component.
    /// Capabilities that are [offered](#offer) or [exposed](#expose) from `self` must be declared
    /// here.
    ///
    /// # Capability fields
    ///
    /// This supports the following capability keys. Exactly one of these must be set:
    ///
    /// - `protocol`: (_optional `string or array of strings`_)
    /// - `service`: (_optional `string or array of strings`_)
    /// - `directory`: (_optional `string`_)
    /// - `storage`: (_optional `string`_)
    /// - `runner`: (_optional `string`_)
    /// - `resolver`: (_optional `string`_)
    /// - `event_stream`: (_optional `string or array of strings`_)
    /// - `dictionary`: (_optional `string`_)
    /// - `config`: (_optional `string`_)
    ///
    /// # Additional fields
    ///
    /// This supports the following additional fields:
    /// [glossary.outgoing directory]: /docs/glossary/README.md#outgoing-directory
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Capability>>,

    /// For executable components, declares capabilities that this
    /// component requires in its [namespace][glossary.namespace] at runtime.
    /// Capabilities are routed from the `parent` unless otherwise specified,
    /// and each capability must have a valid route through all components between
    /// this component and the capability's source.
    ///
    /// # Capability fields
    ///
    /// This supports the following capability keys. Exactly one of these must be set:
    ///
    /// - `service`: (_optional `string or array of strings`_)
    /// - `directory`: (_optional `string`_)
    /// - `protocol`: (_optional `string or array of strings`_)
    /// - `dictionary`: (_optional `string`_)
    /// - `storage`: (_optional `string`_)
    /// - `event_stream`: (_optional `string or array of strings`_)
    /// - `runner`: (_optional `string`_)
    /// - `config`: (_optional `string`_)
    ///
    /// # Additional fields
    ///
    /// This supports the following additional fields:
    /// [glossary.namespace]: /docs/glossary/README.md#namespace
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#use: Option<Vec<Use>>,

    /// Declares the capabilities that are made available to the parent component or to the
    /// framework. It is valid to `expose` from `self` or from a child component.
    ///
    /// # Capability fields
    ///
    /// This supports the following capability keys. Exactly one of these must be set:
    ///
    /// - `service`: (_optional `string or array of strings`_)
    /// - `protocol`: (_optional `string or array of strings`_)
    /// - `directory`: (_optional `string`_)
    /// - `runner`: (_optional `string`_)
    /// - `resolver`: (_optional `string`_)
    /// - `dictionary`: (_optional `string`_)
    /// - `config`: (_optional `string`_)
    ///
    /// # Additional fields
    ///
    /// This supports the following additional fields:
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose: Option<Vec<Expose>>,

    /// Declares the capabilities that are made available to a [child component][doc-children]
    /// instance or a [child collection][doc-collections].
    ///
    /// # Capability fields
    ///
    /// This supports the following capability keys. Exactly one of these must be set:
    ///
    /// - `protocol`: (_optional `string or array of strings`_)
    /// - `service`: (_optional `string or array of strings`_)
    /// - `directory`: (_optional `string`_)
    /// - `storage`: (_optional `string`_)
    /// - `runner`: (_optional `string`_)
    /// - `resolver`: (_optional `string`_)
    /// - `event_stream`: (_optional `string or array of strings`_)
    /// - `dictionary`: (_optional `string`_)
    /// - `config`: (_optional `string`_)
    ///
    /// # Additional fields
    ///
    /// This supports the following additional fields:
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer: Option<Vec<Offer>>,

    /// Contains metadata that components may interpret for their own purposes. The component
    /// framework enforces no schema for this section, but third parties may expect their facets to
    /// adhere to a particular schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<IndexMap<String, Value>>,

    /// The configuration schema as defined by a component. Each key represents a single field
    /// in the schema.
    ///
    /// Configuration fields are JSON objects and must define a `type` which can be one of the
    /// following strings:
    /// `bool`, `uint8`, `int8`, `uint16`, `int16`, `uint32`, `int32`, `uint64`, `int64`,
    /// `string`, `vector`
    ///
    /// Example:
    ///
    /// ```json5
    /// config: {
    ///     debug_mode: {
    ///         type: "bool"
    ///     },
    /// }
    /// ```
    ///
    /// Fields are resolved from a component's package by default. To be able to change the values
    /// at runtime a `mutability` specifier is required.
    ///
    /// Example:
    ///
    /// ```json5
    /// config: {
    ///     verbose: {
    ///         type: "bool",
    ///         mutability: [ "parent" ],
    ///     },
    /// },
    /// ```
    ///
    /// Currently `"parent"` is the only mutability specifier supported.
    ///
    /// Strings must define the `max_size` property as a non-zero integer.
    ///
    /// Example:
    ///
    /// ```json5
    /// config: {
    ///     verbosity: {
    ///         type: "string",
    ///         max_size: 20,
    ///     }
    /// }
    /// ```
    ///
    /// Vectors must set the `max_count` property as a non-zero integer. Vectors must also set the
    /// `element` property as a JSON object which describes the element being contained in the
    /// vector. Vectors can contain booleans, integers, and strings but cannot contain other
    /// vectors.
    ///
    /// Example:
    ///
    /// ```json5
    /// config: {
    ///     tags: {
    ///         type: "vector",
    ///         max_count: 20,
    ///         element: {
    ///             type: "string",
    ///             max_size: 50,
    ///         }
    ///     }
    /// }
    /// ```
    #[reference_doc(json_type = "object")]
    #[serde(skip_serializing_if = "Option::is_none")]
    // NB: Unlike other maps the order of these fields matters for the ABI of generated config
    // libraries. Rather than insertion order, we explicitly sort the fields here to dissuade
    // developers from taking a dependency on the source ordering in their manifest. In the future
    // this will hopefully make it easier to pursue layout size optimizations.
    pub config: Option<BTreeMap<ConfigKey, ConfigValueType>>,
}

impl Document {
    pub fn merge_from(
        &mut self,
        other: &mut Document,
        include_path: &path::Path,
    ) -> Result<(), Error> {
        // Flatten the mergeable fields that may contain a
        // list of capabilities in one clause.
        merge_from_capability_field(&mut self.r#use, &mut other.r#use)?;
        merge_from_capability_field(&mut self.expose, &mut other.expose)?;
        merge_from_capability_field(&mut self.offer, &mut other.offer)?;
        merge_from_capability_field(&mut self.capabilities, &mut other.capabilities)?;
        merge_from_other_field(&mut self.include, &mut other.include);
        merge_from_other_field(&mut self.children, &mut other.children);
        merge_from_other_field(&mut self.collections, &mut other.collections);
        self.merge_environment(other, include_path)?;
        self.merge_program(other, include_path)?;
        self.merge_facets(other, include_path)?;
        self.merge_config(other, include_path)?;

        Ok(())
    }

    pub fn canonicalize(&mut self) {
        // Don't sort `include` - the order there matters.
        if let Some(children) = &mut self.children {
            children.sort_by(|a, b| a.name.cmp(&b.name));
        }
        if let Some(collections) = &mut self.collections {
            collections.sort_by(|a, b| a.name.cmp(&b.name));
        }
        if let Some(environments) = &mut self.environments {
            environments.sort_by(|a, b| a.name.cmp(&b.name));
        }
        if let Some(capabilities) = &mut self.capabilities {
            capabilities.canonicalize();
        }
        if let Some(offers) = &mut self.offer {
            offers.canonicalize();
        }
        if let Some(expose) = &mut self.expose {
            expose.canonicalize();
        }
        if let Some(r#use) = &mut self.r#use {
            r#use.canonicalize();
        }
    }

    fn merge_program(
        &mut self,
        other: &mut Document,
        include_path: &path::Path,
    ) -> Result<(), Error> {
        if let None = other.program {
            return Ok(());
        }
        if let None = self.program {
            self.program = Some(Program::default());
        }
        let my_program = self.program.as_mut().unwrap();
        let other_program = other.program.as_mut().unwrap();
        if let Some(other_runner) = other_program.runner.take() {
            my_program.runner = match &my_program.runner {
                Some(runner) if *runner != other_runner => {
                    return Err(Error::validate(format!(
                        "manifest include had a conflicting `program.runner`: {}",
                        include_path.display()
                    )));
                }
                _ => Some(other_runner),
            }
        }

        Self::merge_maps_with_options(
            &mut my_program.info,
            &other_program.info,
            "program",
            include_path,
            Some(vec!["environ", "features"]),
        )
    }

    fn merge_environment(
        &mut self,
        other: &mut Document,
        _include_path: &path::Path,
    ) -> Result<(), Error> {
        if let None = other.environments {
            return Ok(());
        }
        if let None = self.environments {
            self.environments = Some(vec![]);
        }

        let my_environments = self.environments.as_mut().unwrap();
        let other_environments = other.environments.as_mut().unwrap();
        my_environments.sort_by(|x, y| x.name.cmp(&y.name));
        other_environments.sort_by(|x, y| x.name.cmp(&y.name));

        let all_environments =
            my_environments.into_iter().merge_by(other_environments, |x, y| x.name <= y.name);
        let groups = all_environments.chunk_by(|e| e.name.clone());

        let mut merged_environments = vec![];
        for (name, group) in groups.into_iter() {
            let mut merged_environment = Environment {
                name: name.clone(),
                extends: None,
                runners: None,
                resolvers: None,
                debug: None,
                stop_timeout_ms: None,
            };
            for e in group {
                merged_environment.merge_from(e)?;
            }
            merged_environments.push(merged_environment);
        }

        self.environments = Some(merged_environments);
        Ok(())
    }

    fn merge_maps<'s, Source, Dest>(
        self_map: &mut Dest,
        include_map: Source,
        outer_key: &str,
        include_path: &path::Path,
    ) -> Result<(), Error>
    where
        Source: IntoIterator<Item = (&'s String, &'s Value)>,
        Dest: ValueMap,
    {
        Self::merge_maps_with_options(self_map, include_map, outer_key, include_path, None)
    }

    /// If `allow_array_concatenation_keys` is None, all arrays present in both
    /// `self_map` and `include_map` will be concatenated in the result. If it
    /// is set to Some(vec), only those keys specified will allow concatenation,
    /// with any others returning an error.
    fn merge_maps_with_options<'s, Source, Dest>(
        self_map: &mut Dest,
        include_map: Source,
        outer_key: &str,
        include_path: &path::Path,
        allow_array_concatenation_keys: Option<Vec<&str>>,
    ) -> Result<(), Error>
    where
        Source: IntoIterator<Item = (&'s String, &'s Value)>,
        Dest: ValueMap,
    {
        for (key, value) in include_map {
            match self_map.get_mut(key) {
                None => {
                    // Key not present in self map, insert it from include map.
                    self_map.insert(key.clone(), value.clone());
                }
                // Self and include maps share the same key
                Some(Value::Object(self_nested_map)) => match value {
                    // The include value is an object and can be recursively merged
                    Value::Object(include_nested_map) => {
                        let combined_key = format!("{}.{}", outer_key, key);

                        // Recursively merge maps
                        Self::merge_maps(
                            self_nested_map,
                            include_nested_map,
                            &combined_key,
                            include_path,
                        )?;
                    }
                    _ => {
                        // Cannot merge object and non-object
                        return Err(Error::validate(format!(
                            "manifest include had a conflicting `{}.{}`: {}",
                            outer_key,
                            key,
                            include_path.display()
                        )));
                    }
                },
                Some(Value::Array(self_nested_vec)) => match value {
                    // The include value is an array and can be merged, unless
                    // `allow_array_concatenation_keys` is used and the key is not included.
                    Value::Array(include_nested_vec) => {
                        if let Some(allowed_keys) = &allow_array_concatenation_keys {
                            if !allowed_keys.contains(&key.as_str()) {
                                // This key wasn't present in `allow_array_concatenation_keys` and so
                                // merging is disallowed.
                                return Err(Error::validate(format!(
                                    "manifest include had a conflicting `{}.{}`: {}",
                                    outer_key,
                                    key,
                                    include_path.display()
                                )));
                            }
                        }
                        let mut new_values = include_nested_vec.clone();
                        self_nested_vec.append(&mut new_values);
                    }
                    _ => {
                        // Cannot merge array and non-array
                        return Err(Error::validate(format!(
                            "manifest include had a conflicting `{}.{}`: {}",
                            outer_key,
                            key,
                            include_path.display()
                        )));
                    }
                },
                _ => {
                    // Cannot merge object and non-object
                    return Err(Error::validate(format!(
                        "manifest include had a conflicting `{}.{}`: {}",
                        outer_key,
                        key,
                        include_path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    fn merge_facets(
        &mut self,
        other: &mut Document,
        include_path: &path::Path,
    ) -> Result<(), Error> {
        if let None = other.facets {
            return Ok(());
        }
        if let None = self.facets {
            self.facets = Some(Default::default());
        }
        let my_facets = self.facets.as_mut().unwrap();
        let other_facets = other.facets.as_ref().unwrap();

        Self::merge_maps(my_facets, other_facets, "facets", include_path)
    }

    fn merge_config(
        &mut self,
        other: &mut Document,
        include_path: &path::Path,
    ) -> Result<(), Error> {
        if let Some(other_config) = other.config.as_mut() {
            if let Some(self_config) = self.config.as_mut() {
                for (key, field) in other_config {
                    match self_config.entry(key.clone()) {
                        std::collections::btree_map::Entry::Vacant(v) => {
                            v.insert(field.clone());
                        }
                        std::collections::btree_map::Entry::Occupied(o) => {
                            if o.get() != field {
                                let msg = format!(
                                    "Found conflicting entry for config key `{key}` in `{}`.",
                                    include_path.display()
                                );
                                return Err(Error::validate(&msg));
                            }
                        }
                    }
                }
            } else {
                self.config.replace(std::mem::take(other_config));
            }
        }
        Ok(())
    }

    pub fn includes(&self) -> Vec<String> {
        self.include.clone().unwrap_or_default()
    }

    pub fn all_children_names(&self) -> Vec<&BorrowedName> {
        if let Some(children) = self.children.as_ref() {
            children.iter().map(|c| c.name.as_ref()).collect()
        } else {
            vec![]
        }
    }

    pub fn all_collection_names(&self) -> Vec<&BorrowedName> {
        if let Some(collections) = self.collections.as_ref() {
            collections.iter().map(|c| c.name.as_ref()).collect()
        } else {
            vec![]
        }
    }

    pub fn all_storage_names(&self) -> Vec<&BorrowedName> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities.iter().filter_map(|c| c.storage.as_ref().map(|n| n.as_ref())).collect()
        } else {
            vec![]
        }
    }

    pub fn all_storage_with_sources<'a>(
        &'a self,
    ) -> HashMap<&'a BorrowedName, &'a CapabilityFromRef> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities
                .iter()
                .filter_map(|c| match (c.storage.as_ref().map(Name::as_ref), c.from.as_ref()) {
                    (Some(s), Some(f)) => Some((s, f)),
                    _ => None,
                })
                .collect()
        } else {
            HashMap::new()
        }
    }

    pub fn all_service_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| {
                c.iter()
                    .filter_map(|c| c.service.as_ref().map(|o| o.as_ref()))
                    .map(|p| p.into_iter())
                    .flatten()
                    .collect()
            })
            .unwrap_or_else(|| vec![])
    }

    pub fn all_protocol_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| {
                c.iter()
                    .filter_map(|c| c.protocol.as_ref().map(|o| o.as_ref()))
                    .map(|p| p.into_iter())
                    .flatten()
                    .collect()
            })
            .unwrap_or_else(|| vec![])
    }

    pub fn all_directory_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| c.iter().filter_map(|c| c.directory.as_ref().map(Name::as_ref)).collect())
            .unwrap_or_else(|| vec![])
    }

    pub fn all_runner_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| c.iter().filter_map(|c| c.runner.as_ref().map(Name::as_ref)).collect())
            .unwrap_or_else(|| vec![])
    }

    pub fn all_resolver_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| c.iter().filter_map(|c| c.resolver.as_ref().map(Name::as_ref)).collect())
            .unwrap_or_else(|| vec![])
    }

    pub fn all_dictionary_names(&self) -> Vec<&BorrowedName> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities.iter().filter_map(|c| c.dictionary.as_ref().map(Name::as_ref)).collect()
        } else {
            vec![]
        }
    }

    pub fn all_dictionaries<'a>(&'a self) -> HashMap<&'a BorrowedName, &'a Capability> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities
                .iter()
                .filter_map(|c| match c.dictionary.as_ref().map(Name::as_ref) {
                    Some(s) => Some((s, c)),
                    _ => None,
                })
                .collect()
        } else {
            HashMap::new()
        }
    }

    pub fn all_config_names(&self) -> Vec<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| c.iter().filter_map(|c| c.config.as_ref().map(Name::as_ref)).collect())
            .unwrap_or_else(|| vec![])
    }

    pub fn all_environment_names(&self) -> Vec<&BorrowedName> {
        self.environments
            .as_ref()
            .map(|c| c.iter().map(|s| s.name.as_ref()).collect())
            .unwrap_or_else(|| vec![])
    }

    pub fn all_capability_names(&self) -> HashSet<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| {
                c.iter().fold(HashSet::new(), |mut acc, capability| {
                    acc.extend(capability.names());
                    acc
                })
            })
            .unwrap_or_default()
    }
}

/// Merges `us` into `other` according to the rules documented for [`include`].
/// [`include`]: #include
fn merge_from_capability_field<T: CapabilityClause>(
    us: &mut Option<Vec<T>>,
    other: &mut Option<Vec<T>>,
) -> Result<(), Error> {
    // Empty entries are an error, and merging removes empty entries so we first need to check
    // for them.
    for entry in us.iter().flatten().chain(other.iter().flatten()) {
        if entry.names().is_empty() {
            return Err(Error::Validate {
                err: format!("{}: Missing type name: {:#?}", entry.decl_type(), entry),
                filename: None,
            });
        }
    }

    if let Some(all_ours) = us.as_mut() {
        if let Some(all_theirs) = other.take() {
            for mut theirs in all_theirs {
                for ours in &mut *all_ours {
                    compute_diff(ours, &mut theirs);
                }
                all_ours.push(theirs);
            }
        }
        // Post-filter step: remove empty entries.
        all_ours.retain(|ours| !ours.names().is_empty())
    } else if let Some(theirs) = other.take() {
        us.replace(theirs);
    }
    Ok(())
}

/// Merges `us` into `other` according to the rules documented for [`include`].
/// [`include`]: #include
fn merge_from_other_field<T: std::cmp::PartialEq>(
    us: &mut Option<Vec<T>>,
    other: &mut Option<Vec<T>>,
) {
    if let Some(ours) = us {
        if let Some(theirs) = other.take() {
            // Add their elements, ignoring dupes with ours
            for t in theirs {
                if !ours.contains(&t) {
                    ours.push(t);
                }
            }
        }
    } else if let Some(theirs) = other.take() {
        us.replace(theirs);
    }
}

/// Subtracts the capabilities in `ours` from `theirs` if the declarations match in their type and
/// other fields, resulting in the removal of duplicates between `ours` and `theirs`. Stores the
/// result in `theirs`.
///
/// Inexact matches on `availability` are allowed if there is a partial order between them. The
/// stronger availability is chosen.
fn compute_diff<T: CapabilityClause>(ours: &mut T, theirs: &mut T) {
    // Return early if one is empty.
    if ours.names().is_empty() || theirs.names().is_empty() {
        return;
    }

    // Return early if the types don't match.
    if ours.capability_type().unwrap() != theirs.capability_type().unwrap() {
        return;
    }

    // Check if the non-capability fields match before proceeding.
    let mut ours_partial = ours.clone();
    let mut theirs_partial = theirs.clone();
    for e in [&mut ours_partial, &mut theirs_partial] {
        e.set_names(Vec::new());
        // Availability is allowed to differ (see merge algorithm below)
        e.set_availability(None);
    }
    if ours_partial != theirs_partial {
        // The fields other than `availability` do not match, nothing to remove.
        return;
    }

    // Compare the availabilities.
    let Some(avail_cmp) = ours
        .availability()
        .unwrap_or_default()
        .partial_cmp(&theirs.availability().unwrap_or_default())
    else {
        // The availabilities are incompatible (no partial order).
        return;
    };

    let mut our_names: Vec<Name> = ours.names().into_iter().map(Into::into).collect();
    let mut their_names: Vec<Name> = theirs.names().into_iter().map(Into::into).collect();

    let mut our_entries_to_remove = HashSet::new();
    let mut their_entries_to_remove = HashSet::new();
    for e in &their_names {
        if !our_names.contains(e) {
            // Not a duplicate, so keep.
            continue;
        }
        match avail_cmp {
            cmp::Ordering::Less => {
                // Their availability is stronger, meaning theirs should take
                // priority. Keep `e` in theirs, and remove it from ours.
                our_entries_to_remove.insert(e.clone());
            }
            cmp::Ordering::Greater => {
                // Our availability is stronger, meaning ours should take
                // priority. Remove `e` from theirs.
                their_entries_to_remove.insert(e.clone());
            }
            cmp::Ordering::Equal => {
                // The availabilities are equal, so `e` is a duplicate.
                their_entries_to_remove.insert(e.clone());
            }
        }
    }
    our_names.retain(|e| !our_entries_to_remove.contains(e));
    their_names.retain(|e| !their_entries_to_remove.contains(e));

    ours.set_names(our_names);
    theirs.set_names(their_names);
}

/// Trait that allows us to merge `serde_json::Map`s into `indexmap::IndexMap`s and vice versa.
trait ValueMap {
    fn get_mut(&mut self, key: &str) -> Option<&mut Value>;
    fn insert(&mut self, key: String, val: Value);
}

impl ValueMap for Map<String, Value> {
    fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.get_mut(key)
    }

    fn insert(&mut self, key: String, val: Value) {
        self.insert(key, val);
    }
}

impl ValueMap for IndexMap<String, Value> {
    fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.get_mut(key)
    }

    fn insert(&mut self, key: String, val: Value) {
        self.insert(key, val);
    }
}

macro_rules! merge_spanned_vec {
    ($self:expr, $other:expr, $field:ident) => {
        if let Some(other_vec) = $other.$field.take() {
            if let Some(self_vec) = $self.$field.as_mut() {
                self_vec.extend(other_vec);
            } else {
                $self.$field = Some(other_vec);
            }
        }
    };
}

/// # Component manifest (`.cml`) reference
///
/// A `.cml` file contains a single spanned json5 object literal with the keys below.
#[derive(Deserialize, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParsedDocument {
    pub children: Option<Spanned<Vec<Spanned<ParsedChild>>>>,
    pub collections: Option<Spanned<Vec<Spanned<ParsedCollection>>>>,
    pub capabilities: Option<Spanned<Vec<Spanned<ParsedCapability>>>>,
    pub r#use: Option<Spanned<Vec<Spanned<ParsedUse>>>>,
    pub expose: Option<Spanned<Vec<Spanned<ParsedExpose>>>>,
    pub offer: Option<Spanned<Vec<Spanned<ParsedOffer>>>>,
}

#[derive(Debug, Default)]
pub struct DocumentContext {
    pub children: Option<Vec<ContextSpanned<ContextChild>>>,
    pub collections: Option<Vec<ContextSpanned<ContextCollection>>>,
    pub capabilities: Option<Vec<ContextSpanned<ContextCapability>>>,
    pub r#use: Option<Vec<ContextSpanned<ContextUse>>>,
    pub expose: Option<Vec<ContextSpanned<ContextExpose>>>,
    pub offer: Option<Vec<ContextSpanned<ContextOffer>>>,
}

impl DocumentContext {
    pub fn merge_from(&mut self, mut other: DocumentContext) {
        merge_spanned_vec!(self, other, children);
        merge_spanned_vec!(self, other, collections);
        merge_spanned_vec!(self, other, capabilities);
        merge_spanned_vec!(self, other, r#use);
        merge_spanned_vec!(self, other, expose);
        merge_spanned_vec!(self, other, offer);
    }

    pub fn all_storage_with_sources<'a>(
        &'a self,
    ) -> HashMap<&'a BorrowedName, &'a CapabilityFromRef> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities
                .iter()
                .filter_map(|cap_wrapper| {
                    let c = &cap_wrapper.value;

                    let storage_span_opt = c.storage.as_ref();
                    let source_span_opt = c.from.as_ref();

                    match (storage_span_opt, source_span_opt) {
                        (Some(s_span), Some(f_span)) => {
                            let name_ref: &BorrowedName = s_span.value.as_ref();
                            let source_ref: &CapabilityFromRef = &f_span.value;

                            Some((name_ref, source_ref))
                        }
                        _ => None,
                    }
                })
                .collect()
        } else {
            HashMap::new()
        }
    }

    pub fn all_capability_names(&self) -> HashSet<&BorrowedName> {
        self.capabilities
            .as_ref()
            .map(|c| {
                c.iter().fold(HashSet::new(), |mut acc, capability_wrapper| {
                    let capability = &capability_wrapper.value;
                    acc.extend(capability.names());
                    acc
                })
            })
            .unwrap_or_default()
    }

    pub fn all_collection_names(&self) -> HashSet<&BorrowedName> {
        if let Some(collections) = self.collections.as_ref() {
            collections.iter().map(|c| c.value.name.value.as_ref()).collect()
        } else {
            HashSet::new()
        }
    }

    pub fn all_children_names(&self) -> HashSet<&BorrowedName> {
        if let Some(children) = self.children.as_ref() {
            children.iter().map(|c| c.value.name.value.as_ref()).collect()
        } else {
            HashSet::new()
        }
    }

    pub fn all_dictionaries<'a>(&'a self) -> HashMap<&'a BorrowedName, &'a ContextCapability> {
        if let Some(capabilities) = self.capabilities.as_ref() {
            capabilities
                .iter()
                .filter_map(|cap_wrapper| {
                    let cap = &cap_wrapper.value;
                    let dict_span_opt = cap.dictionary.as_ref();

                    dict_span_opt.and_then(|dict_span| {
                        let name_value = &dict_span.value;
                        let borrowed_name: &BorrowedName = name_value.as_ref();
                        Some((borrowed_name, cap))
                    })
                })
                .collect()
        } else {
            HashMap::new()
        }
    }
}

pub fn convert_parsed_to_document(
    parsed_doc: ParsedDocument,
    file_arc: Arc<PathBuf>,
    buffer: &String,
) -> DocumentContext {
    DocumentContext {
        children: hydrate_list(parsed_doc.children, &file_arc, buffer),
        collections: hydrate_list(parsed_doc.collections, &file_arc, buffer),
        capabilities: hydrate_list(parsed_doc.capabilities, &file_arc, buffer),
        r#use: hydrate_list(parsed_doc.r#use, &file_arc, buffer),
        expose: hydrate_list(parsed_doc.expose, &file_arc, buffer),
        offer: hydrate_list(parsed_doc.offer, &file_arc, buffer),
    }
}
