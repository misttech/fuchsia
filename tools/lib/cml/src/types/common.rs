// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{Path, byte_index_to_location};
use json_spanned_value::Spanned;

use crate::error::{Error, Location};
use cm_types::BorrowedName;

use crate::OneOrMany;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    pub file: Arc<PathBuf>,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextSpanned<T> {
    pub value: T,
    pub origin: Origin,
}

/// Hydrate is used to translate a json_spanned::Spanned type to a
/// ContextSpanned type. The ContextSpanned type is used by validation.
pub trait Hydrate {
    type Output;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Self::Output;
}

pub fn hydrate_list<P, C>(
    raw_list: Option<Spanned<Vec<Spanned<P>>>>,
    file: &Arc<PathBuf>,
    buffer: &String,
) -> Option<Vec<ContextSpanned<C>>>
where
    P: Hydrate<Output = C>,
{
    raw_list.map(|spanned_vec| {
        spanned_vec
            .into_inner()
            .into_iter()
            .map(|spanned_item| {
                let span = spanned_item.span();
                let location = byte_index_to_location(buffer, span.0);
                let parsed_item = spanned_item.into_inner();

                let context_item = parsed_item.hydrate(file, buffer);

                ContextSpanned {
                    value: context_item,
                    origin: Origin { file: file.clone(), location },
                }
            })
            .collect()
    })
}

pub fn hydrate_required<P, C>(
    spanned: Spanned<P>,
    file: &Arc<PathBuf>,
    buffer: &String,
) -> ContextSpanned<C>
where
    P: Hydrate<Output = C>,
{
    let span = spanned.span();
    let location = byte_index_to_location(buffer, span.0);
    let parsed_value = spanned.into_inner();
    ContextSpanned {
        value: parsed_value.hydrate(file, buffer),
        origin: Origin { file: file.clone(), location },
    }
}

pub fn hydrate_simple<T>(
    spanned: Spanned<T>,
    file: &Arc<PathBuf>,
    buffer: &String,
) -> ContextSpanned<T> {
    let span = spanned.span();
    let location = byte_index_to_location(buffer, span.0);
    ContextSpanned { value: spanned.into_inner(), origin: Origin { file: file.clone(), location } }
}

pub fn hydrate_opt<P, C>(
    opt_spanned: Option<Spanned<P>>,
    file: &Arc<PathBuf>,
    buffer: &String,
) -> Option<ContextSpanned<C>>
where
    P: Hydrate<Output = C>,
{
    opt_spanned.map(|s| hydrate_required(s, file, buffer))
}

pub fn hydrate_opt_simple<T>(
    opt_spanned: Option<Spanned<T>>,
    file: &Arc<PathBuf>,
    buffer: &String,
) -> Option<ContextSpanned<T>> {
    opt_spanned.map(|s| hydrate_simple(s, file, buffer))
}

pub fn option_one_or_many_as_ref_context<T, S: ?Sized>(
    o: &Option<ContextSpanned<OneOrMany<T>>>,
) -> Option<ContextSpanned<OneOrMany<&S>>>
where
    T: AsRef<S>,
{
    o.as_ref().map(|spanned| ContextSpanned {
        origin: spanned.origin.clone(),
        value: match &spanned.value {
            OneOrMany::One(item) => OneOrMany::One(item.as_ref()),
            OneOrMany::Many(items) => {
                OneOrMany::Many(items.iter().map(|item| item.as_ref()).collect())
            }
        },
    })
}

pub trait ContextPathClause {
    fn path(&self) -> Option<&ContextSpanned<Path>>;
}

pub trait ContextCapabilityClause {
    fn service(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn protocol(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn directory(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn storage(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn runner(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn resolver(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn dictionary(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn config(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;
    fn event_stream(&self) -> Option<ContextSpanned<OneOrMany<&BorrowedName>>>;

    // /// Returns the name of the capability for display purposes.
    // /// If `service()` returns `Some`, the capability name must be "service", etc.
    // ///
    // /// Returns an error if the capability name is not set, or if there is more than one.
    fn capability_type(&self, origin: Option<Origin>) -> Result<&'static str, Error> {
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
                Err(Error::validate_context(
                    format!(
                        "`{}` declaration is missing a capability keyword, one of: {}",
                        self.decl_type(),
                        supported_keywords,
                    ),
                    origin,
                ))
            }
            1 => Ok(types[0]),
            _ => Err(Error::validate_context(
                format!(
                    "{} declaration has multiple capability types defined: {:?}",
                    self.decl_type(),
                    types
                ),
                origin,
            )),
        }
    }

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
            .map(|o| {
                o.map(|o| o.value.into_iter().collect::<Vec<&BorrowedName>>()).unwrap_or(vec![])
            })
            .flatten()
            .collect()
    }

    /// Returns true if this capability type allows the ::Many variant of OneOrMany.
    fn are_many_names_allowed(&self) -> bool;

    fn decl_type(&self) -> &'static str;
    fn supported(&self) -> &[&'static str];
}
