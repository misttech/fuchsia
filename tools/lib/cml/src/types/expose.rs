// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::types::common::*;
use crate::types::right::{Rights, RightsClause};
use crate::{
    AnyRef, AsClause, AsClauseContext, Canonicalize, CapabilityClause, DictionaryRef, EventScope,
    FilterClause, FromClause, FromClauseContext, PathClause, SourceAvailability,
    one_or_many_from_context, one_or_many_from_impl, option_one_or_many_as_ref,
};

use crate::one_or_many::OneOrMany;
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DependencyType, HandleType, Name, OnTerminate,
    ParseError, Path, RelativePath, StartupMode, Url,
};
use cml_macro::{OneOrMany, Reference};
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Example:
///
/// ```json5
/// expose: [
///     {
///         directory: "themes",
///         from: "self",
///     },
///     {
///         protocol: "pkg.Cache",
///         from: "#pkg_cache",
///         as: "fuchsia.pkg.PackageCache",
///     },
///     {
///         protocol: [
///             "fuchsia.ui.app.ViewProvider",
///             "fuchsia.fonts.Provider",
///         ],
///         from: "self",
///     },
///     {
///         runner: "web-chromium",
///         from: "#web_runner",
///         as: "web",
///     },
///     {
///         resolver: "full-resolver",
///         from: "#full-resolver",
///     },
/// ],
/// ```
#[derive(Deserialize, Debug, PartialEq, Clone, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list", top_level_doc_after_fields)]
pub struct Expose {
    /// When routing a service, the [name](#name) of a [service capability][doc-service].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub service: Option<OneOrMany<Name>>,

    /// When routing a protocol, the [name](#name) of a [protocol capability][doc-protocol].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub protocol: Option<OneOrMany<Name>>,

    /// When routing a directory, the [name](#name) of a [directory capability][doc-directory].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub directory: Option<OneOrMany<Name>>,

    /// When routing a runner, the [name](#name) of a [runner capability][doc-runners].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub runner: Option<OneOrMany<Name>>,

    /// When routing a resolver, the [name](#name) of a [resolver capability][doc-resolvers].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub resolver: Option<OneOrMany<Name>>,

    /// When routing a dictionary, the [name](#name) of a [dictionary capability][doc-dictionaries].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub dictionary: Option<OneOrMany<Name>>,

    /// When routing a config, the [name](#name) of a configuration capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(skip = true)]
    pub config: Option<OneOrMany<Name>>,

    /// `from`: The source of the capability, one of:
    /// - `self`: This component. Requires a corresponding
    ///     [`capability`](#capabilities) declaration.
    /// - `framework`: The Component Framework runtime.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance.
    pub from: OneOrMany<ExposeFromRef>,

    /// The [name](#name) for the capability as it will be known by the target. If omitted,
    /// defaults to the original name. `as` cannot be used when an array of multiple capability
    /// names is provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#as: Option<Name>,

    /// The capability target. Either `parent` or `framework`. Defaults to `parent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<ExposeToRef>,

    /// (`directory` only) the maximum [directory rights][doc-directory-rights] to apply to
    /// the exposed directory capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[reference_doc(json_type = "array of string")]
    pub rights: Option<Rights>,

    /// (`directory` only) the relative path of a subdirectory within the source directory
    /// capability to route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<RelativePath>,

    /// (`event_stream` only) the name(s) of the event streams being exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_stream: Option<OneOrMany<Name>>,

    /// (`event_stream` only) the scope(s) of the event streams being exposed. This is used to
    /// downscope the range of components to which an event stream refers and make it refer only to
    /// the components defined in the scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<OneOrMany<EventScope>>,

    /// `availability` _(optional)_: The expectations around this capability's availability. Affects
    /// build-time and runtime route validation. One of:
    /// - `required` (default): a required dependency, the source must exist and provide it. Use
    ///     this when the target of this expose requires this capability to function properly.
    /// - `optional`: an optional dependency. Use this when the target of the expose can function
    ///     with or without this capability. The target must not have a `required` dependency on the
    ///     capability. The ultimate source of this expose must be `void` or an actual component.
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
}

