// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::one_or_many::OneOrMany;
use crate::types::capability::ContextCapability;
use crate::types::common::{ContextCapabilityClause, option_one_or_many_as_ref_context};
use crate::types::offer::ContextOffer;
use crate::types::r#use::ContextUse;
use crate::{
    AsClause, AsClauseContext, Capability, CapabilityClause, ContextExpose, ContextSpanned, Error,
    Origin, PathClause, Use, alias_or_name, alias_or_name_context,
};
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    NamespacePath, OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};

use std::fmt;

/// A name/identity of a capability exposed/offered to another component.
///
/// Exposed or offered capabilities have an identifier whose format
/// depends on the capability type. For directories and services this is
/// a path, while for storage this is a storage name. Paths and storage
/// names, however, are in different conceptual namespaces, and can't
/// collide with each other.
///
/// This enum allows such names to be specified disambiguating what
/// namespace they are in.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum CapabilityId<'a> {
    Service(&'a BorrowedName),
    Protocol(&'a BorrowedName),
    Directory(&'a BorrowedName),
    // A service in a `use` declaration has a target path in the component's namespace.
    UsedService(Path),
    // A protocol in a `use` declaration has a target path in the component's namespace.
    UsedProtocol(Path),
    // A directory in a `use` declaration has a target path in the component's namespace.
    UsedDirectory(Path),
    // A storage in a `use` declaration has a target path in the component's namespace.
    UsedStorage(Path),
    // An event stream in a `use` declaration has a target path in the component's namespace.
    UsedEventStream(Path),
    // A configuration in a `use` declaration has a target name that matches a config.
    UsedConfiguration(&'a BorrowedName),
    UsedRunner(&'a BorrowedName),
    // A dictionary in a `use` declaration that has a target path in the component's namespace.
    UsedDictionary(Path),
    Storage(&'a BorrowedName),
    Runner(&'a BorrowedName),
    Resolver(&'a BorrowedName),
    EventStream(&'a BorrowedName),
    Dictionary(&'a BorrowedName),
    Configuration(&'a BorrowedName),
}

/// Generates a `Vec<&BorrowedName>` -> `Vec<CapabilityId>` conversion function.
macro_rules! capability_ids_from_names {
    ($name:ident, $variant:expr) => {
        fn $name(names: Vec<&'a BorrowedName>) -> Vec<Self> {
            names.into_iter().map(|n| $variant(n)).collect()
        }
    };
}

/// Generates a `Vec<ContextSpanned<&BorrowedName>>` -> `Vec<(CapabilityId, Origin)>` conversion function.
macro_rules! capability_ids_from_context_names {
    ($name:ident, $variant:expr) => {
        fn $name(names: Vec<ContextSpanned<&'a BorrowedName>>) -> Vec<(Self, Origin)> {
            names
                .into_iter()
                .map(|spanned_name| ($variant(spanned_name.value), spanned_name.origin))
                .collect()
        }
    };
}

/// Generates a `Vec<Path>` -> `Vec<CapabilityId>` conversion function.
macro_rules! capability_ids_from_paths {
    ($name:ident, $variant:expr) => {
        fn $name(paths: Vec<Path>) -> Vec<Self> {
            paths.into_iter().map(|p| $variant(p)).collect()
        }
    };
}

/// Generates a `Vec<ContextSpanned<Path>>` -> `Vec<(CapabilityId, Origin)>` conversion function.
macro_rules! capability_ids_from_context_paths {
    ($name:ident, $variant:expr) => {
        fn $name(paths: Vec<ContextSpanned<Path>>) -> Vec<(Self, Origin)> {
            paths
                .into_iter()
                .map(|spanned_path| ($variant(spanned_path.value), spanned_path.origin))
                .collect()
        }
    };
}

impl<'a> CapabilityId<'a> {
    /// Human readable description of this capability type.
    pub fn type_str(&self) -> &'static str {
        match self {
            CapabilityId::Service(_) => "service",
            CapabilityId::Protocol(_) => "protocol",
            CapabilityId::Directory(_) => "directory",
            CapabilityId::UsedService(_) => "service",
            CapabilityId::UsedProtocol(_) => "protocol",
            CapabilityId::UsedDirectory(_) => "directory",
            CapabilityId::UsedStorage(_) => "storage",
            CapabilityId::UsedEventStream(_) => "event_stream",
            CapabilityId::UsedRunner(_) => "runner",
            CapabilityId::UsedConfiguration(_) => "config",
            CapabilityId::UsedDictionary(_) => "dictionary",
            CapabilityId::Storage(_) => "storage",
            CapabilityId::Runner(_) => "runner",
            CapabilityId::Resolver(_) => "resolver",
            CapabilityId::EventStream(_) => "event_stream",
            CapabilityId::Dictionary(_) => "dictionary",
            CapabilityId::Configuration(_) => "config",
        }
    }

    /// Return the directory containing the capability, if this capability takes a target path.
    pub fn get_dir_path(&self) -> Option<NamespacePath> {
        match self {
            CapabilityId::UsedService(p)
            | CapabilityId::UsedProtocol(p)
            | CapabilityId::UsedEventStream(p) => Some(p.parent()),
            CapabilityId::UsedDirectory(p)
            | CapabilityId::UsedStorage(p)
            | CapabilityId::UsedDictionary(p) => Some(p.clone().into()),
            _ => None,
        }
    }

    /// Return the target path of the capability, if this capability has one.
    pub fn get_target_path(&self) -> Option<NamespacePath> {
        match self {
            CapabilityId::UsedService(p)
            | CapabilityId::UsedProtocol(p)
            | CapabilityId::UsedEventStream(p)
            | CapabilityId::UsedDirectory(p)
            | CapabilityId::UsedStorage(p)
            | CapabilityId::UsedDictionary(p) => Some(p.clone().into()),
            _ => None,
        }
    }

    /// Given a Use clause, return the set of target identifiers.
    ///
    /// When only one capability identifier is specified, the target identifier name is derived
    /// using the "path" clause. If a "path" clause is not specified, the target identifier is the
    /// same name as the source.
    ///
    /// When multiple capability identifiers are specified, the target names are the same as the
    /// source names.
    pub fn from_use(use_: &'a Use) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        let alias = use_.path.as_ref();
        if let Some(n) = use_.service() {
            return Ok(Self::used_services_from(Self::get_one_or_many_svc_paths(
                n,
                alias,
                use_.capability_type().unwrap(),
            )?));
        } else if let Some(n) = use_.protocol() {
            return Ok(Self::used_protocols_from(Self::get_one_or_many_svc_paths(
                n,
                alias,
                use_.capability_type().unwrap(),
            )?));
        } else if let Some(_) = use_.directory.as_ref() {
            if use_.path.is_none() {
                return Err(Error::validate("\"path\" should be present for `use directory`."));
            }
            return Ok(vec![CapabilityId::UsedDirectory(use_.path.as_ref().unwrap().clone())]);
        } else if let Some(_) = use_.storage.as_ref() {
            if use_.path.is_none() {
                return Err(Error::validate("\"path\" should be present for `use storage`."));
            }
            return Ok(vec![CapabilityId::UsedStorage(use_.path.as_ref().unwrap().clone())]);
        } else if let Some(_) = use_.event_stream() {
            if let Some(path) = use_.path() {
                return Ok(vec![CapabilityId::UsedEventStream(path.clone())]);
            }
            return Ok(vec![CapabilityId::UsedEventStream(Path::new(
                "/svc/fuchsia.component.EventStream",
            )?)]);
        } else if let Some(n) = use_.runner() {
            match n {
                OneOrMany::One(name) => {
                    return Ok(vec![CapabilityId::UsedRunner(name)]);
                }
                OneOrMany::Many(_) => {
                    return Err(Error::validate("`use runner` should occur at most once."));
                }
            }
        } else if let Some(_) = use_.config() {
            return match &use_.key {
                None => Err(Error::validate("\"key\" should be present for `use config`.")),
                Some(name) => Ok(vec![CapabilityId::UsedConfiguration(name)]),
            };
        } else if let Some(n) = use_.dictionary() {
            return Ok(Self::used_dictionaries_from(Self::get_one_or_many_svc_paths(
                n,
                alias,
                use_.capability_type().unwrap(),
            )?));
        }
        // Unsupported capability type.
        let supported_keywords = use_
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate(format!(
            "`{}` declaration is missing a capability keyword, one of: {}",
            use_.decl_type(),
            supported_keywords,
        )))
    }

    pub fn from_capability(capability: &'a Capability) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        if let Some(n) = capability.service() {
            if n.is_many() && capability.path.is_some() {
                return Err(Error::validate(
                    "\"path\" can only be specified when one `service` is supplied.",
                ));
            }
            return Ok(Self::services_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.protocol() {
            if n.is_many() && capability.path.is_some() {
                return Err(Error::validate(
                    "\"path\" can only be specified when one `protocol` is supplied.",
                ));
            }
            return Ok(Self::protocols_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.directory() {
            return Ok(Self::directories_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.storage() {
            if capability.storage_id.is_none() {
                return Err(Error::validate(
                    "Storage declaration is missing \"storage_id\", but is required.",
                ));
            }
            return Ok(Self::storages_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.runner() {
            return Ok(Self::runners_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.resolver() {
            return Ok(Self::resolvers_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.event_stream() {
            return Ok(Self::event_streams_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.dictionary() {
            return Ok(Self::dictionaries_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        } else if let Some(n) = capability.config() {
            return Ok(Self::configurations_from(Self::get_one_or_many_names_no_span(
                n,
                None,
                capability.capability_type().unwrap(),
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = capability
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate(format!(
            "`{}` declaration is missing a capability keyword, one of: {}",
            capability.decl_type(),
            supported_keywords,
        )))
    }

    /// Given an Offer or Expose clause, return the set of target identifiers.
    ///
    /// When only one capability identifier is specified, the target identifier name is derived
    /// using the "as" clause. If an "as" clause is not specified, the target identifier is the
    /// same name as the source.
    ///
    /// When multiple capability identifiers are specified, the target names are the same as the
    /// source names.
    pub fn from_offer_expose<T>(clause: &'a T) -> Result<Vec<Self>, Error>
    where
        T: CapabilityClause + AsClause + fmt::Debug,
    {
        // TODO: Validate that exactly one of these is set.
        let alias = clause.r#as();
        if let Some(n) = clause.service() {
            return Ok(Self::services_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.protocol() {
            return Ok(Self::protocols_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.directory() {
            return Ok(Self::directories_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.storage() {
            return Ok(Self::storages_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.runner() {
            return Ok(Self::runners_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.resolver() {
            return Ok(Self::resolvers_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(event_stream) = clause.event_stream() {
            return Ok(Self::event_streams_from(Self::get_one_or_many_names_no_span(
                event_stream,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.dictionary() {
            return Ok(Self::dictionaries_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        } else if let Some(n) = clause.config() {
            return Ok(Self::configurations_from(Self::get_one_or_many_names_no_span(
                n,
                alias,
                clause.capability_type().unwrap(),
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = clause
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate(format!(
            "`{}` declaration is missing a capability keyword, one of: {}",
            clause.decl_type(),
            supported_keywords,
        )))
    }

    pub fn from_context_capability(
        capability_input: &'a ContextSpanned<ContextCapability>,
    ) -> Result<Vec<(Self, Origin)>, Error> {
        let capability = &capability_input.value;
        let origin = &capability_input.origin;

        if let Some(n) = capability.service() {
            if n.value.is_many()
                && let Some(cs_path) = &capability.path
            {
                return Err(Error::validate_context(
                    "\"path\" can only be specified when one `service` is supplied.",
                    Some(cs_path.origin.clone()),
                ));
            }
            return Ok(Self::services_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.protocol() {
            if n.value.is_many()
                && let Some(cs_path) = &capability.path
            {
                return Err(Error::validate_context(
                    "\"path\" can only be specified when one `protocol` is supplied.",
                    Some(cs_path.origin.clone()),
                ));
            }
            return Ok(Self::protocols_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.directory() {
            return Ok(Self::directories_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(cs_storage) = capability.storage() {
            if capability.storage_id.is_none() {
                return Err(Error::validate_context(
                    "Storage declaration is missing \"storage_id\", but is required.",
                    Some(cs_storage.origin),
                ));
            }
            return Ok(Self::storages_from_context(Self::get_one_or_many_names_context(
                cs_storage,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.runner() {
            return Ok(Self::runners_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.resolver() {
            return Ok(Self::resolvers_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.event_stream() {
            return Ok(Self::event_streams_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.dictionary() {
            return Ok(Self::dictionaries_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        } else if let Some(n) = capability.config() {
            return Ok(Self::configurations_from_context(Self::get_one_or_many_names_context(
                n,
                None,
                capability.capability_type(None).unwrap(),
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = capability
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate_context(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                capability.decl_type(),
                supported_keywords,
            ),
            Some(origin.clone()),
        ))
    }

    pub fn from_context_offer(
        offer_input: &'a ContextSpanned<ContextOffer>,
    ) -> Result<Vec<(Self, Origin)>, Error> {
        let offer = &offer_input.value;
        let origin = &offer_input.origin;

        let alias = offer.r#as();

        if let Some(n) = offer.service() {
            return Ok(Self::services_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.protocol() {
            return Ok(Self::protocols_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.directory() {
            return Ok(Self::directories_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.storage() {
            return Ok(Self::storages_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.runner() {
            return Ok(Self::runners_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.resolver() {
            return Ok(Self::resolvers_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(event_stream) = offer.event_stream() {
            return Ok(Self::event_streams_from_context(Self::get_one_or_many_names_context(
                event_stream,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.dictionary() {
            return Ok(Self::dictionaries_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = offer.config() {
            return Ok(Self::configurations_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                offer.capability_type(Some(origin.clone())).unwrap(),
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = offer
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate_context(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                offer.decl_type(),
                supported_keywords,
            ),
            Some(origin.clone()),
        ))
    }

    pub fn from_context_expose(
        expose_input: &'a ContextSpanned<ContextExpose>,
    ) -> Result<Vec<(Self, Origin)>, Error> {
        let expose = &expose_input.value;
        let origin = &expose_input.origin;

        let alias = expose.r#as();

        if let Some(n) = expose.service() {
            return Ok(Self::services_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.protocol() {
            return Ok(Self::protocols_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.directory() {
            return Ok(Self::directories_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.storage() {
            return Ok(Self::storages_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.runner() {
            return Ok(Self::runners_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.resolver() {
            return Ok(Self::resolvers_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(event_stream) = expose.event_stream() {
            return Ok(Self::event_streams_from_context(Self::get_one_or_many_names_context(
                event_stream,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.dictionary() {
            return Ok(Self::dictionaries_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = expose.config() {
            return Ok(Self::configurations_from_context(Self::get_one_or_many_names_context(
                n,
                alias,
                expose.capability_type(Some(origin.clone())).unwrap(),
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = expose
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate_context(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                expose.decl_type(),
                supported_keywords,
            ),
            Some(origin.clone()),
        ))
    }

    /// Given a ContextUse clause, return the set of target identifiers.
    ///
    /// When only one capability identifier is specified, the target identifier name is derived
    /// using the "path" clause. If a "path" clause is not specified, the target identifier is the
    /// same name as the source.
    ///
    /// When multiple capability identifiers are specified, the target names are the same as the
    /// source names.
    pub fn from_context_use(
        use_input: &'a ContextSpanned<ContextUse>,
    ) -> Result<Vec<(Self, Origin)>, Error> {
        let use_ = &use_input.value;
        let origin = &use_input.origin;

        let alias = use_.path.as_ref();

        if let Some(n) = option_one_or_many_as_ref_context(&use_.service) {
            return Ok(Self::used_services_from_context(Self::get_one_or_many_svc_paths_context(
                n,
                alias,
                use_.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(n) = option_one_or_many_as_ref_context(&use_.protocol) {
            return Ok(Self::used_protocols_from_context(Self::get_one_or_many_svc_paths_context(
                n,
                alias,
                use_.capability_type(Some(origin.clone())).unwrap(),
            )?));
        } else if let Some(_) = &use_.directory {
            if use_.path.is_none() {
                return Err(Error::validate_context(
                    "\"path\" should be present for `use directory`.",
                    Some(origin.clone()),
                ));
            }
            return Ok(vec![(
                CapabilityId::UsedDirectory(use_.path.as_ref().unwrap().value.clone()),
                origin.clone(),
            )]);
        } else if let Some(_) = &use_.storage {
            if use_.path.is_none() {
                return Err(Error::validate_context(
                    "\"path\" should be present for `use storage`.",
                    Some(origin.clone()),
                ));
            }
            return Ok(vec![(
                CapabilityId::UsedStorage(use_.path.as_ref().unwrap().value.clone()),
                origin.clone(),
            )]);
        } else if let Some(_) = &use_.event_stream {
            if let Some(path) = &use_.path {
                return Ok(vec![(
                    CapabilityId::UsedEventStream(path.value.clone()),
                    origin.clone(),
                )]);
            }
            return Ok(vec![(
                CapabilityId::UsedEventStream(Path::new("/svc/fuchsia.component.EventStream")?),
                origin.clone(),
            )]);
        } else if let Some(n) = &use_.runner {
            return Ok(vec![(CapabilityId::UsedRunner(&n.value), n.origin.clone())]);
        } else if let Some(_) = &use_.config {
            return match &use_.key {
                None => Err(Error::validate_context(
                    "\"key\" should be present for `use config`.",
                    Some(origin.clone()),
                )),
                Some(name) => {
                    Ok(vec![(CapabilityId::UsedConfiguration(&name.value), origin.clone())])
                }
            };
        } else if let Some(n) = option_one_or_many_as_ref_context(&use_.dictionary) {
            return Ok(Self::used_dictionaries_from_context(
                Self::get_one_or_many_svc_paths_context(
                    n,
                    alias,
                    use_.capability_type(Some(origin.clone())).unwrap(),
                )?,
            ));
        }

        // Unsupported capability type.
        let supported_keywords = use_
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");

        Err(Error::validate_context(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                use_.decl_type(),
                supported_keywords,
            ),
            Some(origin.clone()),
        ))
    }

    /// Returns the target names as a `Vec` from a declaration with `names` and `alias` as a `Vec`.
    fn get_one_or_many_names_no_span<'b>(
        names: OneOrMany<&'b BorrowedName>,
        alias: Option<&'b BorrowedName>,
        capability_type: &str,
    ) -> Result<Vec<&'b BorrowedName>, Error> {
        let names: Vec<&BorrowedName> = names.into_iter().collect();
        if names.len() == 1 {
            Ok(vec![alias_or_name(alias, &names[0])])
        } else {
            if alias.is_some() {
                return Err(Error::validate(format!(
                    "\"as\" can only be specified when one `{}` is supplied.",
                    capability_type,
                )));
            }
            Ok(names)
        }
    }

    /// Returns the target names as a `Vec` from a declaration with `names` and `alias` as a `Vec`.
    fn get_one_or_many_names_context<'b>(
        name_wrapper: ContextSpanned<OneOrMany<&'b BorrowedName>>,
        alias: Option<ContextSpanned<&'b BorrowedName>>,
        capability_type: &str,
    ) -> Result<Vec<ContextSpanned<&'b BorrowedName>>, Error> {
        let names_origin = name_wrapper.origin;
        let names_vec: Vec<&'b BorrowedName> = name_wrapper.value.into_iter().collect();
        let num_names = names_vec.len();

        if num_names > 1 && alias.is_some() {
            return Err(Error::validate_contexts(
                format!("\"as\" can only be specified when one `{}` is supplied.", capability_type),
                vec![alias.map(|s| s.origin).unwrap_or(names_origin)],
            ));
        }

        if num_names == 1 {
            let final_name_span = alias_or_name_context(alias, names_vec[0], names_origin);
            return Ok(vec![final_name_span]);
        }

        let final_names = names_vec
            .into_iter()
            .map(|name| ContextSpanned { value: name, origin: names_origin.clone() })
            .collect();

        Ok(final_names)
    }

    /// Returns the target paths as a `Vec` from a `use` declaration with `names` and `alias`.
    fn get_one_or_many_svc_paths(
        names: OneOrMany<&BorrowedName>,
        alias: Option<&Path>,
        capability_type: &str,
    ) -> Result<Vec<Path>, Error> {
        let names: Vec<_> = names.into_iter().collect();
        match (names.len(), alias) {
            (_, None) => {
                Ok(names.into_iter().map(|n| format!("/svc/{}", n).parse().unwrap()).collect())
            }
            (1, Some(alias)) => Ok(vec![alias.clone()]),
            (_, Some(_)) => {
                return Err(Error::validate(format!(
                    "\"path\" can only be specified when one `{}` is supplied.",
                    capability_type,
                )));
            }
        }
    }

    fn get_one_or_many_svc_paths_context(
        names: ContextSpanned<OneOrMany<&BorrowedName>>,
        alias: Option<&ContextSpanned<Path>>,
        capability_type: &str,
    ) -> Result<Vec<ContextSpanned<Path>>, Error> {
        let names_origin = &names.origin;
        let names_vec: Vec<_> = names.value.into_iter().collect();

        match (names_vec.len(), alias) {
            (_, None) => {
                let generated_paths = names_vec
                    .into_iter()
                    .map(|n| {
                        let new_path: Path = format!("/svc/{}", n).parse().unwrap();
                        ContextSpanned { value: new_path, origin: names_origin.clone() }
                    })
                    .collect();
                Ok(generated_paths)
            }

            (1, Some(spanned_alias)) => Ok(vec![spanned_alias.clone()]),

            (_, Some(spanned_alias)) => Err(Error::validate_contexts(
                format!(
                    "\"path\" can only be specified when one `{}` is supplied.",
                    capability_type,
                ),
                vec![spanned_alias.origin.clone()],
            )),
        }
    }

    capability_ids_from_names!(services_from, CapabilityId::Service);
    capability_ids_from_names!(protocols_from, CapabilityId::Protocol);
    capability_ids_from_names!(directories_from, CapabilityId::Directory);
    capability_ids_from_names!(storages_from, CapabilityId::Storage);
    capability_ids_from_names!(runners_from, CapabilityId::Runner);
    capability_ids_from_names!(resolvers_from, CapabilityId::Resolver);
    capability_ids_from_names!(event_streams_from, CapabilityId::EventStream);
    capability_ids_from_names!(dictionaries_from, CapabilityId::Dictionary);
    capability_ids_from_names!(configurations_from, CapabilityId::Configuration);

    capability_ids_from_paths!(used_services_from, CapabilityId::UsedService);
    capability_ids_from_paths!(used_protocols_from, CapabilityId::UsedProtocol);
    capability_ids_from_paths!(used_dictionaries_from, CapabilityId::UsedDictionary);

    capability_ids_from_context_names!(services_from_context, CapabilityId::Service);
    capability_ids_from_context_names!(protocols_from_context, CapabilityId::Protocol);
    capability_ids_from_context_names!(directories_from_context, CapabilityId::Directory);
    capability_ids_from_context_names!(storages_from_context, CapabilityId::Storage);
    capability_ids_from_context_names!(runners_from_context, CapabilityId::Runner);
    capability_ids_from_context_names!(resolvers_from_context, CapabilityId::Resolver);
    capability_ids_from_context_names!(event_streams_from_context, CapabilityId::EventStream);
    capability_ids_from_context_names!(dictionaries_from_context, CapabilityId::Dictionary);
    capability_ids_from_context_names!(configurations_from_context, CapabilityId::Configuration);

    capability_ids_from_context_paths!(used_services_from_context, CapabilityId::UsedService);
    capability_ids_from_context_paths!(used_protocols_from_context, CapabilityId::UsedProtocol);
    capability_ids_from_context_paths!(
        used_dictionaries_from_context,
        CapabilityId::UsedDictionary
    );
}

impl fmt::Display for CapabilityId<'_> {
    /// Return the string ID of this clause.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapabilityId::Service(n)
            | CapabilityId::Storage(n)
            | CapabilityId::Runner(n)
            | CapabilityId::UsedRunner(n)
            | CapabilityId::Resolver(n)
            | CapabilityId::EventStream(n)
            | CapabilityId::Configuration(n)
            | CapabilityId::UsedConfiguration(n)
            | CapabilityId::Dictionary(n) => write!(f, "{}", n),
            CapabilityId::UsedService(p)
            | CapabilityId::UsedProtocol(p)
            | CapabilityId::UsedDirectory(p)
            | CapabilityId::UsedStorage(p)
            | CapabilityId::UsedEventStream(p)
            | CapabilityId::UsedDictionary(p) => write!(f, "{}", p),
            CapabilityId::Protocol(p) | CapabilityId::Directory(p) => write!(f, "{}", p),
        }
    }
}
