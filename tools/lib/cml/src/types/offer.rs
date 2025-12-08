// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::types::common::*;
use crate::{
    AnyRef, AsClause, AsClauseContext, Canonicalize, CapabilityClause, CapabilityId, DictionaryRef,
    Error, EventScope, FromClause, FromClauseContext, PathClause, SourceAvailability,
    one_or_many_from_context, one_or_many_from_impl, option_one_or_many_as_ref,
};

use crate::one_or_many::OneOrMany;
use crate::types::right::{Rights, RightsClause};
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DependencyType, HandleType, Name, OnTerminate,
    ParseError, Path, RelativePath, StartupMode, Url,
};
use cml_macro::{OneOrMany, Reference};
use itertools::Either;
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};

use std::fmt;
use std::fmt::Write;
use std::path::PathBuf;
#[allow(unused)] // A test-only macro is defined outside of a test builds.
use std::str::FromStr;
use std::sync::Arc;

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

#[derive(PartialEq, Clone)]
pub enum OfferToAllCapability<'a> {
    Dictionary(&'a str),
    Protocol(&'a str),
}

impl<'a> OfferToAllCapability<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            OfferToAllCapability::Dictionary(name) => name,
            OfferToAllCapability::Protocol(name) => name,
        }
    }

    pub fn offer_type(&self) -> &'static str {
        match self {
            OfferToAllCapability::Dictionary(_) => "Dictionary",
            OfferToAllCapability::Protocol(_) => "Protocol",
        }
    }

    pub fn offer_type_plural(&self) -> &'static str {
        match self {
            OfferToAllCapability::Dictionary(_) => "dictionaries",
            OfferToAllCapability::Protocol(_) => "protocols",
        }
    }
}

pub fn offer_to_all_from_offer(value: &Offer) -> impl Iterator<Item = OfferToAllCapability<'_>> {
    if let Some(protocol) = &value.protocol {
        Either::Left(
            protocol.iter().map(|protocol| OfferToAllCapability::Protocol(protocol.as_str())),
        )
    } else if let Some(dictionary) = &value.dictionary {
        Either::Right(
            dictionary
                .iter()
                .map(|dictionary| OfferToAllCapability::Dictionary(dictionary.as_str())),
        )
    } else {
        panic!("Expected a dictionary or a protocol");
    }
}
pub fn offer_to_all_and_component_diff_sources_message<'a>(
    capability: impl Iterator<Item = OfferToAllCapability<'a>>,
    component: &str,
) -> String {
    let mut output = String::new();
    let mut capability = capability.peekable();
    write!(&mut output, "{} ", capability.peek().unwrap().offer_type()).unwrap();
    for (i, capability) in capability.enumerate() {
        if i > 0 {
            write!(&mut output, ", ").unwrap();
        }
        write!(&mut output, "{}", capability.name()).unwrap();
    }
    write!(
        &mut output,
        r#" is offered to both "all" and child component "{}" with different sources"#,
        component
    )
    .unwrap();
    output
}

