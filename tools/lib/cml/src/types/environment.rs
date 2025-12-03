// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AnyRef, AsClause, CapabilityClause, Error, FromClause, OfferFromRef, PathClause,
    option_one_or_many_as_ref,
};

use crate::one_or_many::OneOrMany;
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use cml_macro::Reference;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize, de};

use std::fmt;

/// Example:
///
/// ```json5
/// environments: [
///     {
///         name: "test-env",
///         extends: "realm",
///         runners: [
///             {
///                 runner: "gtest-runner",
///                 from: "#gtest",
///             },
///         ],
///         resolvers: [
///             {
///                 resolver: "full-resolver",
///                 from: "parent",
///                 scheme: "fuchsia-pkg",
///             },
///         ],
///     },
/// ],
/// ```
#[derive(Deserialize, Debug, PartialEq, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list", top_level_doc_after_fields)]
pub struct Environment {
    /// The name of the environment, which is a string of one or more of the
    /// following characters: `a-z`, `0-9`, `_`, `.`, `-`. The name identifies this
    /// environment when used in a [reference](#references).
    pub name: Name,

    /// How the environment should extend this realm's environment.
    /// - `realm`: Inherit all properties from this component's environment.
    /// - `none`: Start with an empty environment, do not inherit anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<EnvironmentExtends>,

    /// The runners registered in the environment. An array of objects
    /// with the following properties:
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runners: Option<Vec<RunnerRegistration>>,

    /// The resolvers registered in the environment. An array of
    /// objects with the following properties:
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolvers: Option<Vec<ResolverRegistration>>,

    /// Debug protocols available to any component in this environment acquired
    /// through `use from debug`.
    #[reference_doc(recurse)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<Vec<DebugRegistration>>,

    /// The number of milliseconds to wait, after notifying a component in this environment that it
    /// should terminate, before forcibly killing it. This field is required if the environment
    /// extends from `none`.
    #[serde(rename = "__stop_timeout_ms")]
    #[reference_doc(json_type = "number", rename = "__stop_timeout_ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_timeout_ms: Option<StopTimeoutMs>,
}

impl Environment {
    pub fn merge_from(&mut self, other: &mut Self) -> Result<(), Error> {
        if self.extends.is_none() {
            self.extends = other.extends.take();
        } else if other.extends.is_some() && other.extends != self.extends {
            return Err(Error::validate(
                "cannot merge `environments` that declare conflicting `extends`",
            ));
        }

        if self.stop_timeout_ms.is_none() {
            self.stop_timeout_ms = other.stop_timeout_ms;
        } else if other.stop_timeout_ms.is_some() && other.stop_timeout_ms != self.stop_timeout_ms {
            return Err(Error::validate(
                "cannot merge `environments` that declare conflicting `stop_timeout_ms`",
            ));
        }

        // Perform naive vector concatenation and rely on later validation to ensure
        // no conflicting entries.
        match &mut self.runners {
            Some(r) => {
                if let Some(o) = &mut other.runners {
                    r.append(o);
                }
            }
            None => self.runners = other.runners.take(),
        }

        match &mut self.resolvers {
            Some(r) => {
                if let Some(o) = &mut other.resolvers {
                    r.append(o);
                }
            }
            None => self.resolvers = other.resolvers.take(),
        }

        match &mut self.debug {
            Some(r) => {
                if let Some(o) = &mut other.debug {
                    r.append(o);
                }
            }
            None => self.debug = other.debug.take(),
        }
        Ok(())
    }
}

/// A reference in an environment.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"#<environment-name>\"")]
pub enum EnvironmentRef {
    /// A reference to an environment defined in this component.
    Named(Name),
}

#[derive(Deserialize, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentExtends {
    Realm,
    None,
}

/// The stop timeout configured in an environment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StopTimeoutMs(pub u32);

impl<'de> de::Deserialize<'de> for StopTimeoutMs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = StopTimeoutMs;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an unsigned 32-bit integer")
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v < 0 || v > i64::from(u32::max_value()) {
                    return Err(E::invalid_value(
                        de::Unexpected::Signed(v),
                        &"an unsigned 32-bit integer",
                    ));
                }
                Ok(StopTimeoutMs(v as u32))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_i64(value as i64)
            }
        }

        deserializer.deserialize_i64(Visitor)
    }
}

