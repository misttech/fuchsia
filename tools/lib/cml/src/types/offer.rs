// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AnyRef, AsClause, Canonicalize, CapabilityClause, DictionaryRef, EventScope, FromClause,
    PathClause, SourceAvailability, SpannedCapabilityClause, one_or_many_from_impl,
    option_one_or_many_as_ref, option_spanned_one_or_many_as_ref,
};

use crate::one_or_many::OneOrMany;
use crate::types::right::{Rights, RightsClause};
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DependencyType, HandleType, Name, OnTerminate,
    ParseError, Path, RelativePath, StartupMode, Url,
};
use cml_macro::{OneOrMany, Reference};
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};

use std::fmt;

/// Example:
///
/// ```json5
/// offer: [
///     {
///         protocol: "fuchsia.logger.LogSink",
///         from: "#logger",
///         to: [ "#fshost", "#pkg_cache" ],
///         dependency: "weak",
///     },
///     {
///         protocol: [
///             "fuchsia.ui.app.ViewProvider",
///             "fuchsia.fonts.Provider",
///         ],
///         from: "#session",
///         to: [ "#ui_shell" ],
///         dependency: "strong",
///     },
///     {
///         directory: "blobfs",
///         from: "self",
///         to: [ "#pkg_cache" ],
///     },
///     {
///         directory: "fshost-config",
///         from: "parent",
///         to: [ "#fshost" ],
///         as: "config",
///     },
///     {
///         storage: "cache",
///         from: "parent",
///         to: [ "#logger" ],
///     },
///     {
///         runner: "web",
///         from: "parent",
///         to: [ "#user-shell" ],
///     },
///     {
///         resolver: "full-resolver",
///         from: "parent",
///         to: [ "#user-shell" ],
///     },
///     {
///         event_stream: "stopped",
///         from: "framework",
///         to: [ "#logger" ],
///     },
/// ],
/// ```
#[derive(Deserialize, Debug, PartialEq, Clone, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list", top_level_doc_after_fields)]
pub struct Offer {
    /// When routing a service, the [name](#name) of a [service capability][doc-service].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<OneOrMany<Name>>,

    /// When routing a protocol, the [name](#name) of a [protocol capability][doc-protocol].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<OneOrMany<Name>>,

    /// When routing a directory, the [name](#name) of a [directory capability][doc-directory].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<OneOrMany<Name>>,

    /// When routing a runner, the [name](#name) of a [runner capability][doc-runners].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<OneOrMany<Name>>,

    /// When routing a resolver, the [name](#name) of a [resolver capability][doc-resolvers].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<OneOrMany<Name>>,

    /// When routing a storage capability, the [name](#name) of a [storage capability][doc-storage].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<OneOrMany<Name>>,

    /// When routing a dictionary, the [name](#name) of a [dictionary capability][doc-dictionaries].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<OneOrMany<Name>>,

    /// When routing a config, the [name](#name) of a configuration capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<OneOrMany<Name>>,

    /// `from`: The source of the capability, one of:
    /// - `parent`: The component's parent. This source can be used for all
    ///     capability types.
    /// - `self`: This component. Requires a corresponding
    ///     [`capability`](#capabilities) declaration.
    /// - `framework`: The Component Framework runtime.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance. This source can only be used when offering protocol,
    ///     directory, or runner capabilities.
    /// - `void`: The source is intentionally omitted. Only valid when `availability` is
    ///     `optional` or `transitional`.
    pub from: OneOrMany<OfferFromRef>,

    /// Capability target(s). One of:
    /// - `#<target-name>` or \[`#name1`, ...\]: A [reference](#references) to a child or collection,
    ///   or an array of references.
    /// - `all`: Short-hand for an `offer` clause containing all child [references](#references).
    pub to: OneOrMany<OfferToRef>,

    /// An explicit [name](#name) for the capability as it will be known by the target. If omitted,
    /// defaults to the original name. `as` cannot be used when an array of multiple names is
    /// provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#as: Option<Name>,

    /// The type of dependency between the source and
    /// targets, one of:
    /// - `strong`: a strong dependency, which is used to determine shutdown
    ///     ordering. Component manager is guaranteed to stop the target before the
    ///     source. This is the default.
    /// - `weak`: a weak dependency, which is ignored during
    ///     shutdown. When component manager stops the parent realm, the source may
    ///     stop before the clients. Clients of weak dependencies must be able to
    ///     handle these dependencies becoming unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency: Option<DependencyType>,

    /// (`directory` only) the maximum [directory rights][doc-directory-rights] to apply to
    /// the offered directory capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(json_type = "array of string")]
    pub rights: Option<Rights>,