pub fn offer_to_all_and_component_diff_capabilities_message<'a>(
    capability: impl Iterator<Item = OfferToAllCapability<'a>>,
    component: &str,
) -> String {
    let mut output = String::new();
    let mut capability_peek = capability.peekable();

    // Clone is needed so the iterator can be moved forward.
    // This doesn't actually allocate memory or copy a string, as only the reference
    // held by the OfferToAllCapability<'a> is copied.
    let first_offer_to_all = capability_peek.peek().unwrap().clone();
    write!(&mut output, "{} ", first_offer_to_all.offer_type()).unwrap();
    for (i, capability) in capability_peek.enumerate() {
        if i > 0 {
            write!(&mut output, ", ").unwrap();
        }
        write!(&mut output, "{}", capability.name()).unwrap();
    }
    write!(&mut output, r#" is aliased to "{}" with the same name as an offer to "all", but from different source {}"#, component, first_offer_to_all.offer_type_plural()).unwrap();
    output
}

/// Returns `Ok(true)` if desugaring the `offer_to_all` using `name` duplicates
/// `specific_offer`. Returns `Ok(false)` if not a duplicate.
///
/// Returns Err if there is a validation error.
pub fn offer_to_all_would_duplicate(
    offer_to_all: &Offer,
    specific_offer: &Offer,
    target: &cm_types::BorrowedName,
) -> Result<bool, Error> {
    // Only protocols and dictionaries may be offered to all
    assert!(offer_to_all.protocol.is_some() || offer_to_all.dictionary.is_some());

    // If none of the pairs of the cross products of the two offer's protocols
    // match, then the offer is certainly not a duplicate
    if CapabilityId::from_offer_expose(specific_offer).iter().flatten().all(
        |specific_offer_cap_id| {
            CapabilityId::from_offer_expose(offer_to_all)
                .iter()
                .flatten()
                .all(|offer_to_all_cap_id| offer_to_all_cap_id != specific_offer_cap_id)
        },
    ) {
        return Ok(false);
    }

    let to_field_matches = specific_offer.to.iter().any(
        |specific_offer_to| matches!(specific_offer_to, OfferToRef::Named(c) if **c == *target),
    );

    if !to_field_matches {
        return Ok(false);
    }

    if offer_to_all.from != specific_offer.from {
        return Err(Error::validate(offer_to_all_and_component_diff_sources_message(
            offer_to_all_from_offer(offer_to_all),
            target.as_str(),
        )));
    }

    // Since the capability ID's match, the underlying protocol must also match
    if offer_to_all_from_offer(offer_to_all).all(|to_all_protocol| {
        offer_to_all_from_offer(specific_offer)
            .all(|to_specific_protocol| to_all_protocol != to_specific_protocol)
    }) {
        return Err(Error::validate(offer_to_all_and_component_diff_capabilities_message(
            offer_to_all_from_offer(offer_to_all),
            target.as_str(),
        )));
    }

    Ok(true)
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

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ParsedOffer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Spanned<OneOrMany<Name>>>,
    pub protocol: Option<Spanned<OneOrMany<Name>>>,
    pub directory: Option<Spanned<OneOrMany<Name>>>,
    pub runner: Option<Spanned<OneOrMany<Name>>>,
    pub resolver: Option<Spanned<OneOrMany<Name>>>,
    pub storage: Option<Spanned<OneOrMany<Name>>>,
    pub dictionary: Option<Spanned<OneOrMany<Name>>>,
    pub config: Option<Spanned<OneOrMany<Name>>>,
    pub from: Spanned<OneOrMany<OfferFromRef>>,
    pub to: Spanned<OneOrMany<OfferToRef>>,
    pub r#as: Option<Spanned<Name>>,
    pub dependency: Option<Spanned<DependencyType>>,
    pub rights: Option<Spanned<Rights>>,
    pub subdir: Option<Spanned<RelativePath>>,
    pub event_stream: Option<Spanned<OneOrMany<Name>>>,
    pub scope: Option<Spanned<OneOrMany<EventScope>>>,
    pub availability: Option<Spanned<Availability>>,
    pub source_availability: Option<Spanned<SourceAvailability>>,
}

#[derive(Debug, Clone)]
pub struct ContextOffer {
    pub service: Option<ContextSpanned<OneOrMany<Name>>>,
    pub protocol: Option<ContextSpanned<OneOrMany<Name>>>,
    pub directory: Option<ContextSpanned<OneOrMany<Name>>>,
    pub runner: Option<ContextSpanned<OneOrMany<Name>>>,
    pub resolver: Option<ContextSpanned<OneOrMany<Name>>>,
    pub storage: Option<ContextSpanned<OneOrMany<Name>>>,
    pub dictionary: Option<ContextSpanned<OneOrMany<Name>>>,
    pub config: Option<ContextSpanned<OneOrMany<Name>>>,
    pub from: ContextSpanned<OneOrMany<OfferFromRef>>,
    pub to: ContextSpanned<OneOrMany<OfferToRef>>,
    pub r#as: Option<ContextSpanned<Name>>,
    pub dependency: Option<ContextSpanned<DependencyType>>,
    pub rights: Option<ContextSpanned<Rights>>,
    pub subdir: Option<ContextSpanned<RelativePath>>,
    pub event_stream: Option<ContextSpanned<OneOrMany<Name>>>,
    pub scope: Option<ContextSpanned<OneOrMany<EventScope>>>,
    pub availability: Option<ContextSpanned<Availability>>,
    pub source_availability: Option<ContextSpanned<SourceAvailability>>,
}

impl ContextCapabilityClause for ContextOffer {
    fn service(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.service)
    }
    fn protocol(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.protocol)
    }
    fn directory(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.directory)
    }
    fn storage(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.storage)
    }
    fn runner(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.runner)
    }
    fn resolver(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.resolver)
    }
    fn event_stream(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.event_stream)
    }
    fn dictionary(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.dictionary)
    }
    fn config(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>> {
        option_one_or_many_as_ref_context(&self.config)
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
            "event_stream",
            "runner",
            "resolver",
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
        .contains(&self.capability_type(None).unwrap())
    }
}

