// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AsClause, Capability, CapabilityClause, Error, Location, PathClause, SpannedCapability,
    SpannedCapabilityClause, SpannedExpose, SpannedOffer, SpannedUse, Use, alias_or_name,
    byte_index_to_location,
};

use crate::one_or_many::OneOrMany;
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    NamespacePath, OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use json_spanned_value::Spanned;

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

/// Generates a `Vec<Path>` -> `Vec<CapabilityId>` conversion function.
macro_rules! capability_ids_from_paths {
    ($name:ident, $variant:expr) => {
        fn $name(paths: Vec<Path>) -> Vec<Self> {
            paths.into_iter().map(|p| $variant(p)).collect()
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

    /// Given a Use clause, return the set of target identifiers.
    ///
    /// When only one capability identifier is specified, the target identifier name is derived
    /// using the "path" clause. If a "path" clause is not specified, the target identifier is the
    /// same name as the source.
    ///
    /// When multiple capability identifiers are specified, the target names are the same as the
    /// source names.
    pub fn from_spanned_use(
        use_: &'a Spanned<SpannedUse>,
        filename: Option<&std::path::Path>,
        file_source: Option<&String>,
    ) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        let alias = use_.path.as_ref();
        if let Some(n) = use_.service() {
            return Ok(Self::used_services_from(Self::get_one_or_many_svc_paths(
                n,
                alias.map(|v| &**v),
                use_.capability_type().unwrap(),
            )?));
        } else if let Some(n) = use_.protocol() {
            return Ok(Self::used_protocols_from(Self::get_one_or_many_svc_paths(
                n,
                alias.map(|v| &**v),
                use_.capability_type().unwrap(),
            )?));
        } else if let Some(_) = use_.directory.as_ref() {
            if use_.path.is_none() {
                let location =
                    byte_index_to_location(file_source, use_.directory.as_ref().unwrap().span().0);
                return Err(Error::validate_with_span(
                    "\"path\" should be present for `use directory`.",
                    location,
                    filename,
                ));
            }
            return Ok(vec![CapabilityId::UsedDirectory(
                use_.path.as_ref().unwrap().get_ref().clone(),
            )]);
        } else if let Some(_) = use_.storage.as_ref() {
            if use_.path.is_none() {
                let location =
                    byte_index_to_location(file_source, use_.storage.as_ref().unwrap().span().0);
                return Err(Error::validate_with_span(
                    "\"path\" should be present for `use storage`.",
                    location,
                    filename,
                ));
            }
            return Ok(vec![CapabilityId::UsedStorage(
                use_.path.as_ref().unwrap().get_ref().clone(),
            )]);
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
                    let location =
                        byte_index_to_location(file_source, use_.runner.as_ref().unwrap().span().0);
                    return Err(Error::validate_with_span(
                        "`use runner` should occur at most once.",
                        location,
                        filename,
                    ));
                }
            }
        } else if let Some(_) = use_.config() {
            return match &use_.key {
                None => {
                    let location =
                        byte_index_to_location(file_source, use_.config.as_ref().unwrap().span().0);
                    Err(Error::validate_with_span(
                        "\"key\" should be present for `use config`.",
                        location,
                        filename,
                    ))
                }
                Some(name) => Ok(vec![CapabilityId::UsedConfiguration(name)]),
            };
        } else if let Some(n) = use_.dictionary() {
            return Ok(Self::used_dictionaries_from(Self::get_one_or_many_svc_paths(
                n,
                alias.map(|v| &**v),
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

        let location = byte_index_to_location(file_source, use_.span().0);

        Err(Error::validate_with_span(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                use_.decl_type(),
                supported_keywords,
            ),
            location,
            filename,
        ))
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

    pub fn from_spanned_capability(
        capability: &'a Spanned<SpannedCapability>,
        filename: Option<&std::path::Path>,
        file_source: Option<&String>,
    ) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        if let Some(n) = capability.service() {
            if n.is_many() && capability.path.is_some() {
                let location =
                    byte_index_to_location(file_source, capability.path.as_ref().unwrap().span().0);
                return Err(Error::validate_with_span(
                    "\"path\" can only be specified when one `service` is supplied.",
                    location,
                    filename,
                ));
            }
            return Ok(Self::services_from(Self::get_names(n)?));
        } else if let Some(n) = capability.protocol() {
            if n.is_many() && capability.path.is_some() {
                let location =
                    byte_index_to_location(file_source, capability.path.as_ref().unwrap().span().0);
                return Err(Error::validate_with_span(
                    "\"path\" can only be specified when one `protocol` is supplied.",
                    location,
                    filename,
                ));
            }
            return Ok(Self::protocols_from(Self::get_names(n)?));
        } else if let Some(n) = capability.directory() {
            return Ok(Self::directories_from(Self::get_names(n)?));
        } else if let Some(n) = capability.storage() {
            if capability.storage_id.is_none() {
                let location = byte_index_to_location(
                    file_source,
                    capability.storage.as_ref().unwrap().span().0,
                );
                return Err(Error::validate_with_span(
                    "Storage declaration is missing \"storage_id\", but is required.",
                    location,
                    filename,
                ));
            }
            return Ok(Self::storages_from(Self::get_names(n)?));
        } else if let Some(n) = capability.runner() {
            return Ok(Self::runners_from(Self::get_names(n)?));
        } else if let Some(n) = capability.resolver() {
            return Ok(Self::resolvers_from(Self::get_names(n)?));
        } else if let Some(n) = capability.event_stream() {
            return Ok(Self::event_streams_from(Self::get_names(n)?));
        } else if let Some(n) = capability.dictionary() {
            return Ok(Self::dictionaries_from(Self::get_names(n)?));
        } else if let Some(n) = capability.config() {
            return Ok(Self::configurations_from(Self::get_names(n)?));
        }

        // Unsupported capability type.
        let supported_keywords = capability
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        let location = byte_index_to_location(file_source, capability.span().0);

        Err(Error::validate_with_span(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                capability.decl_type(),
                supported_keywords,
            ),
            location,
            filename,
        ))
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

    /// Given an Offer or Expose clause, return the set of target identifiers.
    ///
    /// When only one capability identifier is specified, the target identifier name is derived
    /// using the "as" clause. If an "as" clause is not specified, the target identifier is the
    /// same name as the source.
    ///
    /// When multiple capability identifiers are specified, the target names are the same as the
    /// source names.
    pub fn from_spanned_expose(
        expose: &'a Spanned<SpannedExpose>,
        filename: Option<&std::path::Path>,
        file_source: Option<&String>,
    ) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        let alias = expose.r#as();
        let location = byte_index_to_location(file_source, expose.span().0);

        if let Some(n) = expose.service() {
            return Ok(Self::services_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.protocol() {
            return Ok(Self::protocols_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.directory() {
            return Ok(Self::directories_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.storage() {
            return Ok(Self::storages_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.runner() {
            return Ok(Self::runners_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.resolver() {
            return Ok(Self::resolvers_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(event_stream) = expose.event_stream() {
            return Ok(Self::event_streams_from(Self::get_one_or_many_names(
                event_stream,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.dictionary() {
            return Ok(Self::dictionaries_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = expose.config() {
            return Ok(Self::configurations_from(Self::get_one_or_many_names(
                n,
                alias,
                expose.capability_type().unwrap(),
                location,
                filename,
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = expose
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate_with_span(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                expose.decl_type(),
                supported_keywords,
            ),
            location,
            filename,
        ))
    }

    pub fn from_spanned_offer(
        offer: &'a Spanned<SpannedOffer>,
        filename: Option<&std::path::Path>,
        file_source: Option<&String>,
    ) -> Result<Vec<Self>, Error> {
        // TODO: Validate that exactly one of these is set.
        let alias = offer.r#as();
        let location = byte_index_to_location(file_source, offer.span().0);

        if let Some(n) = offer.service() {
            return Ok(Self::services_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.protocol() {
            return Ok(Self::protocols_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.directory() {
            return Ok(Self::directories_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.storage() {
            return Ok(Self::storages_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.runner() {
            return Ok(Self::runners_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.resolver() {
            return Ok(Self::resolvers_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(event_stream) = offer.event_stream() {
            return Ok(Self::event_streams_from(Self::get_one_or_many_names(
                event_stream,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.dictionary() {
            return Ok(Self::dictionaries_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        } else if let Some(n) = offer.config() {
            return Ok(Self::configurations_from(Self::get_one_or_many_names(
                n,
                alias,
                offer.capability_type().unwrap(),
                location,
                filename,
            )?));
        }

        // Unsupported capability type.
        let supported_keywords = offer
            .supported()
            .into_iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::validate_with_span(
            format!(
                "`{}` declaration is missing a capability keyword, one of: {}",
                offer.decl_type(),
                supported_keywords,
            ),
            location,
            filename,
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

    /// Returns the target names as a `Vec`.
    fn get_names<'b>(names: OneOrMany<&'b BorrowedName>) -> Result<Vec<&'b BorrowedName>, Error> {
        let names: Vec<&BorrowedName> = names.into_iter().collect();
        Ok(names)
    }

    /// Returns the target names as a `Vec` from a declaration with `names` and `alias` as a `Vec`.
    fn get_one_or_many_names<'b>(
        names: OneOrMany<&'b BorrowedName>,
        alias: Option<&'b BorrowedName>,
        capability_type: &str,
        location: Option<Location>,
        filepath: Option<&std::path::Path>,
    ) -> Result<Vec<&'b BorrowedName>, Error> {
        let names: Vec<&BorrowedName> = names.into_iter().collect();
        if names.len() == 1 {
            Ok(vec![alias_or_name(alias, &names[0])])
        } else {
            if alias.is_some() {
                return Err(Error::validate_with_span(
                    format!(
                        "\"as\" can only be specified when one `{}` is supplied.",
                        capability_type,
                    ),
                    location,
                    filepath,
                ));
            }
            Ok(names)
        }
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