    /// (`directory` only) the relative path of a subdirectory within the source directory
    /// capability to route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<RelativePath>,

    /// (`event_stream` only) the name(s) of the event streams being offered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_stream: Option<OneOrMany<Name>>,

    /// (`event_stream` only) When defined the event stream will contain events about only the
    /// components defined in the scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<OneOrMany<EventScope>>,

    /// `availability` _(optional)_: The expectations around this capability's availability. Affects
    /// build-time and runtime route validation. One of:
    /// - `required` (default): a required dependency, the source must exist and provide it. Use
    ///     this when the target of this offer requires this capability to function properly.
    /// - `optional`: an optional dependency. Use this when the target of the offer can function
    ///     with or without this capability. The target must not have a `required` dependency on the
    ///     capability. The ultimate source of this offer must be `void` or an actual component.
    /// - `same_as_target`: the availability expectations of this capability will match the
    ///     target's. If the target requires the capability, then this field is set to `required`.
    ///     If the target has an optional dependency on the capability, then the field is set to
    ///     `optional`.
    /// - `transitional`: like `optional`, but will tolerate a missing source. Use this
    ///     only to avoid validation errors during transitional periods of multi-step code changes.
    ///
    /// For more information, see the
    /// [availability](/docs/concepts/components/v2/capabilities/availability.md) documentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<Availability>,

    /// Whether or not the source of this offer must exist. One of:
    /// - `required` (default): the source (`from`) must be defined in this manifest.
    /// - `unknown`: the source of this offer will be rewritten to `void` if its source (`from`)
    ///     is not defined in this manifest after includes are processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_availability: Option<SourceAvailability>,

    /// Whether or not the target of this offer must exist. One of:
    /// - `required` (default): the target (`to`) must be defined in this
    ///   manifest.
    /// - `unknown`: this offer is omitted if its target (`to`) is not defined
    ///     in this manifest after includes are processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_availability: Option<TargetAvailability>,
}

impl Offer {
    /// Creates a new empty offer. This offer just has the `from` and `to` fields set, so to make
    /// it useful it needs at least the capability name set in the necessary attribute.
    pub fn empty(from: OneOrMany<OfferFromRef>, to: OneOrMany<OfferToRef>) -> Offer {
        Self {
            protocol: None,
            from,
            to,
            r#as: None,
            service: None,
            directory: None,
            config: None,
            runner: None,
            resolver: None,
            storage: None,
            dictionary: None,
            dependency: None,
            rights: None,
            subdir: None,
            event_stream: None,
            scope: None,
            availability: None,
            source_availability: None,
            target_availability: None,
        }
    }
}

impl FromClause for Offer {
    fn from_(&self) -> OneOrMany<AnyRef<'_>> {
        one_or_many_from_impl(&self.from)
    }
}

impl Canonicalize for Offer {
    fn canonicalize(&mut self) {
        // Sort the names of the capabilities. Only capabilities with OneOrMany values are included here.
        if let Some(service) = &mut self.service {
            service.canonicalize();
        } else if let Some(protocol) = &mut self.protocol {
            protocol.canonicalize();
        } else if let Some(directory) = &mut self.directory {
            directory.canonicalize();
        } else if let Some(runner) = &mut self.runner {
            runner.canonicalize();
        } else if let Some(resolver) = &mut self.resolver {
            resolver.canonicalize();
        } else if let Some(storage) = &mut self.storage {
            storage.canonicalize();
        } else if let Some(event_stream) = &mut self.event_stream {
            event_stream.canonicalize();
            if let Some(scope) = &mut self.scope {
                scope.canonicalize();
            }
        }
    }
}

impl CapabilityClause for Offer {
    fn service(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.service)
    }
    fn protocol(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.protocol)
    }
    fn directory(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.directory)
    }
    fn storage(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.storage)
    }
    fn runner(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.runner)
    }
    fn resolver(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.resolver)
    }
    fn event_stream(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.event_stream)
    }
    fn dictionary(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.dictionary)
    }
    fn config(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.config)
    }

    fn set_service(&mut self, o: Option<OneOrMany<Name>>) {
        self.service = o;
    }
    fn set_protocol(&mut self, o: Option<OneOrMany<Name>>) {
        self.protocol = o;
    }
    fn set_directory(&mut self, o: Option<OneOrMany<Name>>) {
        self.directory = o;
    }
    fn set_storage(&mut self, o: Option<OneOrMany<Name>>) {
        self.storage = o;
    }
    fn set_runner(&mut self, o: Option<OneOrMany<Name>>) {
        self.runner = o;
    }
    fn set_resolver(&mut self, o: Option<OneOrMany<Name>>) {
        self.resolver = o;
    }
    fn set_event_stream(&mut self, o: Option<OneOrMany<Name>>) {
        self.event_stream = o;
    }
    fn set_dictionary(&mut self, o: Option<OneOrMany<Name>>) {
        self.dictionary = o;
    }
    fn set_config(&mut self, o: Option<OneOrMany<Name>>) {
        self.config = o
    }

    fn availability(&self) -> Option<Availability> {
        self.availability
    }
    fn set_availability(&mut self, a: Option<Availability>) {
        self.availability = a;
    }

    fn decl_type(&self) -> &'static str {
        "offer"
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
            "config",
        ]
    }
    fn are_many_names_allowed(&self) -> bool {
        [
            "service",
            "protocol",
            "directory",
            "storage",
            "runner",
            "resolver",
            "event_stream",
            "config",
        ]
        .contains(&self.capability_type().unwrap())
    }
}