impl PartialEq for ContextOffer {
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
            && cmp!(runner)
            && cmp!(resolver)
            && cmp!(storage)
            && cmp!(dictionary)
            && cmp!(config)
            && self.from.value == other.from.value
            && self.to.value == other.to.value
            && cmp!(r#as)
            && cmp!(dependency)
            && cmp!(rights)
            && cmp!(subdir)
            && cmp!(event_stream)
            && cmp!(scope)
            && cmp!(availability)
            && cmp!(source_availability)
    }
}

impl Eq for ContextOffer {}

impl ContextPathClause for ContextOffer {
    fn path(&self) -> Option<&ContextSpanned<Path>> {
        None
    }
}

impl AsClauseContext for ContextOffer {
    fn r#as(&self) -> Option<ContextSpanned<&BorrowedName>> {
        self.r#as.as_ref().map(|spanned_name| ContextSpanned {
            value: spanned_name.value.as_ref(),
            origin: spanned_name.origin.clone(),
        })
    }
}

impl FromClauseContext for ContextOffer {
    fn from_(&self) -> ContextSpanned<OneOrMany<AnyRef<'_>>> {
        one_or_many_from_context(&self.from)
    }
}

impl Hydrate for ParsedOffer {
    type Output = ContextOffer;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Self::Output {
        ContextOffer {
            service: hydrate_opt_simple(self.service, file, buffer),
            protocol: hydrate_opt_simple(self.protocol, file, buffer),
            directory: hydrate_opt_simple(self.directory, file, buffer),
            runner: hydrate_opt_simple(self.runner, file, buffer),
            resolver: hydrate_opt_simple(self.resolver, file, buffer),
            storage: hydrate_opt_simple(self.storage, file, buffer),
            dictionary: hydrate_opt_simple(self.dictionary, file, buffer),
            config: hydrate_opt_simple(self.config, file, buffer),
            from: hydrate_simple(self.from, file, buffer),
            to: hydrate_simple(self.to, file, buffer),
            r#as: hydrate_opt_simple(self.r#as, file, buffer),
            dependency: hydrate_opt_simple(self.dependency, file, buffer),
            rights: hydrate_opt_simple(self.rights, file, buffer),
            subdir: hydrate_opt_simple(self.subdir, file, buffer),
            event_stream: hydrate_opt_simple(self.event_stream, file, buffer),
            scope: hydrate_opt_simple(self.scope, file, buffer),
            availability: hydrate_opt_simple(self.availability, file, buffer),
            source_availability: hydrate_opt_simple(self.source_availability, file, buffer),
        }
    }
}

pub fn offer_to_all_from_context_offer(
    value: &ContextOffer,
) -> impl Iterator<Item = OfferToAllCapability<'_>> {
    if let Some(protocol) = &value.protocol {
        Either::Left(
            protocol.value.iter().map(|protocol| OfferToAllCapability::Protocol(protocol.as_str())),
        )
    } else if let Some(dictionary) = &value.dictionary {
        Either::Right(
            dictionary
                .value
                .iter()
                .map(|dictionary| OfferToAllCapability::Dictionary(dictionary.as_str())),
        )
    } else {
        panic!("Expected a dictionary or a protocol");
    }
}

