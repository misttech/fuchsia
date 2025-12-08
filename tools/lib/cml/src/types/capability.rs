// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AnyRef, AsClause, AsClauseContext, Canonicalize, CapabilityClause, ConfigNestedValueType,
    ConfigType, FilterClause, PathClause,
};

use crate::one_or_many::{OneOrMany, always_one, option_one_or_many_as_ref};
use crate::types::common::*;
use crate::types::right::{Rights, RightsClause};
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use cml_macro::Reference;
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::num::NonZeroU32;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize, Debug, PartialEq, Clone, ReferenceDoc, Serialize, Default)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list")]
pub struct Capability {
    /// The [name](#name) for this service capability. Specifying `path` is valid
    /// only when this value is a string.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub service: Option<OneOrMany<Name>>,

    /// The [name](#name) for this protocol capability. Specifying `path` is valid
    /// only when this value is a string.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub protocol: Option<OneOrMany<Name>>,

    /// The [name](#name) for this directory capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub directory: Option<Name>,

    /// The [name](#name) for this storage capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub storage: Option<Name>,

    /// The [name](#name) for this runner capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub runner: Option<Name>,

    /// The [name](#name) for this resolver capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub resolver: Option<Name>,

    /// The [name](#name) for this event_stream capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub event_stream: Option<OneOrMany<Name>>,

    /// The [name](#name) for this dictionary capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub dictionary: Option<Name>,

    /// The [name](#name) for this configuration capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub config: Option<Name>,

    /// The path within the [outgoing directory][glossary.outgoing directory] of the component's
    /// program to source the capability.
    ///
    /// For `protocol` and `service`, defaults to `/svc/${protocol}`, otherwise required.
    ///
    /// For `protocol`, the target of the path MUST be a channel, which tends to speak
    /// the protocol matching the name of this capability.
    ///
    /// For `service`, `directory`, the target of the path MUST be a directory.
    ///
    /// For `runner`, the target of the path MUST be a channel and MUST speak
    /// the protocol `fuchsia.component.runner.ComponentRunner`.
    ///
    /// For `resolver`, the target of the path MUST be a channel and MUST speak
    /// the protocol `fuchsia.component.resolution.Resolver`.
    ///
    /// For `dictionary`, this is optional. If provided, it is a path to a
    /// `fuchsia.component.sandbox/DictionaryRouter` served by the program which should return a
    /// `fuchsia.component.sandbox/DictionaryRef`, by which the program may dynamically provide
    /// a dictionary from itself. If this is set for `dictionary`, `offer` to this dictionary
    /// is not allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Path>,

    /// (`directory` only) The maximum [directory rights][doc-directory-rights] that may be set
    /// when using this directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(json_type = "array of string")]
    pub rights: Option<Rights>,

    /// (`storage` only) The source component of an existing directory capability backing this
    /// storage capability, one of:
    /// - `parent`: The component's parent.
    /// - `self`: This component.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<CapabilityFromRef>,

    /// (`storage` only) The [name](#name) of the directory capability backing the storage. The
    /// capability must be available from the component referenced in `from`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backing_dir: Option<Name>,

    /// (`storage` only) A subdirectory within `backing_dir` where per-component isolated storage
    /// directories are created
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<RelativePath>,

    /// (`storage` only) The identifier used to isolated storage for a component, one of:
    /// - `static_instance_id`: The instance ID in the component ID index is used
    ///     as the key for a component's storage. Components which are not listed in
    ///     the component ID index will not be able to use this storage capability.
    /// - `static_instance_id_or_moniker`: If the component is listed in the
    ///     component ID index, the instance ID is used as the key for a component's
    ///     storage. Otherwise, the component's moniker from the storage
    ///     capability is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_id: Option<StorageId>,

    /// (`configuration` only) The type of configuration, one of:
    /// - `bool`: Boolean type.
    /// - `uint8`: Unsigned 8 bit type.
    /// - `uint16`: Unsigned 16 bit type.
    /// - `uint32`: Unsigned 32 bit type.
    /// - `uint64`: Unsigned 64 bit type.
    /// - `int8`: Signed 8 bit type.
    /// - `int16`: Signed 16 bit type.
    /// - `int32`: Signed 32 bit type.
    /// - `int64`: Signed 64 bit type.
    /// - `string`: ASCII string type.
    /// - `vector`: Vector type. See `element` for the type of the element within the vector.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    #[reference_doc(rename = "type")]
    pub config_type: Option<ConfigType>,

