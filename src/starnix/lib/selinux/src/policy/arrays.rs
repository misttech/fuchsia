// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::ValidateError;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};
use super::view::{ArrayView, Walk};
use super::{Array, ClassId, Counted, MlsRange, Parse, PolicyValidationContext, TypeId, Validate};
use crate::new_policy::Context;
use crate::new_policy::traits::PolicyId;
use crate::policy::view::Hashable;
use anyhow::Context as _;
use std::hash::{Hash, Hasher};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

#[allow(type_alias_bounds)]
pub(super) type SimpleArray<T> = Array<le::U32, T>;

impl<T: Validate> Validate for SimpleArray<T> {
    type Error = <T as Validate>::Error;
    /// Default implementation of `Validate` for `SimpleArray<T>`, validating individual T
    /// objects. It assumes no internal constraints between the objects.
    /// Override this function for types with more complex validation requirements.
    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        self.data.validate(context)
    }
}

pub(super) type SimpleArrayView<T> = ArrayView<le::U32, T>;

impl<T: Validate + Parse + Walk> Validate for SimpleArrayView<T> {
    type Error = anyhow::Error;

    /// Defers to `self.data` for validation. `self.data` has access to all information, including
    /// size stored in `self.metadata`.
    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        for item in self.data().iter(&context.data) {
            item.validate(context)?;
        }
        Ok(())
    }
}

impl Counted for le::U32 {
    fn count(&self) -> u32 {
        self.get()
    }
}

impl Validate for GenericFsContext {
    type Error = anyhow::Error;

    /// TODO: Validate sequence of  [`GenericFsContext`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Information parsed parsed from `genfscon [fs_type] [partial_path] [fs_context]` statements
/// about a specific filesystem type.
#[derive(Debug)]
pub(super) struct GenericFsContext {
    fs_type: SimpleArray<u8>,
    fs_context: SimpleArrayView<FsContext>,
}

impl GenericFsContext {
    /// Returns the `fs_type` representation to be used when looking up in a CustomKeyHashedView.
    pub(super) fn for_query(fs_type: &str) -> SimpleArray<u8> {
        Array { data: fs_type.as_bytes().to_vec(), metadata: le::U32::new(fs_type.len() as u32) }
    }
}

impl Parse for GenericFsContext {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (fs_type, tail) = SimpleArray::<u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing fs_type for generic fs context")?;

        let (fs_context, tail) = SimpleArrayView::<FsContext>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing fs_context for generic fs context")?;

        Ok((Self { fs_type, fs_context }, tail))
    }
}

impl Hashable for GenericFsContext {
    type Key = SimpleArray<u8>;
    type Value = FsContext;

    fn key(&self) -> &Self::Key {
        &self.fs_type
    }

    fn values(&self) -> &SimpleArrayView<Self::Value> {
        &self.fs_context
    }
}

impl Eq for SimpleArray<u8> {}

impl Hash for SimpleArray<u8> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.data.hash(state);
    }
}

impl SimpleArrayView<FsContext> {
    fn try_validate_alphabetic_order(&self, context: &PolicyValidationContext) -> bool {
        self.data()
            .iter(&context.data)
            .map(|view| view.parse(&context.data).partial_path().to_vec())
            .is_sorted_by(|a, b| a <= b)
    }

    fn try_validate_length_descending_order(&self, context: &PolicyValidationContext) -> bool {
        self.data()
            .iter(&context.data)
            .map(|view| view.parse(&context.data).partial_path().len())
            .is_sorted_by(|a, b| a >= b)
    }
}

impl Validate for SimpleArrayView<FsContext> {
    type Error = anyhow::Error;

    /// Checks that the sequence of [`FsContext`] objects is valid.
    /// To be valid, FsContexts must be sorted by either:
    /// - the length of sub-paths (descending order).
    /// - alphabetically by sub-paths (ascending order).
    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        if !self.try_validate_alphabetic_order(context)
            && !self.try_validate_length_descending_order(context)
        {
            return Err(anyhow::anyhow!(
                "FsContexts must be sorted by partial path length (descending) or alphabetically.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct FsContext {
    /// The partial path, relative to the root of the filesystem. The partial path can only be set for
    /// virtual filesystems, like `proc/`. Otherwise, this must be `/`
    partial_path: SimpleArray<u8>,
    /// Optional. When provided, the context will only be applied to files of this type. Allowed files
    /// types are: blk_file, chr_file, dir, fifo_file, lnk_file, sock_file, file. When set to 0, the
    /// context applies to all file types.
    class: le::U32,
    /// The security context allocated to the filesystem.
    context: Context,
}

impl FsContext {
    pub(super) fn partial_path(&self) -> &[u8] {
        &self.partial_path.data
    }

    pub(super) fn context(&self) -> &Context {
        &self.context
    }

    pub(super) fn class(&self) -> Option<ClassId> {
        ClassId::try_from(self.class.get()).ok()
    }
}

impl Parse for FsContext
where
    SimpleArray<u8>: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (partial_path, tail) = SimpleArray::<u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing filesystem context partial path")?;

        let (class, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for filesystem context")?;

        Ok((Self { partial_path, class, context }, tail))
    }
}

impl Walk for FsContext {
    fn walk(policy_data: &PolicyData, offset: PolicyOffset) -> PolicyOffset {
        let cursor = PolicyCursor::new_at(policy_data, offset);
        let (_, tail) = FsContext::parse(cursor)
            .map_err(Into::<anyhow::Error>::into)
            .expect("policy should be valid");
        tail.offset()
    }
}

impl Validate for RangeTransition {
    type Error = anyhow::Error;
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        if self.metadata.target_class.get() == 0 {
            return Err(ValidateError::NonOptionalIdIsZero.into());
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct RangeTransition {
    metadata: RangeTransitionMetadata,
    mls_range: MlsRange,
}

impl RangeTransition {
    pub fn source_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.source_type.get()).unwrap()
    }

    pub fn target_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.target_type.get()).unwrap()
    }

    pub fn target_class(&self) -> ClassId {
        ClassId::try_from(self.metadata.target_class.get()).unwrap()
    }

    pub fn mls_range(&self) -> &MlsRange {
        &self.mls_range
    }
}

impl Parse for RangeTransition
where
    MlsRange: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) = PolicyCursor::parse::<RangeTransitionMetadata>(tail)
            .context("parsing range transition metadata")?;

        let (mls_range, tail) = MlsRange::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing mls range for range transition")?;

        Ok((Self { metadata, mls_range }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct RangeTransitionMetadata {
    source_type: le::U32,
    target_type: le::U32,
    target_class: le::U32,
}