/// Returns `Ok(true)` if desugaring the `offer_to_all` using `name` duplicates
/// `specific_offer`. Returns `Ok(false)` if not a duplicate.
///
/// Returns Err if there is a validation error.
pub fn offer_to_all_would_duplicate_context(
    offer_to_all: &ContextSpanned<ContextOffer>,
    specific_offer: &ContextSpanned<ContextOffer>,
    target: &cm_types::BorrowedName,
) -> Result<bool, Error> {
    // Only protocols and dictionaries may be offered to all
    assert!(offer_to_all.value.protocol.is_some() || offer_to_all.value.dictionary.is_some());

    // If none of the pairs of the cross products of the two offer's protocols
    // match, then the offer is certainly not a duplicate
    if CapabilityId::from_context_offer(specific_offer).iter().flatten().all(
        |specific_offer_cap_id| {
            CapabilityId::from_context_offer(offer_to_all)
                .iter()
                .flatten()
                .all(|offer_to_all_cap_id| offer_to_all_cap_id.0 != specific_offer_cap_id.0)
        },
    ) {
        return Ok(false);
    }

    let to_field_matches = specific_offer.value.to.value.iter().any(
        |specific_offer_to| matches!(specific_offer_to, OfferToRef::Named(c) if **c == *target),
    );

    if !to_field_matches {
        return Ok(false);
    }

    if offer_to_all.value.from != specific_offer.value.from {
        return Err(Error::validate_contexts(
            offer_to_all_and_component_diff_sources_message(
                offer_to_all_from_context_offer(&offer_to_all.value),
                target.as_str(),
            ),
            vec![offer_to_all.origin.clone(), specific_offer.origin.clone()],
        ));
    }

    // Since the capability ID's match, the underlying protocol must also match
    if offer_to_all_from_context_offer(&offer_to_all.value).all(|to_all_protocol| {
        offer_to_all_from_context_offer(&specific_offer.value)
            .all(|to_specific_protocol| to_all_protocol != to_specific_protocol)
    }) {
        return Err(Error::validate_contexts(
            offer_to_all_and_component_diff_capabilities_message(
                offer_to_all_from_context_offer(&offer_to_all.value),
                target.as_str(),
            ),
            vec![offer_to_all.origin.clone(), specific_offer.origin.clone()],
        ));
    }

    Ok(true)
}

#[cfg(test)]
pub fn create_offer(
    protocol_name: &str,
    from: OneOrMany<OfferFromRef>,
    to: OneOrMany<OfferToRef>,
) -> Offer {
    Offer {
        protocol: Some(OneOrMany::One(Name::from_str(protocol_name).unwrap())),
        ..Offer::empty(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offer_would_duplicate() {
        let offer = create_offer(
            "fuchsia.logger.LegacyLog",
            OneOrMany::One(OfferFromRef::Parent {}),
            OneOrMany::One(OfferToRef::Named(Name::from_str("something").unwrap())),
        );

        let offer_to_all = create_offer(
            "fuchsia.logger.LogSink",
            OneOrMany::One(OfferFromRef::Parent {}),
            OneOrMany::One(OfferToRef::All),
        );

        // different protocols
        assert!(
            !offer_to_all_would_duplicate(
                &offer_to_all,
                &offer,
                &Name::from_str("something").unwrap()
            )
            .unwrap()
        );

        let offer = create_offer(
            "fuchsia.logger.LogSink",
            OneOrMany::One(OfferFromRef::Parent {}),
            OneOrMany::One(OfferToRef::Named(Name::from_str("not-something").unwrap())),
        );

        // different targets
        assert!(
            !offer_to_all_would_duplicate(
                &offer_to_all,
                &offer,
                &Name::from_str("something").unwrap()
            )
            .unwrap()
        );

        let mut offer = create_offer(
            "fuchsia.logger.LogSink",
            OneOrMany::One(OfferFromRef::Parent {}),
            OneOrMany::One(OfferToRef::Named(Name::from_str("something").unwrap())),
        );

        offer.r#as = Some(Name::from_str("FakeLog").unwrap());

        // target has alias
        assert!(
            !offer_to_all_would_duplicate(
                &offer_to_all,
                &offer,
                &Name::from_str("something").unwrap()
            )
            .unwrap()
        );

        let offer = create_offer(
            "fuchsia.logger.LogSink",
            OneOrMany::One(OfferFromRef::Parent {}),
            OneOrMany::One(OfferToRef::Named(Name::from_str("something").unwrap())),
        );

        assert!(
            offer_to_all_would_duplicate(
                &offer_to_all,
                &offer,
                &Name::from_str("something").unwrap()
            )
            .unwrap()
        );

        let offer = create_offer(
            "fuchsia.logger.LogSink",
            OneOrMany::One(OfferFromRef::Named(Name::from_str("other").unwrap())),
            OneOrMany::One(OfferToRef::Named(Name::from_str("something").unwrap())),
        );

        assert!(
            offer_to_all_would_duplicate(
                &offer_to_all,
                &offer,
                &Name::from_str("something").unwrap()
            )
            .is_err()
        );
    }
}