    /// (`configuration` only) Only supported if this configuration `type` is 'string'.
    /// This is the max size of the string.
    #[serde(rename = "max_size", skip_serializing_if = "Option::is_none")]
    #[reference_doc(rename = "max_size")]
    pub config_max_size: Option<NonZeroU32>,

    /// (`configuration` only) Only supported if this configuration `type` is 'vector'.
    /// This is the max number of elements in the vector.
    #[serde(rename = "max_count", skip_serializing_if = "Option::is_none")]
    #[reference_doc(rename = "max_count")]
    pub config_max_count: Option<NonZeroU32>,

    /// (`configuration` only) Only supported if this configuration `type` is 'vector'.
    /// This is the type of the elements in the configuration vector.
    ///
    /// Example (simple type):
    ///
    /// ```json5
    /// { type: "uint8" }
    /// ```
    ///
    /// Example (string type):
    ///
    /// ```json5
    /// {
    ///   type: "string",
    ///   max_size: 100,
    /// }
    /// ```
    #[serde(rename = "element", skip_serializing_if = "Option::is_none")]
    #[reference_doc(rename = "element", json_type = "object")]
    pub config_element_type: Option<ConfigNestedValueType>,

    /// (`configuration` only) The value of the configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,

    /// (`protocol` only) Specifies when the framework will open the protocol
    /// from this component's outgoing directory when someone requests the
    /// capability. Allowed values are:
    ///
    /// - `eager`: (default) the framework will open the capability as soon as
    ///   some consumer component requests it.
    /// - `on_readable`: the framework will open the capability when the server
    ///   endpoint pipelined in a connection request becomes readable.
    ///
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryType>,
}

impl Canonicalize for Capability {
    fn canonicalize(&mut self) {
        // Sort the names of the capabilities. Only capabilities with OneOrMany values are included here.
        if let Some(service) = &mut self.service {
            service.canonicalize()
        } else if let Some(protocol) = &mut self.protocol {
            protocol.canonicalize()
        } else if let Some(event_stream) = &mut self.event_stream {
            event_stream.canonicalize()
        }
    }
}

impl AsClause for Capability {
    fn r#as(&self) -> Option<&BorrowedName> {
        None
    }
}

impl PathClause for Capability {
    fn path(&self) -> Option<&Path> {
        self.path.as_ref()
    }
}

impl FilterClause for Capability {
    fn filter(&self) -> Option<&Map<String, Value>> {
        None
    }
}

impl RightsClause for Capability {
    fn rights(&self) -> Option<&Rights> {
        self.rights.as_ref()
    }
}

impl CapabilityClause for Capability {
    fn service(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.service)
    }
    fn protocol(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.protocol)
    }
    fn directory(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.directory.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }
    fn storage(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.storage.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }
    fn runner(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.runner.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }
    fn resolver(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.resolver.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }
    fn event_stream(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.event_stream)
    }
    fn dictionary(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.dictionary.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }
    fn config(&self) -> Option<OneOrMany<&BorrowedName>> {
        self.config.as_ref().map(|n| OneOrMany::One(n.as_ref()))
    }

    fn set_service(&mut self, o: Option<OneOrMany<Name>>) {
        self.service = o;
    }
    fn set_protocol(&mut self, o: Option<OneOrMany<Name>>) {
        self.protocol = o;
    }
    fn set_directory(&mut self, o: Option<OneOrMany<Name>>) {
        self.directory = always_one(o);
    }
    fn set_storage(&mut self, o: Option<OneOrMany<Name>>) {
        self.storage = always_one(o);
    }
    fn set_runner(&mut self, o: Option<OneOrMany<Name>>) {
        self.runner = always_one(o);
    }
    fn set_resolver(&mut self, o: Option<OneOrMany<Name>>) {
        self.resolver = always_one(o);
    }
    fn set_event_stream(&mut self, o: Option<OneOrMany<Name>>) {
        self.event_stream = o;
    }
    fn set_dictionary(&mut self, o: Option<OneOrMany<Name>>) {
        self.dictionary = always_one(o);
    }

    fn set_config(&mut self, o: Option<OneOrMany<Name>>) {
        self.config = always_one(o);
    }

    fn availability(&self) -> Option<Availability> {
        None
    }
    fn set_availability(&mut self, _a: Option<Availability>) {}

    fn decl_type(&self) -> &'static str {
        "capability"
    }
    fn supported(&self) -> &[&'static str] {
        &[
            "service",
            "protocol",
            "directory",
            "storage",
            "runner",
            "resolver",
            "event_stream",
            "dictionary",
            "config",
        ]
    }
    fn are_many_names_allowed(&self) -> bool {
        ["service", "protocol", "event_stream"].contains(&self.capability_type().unwrap())
    }
}