impl Expose {
    pub fn new_from(from: OneOrMany<ExposeFromRef>) -> Self {
        Self {
            from,
            service: None,
            protocol: None,
            directory: None,
            config: None,
            runner: None,
            resolver: None,
            dictionary: None,
            r#as: None,
            to: None,
            rights: None,
            subdir: None,
            event_stream: None,
            scope: None,
            availability: None,
            source_availability: None,
        }
    }
}

impl FromClause for Expose {
    fn from_(&self) -> OneOrMany<AnyRef<'_>> {
        one_or_many_from_impl(&self.from)
    }
}

impl AsClause for Expose {
    fn r#as(&self) -> Option<&BorrowedName> {
        self.r#as.as_ref().map(Name::as_ref)
    }
}

impl PathClause for Expose {
    fn path(&self) -> Option<&Path> {
        None
    }
}

impl FilterClause for Expose {
    fn filter(&self) -> Option<&Map<String, Value>> {
        None
    }
}

impl RightsClause for Expose {
    fn rights(&self) -> Option<&Rights> {
        self.rights.as_ref()
    }
}

impl Canonicalize for Expose {
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
        } else if let Some(event_stream) = &mut self.event_stream {
            event_stream.canonicalize();
            if let Some(scope) = &mut self.scope {
                scope.canonicalize();
            }
        }
        // TODO(https://fxbug.dev/300500098): canonicalize dictionaries
    }
}

impl CapabilityClause for Expose {
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
        None
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
    fn set_storage(&mut self, _o: Option<OneOrMany<Name>>) {}
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
        self.config = o;
    }

    fn availability(&self) -> Option<Availability> {
        None
    }
    fn set_availability(&mut self, _a: Option<Availability>) {}

    fn decl_type(&self) -> &'static str {
        "expose"
    }
    fn supported(&self) -> &[&'static str] {
        &[
            "service",
            "protocol",
            "directory",
            "runner",
            "resolver",
            "event_stream",
            "dictionary",
            "config",
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
            "dictionary",
            "config",
        ]
        .contains(&self.capability_type().unwrap())
    }
}

/// Generates deserializer for `OneOrMany<ExposeFromRef>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "one or an array of \"framework\", \"self\", \"#<child-name>\", or a dictionary path",
    inner_type = "ExposeFromRef",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyExposeFromRefs;

/// A reference in an `expose from`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"framework\", \"self\", \"void\", or \"#<child-name>\"")]
pub enum ExposeFromRef {
    /// A reference to a child or collection.
    Named(Name),
    /// A reference to the framework.
    Framework,
    /// A reference to this component.
    Self_,
    /// An intentionally omitted source.
    Void,
    /// A reference to a dictionary.
    Dictionary(DictionaryRef),
}

