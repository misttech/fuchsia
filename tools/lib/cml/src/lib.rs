// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A library of common utilities used by `cmc` and related tools.
//! To manually regenerate reference documentation from doc comments in
//! this file, see the instructions at:
//!
//!   tools/lib/reference_doc/macro/derive-reference-doc-tests/src/test_data/README.md

pub mod error;
pub mod features;
pub mod one_or_many;
pub mod types;
pub(crate) mod validate;

#[allow(unused)] // A test-only macro is defined outside of a test builds.
pub mod translate;

use crate::error::Error;
use cml_macro::{OneOrMany, Reference};
use json5format::{FormatOptions, PathOption};
use maplit::{hashmap, hashset};
use serde::{Deserialize, Serialize, de, ser};
use serde_json::{Map, Value};
use std::fmt;
use std::hash::Hash;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::Arc;

pub use crate::types::capability::{Capability, CapabilityFromRef, ContextCapability};
pub use crate::types::capability_id::CapabilityId;
pub use crate::types::child::Child;
pub use crate::types::collection::Collection;
use crate::types::common::{ContextCapabilityClause, ContextSpanned, Origin};
pub use crate::types::document::{
    Document, DocumentContext, ParsedDocument, convert_parsed_to_document,
};
pub use crate::types::environment::{Environment, ResolverRegistration};
pub use crate::types::expose::{ContextExpose, Expose};
pub use crate::types::offer::{
    Offer, OfferFromRef, OfferToAllCapability, OfferToRef, offer_to_all_from_offer,
};
pub use crate::types::program::Program;
pub use crate::types::r#use::{Use, UseFromRef};

pub use cm_types::{
    AllowedOffers, Availability, BorrowedName, BoundedName, DeliveryType, DependencyType,
    Durability, HandleType, Name, NamespacePath, OnTerminate, ParseError, Path, RelativePath,
    StartupMode, StorageId, Url,
};
use error::Location;

pub use crate::one_or_many::OneOrMany;
pub use crate::translate::{CompileOptions, compile};
pub use crate::validate::{CapabilityRequirements, MustUseRequirement};

/// Parses a string `buffer` into a [Document]. `file` is used for error reporting.
pub fn parse_one_document(buffer: &String, file: &std::path::Path) -> Result<Document, Error> {
    serde_json5::from_str(&buffer).map_err(|e| {
        let serde_json5::Error::Message { location, msg } = e;
        let location = location.map(|l| Location { line: l.line, column: l.column });
        Error::parse(msg, location, Some(file))
    })
}

pub fn load_cml_with_context(
    buffer: &String,
    file: &std::path::Path,
) -> Result<DocumentContext, Error> {
    let file_arc = Arc::new(file.to_path_buf());
    let parsed_doc: ParsedDocument = json_spanned_value::from_str(&buffer).map_err(|e| {
        let location = Location { line: e.line(), column: e.column() };
        Error::parse(e, Some(location), Some(file))
    })?;
    let doc = convert_parsed_to_document(parsed_doc, file_arc, buffer);
    Ok(doc)
}

/// Parses a string `buffer` into a vector of [Document]. `file` is used for error reporting.
/// Supports JSON encoded as an array of Document JSON objects.
pub fn parse_many_documents(
    buffer: &String,
    file: &std::path::Path,
) -> Result<Vec<Document>, Error> {
    let res: Result<Vec<Document>, _> = serde_json5::from_str(&buffer);
    match res {
        Err(_) => {
            let d = parse_one_document(buffer, file)?;
            Ok(vec![d])
        }
        Ok(docs) => Ok(docs),
    }
}

pub fn byte_index_to_location(source: &String, index: usize) -> Location {
    let mut line = 1usize;
    let mut column = 1usize;

    for (i, ch) in source.char_indices() {
        if i == index {
            break;
        }

        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    return Location { line, column };
}

/// Generates deserializer for `OneOrMany<Name>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "a name or nonempty array of names, with unique elements",
    inner_type = "Name",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyNames;

/// Generates deserializer for `OneOrMany<Path>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "a path or nonempty array of paths, with unique elements",
    inner_type = "Path",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyPaths;

/// Generates deserializer for `OneOrMany<EventScope>`.
#[derive(OneOrMany, Debug, Clone)]
#[one_or_many(
    expected = "one or an array of \"#<collection-name>\", or \"#<child-name>\"",
    inner_type = "EventScope",
    min_length = 1,
    unique_items = true
)]
pub struct OneOrManyEventScope;

/// A reference in an `offer to` or `exose to`.
#[derive(Debug, Deserialize, PartialEq, Eq, Hash, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAvailability {
    Required,
    Unknown,
}

impl Default for SourceAvailability {
    fn default() -> Self {
        Self::Required
    }
}

impl<T> Canonicalize for Vec<T>
where
    T: Canonicalize + CapabilityClause + PathClause,
{
    fn canonicalize(&mut self) {
        // Collapse like-entries into one. Like entries are those that are equal in all fields
        // but their capability names. Accomplish this by collecting all the names into a vector
        // keyed by an instance of T with its names removed.
        let mut to_merge: Vec<(T, Vec<Name>)> = vec![];
        let mut to_keep: Vec<T> = vec![];
        self.iter().for_each(|c| {
            // Any entry with a `path` set cannot be merged with another.
            if !c.are_many_names_allowed() || c.path().is_some() {
                to_keep.push(c.clone());
                return;
            }
            let mut names: Vec<Name> = c.names().into_iter().map(Into::into).collect();
            let mut copy: T = c.clone();
            copy.set_names(vec![Name::from_str("a").unwrap()]); // The name here is arbitrary.
            let r = to_merge.iter().position(|(t, _)| t == &copy);
            match r {
                Some(i) => to_merge[i].1.append(&mut names),
                None => to_merge.push((copy, names)),
            };
        });
        let mut merged = to_merge
            .into_iter()
            .map(|(mut t, names)| {
                t.set_names(names);
                t
            })
            .collect::<Vec<_>>();
        to_keep.append(&mut merged);
        *self = to_keep;

        self.iter_mut().for_each(|c| c.canonicalize());
        self.sort_by(|a, b| {
            // Sort by capability type, then by the name of the first entry for
            // that type.
            let a_type = a.capability_type().unwrap();
            let b_type = b.capability_type().unwrap();
            a_type.cmp(b_type).then_with(|| {
                let a_names = a.names();
                let b_names = b.names();
                let a_first_name = a_names.first().unwrap();
                let b_first_name = b_names.first().unwrap();
                a_first_name.cmp(b_first_name)
            })
        });
    }
}

/// A relative reference to another object. This is a generic type that can encode any supported
/// reference subtype. For named references, it holds a reference to the name instead of the name
/// itself.
///
/// Objects of this type are usually derived from conversions of context-specific reference
/// types that `#[derive(Reference)]`. This type makes it easy to write helper functions that operate on
/// generic references.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum AnyRef<'a> {
    /// A named reference. Parsed as `#name`.
    Named(&'a BorrowedName),
    /// A reference to the parent. Parsed as `parent`.
    Parent,
    /// A reference to the framework (component manager). Parsed as `framework`.
    Framework,
    /// A reference to the debug. Parsed as `debug`.
    Debug,
    /// A reference to this component. Parsed as `self`.
    Self_,
    /// An intentionally omitted reference.
    Void,
    /// A reference to a dictionary. Parsed as a dictionary path.
    Dictionary(&'a DictionaryRef),
    /// A reference to a dictionary defined by this component. Parsed as
    /// `self/<dictionary>`.
    OwnDictionary(&'a BorrowedName),
}

/// Format an `AnyRef` as a string.
impl fmt::Display for AnyRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => write!(f, "#{}", name),
            Self::Parent => write!(f, "parent"),
            Self::Framework => write!(f, "framework"),
            Self::Debug => write!(f, "debug"),
            Self::Self_ => write!(f, "self"),
            Self::Void => write!(f, "void"),
            Self::Dictionary(d) => write!(f, "{}", d),
            Self::OwnDictionary(name) => write!(f, "self/{}", name),
        }
    }
}

/// A reference to a (possibly nested) dictionary.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct DictionaryRef {
    /// Path to the dictionary relative to `root_dictionary`.
    pub path: RelativePath,
    pub root: RootDictionaryRef,
}

impl<'a> From<&'a DictionaryRef> for AnyRef<'a> {
    fn from(r: &'a DictionaryRef) -> Self {
        Self::Dictionary(r)
    }
}

impl FromStr for DictionaryRef {
    type Err = ParseError;

    fn from_str(path: &str) -> Result<Self, ParseError> {
        match path.find('/') {
            Some(n) => {
                let root = path[..n].parse().map_err(|_| ParseError::InvalidValue)?;
                let path = RelativePath::new(&path[n + 1..])?;
                Ok(Self { root, path })
            }
            None => Err(ParseError::InvalidValue),
        }
    }
}

impl fmt::Display for DictionaryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.root, self.path)
    }
}

impl ser::Serialize for DictionaryRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        format!("{}", self).serialize(serializer)
    }
}

const DICTIONARY_REF_EXPECT_STR: &str = "a path to a dictionary no more \
    than 4095 characters in length";