/// A reference in a `storage from`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"parent\", \"self\", or \"#<child-name>\"")]
pub enum CapabilityFromRef {
    /// A reference to a child.
    Named(Name),
    /// A reference to the parent.
    Parent,
    /// A reference to this component.
    Self_,
}

#[derive(Deserialize, Default, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ParsedCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Spanned<OneOrMany<Name>>>,
    pub protocol: Option<Spanned<OneOrMany<Name>>>,
    pub directory: Option<Spanned<Name>>,
    pub storage: Option<Spanned<Name>>,
    pub runner: Option<Spanned<Name>>,
    pub resolver: Option<Spanned<Name>>,
    pub event_stream: Option<Spanned<OneOrMany<Name>>>,
    pub dictionary: Option<Spanned<Name>>,
    pub config: Option<Spanned<Name>>,
    pub path: Option<Spanned<Path>>,
    pub rights: Option<Spanned<Rights>>,
    pub from: Option<Spanned<CapabilityFromRef>>,
    pub backing_dir: Option<Spanned<Name>>,
    pub subdir: Option<Spanned<RelativePath>>,
    pub storage_id: Option<Spanned<StorageId>>,

    #[serde(rename = "type", default)]
    pub config_type: Option<Spanned<ConfigType>>,
    #[serde(rename = "max_size", default)]
    pub config_max_size: Option<Spanned<NonZeroU32>>,
    #[serde(rename = "max_count", default)]
    pub config_max_count: Option<Spanned<NonZeroU32>>,
    #[serde(rename = "element", default)]
    pub config_element_type: Option<Spanned<ConfigNestedValueType>>,
    pub value: Option<Spanned<serde_json::Value>>,
    pub delivery: Option<Spanned<DeliveryType>>,
}

#[derive(Debug, Clone)]
pub struct ContextCapability {
    pub service: Option<ContextSpanned<OneOrMany<Name>>>,
    pub protocol: Option<ContextSpanned<OneOrMany<Name>>>,
    pub directory: Option<ContextSpanned<Name>>,
    pub storage: Option<ContextSpanned<Name>>,
    pub runner: Option<ContextSpanned<Name>>,
    pub resolver: Option<ContextSpanned<Name>>,
    pub event_stream: Option<ContextSpanned<OneOrMany<Name>>>,
    pub dictionary: Option<ContextSpanned<Name>>,
    pub config: Option<ContextSpanned<Name>>,
    pub path: Option<ContextSpanned<Path>>,
    pub rights: Option<ContextSpanned<Rights>>,
    pub from: Option<ContextSpanned<CapabilityFromRef>>,
    pub backing_dir: Option<ContextSpanned<Name>>,
    pub subdir: Option<ContextSpanned<RelativePath>>,
    pub storage_id: Option<ContextSpanned<StorageId>>,
    pub config_type: Option<ContextSpanned<ConfigType>>,
    pub config_max_size: Option<ContextSpanned<NonZeroU32>>,
    pub config_max_count: Option<ContextSpanned<NonZeroU32>>,
    pub config_element_type: Option<ContextSpanned<ConfigNestedValueType>>,
    pub value: Option<ContextSpanned<serde_json::Value>>,
    pub delivery: Option<ContextSpanned<DeliveryType>>,
}