impl PathClause for Offer {
    fn path(&self) -> Option<&Path> {
        None
    }
}

impl RightsClause for Offer {
    fn rights(&self) -> Option<&Rights> {
        self.rights.as_ref()
    }
}

impl AsClause for Offer {
    fn r#as(&self) -> Option<&BorrowedName> {
        self.r#as.as_ref().map(Name::as_ref)
    }
}

/// A reference in an `offer to`.
#[derive(Debug, Deserialize, PartialEq, Eq, Hash, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAvailability {
    Required,
    Unknown,
}

#[derive(Deserialize, Debug, PartialEq, Clone, ReferenceDoc)]
#[serde(deny_unknown_fields)]
pub struct SpannedOffer {
    /// When routing a service, the [name](#name) of a [service capability][doc-service].
    pub service: Option<OneOrMany<Name>>,

    /// When routing a protocol, the [name](#name) of a [protocol capability][doc-protocol].
    pub protocol: Option<Spanned<OneOrMany<Name>>>,

    /// When routing a directory, the [name](#name) of a [directory capability][doc-directory].
    pub directory: Option<OneOrMany<Name>>,

    /// When routing a runner, the [name](#name) of a [runner capability][doc-runners].
    pub runner: Option<OneOrMany<Name>>,

    /// When routing a resolver, the [name](#name) of a [resolver capability][doc-resolvers].
    pub resolver: Option<OneOrMany<Name>>,

    /// When routing a storage capability, the [name](#name) of a [storage capability][doc-storage].
    pub storage: Option<Spanned<OneOrMany<Name>>>,

    /// When routing a dictionary, the [name](#name) of a [dictionary capability][doc-dictionaries].
    pub dictionary: Option<Spanned<OneOrMany<Name>>>,

    /// When routing a config, the [name](#name) of a configuration capability.
    pub config: Option<OneOrMany<Name>>,

    /// `from`: The source of the capability, one of:
    /// - `parent`: The component's parent. This source can be used for all
    ///     capability types.
    /// - `self`: This component. Requires a corresponding
    ///     [`capability`](#capabilities) declaration.
    /// - `framework`: The Component Framework runtime.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance. This source can only be used when offering protocol,
    ///     directory, or runner capabilities.
    /// - `void`: The source is intentionally omitted. Only valid when `availability` is
    ///     `optional` or `transitional`.
    pub from: OneOrMany<OfferFromRef>,

    /// Capability target(s). One of:
    /// - `#<target-name>` or \[`#name1`, ...\]: A [reference](#references) to a child or collection,
    ///   or an array of references.
    /// - `all`: Short-hand for an `offer` clause containing all child [references](#references).
    pub to: OneOrMany<OfferToRef>,

    /// An explicit [name](#name) for the capability as it will be known by the target. If omitted,
    /// defaults to the original name. `as` cannot be used when an array of multiple names is
    /// provided.
    pub r#as: Option<Spanned<Name>>,

    /// The type of dependency between the source and
    /// targets, one of:
    /// - `strong`: a strong dependency, which is used to determine shutdown
    ///     ordering. Component manager is guaranteed to stop the target before the
    ///     source. This is the default.
    /// - `weak`: a weak dependency, which is ignored during
    ///     shutdown. When component manager stops the parent realm, the source may
    ///     stop before the clients. Clients of weak dependencies must be able to
    ///     handle these dependencies becoming unavailable.
    pub dependency: Option<Spanned<DependencyType>>,

    /// (`directory` only) the maximum [directory rights][doc-directory-rights] to apply to
    /// the offered directory capability.
    pub rights: Option<Spanned<Rights>>,

    /// (`directory` only) the relative path of a subdirectory within the source directory
    /// capability to route.
    pub subdir: Option<RelativePath>,