impl<'de> de::Deserialize<'de> for DictionaryRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = DictionaryRef;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(DICTIONARY_REF_EXPECT_STR)
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                s.parse().map_err(|err| match err {
                    ParseError::InvalidValue => {
                        E::invalid_value(de::Unexpected::Str(s), &DICTIONARY_REF_EXPECT_STR)
                    }
                    ParseError::TooLong | ParseError::Empty => {
                        E::invalid_length(s.len(), &DICTIONARY_REF_EXPECT_STR)
                    }
                    e => {
                        panic!("unexpected parse error: {:?}", e);
                    }
                })
            }
        }

        deserializer.deserialize_string(Visitor)
    }
}

/// A reference to a root dictionary.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference)]
#[reference(expected = "\"parent\", \"self\", \"#<child-name>\"")]
pub enum RootDictionaryRef {
    /// A reference to a child.
    Named(Name),
    /// A reference to the parent.
    Parent,
    /// A reference to this component.
    Self_,
}

/// The scope of an event.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Reference, Ord, PartialOrd)]
#[reference(expected = "\"#<collection-name>\", \"#<child-name>\", or none")]
pub enum EventScope {
    /// A reference to a child or a collection.
    Named(Name),
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigType {
    Bool,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Int8,
    Int16,
    Int32,
    Int64,
    String,
    Vector,
}

impl From<&cm_rust::ConfigValueType> for ConfigType {
    fn from(value: &cm_rust::ConfigValueType) -> Self {
        match value {
            cm_rust::ConfigValueType::Bool => ConfigType::Bool,
            cm_rust::ConfigValueType::Uint8 => ConfigType::Uint8,
            cm_rust::ConfigValueType::Int8 => ConfigType::Int8,
            cm_rust::ConfigValueType::Uint16 => ConfigType::Uint16,
            cm_rust::ConfigValueType::Int16 => ConfigType::Int16,
            cm_rust::ConfigValueType::Uint32 => ConfigType::Uint32,
            cm_rust::ConfigValueType::Int32 => ConfigType::Int32,
            cm_rust::ConfigValueType::Uint64 => ConfigType::Uint64,
            cm_rust::ConfigValueType::Int64 => ConfigType::Int64,
            cm_rust::ConfigValueType::String { .. } => ConfigType::String,
            cm_rust::ConfigValueType::Vector { .. } => ConfigType::Vector,
        }
    }
}

#[derive(Clone, Deserialize, Debug, PartialEq, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "lowercase")]
pub enum ConfigNestedValueType {
    Bool {},
    Uint8 {},
    Uint16 {},
    Uint32 {},
    Uint64 {},
    Int8 {},
    Int16 {},
    Int32 {},
    Int64 {},
    String { max_size: NonZeroU32 },
}

impl ConfigNestedValueType {
    /// Update the hasher by digesting the ConfigVectorElementType enum value
    pub fn update_digest(&self, hasher: &mut impl sha2::Digest) {
        let val = match self {
            ConfigNestedValueType::Bool {} => 0u8,
            ConfigNestedValueType::Uint8 {} => 1u8,
            ConfigNestedValueType::Uint16 {} => 2u8,
            ConfigNestedValueType::Uint32 {} => 3u8,
            ConfigNestedValueType::Uint64 {} => 4u8,
            ConfigNestedValueType::Int8 {} => 5u8,
            ConfigNestedValueType::Int16 {} => 6u8,
            ConfigNestedValueType::Int32 {} => 7u8,
            ConfigNestedValueType::Int64 {} => 8u8,
            ConfigNestedValueType::String { max_size } => {
                hasher.update(max_size.get().to_le_bytes());
                9u8
            }
        };
        hasher.update([val])
    }
}

impl From<ConfigNestedValueType> for cm_rust::ConfigNestedValueType {
    fn from(value: ConfigNestedValueType) -> Self {
        match value {
            ConfigNestedValueType::Bool {} => cm_rust::ConfigNestedValueType::Bool,
            ConfigNestedValueType::Uint8 {} => cm_rust::ConfigNestedValueType::Uint8,
            ConfigNestedValueType::Uint16 {} => cm_rust::ConfigNestedValueType::Uint16,
            ConfigNestedValueType::Uint32 {} => cm_rust::ConfigNestedValueType::Uint32,
            ConfigNestedValueType::Uint64 {} => cm_rust::ConfigNestedValueType::Uint64,
            ConfigNestedValueType::Int8 {} => cm_rust::ConfigNestedValueType::Int8,
            ConfigNestedValueType::Int16 {} => cm_rust::ConfigNestedValueType::Int16,
            ConfigNestedValueType::Int32 {} => cm_rust::ConfigNestedValueType::Int32,
            ConfigNestedValueType::Int64 {} => cm_rust::ConfigNestedValueType::Int64,
            ConfigNestedValueType::String { max_size } => {
                cm_rust::ConfigNestedValueType::String { max_size: max_size.into() }
            }
        }
    }
}

impl TryFrom<&cm_rust::ConfigNestedValueType> for ConfigNestedValueType {
    type Error = ();
    fn try_from(nested: &cm_rust::ConfigNestedValueType) -> Result<Self, ()> {
        Ok(match nested {
            cm_rust::ConfigNestedValueType::Bool => ConfigNestedValueType::Bool {},
            cm_rust::ConfigNestedValueType::Uint8 => ConfigNestedValueType::Uint8 {},
            cm_rust::ConfigNestedValueType::Int8 => ConfigNestedValueType::Int8 {},
            cm_rust::ConfigNestedValueType::Uint16 => ConfigNestedValueType::Uint16 {},
            cm_rust::ConfigNestedValueType::Int16 => ConfigNestedValueType::Int16 {},
            cm_rust::ConfigNestedValueType::Uint32 => ConfigNestedValueType::Uint32 {},
            cm_rust::ConfigNestedValueType::Int32 => ConfigNestedValueType::Int32 {},
            cm_rust::ConfigNestedValueType::Uint64 => ConfigNestedValueType::Uint64 {},
            cm_rust::ConfigNestedValueType::Int64 => ConfigNestedValueType::Int64 {},
            cm_rust::ConfigNestedValueType::String { max_size } => {
                ConfigNestedValueType::String { max_size: NonZeroU32::new(*max_size).ok_or(())? }
            }
        })
    }
}

#[derive(Clone, Hash, Debug, PartialEq, PartialOrd, Eq, Ord, Serialize)]
pub struct ConfigKey(String);

impl ConfigKey {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ConfigKey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let length = s.len();
        if length == 0 {
            return Err(ParseError::Empty);
        }
        if length > 64 {
            return Err(ParseError::TooLong);
        }

        // identifiers must start with a letter
        let first_is_letter = s.chars().next().expect("non-empty string").is_ascii_lowercase();
        // can contain letters, numbers, and underscores
        let contains_invalid_chars =
            s.chars().any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        // cannot end with an underscore
        let last_is_underscore = s.chars().next_back().expect("non-empty string") == '_';

        if !first_is_letter || contains_invalid_chars || last_is_underscore {
            return Err(ParseError::InvalidValue);
        }

        Ok(Self(s.to_string()))
    }
}

impl<'de> de::Deserialize<'de> for ConfigKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ConfigKey;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a non-empty string no more than 64 characters in length, which must \
                    start with a letter, can contain letters, numbers, and underscores, \
                    but cannot end with an underscore",
                )
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                s.parse().map_err(|err| match err {
                    ParseError::InvalidValue => E::invalid_value(
                        de::Unexpected::Str(s),
                        &"a name which must start with a letter, can contain letters, \
                        numbers, and underscores, but cannot end with an underscore",
                    ),
                    ParseError::TooLong | ParseError::Empty => E::invalid_length(
                        s.len(),
                        &"a non-empty name no more than 64 characters in length",
                    ),
                    e => {
                        panic!("unexpected parse error: {:?}", e);
                    }
                })
            }
        }
        deserializer.deserialize_string(Visitor)
    }
}

#[derive(Clone, Deserialize, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum ConfigRuntimeSource {
    Parent,
}