impl ContextCapabilityClause for ContextCapability {
    fn service(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.service)
    }
    fn protocol(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.protocol)
    }
    fn directory(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.directory.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }
    fn storage(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.storage.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }
    fn runner(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.runner.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }
    fn resolver(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.resolver.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }
    fn event_stream(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.event_stream)
    }
    fn dictionary(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.dictionary.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }
    fn config(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        self.config.as_ref().map(|s| ContextSpanned {
            value: OneOrMany::One((s.value).as_ref()),
            origin: s.origin.clone(),
        })
    }

    fn decl_type(&self) -> &'static str {
        "capability"
    }
    fn supported(&self) -> &[&'static str] {
        &[
            "service",
            "protocol",
            "directory",
            "storage",
            "event_stream",
            "runner",
            "resolver",
            "config",
            "dicitionary",
        ]
    }
    fn are_many_names_allowed(&self) -> bool {
        [
            "service",
            "protocol",
            "directory",
            "runner",
            "resolver",
            "event_stream",
            "config",
            "dicitionary",
        ]
        .contains(&self.capability_type(None).unwrap())
    }
}

impl PartialEq for ContextCapability {
    fn eq(&self, other: &Self) -> bool {
        macro_rules! cmp {
            ($field:ident) => {
                match (&self.$field, &other.$field) {
                    (Some(a), Some(b)) => a.value == b.value,
                    (None, None) => true,
                    _ => false,
                }
            };
        }

        cmp!(service)
            && cmp!(protocol)
            && cmp!(directory)
            && cmp!(storage)
            && cmp!(runner)
            && cmp!(resolver)
            && cmp!(dictionary)
            && cmp!(config)
            && cmp!(path)
            && cmp!(rights)
            && cmp!(from)
            && cmp!(event_stream)
            && cmp!(backing_dir)
            && cmp!(subdir)
            && cmp!(storage_id)
            && cmp!(config_type)
            && cmp!(config_max_size)
            && cmp!(config_max_count)
            && cmp!(config_element_type)
            && cmp!(value)
            && cmp!(delivery)
    }
}

impl Eq for ContextCapability {}

impl ContextPathClause for ContextCapability {
    fn path(&self) -> Option<&ContextSpanned<Path>> {
        self.path.as_ref()
    }
}

impl AsClauseContext for ContextCapability {
    fn r#as(&self) -> Option<ContextSpanned<&BorrowedName>> {
        None
    }
}

impl Hydrate for ParsedCapability {
    type Output = ContextCapability;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Self::Output {
        ContextCapability {
            service: hydrate_opt_simple(self.service, file, buffer),
            protocol: hydrate_opt_simple(self.protocol, file, buffer),
            directory: hydrate_opt_simple(self.directory, file, buffer),
            storage: hydrate_opt_simple(self.storage, file, buffer),
            runner: hydrate_opt_simple(self.runner, file, buffer),
            resolver: hydrate_opt_simple(self.resolver, file, buffer),
            dictionary: hydrate_opt_simple(self.dictionary, file, buffer),
            config: hydrate_opt_simple(self.config, file, buffer),
            path: hydrate_opt_simple(self.path, file, buffer),
            rights: hydrate_opt_simple(self.rights, file, buffer),
            from: hydrate_opt_simple(self.from, file, buffer),
            event_stream: hydrate_opt_simple(self.event_stream, file, buffer),
            backing_dir: hydrate_opt_simple(self.backing_dir, file, buffer),
            subdir: hydrate_opt_simple(self.subdir, file, buffer),
            storage_id: hydrate_opt_simple(self.storage_id, file, buffer),
            config_type: hydrate_opt_simple(self.config_type, file, buffer),
            config_max_size: hydrate_opt_simple(self.config_max_size, file, buffer),
            config_max_count: hydrate_opt_simple(self.config_max_count, file, buffer),
            config_element_type: hydrate_opt_simple(self.config_element_type, file, buffer),
            value: hydrate_opt_simple(self.value, file, buffer),
            delivery: hydrate_opt_simple(self.delivery, file, buffer),
        }
    }
}