/// A reference in an environment registration.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"parent\", \"self\", or \"#<child-name>\"")]
pub enum RegistrationRef {
    /// A reference to a child.
    Named(Name),
    /// A reference to the parent.
    Parent,
    /// A reference to this component.
    Self_,
}

#[derive(Deserialize, Debug, PartialEq, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list")]
pub struct RunnerRegistration {
    /// The [name](#name) of a runner capability, whose source is specified in `from`.
    pub runner: Name,

    /// The source of the runner capability, one of:
    /// - `parent`: The component's parent.
    /// - `self`: This component.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance.
    pub from: RegistrationRef,

    /// An explicit name for the runner as it will be known in
    /// this environment. If omitted, defaults to `runner`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#as: Option<Name>,
}

#[derive(Deserialize, Debug, PartialEq, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list")]
pub struct ResolverRegistration {
    /// The [name](#name) of a resolver capability,
    /// whose source is specified in `from`.
    pub resolver: Name,

    /// The source of the resolver capability, one of:
    /// - `parent`: The component's parent.
    /// - `self`: This component.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance.
    pub from: RegistrationRef,

    /// The URL scheme for which the resolver should handle
    /// resolution.
    pub scheme: cm_types::UrlScheme,
}

impl FromClause for RunnerRegistration {
    fn from_(&self) -> OneOrMany<AnyRef<'_>> {
        OneOrMany::One(AnyRef::from(&self.from))
    }
}

impl FromClause for ResolverRegistration {
    fn from_(&self) -> OneOrMany<AnyRef<'_>> {
        OneOrMany::One(AnyRef::from(&self.from))
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list")]
pub struct DebugRegistration {
    /// The name(s) of the protocol(s) to make available.
    pub protocol: Option<OneOrMany<Name>>,

    /// The source of the capability(s), one of:
    /// - `parent`: The component's parent.
    /// - `self`: This component.
    /// - `#<child-name>`: A [reference](#references) to a child component
    ///     instance.
    pub from: OfferFromRef,

    /// If specified, the name that the capability in `protocol` should be made
    /// available as to clients. Disallowed if `protocol` is an array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#as: Option<Name>,
}

impl AsClause for DebugRegistration {
    fn r#as(&self) -> Option<&BorrowedName> {
        self.r#as.as_ref().map(Name::as_ref)
    }
}

impl PathClause for DebugRegistration {
    fn path(&self) -> Option<&Path> {
        None
    }
}

impl FromClause for DebugRegistration {
    fn from_(&self) -> OneOrMany<AnyRef<'_>> {
        OneOrMany::One(AnyRef::from(&self.from))
    }
}

impl CapabilityClause for DebugRegistration {
    fn service(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn protocol(&self) -> Option<OneOrMany<&BorrowedName>> {
        option_one_or_many_as_ref(&self.protocol)
    }
    fn directory(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn storage(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn runner(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn resolver(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn event_stream(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn dictionary(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }
    fn config(&self) -> Option<OneOrMany<&BorrowedName>> {
        None
    }

    fn set_service(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_protocol(&mut self, o: Option<OneOrMany<Name>>) {
        self.protocol = o;
    }
    fn set_directory(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_storage(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_runner(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_resolver(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_event_stream(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_dictionary(&mut self, _o: Option<OneOrMany<Name>>) {}
    fn set_config(&mut self, _o: Option<OneOrMany<Name>>) {}

    fn availability(&self) -> Option<Availability> {
        None
    }
    fn set_availability(&mut self, _a: Option<Availability>) {}

    fn decl_type(&self) -> &'static str {
        "debug"
    }
    fn supported(&self) -> &[&'static str] {
        &["service", "protocol"]
    }
    fn are_many_names_allowed(&self) -> bool {
        ["protocol"].contains(&self.capability_type().unwrap())
    }
}