/// A reference in an `expose to`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"parent\", \"framework\", or none")]
pub enum ExposeToRef {
    /// A reference to the parent.
    Parent,
    /// A reference to the framework.
    Framework,
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ParsedExpose {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Spanned<OneOrMany<Name>>>,
    pub protocol: Option<Spanned<OneOrMany<Name>>>,
    pub directory: Option<Spanned<OneOrMany<Name>>>,
    pub runner: Option<Spanned<OneOrMany<Name>>>,
    pub resolver: Option<Spanned<OneOrMany<Name>>>,
    pub dictionary: Option<Spanned<OneOrMany<Name>>>,
    pub config: Option<Spanned<OneOrMany<Name>>>,
    pub from: Spanned<OneOrMany<ExposeFromRef>>,
    pub to: Option<Spanned<ExposeToRef>>,
    pub r#as: Option<Spanned<Name>>,
    pub rights: Option<Spanned<Rights>>,
    pub subdir: Option<Spanned<RelativePath>>,
    pub event_stream: Option<Spanned<OneOrMany<Name>>>,
    pub scope: Option<Spanned<OneOrMany<EventScope>>>,
    pub availability: Option<Spanned<Availability>>,
    pub source_availability: Option<Spanned<SourceAvailability>>,
}

#[derive(Debug, Clone)]
pub struct ContextExpose {
    pub service: Option<ContextSpanned<OneOrMany<Name>>>,
    pub protocol: Option<ContextSpanned<OneOrMany<Name>>>,
    pub directory: Option<ContextSpanned<OneOrMany<Name>>>,
    pub runner: Option<ContextSpanned<OneOrMany<Name>>>,
    pub resolver: Option<ContextSpanned<OneOrMany<Name>>>,
    pub dictionary: Option<ContextSpanned<OneOrMany<Name>>>,
    pub config: Option<ContextSpanned<OneOrMany<Name>>>,
    pub from: ContextSpanned<OneOrMany<ExposeFromRef>>,
    pub to: Option<ContextSpanned<ExposeToRef>>,
    pub r#as: Option<ContextSpanned<Name>>,
    pub rights: Option<ContextSpanned<Rights>>,
    pub subdir: Option<ContextSpanned<RelativePath>>,
    pub event_stream: Option<ContextSpanned<OneOrMany<Name>>>,
    pub scope: Option<ContextSpanned<OneOrMany<EventScope>>>,
    pub availability: Option<ContextSpanned<Availability>>,
    pub source_availability: Option<ContextSpanned<SourceAvailability>>,
}

impl ContextCapabilityClause for ContextExpose {
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
        None
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
        "expose"
    }
    fn supported(&self) -> &[&'static str] {
        &[
            "service",
            "protocol",
            "directory",
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

impl PartialEq for ContextExpose {
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
            && cmp!(dictionary)
            && cmp!(config)
            && self.from.value == other.from.value
            && cmp!(to)
            && cmp!(r#as)
            && cmp!(rights)
            && cmp!(subdir)
            && cmp!(event_stream)
            && cmp!(scope)
            && cmp!(availability)
            && cmp!(source_availability)
    }
}

impl Eq for ContextExpose {}

impl ContextPathClause for ContextExpose {
    fn path(&self) -> Option<&ContextSpanned<Path>> {
        None
    }
}

impl AsClauseContext for ContextExpose {
    fn r#as(&self) -> Option<ContextSpanned<&BorrowedName>> {
        self.r#as.as_ref().map(|spanned_name| ContextSpanned {
            value: spanned_name.value.as_ref(),
            origin: spanned_name.origin.clone(),
        })
    }
}

impl FromClauseContext for ContextExpose {
    fn from_(&self) -> ContextSpanned<OneOrMany<AnyRef<'_>>> {
        one_or_many_from_context(&self.from)
    }
}

impl Hydrate for ParsedExpose {
    type Output = ContextExpose;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Self::Output {
        ContextExpose {
            service: hydrate_opt_simple(self.service, file, buffer),
            protocol: hydrate_opt_simple(self.protocol, file, buffer),
            directory: hydrate_opt_simple(self.directory, file, buffer),
            runner: hydrate_opt_simple(self.runner, file, buffer),
            resolver: hydrate_opt_simple(self.resolver, file, buffer),
            dictionary: hydrate_opt_simple(self.dictionary, file, buffer),
            config: hydrate_opt_simple(self.config, file, buffer),
            from: hydrate_simple(self.from, file, buffer),
            to: hydrate_opt_simple(self.to, file, buffer),
            r#as: hydrate_opt_simple(self.r#as, file, buffer),
            rights: hydrate_opt_simple(self.rights, file, buffer),
            subdir: hydrate_opt_simple(self.subdir, file, buffer),
            event_stream: hydrate_opt_simple(self.event_stream, file, buffer),
            scope: hydrate_opt_simple(self.scope, file, buffer),
            availability: hydrate_opt_simple(self.availability, file, buffer),
            source_availability: hydrate_opt_simple(self.source_availability, file, buffer),
        }
    }
}