    /// (`event_stream` only) the name(s) of the event streams being offered.
    pub event_stream: Option<Spanned<OneOrMany<Name>>>,

    /// (`event_stream` only) When defined the event stream will contain events about only the
    /// components defined in the scope.
    pub scope: Option<OneOrMany<EventScope>>,

    /// `availability` _(optional)_: The expectations around this capability's availability. Affects
    /// build-time and runtime route validation. One of:
    /// - `required` (default): a required dependency, the source must exist and provide it. Use
    ///     this when the target of this offer requires this capability to function properly.
    /// - `optional`: an optional dependency. Use this when the target of the offer can function
    ///     with or without this capability. The target must not have a `required` dependency on the
    ///     capability. The ultimate source of this offer must be `void` or an actual component.
    /// - `same_as_target`: the availability expectations of this capability will match the
    ///     target's. If the target requires the capability, then this field is set to `required`.
    ///     If the target has an optional dependency on the capability, then the field is set to
    ///     `optional`.
    /// - `transitional`: like `optional`, but will tolerate a missing source. Use this
    ///     only to avoid validation errors during transitional periods of multi-step code changes.
    ///
    /// For more information, see the
    /// [availability](/docs/concepts/components/v2/capabilities/availability.md) documentation.
    pub availability: Option<Availability>,

    /// Whether or not the source of this offer must exist. One of:
    /// - `required` (default): the source (`from`) must be defined in this manifest.
    /// - `unknown`: the source of this offer will be rewritten to `void` if its source (`from`)
    ///     is not defined in this manifest after includes are processed.
    pub source_availability: Option<SourceAvailability>,

    /// Whether or not the target of this offer must exist. One of:
    /// - `required` (default): the target (`to`) must be defined in this
    ///   manifest.
    /// - `unknown`: this offer is omitted if its target (`to`) is not defined
    ///     in this manifest after includes are processed.
    pub target_availability: Option<TargetAvailability>,
}

impl SpannedCapabilityClause for SpannedOffer {
    fn service(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.service)
    }
    fn protocol(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_spanned_one_or_many_as_ref(&self.protocol)
    }
    fn directory(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.directory)
    }
    fn storage(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_spanned_one_or_many_as_ref(&self.storage)
    }
    fn runner(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.runner)
    }
    fn resolver(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.resolver)
    }
    fn event_stream(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_spanned_one_or_many_as_ref(&self.event_stream)
    }
    fn dictionary(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_spanned_one_or_many_as_ref(&self.dictionary)
    }
    fn config(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.config)
    }

    fn decl_type(&self) -> &'static str {
        "offer"
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
            "config",
        ]
    }
}

impl AsClause for SpannedOffer {
    fn r#as(&self) -> Option<&BorrowedName> {
        self.r#as.as_ref().map(|spanned_value| {
            let bounded_name: &BoundedName<255> = spanned_value.as_ref();
            let borrowed_name: &BorrowedName = bounded_name.as_ref();
            borrowed_name
        })
    }
}

/// A reference in an `offer from`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(
    expected = "\"parent\", \"framework\", \"self\", \"void\", \"#<child-name>\", or a dictionary path"
)]
pub enum OfferFromRef {
    /// A reference to a child or collection.
    Named(Name),
    /// A reference to the parent.
    Parent,
    /// A reference to the framework.
    Framework,
    /// A reference to this component.
    Self_,
    /// An intentionally omitted source.
    Void,
    /// A reference to a dictionary.
    Dictionary(DictionaryRef),
}

impl OfferFromRef {
    pub fn is_named(&self) -> bool {
        match self {
            OfferFromRef::Named(_) => true,
            _ => false,
        }
    }
}

/// A reference in an `offer to`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"#<child-name>\", \"#<collection-name>\", or \"self/<dictionary>\"")]
pub enum OfferToRef {
    /// A reference to a child or collection.
    Named(Name),

    /// Syntax sugar that results in the offer decl applying to all children and collections
    All,

    /// A reference to a dictionary defined by this component, the form "self/<dictionary>".
    OwnDictionary(Name),
}

/// Generates deserializer for `OneOrMany<OfferToRef>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "one or an array of \"#<child-name>\", \"#<collection-name>\", or \"self/<dictionary>\", with unique elements",
    inner_type = "OfferToRef",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyOfferToRefs;

/// Generates deserializer for `OneOrMany<OfferFromRef>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "one or an array of \"parent\", \"framework\", \"self\", \"#<child-name>\", \"#<collection-name>\", or a dictionary path",
    inner_type = "OfferFromRef",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyOfferFromRefs;