#[derive(Clone, Deserialize, Debug, PartialEq, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "lowercase")]
pub enum ConfigValueType {
    Bool {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Uint8 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Uint16 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Uint32 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Uint64 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Int8 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Int16 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Int32 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Int64 {
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    String {
        max_size: NonZeroU32,
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
    Vector {
        max_count: NonZeroU32,
        element: ConfigNestedValueType,
        mutability: Option<Vec<ConfigRuntimeSource>>,
    },
}

impl ConfigValueType {
    /// Update the hasher by digesting the ConfigValueType enum value
    pub fn update_digest(&self, hasher: &mut impl sha2::Digest) {
        let val = match self {
            ConfigValueType::Bool { .. } => 0u8,
            ConfigValueType::Uint8 { .. } => 1u8,
            ConfigValueType::Uint16 { .. } => 2u8,
            ConfigValueType::Uint32 { .. } => 3u8,
            ConfigValueType::Uint64 { .. } => 4u8,
            ConfigValueType::Int8 { .. } => 5u8,
            ConfigValueType::Int16 { .. } => 6u8,
            ConfigValueType::Int32 { .. } => 7u8,
            ConfigValueType::Int64 { .. } => 8u8,
            ConfigValueType::String { max_size, .. } => {
                hasher.update(max_size.get().to_le_bytes());
                9u8
            }
            ConfigValueType::Vector { max_count, element, .. } => {
                hasher.update(max_count.get().to_le_bytes());
                element.update_digest(hasher);
                10u8
            }
        };
        hasher.update([val])
    }
}

impl From<ConfigValueType> for cm_rust::ConfigValueType {
    fn from(value: ConfigValueType) -> Self {
        match value {
            ConfigValueType::Bool { .. } => cm_rust::ConfigValueType::Bool,
            ConfigValueType::Uint8 { .. } => cm_rust::ConfigValueType::Uint8,
            ConfigValueType::Uint16 { .. } => cm_rust::ConfigValueType::Uint16,
            ConfigValueType::Uint32 { .. } => cm_rust::ConfigValueType::Uint32,
            ConfigValueType::Uint64 { .. } => cm_rust::ConfigValueType::Uint64,
            ConfigValueType::Int8 { .. } => cm_rust::ConfigValueType::Int8,
            ConfigValueType::Int16 { .. } => cm_rust::ConfigValueType::Int16,
            ConfigValueType::Int32 { .. } => cm_rust::ConfigValueType::Int32,
            ConfigValueType::Int64 { .. } => cm_rust::ConfigValueType::Int64,
            ConfigValueType::String { max_size, .. } => {
                cm_rust::ConfigValueType::String { max_size: max_size.into() }
            }
            ConfigValueType::Vector { max_count, element, .. } => {
                cm_rust::ConfigValueType::Vector {
                    max_count: max_count.into(),
                    nested_type: element.into(),
                }
            }
        }
    }
}

pub trait FromClause {
    fn from_(&self) -> OneOrMany<AnyRef<'_>>;
}

pub trait FromClauseContext {
    fn from_(&self) -> ContextSpanned<OneOrMany<AnyRef<'_>>>;
}

pub trait CapabilityClause: Clone + PartialEq + std::fmt::Debug {
    fn service(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn protocol(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn directory(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn storage(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn runner(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn resolver(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn event_stream(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn dictionary(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn config(&self) -> Option<OneOrMany<&BorrowedName>>;
    fn set_service(&mut self, o: Option<OneOrMany<Name>>);
    fn set_protocol(&mut self, o: Option<OneOrMany<Name>>);
    fn set_directory(&mut self, o: Option<OneOrMany<Name>>);
    fn set_storage(&mut self, o: Option<OneOrMany<Name>>);
    fn set_runner(&mut self, o: Option<OneOrMany<Name>>);
    fn set_resolver(&mut self, o: Option<OneOrMany<Name>>);
    fn set_event_stream(&mut self, o: Option<OneOrMany<Name>>);
    fn set_dictionary(&mut self, o: Option<OneOrMany<Name>>);
    fn set_config(&mut self, o: Option<OneOrMany<Name>>);

    fn availability(&self) -> Option<Availability>;
    fn set_availability(&mut self, a: Option<Availability>);

    /// Returns the name of the capability for display purposes.
    /// If `service()` returns `Some`, the capability name must be "service", etc.
    ///
    /// Returns an error if the capability name is not set, or if there is more than one.
    fn capability_type(&self) -> Result<&'static str, Error> {
        let mut types = Vec::new();
        if self.service().is_some() {
            types.push("service");
        }
        if self.protocol().is_some() {
            types.push("protocol");
        }
        if self.directory().is_some() {
            types.push("directory");
        }
        if self.storage().is_some() {
            types.push("storage");
        }
        if self.event_stream().is_some() {
            types.push("event_stream");
        }
        if self.runner().is_some() {
            types.push("runner");
        }
        if self.config().is_some() {
            types.push("config");
        }
        if self.resolver().is_some() {
            types.push("resolver");
        }
        if self.dictionary().is_some() {
            types.push("dictionary");
        }
        match types.len() {
            0 => {
                let supported_keywords = self
                    .supported()
                    .into_iter()
                    .map(|k| format!("\"{}\"", k))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(Error::validate(format!(
                    "`{}` declaration is missing a capability keyword, one of: {}",
                    self.decl_type(),
                    supported_keywords,
                )))
            }
            1 => Ok(types[0]),
            _ => Err(Error::validate(format!(
                "{} declaration has multiple capability types defined: {:?}",
                self.decl_type(),
                types
            ))),
        }
    }

    /// Returns true if this capability type allows the ::Many variant of OneOrMany.
    fn are_many_names_allowed(&self) -> bool;

    fn decl_type(&self) -> &'static str;
    fn supported(&self) -> &[&'static str];

    /// Returns the names of the capabilities in this clause.
    /// If `protocol()` returns `Some(OneOrMany::Many(vec!["a", "b"]))`, this returns!["a", "b"].
    fn names(&self) -> Vec<&BorrowedName> {
        let res = vec![
            self.service(),
            self.protocol(),
            self.directory(),
            self.storage(),
            self.runner(),
            self.config(),
            self.resolver(),
            self.event_stream(),
            self.dictionary(),
        ];
        res.into_iter()
            .map(|o| o.map(|o| o.into_iter().collect::<Vec<&BorrowedName>>()).unwrap_or(vec![]))
            .flatten()
            .collect()
    }

    fn set_names(&mut self, names: Vec<Name>) {
        let names = match names.len() {
            0 => None,
            1 => Some(OneOrMany::One(names.first().unwrap().clone())),
            _ => Some(OneOrMany::Many(names)),
        };

        let cap_type = self.capability_type().unwrap();
        if cap_type == "protocol" {
            self.set_protocol(names);
        } else if cap_type == "service" {
            self.set_service(names);
        } else if cap_type == "directory" {
            self.set_directory(names);
        } else if cap_type == "storage" {
            self.set_storage(names);
        } else if cap_type == "runner" {
            self.set_runner(names);
        } else if cap_type == "resolver" {
            self.set_resolver(names);
        } else if cap_type == "event_stream" {
            self.set_event_stream(names);
        } else if cap_type == "dictionary" {
            self.set_dictionary(names);
        } else if cap_type == "config" {
            self.set_config(names);
        } else {
            panic!("Unknown capability type {}", cap_type);
        }
    }
}

trait Canonicalize {
    fn canonicalize(&mut self);
}

pub trait AsClause {
    fn r#as(&self) -> Option<&BorrowedName>;
}

pub trait AsClauseContext {
    fn r#as(&self) -> Option<ContextSpanned<&BorrowedName>>;
}

pub trait PathClause {
    fn path(&self) -> Option<&Path>;
}

pub trait FilterClause {
    fn filter(&self) -> Option<&Map<String, Value>>;
}

pub fn alias_or_name<'a>(
    alias: Option<&'a BorrowedName>,
    name: &'a BorrowedName,
) -> &'a BorrowedName {
    alias.unwrap_or(name)
}

pub fn alias_or_name_context<'a>(
    alias: Option<ContextSpanned<&'a BorrowedName>>,
    name: &'a BorrowedName,
    origin: Origin,
) -> ContextSpanned<&'a BorrowedName> {
    alias.unwrap_or(ContextSpanned { value: name, origin })
}

pub fn alias_or_path<'a>(alias: Option<&'a Path>, path: &'a Path) -> &'a Path {
    alias.unwrap_or(path)
}

pub fn format_cml(buffer: &str, file: Option<&std::path::Path>) -> Result<Vec<u8>, Error> {
    let general_order = PathOption::PropertyNameOrder(vec![
        "name",
        "url",
        "startup",
        "environment",
        "config",
        "dictionary",
        "durability",
        "service",
        "protocol",
        "directory",
        "storage",
        "runner",
        "resolver",
        "event",
        "event_stream",
        "from",
        "as",
        "to",
        "rights",
        "path",
        "subdir",
        "filter",
        "dependency",
        "extends",
        "runners",
        "resolvers",
        "debug",
    ]);
    let options = FormatOptions {
        collapse_containers_of_one: true,
        sort_array_items: true, // but use options_by_path to turn this off for program args
        options_by_path: hashmap! {
            "/*" => hashset! {
                PathOption::PropertyNameOrder(vec![
                    "include",
                    "program",
                    "children",
                    "collections",
                    "capabilities",
                    "use",
                    "offer",
                    "expose",
                    "environments",
                    "facets",
                ])
            },
            "/*/program" => hashset! {
                PathOption::CollapseContainersOfOne(false),
                PathOption::PropertyNameOrder(vec![
                    "runner",
                    "binary",
                    "args",
                ]),
            },
            "/*/program/*" => hashset! {
                PathOption::SortArrayItems(false),
            },
            "/*/*/*" => hashset! {
                general_order.clone()
            },
            "/*/*/*/*/*" => hashset! {
                general_order
            },
        },
        ..Default::default()
    };

    json5format::format(buffer, file.map(|f| f.to_string_lossy().to_string()), Some(options))
        .map_err(|e| Error::json5(e, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::document::Document;
    use crate::types::environment::RunnerRegistration;
    use crate::types::right::Right;
    use assert_matches::assert_matches;
    use difference::Changeset;
    use fidl_fuchsia_io as fio;
    use serde_json::{json, to_string_pretty, to_value};
    use std::path;
    use std::path::Path;
    use test_case::test_case;

    macro_rules! assert_json_eq {
        ($a:expr, $e:expr) => {{
            if $a != $e {
                let expected = to_string_pretty(&$e).unwrap();
                let actual = to_string_pretty(&$a).unwrap();
                assert_eq!(
                    $a,
                    $e,
                    "JSON actual != expected. Diffs:\n\n{}",
                    Changeset::new(&actual, &expected, "\n")
                );
            }
        }};
    }

    // Exercise reference parsing tests on `OfferFromRef` because it contains every reference
    // subtype.

    #[test]
    fn test_parse_named_reference() {
        assert_matches!("#some-child".parse::<OfferFromRef>(), Ok(OfferFromRef::Named(name)) if name == "some-child");
        assert_matches!("#A".parse::<OfferFromRef>(), Ok(OfferFromRef::Named(name)) if name == "A");
        assert_matches!("#7".parse::<OfferFromRef>(), Ok(OfferFromRef::Named(name)) if name == "7");
        assert_matches!("#_".parse::<OfferFromRef>(), Ok(OfferFromRef::Named(name)) if name == "_");

        assert_matches!("#-".parse::<OfferFromRef>(), Err(_));
        assert_matches!("#.".parse::<OfferFromRef>(), Err(_));
        assert_matches!("#".parse::<OfferFromRef>(), Err(_));
        assert_matches!("some-child".parse::<OfferFromRef>(), Err(_));
    }

    #[test]
    fn test_parse_reference_test() {
        assert_matches!("parent".parse::<OfferFromRef>(), Ok(OfferFromRef::Parent));
        assert_matches!("framework".parse::<OfferFromRef>(), Ok(OfferFromRef::Framework));
        assert_matches!("self".parse::<OfferFromRef>(), Ok(OfferFromRef::Self_));
        assert_matches!("#child".parse::<OfferFromRef>(), Ok(OfferFromRef::Named(name)) if name == "child");

        assert_matches!("invalid".parse::<OfferFromRef>(), Err(_));
        assert_matches!("#invalid-child^".parse::<OfferFromRef>(), Err(_));
    }

    fn json_value_from_str(json: &str, filename: &Path) -> Result<Value, Error> {
        serde_json::from_str(json).map_err(|e| {
            Error::parse(
                format!("Couldn't read input as JSON: {}", e),
                Some(Location { line: e.line(), column: e.column() }),
                Some(filename),
            )
        })
    }

    fn parse_as_ref(input: &str) -> Result<OfferFromRef, Error> {
        serde_json::from_value::<OfferFromRef>(json_value_from_str(input, &Path::new("test.cml"))?)
            .map_err(|e| Error::parse(format!("{}", e), None, None))
    }

    #[test]
    fn test_deserialize_ref() -> Result<(), Error> {
        assert_matches!(parse_as_ref("\"self\""), Ok(OfferFromRef::Self_));
        assert_matches!(parse_as_ref("\"parent\""), Ok(OfferFromRef::Parent));
        assert_matches!(parse_as_ref("\"#child\""), Ok(OfferFromRef::Named(name)) if name == "child");

        assert_matches!(parse_as_ref(r#""invalid""#), Err(_));

        Ok(())
    }

    macro_rules! test_parse_rights {
        (
            $(
                ($input:expr, $expected:expr),
            )+
        ) => {
            #[test]
            fn parse_rights() {
                $(
                    parse_rights_test($input, $expected);
                )+
            }
        }
    }

    fn parse_rights_test(input: &str, expected: Right) {
        let r: Right = serde_json5::from_str(&format!("\"{}\"", input)).expect("invalid json");
        assert_eq!(r, expected);
    }

    test_parse_rights! {
        ("connect", Right::Connect),
        ("enumerate", Right::Enumerate),
        ("execute", Right::Execute),
        ("get_attributes", Right::GetAttributes),
        ("modify_directory", Right::ModifyDirectory),
        ("read_bytes", Right::ReadBytes),
        ("traverse", Right::Traverse),
        ("update_attributes", Right::UpdateAttributes),
        ("write_bytes", Right::WriteBytes),
        ("r*", Right::ReadAlias),
        ("w*", Right::WriteAlias),
        ("x*", Right::ExecuteAlias),
        ("rw*", Right::ReadWriteAlias),
        ("rx*", Right::ReadExecuteAlias),
    }

    macro_rules! test_expand_rights {
        (
            $(
                ($input:expr, $expected:expr),
            )+
        ) => {
            #[test]
            fn expand_rights() {
                $(
                    expand_rights_test($input, $expected);
                )+
            }
        }
    }

    fn expand_rights_test(input: Right, expected: Vec<fio::Operations>) {
        assert_eq!(input.expand(), expected);
    }

    test_expand_rights! {
        (Right::Connect, vec![fio::Operations::CONNECT]),
        (Right::Enumerate, vec![fio::Operations::ENUMERATE]),
        (Right::Execute, vec![fio::Operations::EXECUTE]),
        (Right::GetAttributes, vec![fio::Operations::GET_ATTRIBUTES]),
        (Right::ModifyDirectory, vec![fio::Operations::MODIFY_DIRECTORY]),
        (Right::ReadBytes, vec![fio::Operations::READ_BYTES]),
        (Right::Traverse, vec![fio::Operations::TRAVERSE]),
        (Right::UpdateAttributes, vec![fio::Operations::UPDATE_ATTRIBUTES]),
        (Right::WriteBytes, vec![fio::Operations::WRITE_BYTES]),
        (Right::ReadAlias, vec![
            fio::Operations::CONNECT,
            fio::Operations::ENUMERATE,
            fio::Operations::TRAVERSE,
            fio::Operations::READ_BYTES,
            fio::Operations::GET_ATTRIBUTES,
        ]),
        (Right::WriteAlias, vec![
            fio::Operations::CONNECT,
            fio::Operations::ENUMERATE,
            fio::Operations::TRAVERSE,
            fio::Operations::WRITE_BYTES,
            fio::Operations::MODIFY_DIRECTORY,
            fio::Operations::UPDATE_ATTRIBUTES,
        ]),
        (Right::ExecuteAlias, vec![
            fio::Operations::CONNECT,
            fio::Operations::ENUMERATE,
            fio::Operations::TRAVERSE,
            fio::Operations::EXECUTE,
        ]),
        (Right::ReadWriteAlias, vec![
            fio::Operations::CONNECT,
            fio::Operations::ENUMERATE,
            fio::Operations::TRAVERSE,
            fio::Operations::READ_BYTES,
            fio::Operations::WRITE_BYTES,
            fio::Operations::MODIFY_DIRECTORY,
            fio::Operations::GET_ATTRIBUTES,
            fio::Operations::UPDATE_ATTRIBUTES,
        ]),
        (Right::ReadExecuteAlias, vec![
            fio::Operations::CONNECT,
            fio::Operations::ENUMERATE,
            fio::Operations::TRAVERSE,
            fio::Operations::READ_BYTES,
            fio::Operations::GET_ATTRIBUTES,
            fio::Operations::EXECUTE,
        ]),
    }

    #[test]
    fn test_deny_unknown_fields() {
        assert_matches!(serde_json5::from_str::<Document>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Environment>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<RunnerRegistration>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<ResolverRegistration>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Use>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Expose>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Offer>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Capability>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Child>("{ unknown: \"\" }"), Err(_));
        assert_matches!(serde_json5::from_str::<Collection>("{ unknown: \"\" }"), Err(_));
    }

    // TODO: Use Default::default() instead

    fn empty_offer() -> Offer {
        Offer {
            service: None,
            protocol: None,
            directory: None,
            storage: None,
            runner: None,
            resolver: None,
            dictionary: None,
            config: None,
            from: OneOrMany::One(OfferFromRef::Self_),
            to: OneOrMany::Many(vec![]),
            r#as: None,
            rights: None,
            subdir: None,
            dependency: None,
            event_stream: None,
            scope: None,
            availability: None,
            source_availability: None,
            target_availability: None,
        }
    }

    fn empty_use() -> Use {
        Use {
            service: None,
            protocol: None,
            scope: None,
            directory: None,
            storage: None,
            config: None,
            dictionary: None,
            key: None,
            from: None,
            path: None,
            numbered_handle: None,
            rights: None,
            subdir: None,
            event_stream: None,
            runner: None,
            filter: None,
            dependency: None,
            availability: None,
            config_element_type: None,
            config_max_count: None,
            config_max_size: None,
            config_type: None,
            config_default: None,
        }
    }

    #[test]
    fn test_capability_id() -> Result<(), Error> {
        // service
        let a: Name = "a".parse().unwrap();
        let b: Name = "b".parse().unwrap();
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                service: Some(OneOrMany::One(a.clone())),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Service(&a)]
        );
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                service: Some(OneOrMany::Many(vec![a.clone(), b.clone()],)),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Service(&a), CapabilityId::Service(&b)]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                service: Some(OneOrMany::One(a.clone())),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedService("/svc/a".parse().unwrap())]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                service: Some(OneOrMany::Many(vec![a.clone(), b.clone(),],)),
                ..empty_use()
            },)?,
            vec![
                CapabilityId::UsedService("/svc/a".parse().unwrap()),
                CapabilityId::UsedService("/svc/b".parse().unwrap())
            ]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                event_stream: Some(OneOrMany::One(Name::new("test".to_string()).unwrap())),
                path: Some(cm_types::Path::new("/svc/myevent".to_string()).unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedEventStream("/svc/myevent".parse().unwrap()),]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                event_stream: Some(OneOrMany::One(Name::new("test".to_string()).unwrap())),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedEventStream(
                "/svc/fuchsia.component.EventStream".parse().unwrap()
            ),]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                service: Some(OneOrMany::One(a.clone())),
                path: Some("/b".parse().unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedService("/b".parse().unwrap())]
        );

        // protocol
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                protocol: Some(OneOrMany::One(a.clone())),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Protocol(&a)]
        );
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                protocol: Some(OneOrMany::Many(vec![a.clone(), b.clone()],)),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Protocol(&a), CapabilityId::Protocol(&b)]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                protocol: Some(OneOrMany::One(a.clone())),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedProtocol("/svc/a".parse().unwrap())]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                protocol: Some(OneOrMany::Many(vec![a.clone(), b.clone(),],)),
                ..empty_use()
            },)?,
            vec![
                CapabilityId::UsedProtocol("/svc/a".parse().unwrap()),
                CapabilityId::UsedProtocol("/svc/b".parse().unwrap())
            ]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                protocol: Some(OneOrMany::One(a.clone())),
                path: Some("/b".parse().unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedProtocol("/b".parse().unwrap())]
        );

        // directory
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                directory: Some(OneOrMany::One(a.clone())),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Directory(&a)]
        );
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                directory: Some(OneOrMany::Many(vec![a.clone(), b.clone()])),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Directory(&a), CapabilityId::Directory(&b),]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                directory: Some(a.clone()),
                path: Some("/b".parse().unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedDirectory("/b".parse().unwrap())]
        );

        // storage
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                storage: Some(OneOrMany::One(a.clone())),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Storage(&a)]
        );
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                storage: Some(OneOrMany::Many(vec![a.clone(), b.clone()])),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Storage(&a), CapabilityId::Storage(&b),]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                storage: Some(a.clone()),
                path: Some("/b".parse().unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedStorage("/b".parse().unwrap())]
        );

        // runner
        assert_eq!(
            CapabilityId::from_use(&Use { runner: Some("elf".parse().unwrap()), ..empty_use() })?,
            vec![CapabilityId::UsedRunner(BorrowedName::new("elf").unwrap())]
        );

        // dictionary
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                dictionary: Some(OneOrMany::One(a.clone())),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Dictionary(&a)]
        );
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                dictionary: Some(OneOrMany::Many(vec![a.clone(), b.clone()],)),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Dictionary(&a), CapabilityId::Dictionary(&b)]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                dictionary: Some(OneOrMany::One(a.clone())),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedDictionary("/svc/a".parse().unwrap())]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                dictionary: Some(OneOrMany::Many(vec![a.clone(), b.clone(),],)),
                ..empty_use()
            },)?,
            vec![
                CapabilityId::UsedDictionary("/svc/a".parse().unwrap()),
                CapabilityId::UsedDictionary("/svc/b".parse().unwrap())
            ]
        );
        assert_eq!(
            CapabilityId::from_use(&Use {
                dictionary: Some(OneOrMany::One(a.clone())),
                path: Some("/b".parse().unwrap()),
                ..empty_use()
            },)?,
            vec![CapabilityId::UsedDictionary("/b".parse().unwrap())]
        );

        // "as" aliasing.
        assert_eq!(
            CapabilityId::from_offer_expose(&Offer {
                service: Some(OneOrMany::One(a.clone())),
                r#as: Some(b.clone()),
                ..empty_offer()
            },)?,
            vec![CapabilityId::Service(&b)]
        );

        // Error case.
        assert_matches!(CapabilityId::from_offer_expose(&empty_offer()), Err(_));

        Ok(())
    }

    fn document(contents: serde_json::Value) -> Document {
        serde_json5::from_str::<Document>(&contents.to_string()).unwrap()
    }

    #[test]
    fn test_includes() {
        assert_eq!(document(json!({})).includes(), Vec::<String>::new());
        assert_eq!(document(json!({ "include": []})).includes(), Vec::<String>::new());
        assert_eq!(
            document(json!({ "include": [ "foo.cml", "bar.cml" ]})).includes(),
            vec!["foo.cml", "bar.cml"]
        );
    }

    #[test]
    fn test_merge_same_section() {
        let mut some = document(json!({ "use": [{ "protocol": "foo" }] }));
        let mut other = document(json!({ "use": [{ "protocol": "bar" }] }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let uses = some.r#use.as_ref().unwrap();
        assert_eq!(uses.len(), 2);
        assert_eq!(
            uses[0].protocol.as_ref().unwrap(),
            &OneOrMany::One("foo".parse::<Name>().unwrap())
        );
        assert_eq!(
            uses[1].protocol.as_ref().unwrap(),
            &OneOrMany::One("bar".parse::<Name>().unwrap())
        );
    }

    #[test]
    fn test_merge_upgraded_availability() {
        let mut some =
            document(json!({ "use": [{ "protocol": "foo", "availability": "optional" }] }));
        let mut other1 = document(json!({ "use": [{ "protocol": "foo" }] }));
        let mut other2 =
            document(json!({ "use": [{ "protocol": "foo", "availability": "transitional" }] }));
        let mut other3 =
            document(json!({ "use": [{ "protocol": "foo", "availability": "same_as_target" }] }));
        some.merge_from(&mut other1, &Path::new("some/path")).unwrap();
        some.merge_from(&mut other2, &Path::new("some/path")).unwrap();
        some.merge_from(&mut other3, &Path::new("some/path")).unwrap();
        let uses = some.r#use.as_ref().unwrap();
        assert_eq!(uses.len(), 2);
        assert_eq!(
            uses[0].protocol.as_ref().unwrap(),
            &OneOrMany::One("foo".parse::<Name>().unwrap())
        );
        assert!(uses[0].availability.is_none());
        assert_eq!(
            uses[1].protocol.as_ref().unwrap(),
            &OneOrMany::One("foo".parse::<Name>().unwrap())
        );
        assert_eq!(uses[1].availability.as_ref().unwrap(), &Availability::SameAsTarget,);
    }

    #[test]
    fn test_merge_different_sections() {
        let mut some = document(json!({ "use": [{ "protocol": "foo" }] }));
        let mut other = document(json!({ "expose": [{ "protocol": "bar", "from": "self" }] }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let uses = some.r#use.as_ref().unwrap();
        let exposes = some.expose.as_ref().unwrap();
        assert_eq!(uses.len(), 1);
        assert_eq!(exposes.len(), 1);
        assert_eq!(
            uses[0].protocol.as_ref().unwrap(),
            &OneOrMany::One("foo".parse::<Name>().unwrap())
        );
        assert_eq!(
            exposes[0].protocol.as_ref().unwrap(),
            &OneOrMany::One("bar".parse::<Name>().unwrap())
        );
    }

    #[test]
    fn test_merge_environments() {
        let mut some = document(json!({ "environments": [
            {
                "name": "one",
                "extends": "realm",
            },
            {
                "name": "two",
                "extends": "none",
                "runners": [
                    {
                        "runner": "r1",
                        "from": "#c1",
                    },
                    {
                        "runner": "r2",
                        "from": "#c2",
                    },
                ],
                "resolvers": [
                    {
                        "resolver": "res1",
                        "from": "#c1",
                        "scheme": "foo",
                    },
                ],
                "debug": [
                    {
                        "protocol": "baz",
                        "from": "#c2"
                    }
                ]
            },
        ]}));
        let mut other = document(json!({ "environments": [
            {
                "name": "two",
                "__stop_timeout_ms": 100,
                "runners": [
                    {
                        "runner": "r3",
                        "from": "#c3",
                    },
                ],
                "resolvers": [
                    {
                        "resolver": "res2",
                        "from": "#c1",
                        "scheme": "bar",
                    },
                ],
                "debug": [
                    {
                        "protocol": "faz",
                        "from": "#c2"
                    }
                ]
            },
            {
                "name": "three",
                "__stop_timeout_ms": 1000,
            },
        ]}));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        assert_eq!(
            to_value(some).unwrap(),
            json!({"environments": [
                {
                    "name": "one",
                    "extends": "realm",
                },
                {
                    "name": "three",
                    "__stop_timeout_ms": 1000,
                },
                {
                    "name": "two",
                    "extends": "none",
                    "__stop_timeout_ms": 100,
                    "runners": [
                        {
                            "runner": "r1",
                            "from": "#c1",
                        },
                        {
                            "runner": "r2",
                            "from": "#c2",
                        },
                        {
                            "runner": "r3",
                            "from": "#c3",
                        },
                    ],
                    "resolvers": [
                        {
                            "resolver": "res1",
                            "from": "#c1",
                            "scheme": "foo",
                        },
                        {
                            "resolver": "res2",
                            "from": "#c1",
                            "scheme": "bar",
                        },
                    ],
                    "debug": [
                        {
                            "protocol": "baz",
                            "from": "#c2"
                        },
                        {
                            "protocol": "faz",
                            "from": "#c2"
                        }
                    ]
                },
            ]})
        );
    }

    #[test]
    fn test_merge_environments_errors() {
        {
            let mut some = document(json!({"environments": [{"name": "one", "extends": "realm"}]}));
            let mut other = document(json!({"environments": [{"name": "one", "extends": "none"}]}));
            assert!(some.merge_from(&mut other, &Path::new("some/path")).is_err());
        }
        {
            let mut some =
                document(json!({"environments": [{"name": "one", "__stop_timeout_ms": 10}]}));
            let mut other =
                document(json!({"environments": [{"name": "one", "__stop_timeout_ms": 20}]}));
            assert!(some.merge_from(&mut other, &Path::new("some/path")).is_err());
        }

        // It's ok if the values match.
        {
            let mut some = document(json!({"environments": [{"name": "one", "extends": "realm"}]}));
            let mut other =
                document(json!({"environments": [{"name": "one", "extends": "realm"}]}));
            some.merge_from(&mut other, &Path::new("some/path")).unwrap();
            assert_eq!(
                to_value(some).unwrap(),
                json!({"environments": [{"name": "one", "extends": "realm"}]})
            );
        }
        {
            let mut some =
                document(json!({"environments": [{"name": "one", "__stop_timeout_ms": 10}]}));
            let mut other =
                document(json!({"environments": [{"name": "one", "__stop_timeout_ms": 10}]}));
            some.merge_from(&mut other, &Path::new("some/path")).unwrap();
            assert_eq!(
                to_value(some).unwrap(),
                json!({"environments": [{"name": "one", "__stop_timeout_ms": 10}]})
            );
        }
    }

    #[test]
    fn test_merge_from_other_config() {
        let mut some = document(json!({}));
        let mut other = document(json!({ "config": { "bar": { "type": "bool" } } }));

        some.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        let expected = document(json!({ "config": { "bar": { "type": "bool" } } }));
        assert_eq!(some.config, expected.config);
    }

    #[test]
    fn test_merge_from_some_config() {
        let mut some = document(json!({ "config": { "bar": { "type": "bool" } } }));
        let mut other = document(json!({}));

        some.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        let expected = document(json!({ "config": { "bar": { "type": "bool" } } }));
        assert_eq!(some.config, expected.config);
    }

    #[test]
    fn test_merge_from_config() {
        let mut some = document(json!({ "config": { "foo": { "type": "bool" } } }));
        let mut other = document(json!({ "config": { "bar": { "type": "bool" } } }));
        some.merge_from(&mut other, &path::Path::new("some/path")).unwrap();

        assert_eq!(
            some,
            document(json!({
                "config": {
                    "foo": { "type": "bool" },
                    "bar": { "type": "bool" },
                }
            })),
        );
    }

    #[test]
    fn test_merge_from_config_dedupe_identical_fields() {
        let mut some = document(json!({ "config": { "foo": { "type": "bool" } } }));
        let mut other = document(json!({ "config": { "foo": { "type": "bool" } } }));
        some.merge_from(&mut other, &path::Path::new("some/path")).unwrap();

        assert_eq!(some, document(json!({ "config": { "foo": { "type": "bool" } } })));
    }

    #[test]
    fn test_merge_from_config_conflicting_keys() {
        let mut some = document(json!({ "config": { "foo": { "type": "bool" } } }));
        let mut other = document(json!({ "config": { "foo": { "type": "uint8" } } }));

        assert_matches::assert_matches!(
            some.merge_from(&mut other, &path::Path::new("some/path")),
            Err(Error::Validate { err, .. })
                if err == "Found conflicting entry for config key `foo` in `some/path`."
        );
    }

    #[test]
    fn test_canonicalize() {
        let mut some = document(json!({
            "children": [
                // Will be sorted by name
                { "name": "b_child", "url": "http://foo/b" },
                { "name": "a_child", "url": "http://foo/a" },
            ],
            "environments": [
                // Will be sorted by name
                { "name": "b_env" },
                { "name": "a_env" },
            ],
            "collections": [
                // Will be sorted by name
                { "name": "b_coll", "durability": "transient" },
                { "name": "a_coll", "durability": "transient" },
            ],
            // Will have entries sorted by capability type, then
            // by capability name (using the first entry in Many cases).
            "capabilities": [
                // Will be merged with "bar"
                { "protocol": ["foo"] },
                { "protocol": "bar" },
                // Will not be merged, but will be sorted before "bar"
                { "protocol": "arg", "path": "/arg" },
                // Will have list of names sorted
                { "service": ["b", "a"] },
                // Will have list of names sorted
                { "event_stream": ["b", "a"] },
                { "runner": "myrunner" },
                // The following two will *not* be merged, because they have a `path`.
                { "runner": "mypathrunner1", "path": "/foo" },
                { "runner": "mypathrunner2", "path": "/foo" },
            ],
            // Same rules as for "capabilities".
            "offer": [
                // Will be sorted after "bar"
                { "protocol": "baz", "from": "#a_child", "to": "#c_child"  },
                // The following two entries will be merged
                { "protocol": ["foo"], "from": "#a_child", "to": "#b_child"  },
                { "protocol": "bar", "from": "#a_child", "to": "#b_child"  },
                // Will have list of names sorted
                { "service": ["b", "a"], "from": "#a_child", "to": "#b_child"  },
                // Will have list of names sorted
                {
                    "event_stream": ["b", "a"],
                    "from": "#a_child",
                    "to": "#b_child",
                    "scope": ["#b", "#c", "#a"]  // Also gets sorted
                },
                { "runner": [ "myrunner", "a" ], "from": "#a_child", "to": "#b_child"  },
                { "runner": [ "b" ], "from": "#a_child", "to": "#b_child"  },
                { "directory": [ "b" ], "from": "#a_child", "to": "#b_child"  },
            ],
            "expose": [
                { "protocol": ["foo"], "from": "#a_child" },
                { "protocol": "bar", "from": "#a_child" },  // Will appear before protocol: foo
                // Will have list of names sorted
                { "service": ["b", "a"], "from": "#a_child" },
                // Will have list of names sorted
                {
                    "event_stream": ["b", "a"],
                    "from": "#a_child",
                    "scope": ["#b", "#c", "#a"]  // Also gets sorted
                },
                { "runner": [ "myrunner", "a" ], "from": "#a_child" },
                { "runner": [ "b" ], "from": "#a_child" },
                { "directory": [ "b" ], "from": "#a_child" },
            ],
            "use": [
                // Will be sorted after "baz"
                { "protocol": ["zazzle"], "path": "/zazbaz" },
                // These will be merged
                { "protocol": ["foo"] },
                { "protocol": "bar" },
                // Will have list of names sorted
                { "service": ["b", "a"] },
                // Will have list of names sorted
                { "event_stream": ["b", "a"], "scope": ["#b", "#a"] },
            ],
        }));
        some.canonicalize();

        assert_json_eq!(
            some,
            document(json!({
                "children": [
                    { "name": "a_child", "url": "http://foo/a" },
                    { "name": "b_child", "url": "http://foo/b" },
                ],
                "collections": [
                    { "name": "a_coll", "durability": "transient" },
                    { "name": "b_coll", "durability": "transient" },
                ],
                "environments": [
                    { "name": "a_env" },
                    { "name": "b_env" },
                ],
                "capabilities": [
                    { "event_stream": ["a", "b"] },
                    { "protocol": "arg", "path": "/arg" },
                    { "protocol": ["bar", "foo"] },
                    { "runner": "mypathrunner1", "path": "/foo" },
                    { "runner": "mypathrunner2", "path": "/foo" },
                    { "runner": "myrunner" },
                    { "service": ["a", "b"] },
                ],
                "use": [
                    { "event_stream": ["a", "b"], "scope": ["#a", "#b"] },
                    { "protocol": ["bar", "foo"] },
                    { "protocol": "zazzle", "path": "/zazbaz" },
                    { "service": ["a", "b"] },
                ],
                "offer": [
                    { "directory": "b", "from": "#a_child", "to": "#b_child" },
                    {
                        "event_stream": ["a", "b"],
                        "from": "#a_child",
                        "to": "#b_child",
                        "scope": ["#a", "#b", "#c"],
                    },
                    { "protocol": ["bar", "foo"], "from": "#a_child", "to": "#b_child" },
                    { "protocol": "baz", "from": "#a_child", "to": "#c_child"  },
                    { "runner": [ "a", "b", "myrunner" ], "from": "#a_child", "to": "#b_child" },
                    { "service": ["a", "b"], "from": "#a_child", "to": "#b_child" },
                ],
                "expose": [
                    { "directory": "b", "from": "#a_child" },
                    {
                        "event_stream": ["a", "b"],
                        "from": "#a_child",
                        "scope": ["#a", "#b", "#c"],
                    },
                    { "protocol": ["bar", "foo"], "from": "#a_child" },
                    { "runner": [ "a", "b", "myrunner" ], "from": "#a_child" },
                    { "service": ["a", "b"], "from": "#a_child" },
                ],
            }))
        )
    }

    #[test]
    fn deny_unknown_config_type_fields() {
        let input = json!({ "config": { "foo": { "type": "bool", "unknown": "should error" } } });
        serde_json5::from_str::<Document>(&input.to_string())
            .expect_err("must reject unknown config field attributes");
    }

    #[test]
    fn deny_unknown_config_nested_type_fields() {
        let input = json!({
            "config": {
                "foo": {
                    "type": "vector",
                    "max_count": 10,
                    "element": {
                        "type": "bool",
                        "unknown": "should error"
                    },

                }
            }
        });
        serde_json5::from_str::<Document>(&input.to_string())
            .expect_err("must reject unknown config field attributes");
    }

    #[test]
    fn test_merge_from_program() {
        let mut some = document(json!({ "program": { "binary": "bin/hello_world" } }));
        let mut other = document(json!({ "program": { "runner": "elf" } }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let expected =
            document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        assert_eq!(some.program, expected.program);
    }

    #[test]
    fn test_merge_from_program_without_runner() {
        let mut some =
            document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        // https://fxbug.dev/42160240: merging with a document that doesn't have a runner doesn't override the
        // runner that we already have assigned.
        let mut other = document(json!({ "program": {} }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let expected =
            document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        assert_eq!(some.program, expected.program);
    }

    #[test]
    fn test_merge_from_program_overlapping_environ() {
        // It's ok to merge `program.environ` by concatenating the arrays together.
        let mut some = document(json!({ "program": { "environ": ["1"] } }));
        let mut other = document(json!({ "program": { "environ": ["2"] } }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let expected = document(json!({ "program": { "environ": ["1", "2"] } }));
        assert_eq!(some.program, expected.program);
    }

    #[test]
    fn test_merge_from_program_overlapping_runner() {
        // It's ok to merge `program.runner = "elf"` with `program.runner = "elf"`.
        let mut some =
            document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        let mut other = document(json!({ "program": { "runner": "elf" } }));
        some.merge_from(&mut other, &Path::new("some/path")).unwrap();
        let expected =
            document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        assert_eq!(some.program, expected.program);
    }

    #[test_case(
        document(json!({ "program": { "runner": "elf" } })),
        document(json!({ "program": { "runner": "fle" } })),
        "runner"
        ; "when_runner_conflicts"
    )]
    #[test_case(
        document(json!({ "program": { "binary": "bin/hello_world" } })),
        document(json!({ "program": { "binary": "bin/hola_mundo" } })),
        "binary"
        ; "when_binary_conflicts"
    )]
    #[test_case(
        document(json!({ "program": { "args": ["a".to_owned()] } })),
        document(json!({ "program": { "args": ["b".to_owned()] } })),
        "args"
        ; "when_args_conflicts"
    )]
    fn test_merge_from_program_error(mut some: Document, mut other: Document, field: &str) {
        assert_matches::assert_matches!(
            some.merge_from(&mut other, &path::Path::new("some/path")),
            Err(Error::Validate {  err, .. })
                if err == format!("manifest include had a conflicting `program.{}`: some/path", field)
        );
    }

    #[test_case(
        document(json!({ "facets": { "my.key": "my.value" } })),
        document(json!({ "facets": { "other.key": "other.value" } })),
        document(json!({ "facets": { "my.key": "my.value",  "other.key": "other.value" } }))
        ; "two separate keys"
    )]
    #[test_case(
        document(json!({ "facets": { "my.key": "my.value" } })),
        document(json!({ "facets": {} })),
        document(json!({ "facets": { "my.key": "my.value" } }))
        ; "empty other facet"
    )]
    #[test_case(
        document(json!({ "facets": {} })),
        document(json!({ "facets": { "other.key": "other.value" } })),
        document(json!({ "facets": { "other.key": "other.value" } }))
        ; "empty my facet"
    )]
    #[test_case(
        document(json!({ "facets": { "key": { "type": "some_type" } } })),
        document(json!({ "facets": { "key": { "runner": "some_runner"} } })),
        document(json!({ "facets": { "key": { "type": "some_type", "runner": "some_runner" } } }))
        ; "nested facet key"
    )]
    #[test_case(
        document(json!({ "facets": { "key": { "type": "some_type", "nested_key": { "type": "new type" }}}})),
        document(json!({ "facets": { "key": { "nested_key": { "runner": "some_runner" }} } })),
        document(json!({ "facets": { "key": { "type": "some_type", "nested_key": { "runner": "some_runner", "type": "new type" }}}}))
        ; "double nested facet key"
    )]
    #[test_case(
        document(json!({ "facets": { "key": { "array_key": ["value_1", "value_2"] } } })),
        document(json!({ "facets": { "key": { "array_key": ["value_3", "value_4"] } } })),
        document(json!({ "facets": { "key": { "array_key": ["value_1", "value_2", "value_3", "value_4"] } } }))
        ; "merge array values"
    )]
    fn test_merge_from_facets(mut my: Document, mut other: Document, expected: Document) {
        my.merge_from(&mut other, &Path::new("some/path")).unwrap();
        assert_eq!(my.facets, expected.facets);
    }

    #[test_case(
        document(json!({ "facets": { "key": "my.value" }})),
        document(json!({ "facets": { "key": "other.value" }})),
        "facets.key"
        ; "conflict first level keys"
    )]
    #[test_case(
        document(json!({ "facets": { "key":  {"type": "cts" }}})),
        document(json!({ "facets": { "key":  {"type": "system" }}})),
        "facets.key.type"
        ; "conflict second level keys"
    )]
    #[test_case(
        document(json!({ "facets": { "key":  {"type": {"key": "value" }}}})),
        document(json!({ "facets": { "key":  {"type": "system" }}})),
        "facets.key.type"
        ; "incompatible self nested type"
    )]
    #[test_case(
        document(json!({ "facets": { "key":  {"type": "system" }}})),
        document(json!({ "facets": { "key":  {"type":  {"key": "value" }}}})),
        "facets.key.type"
        ; "incompatible other nested type"
    )]
    #[test_case(
        document(json!({ "facets": { "key":  {"type": {"key": "my.value" }}}})),
        document(json!({ "facets": { "key":  {"type":  {"key": "some.value" }}}})),
        "facets.key.type.key"
        ; "conflict third level keys"
    )]
    #[test_case(
        document(json!({ "facets": { "key":  {"type": [ "value_1" ]}}})),
        document(json!({ "facets": { "key":  {"type":  "value_2" }}})),
        "facets.key.type"
        ; "incompatible keys"
    )]
    fn test_merge_from_facet_error(mut my: Document, mut other: Document, field: &str) {
        assert_matches::assert_matches!(
            my.merge_from(&mut other, &path::Path::new("some/path")),
            Err(Error::Validate {  err, .. })
                if err == format!("manifest include had a conflicting `{}`: some/path", field)
        );
    }

    #[test_case("protocol")]
    #[test_case("service")]
    #[test_case("event_stream")]
    fn test_merge_from_duplicate_use_array(typename: &str) {
        let mut my = document(json!({ "use": [{ typename: "a" }]}));
        let mut other = document(json!({ "use": [
            { typename: ["a", "b"], "availability": "optional"}
        ]}));
        let result = document(json!({ "use": [
            { typename: "a" },
            { typename: "b", "availability": "optional" },
        ]}));

        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case("directory")]
    #[test_case("storage")]
    fn test_merge_from_duplicate_use_noarray(typename: &str) {
        let mut my = document(json!({ "use": [{ typename: "a", "path": "/a"}]}));
        let mut other = document(json!({ "use": [
            { typename: "a", "path": "/a", "availability": "optional" },
            { typename: "b", "path": "/b", "availability": "optional" },
        ]}));
        let result = document(json!({ "use": [
            { typename: "a", "path": "/a" },
            { typename: "b", "path": "/b", "availability": "optional" },
        ]}));
        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case("protocol")]
    #[test_case("service")]
    #[test_case("event_stream")]
    fn test_merge_from_duplicate_capabilities_array(typename: &str) {
        let mut my = document(json!({ "capabilities": [{ typename: "a" }]}));
        let mut other = document(json!({ "capabilities": [ { typename: ["a", "b"] } ]}));
        let result = document(json!({ "capabilities": [ { typename: "a" }, { typename: "b" } ]}));

        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case("directory")]
    #[test_case("storage")]
    #[test_case("runner")]
    #[test_case("resolver")]
    fn test_merge_from_duplicate_capabilities_noarray(typename: &str) {
        let mut my = document(json!({ "capabilities": [{ typename: "a", "path": "/a"}]}));
        let mut other = document(json!({ "capabilities": [
            { typename: "a", "path": "/a" },
            { typename: "b", "path": "/b" },
        ]}));
        let result = document(json!({ "capabilities": [
            { typename: "a", "path": "/a" },
            { typename: "b", "path": "/b" },
        ]}));
        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test]
    fn test_merge_with_empty_names() {
        // This document is an error because there is no capability name.
        let mut my = document(json!({ "capabilities": [{ "path": "/a"}]}));

        let mut other = document(json!({ "capabilities": [
            { "directory": "a", "path": "/a" },
            { "directory": "b", "path": "/b" },
        ]}));
        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap_err();
    }

    #[test_case("protocol")]
    #[test_case("service")]
    #[test_case("event_stream")]
    #[test_case("directory")]
    #[test_case("storage")]
    #[test_case("runner")]
    #[test_case("resolver")]
    fn test_merge_from_duplicate_offers(typename: &str) {
        let mut my = document(json!({ "offer": [{ typename: "a", "from": "self", "to": "#c" }]}));
        let mut other = document(json!({ "offer": [
            { typename: ["a", "b"], "from": "self", "to": "#c", "availability": "optional" }
        ]}));
        let result = document(json!({ "offer": [
            { typename: "a", "from": "self", "to": "#c" },
            { typename: "b", "from": "self", "to": "#c", "availability": "optional" },
        ]}));

        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case("protocol")]
    #[test_case("service")]
    #[test_case("event_stream")]
    #[test_case("directory")]
    #[test_case("runner")]
    #[test_case("resolver")]
    fn test_merge_from_duplicate_exposes(typename: &str) {
        let mut my = document(json!({ "expose": [{ typename: "a", "from": "self" }]}));
        let mut other = document(json!({ "expose": [
            { typename: ["a", "b"], "from": "self" }
        ]}));
        let result = document(json!({ "expose": [
            { typename: "a", "from": "self" },
            { typename: "b", "from": "self" },
        ]}));

        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case(
        document(json!({ "use": [
            { "protocol": "a", "availability": "required" },
            { "protocol": "b", "availability": "optional" },
            { "protocol": "c", "availability": "transitional" },
            { "protocol": "d", "availability": "same_as_target" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a"], "availability": "required" },
            { "protocol": ["b"], "availability": "optional" },
            { "protocol": ["c"], "availability": "transitional" },
            { "protocol": ["d"], "availability": "same_as_target" },
        ]})),
        document(json!({ "use": [
            { "protocol": "a", "availability": "required" },
            { "protocol": "b", "availability": "optional" },
            { "protocol": "c", "availability": "transitional" },
            { "protocol": "d", "availability": "same_as_target" },
        ]}))
        ; "merge both same"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": "a", "availability": "optional" },
            { "protocol": "b", "availability": "transitional" },
            { "protocol": "c", "availability": "transitional" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a", "x"], "availability": "required" },
            { "protocol": ["b", "y"], "availability": "optional" },
            { "protocol": ["c", "z"], "availability": "required" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a", "x"], "availability": "required" },
            { "protocol": ["b", "y"], "availability": "optional" },
            { "protocol": ["c", "z"], "availability": "required" },
        ]}))
        ; "merge with upgrade"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": "a", "availability": "required" },
            { "protocol": "b", "availability": "optional" },
            { "protocol": "c", "availability": "required" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a", "x"], "availability": "optional" },
            { "protocol": ["b", "y"], "availability": "transitional" },
            { "protocol": ["c", "z"], "availability": "transitional" },
        ]})),
        document(json!({ "use": [
            { "protocol": "a", "availability": "required" },
            { "protocol": "b", "availability": "optional" },
            { "protocol": "c", "availability": "required" },
            { "protocol": "x", "availability": "optional" },
            { "protocol": "y", "availability": "transitional" },
            { "protocol": "z", "availability": "transitional" },
        ]}))
        ; "merge with downgrade"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": "a", "availability": "optional" },
            { "protocol": "b", "availability": "transitional" },
            { "protocol": "c", "availability": "transitional" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a", "x"], "availability": "same_as_target" },
            { "protocol": ["b", "y"], "availability": "same_as_target" },
            { "protocol": ["c", "z"], "availability": "same_as_target" },
        ]})),
        document(json!({ "use": [
            { "protocol": "a", "availability": "optional" },
            { "protocol": "b", "availability": "transitional" },
            { "protocol": "c", "availability": "transitional" },
            { "protocol": ["a", "x"], "availability": "same_as_target" },
            { "protocol": ["b", "y"], "availability": "same_as_target" },
            { "protocol": ["c", "z"], "availability": "same_as_target" },
        ]}))
        ; "merge with no replacement"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": ["a", "b", "c"], "availability": "optional" },
            { "protocol": "d", "availability": "same_as_target" },
            { "protocol": ["e", "f"] },
        ]})),
        document(json!({ "use": [
            { "protocol": ["c", "e", "g"] },
            { "protocol": ["d", "h"] },
            { "protocol": ["f", "i"], "availability": "transitional" },
        ]})),
        document(json!({ "use": [
            { "protocol": ["a", "b"], "availability": "optional" },
            { "protocol": "d", "availability": "same_as_target" },
            { "protocol": ["e", "f"] },
            { "protocol": ["c", "g"] },
            { "protocol": ["d", "h"] },
            { "protocol": "i", "availability": "transitional" },
        ]}))
        ; "merge multiple"
    )]

    fn test_merge_from_duplicate_capability_availability(
        mut my: Document,
        mut other: Document,
        result: Document,
    ) {
        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case(
        document(json!({ "use": [{ "protocol": ["a", "b"] }]})),
        document(json!({ "use": [{ "protocol": ["c", "d"] }]})),
        document(json!({ "use": [
            { "protocol": ["a", "b"] }, { "protocol": ["c", "d"] }
        ]}))
        ; "merge capabilities with disjoint sets"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": ["a"] },
            { "protocol": "b" },
        ]})),
        document(json!({ "use": [{ "protocol": ["a", "b"] }]})),
        document(json!({ "use": [
            { "protocol": ["a"] }, { "protocol": "b" },
        ]}))
        ; "merge capabilities with equal set"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": ["a", "b"] },
            { "protocol": "c" },
        ]})),
        document(json!({ "use": [{ "protocol": ["a", "b"] }]})),
        document(json!({ "use": [
            { "protocol": ["a", "b"] }, { "protocol": "c" },
        ]}))
        ; "merge capabilities with subset"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": ["a", "b"] },
        ]})),
        document(json!({ "use": [{ "protocol": ["a", "b", "c"] }]})),
        document(json!({ "use": [
            { "protocol": ["a", "b"] },
            { "protocol": "c" },
        ]}))
        ; "merge capabilities with superset"
    )]
    #[test_case(
        document(json!({ "use": [
            { "protocol": ["a", "b"] },
        ]})),
        document(json!({ "use": [{ "protocol": ["b", "c", "d"] }]})),
        document(json!({ "use": [
            { "protocol": ["a", "b"] }, { "protocol": ["c", "d"] }
        ]}))
        ; "merge capabilities with intersection"
    )]
    #[test_case(
        document(json!({ "use": [{ "protocol": ["a", "b"] }]})),
        document(json!({ "use": [
            { "protocol": ["c", "b", "d"] },
            { "protocol": ["e", "d"] },
        ]})),
        document(json!({ "use": [
            {"protocol": ["a", "b"] },
            {"protocol": ["c", "d"] },
            {"protocol": "e" }]}))
        ; "merge capabilities from multiple arrays"
    )]
    #[test_case(
        document(json!({ "use": [{ "protocol": "foo.bar.Baz", "from": "self"}]})),
        document(json!({ "use": [{ "service": "foo.bar.Baz", "from": "self"}]})),
        document(json!({ "use": [
            {"protocol": "foo.bar.Baz", "from": "self"},
            {"service": "foo.bar.Baz", "from": "self"}]}))
        ; "merge capabilities, types don't match"
    )]
    #[test_case(
        document(json!({ "use": [{ "protocol": "foo.bar.Baz", "from": "self"}]})),
        document(json!({ "use": [{ "protocol": "foo.bar.Baz" }]})),
        document(json!({ "use": [
            {"protocol": "foo.bar.Baz", "from": "self"},
            {"protocol": "foo.bar.Baz"}]}))
        ; "merge capabilities, fields don't match"
    )]

    fn test_merge_from_duplicate_capability(
        mut my: Document,
        mut other: Document,
        result: Document,
    ) {
        my.merge_from(&mut other, &path::Path::new("some/path")).unwrap();
        assert_eq!(my, result);
    }

    #[test_case(&Right::Connect; "connect right")]
    #[test_case(&Right::Enumerate; "enumerate right")]
    #[test_case(&Right::Execute; "execute right")]
    #[test_case(&Right::GetAttributes; "getattr right")]
    #[test_case(&Right::ModifyDirectory; "modifydir right")]
    #[test_case(&Right::ReadBytes; "readbytes right")]
    #[test_case(&Right::Traverse; "traverse right")]
    #[test_case(&Right::UpdateAttributes; "updateattrs right")]
    #[test_case(&Right::WriteBytes; "writebytes right")]
    #[test_case(&Right::ReadAlias; "r right")]
    #[test_case(&Right::WriteAlias; "w right")]
    #[test_case(&Right::ExecuteAlias; "x right")]
    #[test_case(&Right::ReadWriteAlias; "rw right")]
    #[test_case(&Right::ReadExecuteAlias; "rx right")]
    #[test_case(&OfferFromRef::Self_; "offer from self")]
    #[test_case(&OfferFromRef::Parent; "offer from parent")]
    #[test_case(&OfferFromRef::Named(Name::new("child".to_string()).unwrap()); "offer from named")]
    #[test_case(
        &document(json!({}));
        "empty document"
    )]
    #[test_case(
        &document(json!({ "use": [{ "protocol": "foo.bar.Baz", "from": "self"}]}));
        "use one from self"
    )]
    #[test_case(
        &document(json!({ "use": [{ "protocol": ["foo.bar.Baz", "some.other.Protocol"], "from": "self"}]}));
        "use multiple from self"
    )]
    #[test_case(
        &document(json!({
            "offer": [{ "protocol": "foo.bar.Baz", "from": "self", "to": "#elements"}],
            "collections" :[{"name": "elements", "durability": "transient" }]
        }));
        "offer from self to collection"
    )]
    #[test_case(
        &document(json!({
            "offer": [
                { "service": "foo.bar.Baz", "from": "self", "to": "#elements" },
                { "service": "some.other.Service", "from": "self", "to": "#elements"},
            ],
            "collections":[ {"name": "elements", "durability": "transient"} ]}));
        "service offers"
    )]
    #[test_case(
        &document(json!({ "expose": [{ "protocol": ["foo.bar.Baz", "some.other.Protocol"], "from": "self"}]}));
        "expose protocols from self"
    )]
    #[test_case(
        &document(json!({ "expose": [{ "service": ["foo.bar.Baz", "some.other.Service"], "from": "self"}]}));
        "expose service from self"
    )]
    #[test_case(
        &document(json!({ "capabilities": [{ "protocol": "foo.bar.Baz", "from": "self"}]}));
        "capabilities from self"
    )]
    #[test_case(
        &document(json!({ "facets": { "my.key": "my.value" } }));
        "facets"
    )]
    #[test_case(
        &document(json!({ "program": { "binary": "bin/hello_world", "runner": "elf" } }));
        "elf runner program"
    )]
    fn serialize_roundtrips<T>(val: &T)
    where
        T: serde::de::DeserializeOwned + Serialize + PartialEq + std::fmt::Debug,
    {
        let raw = serde_json::to_string(val).expect("serializing `val` should work");
        let parsed: T =
            serde_json::from_str(&raw).expect("must be able to parse back serialized value");
        assert_eq!(val, &parsed, "parsed value must equal original value");
    }
}
