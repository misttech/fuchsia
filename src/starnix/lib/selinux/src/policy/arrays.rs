// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::policy::view::Hashable;

use super::error::ValidateError;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};
use super::view::{ArrayView, Walk};
use super::{
    Array, ClassId, Counted, MlsLevel, MlsRange, Parse, PolicyValidationContext, RoleId, TypeId,
    UserId, Validate,
};
use crate::new_policy::traits::PolicyId;
use anyhow::Context as _;
use std::hash::{Hash, Hasher};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

pub(super) const MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY: u32 = 31;

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

#[derive(Debug, PartialEq)]
pub(super) struct Context {
    metadata: ContextMetadata,
    mls_range: MlsRange,
}

impl Context {
    pub(super) fn user_id(&self) -> UserId {
        UserId::from_u32(self.metadata.user.get()).unwrap()
    }
    pub(super) fn role_id(&self) -> RoleId {
        RoleId::from_u32(self.metadata.role.get()).unwrap()
    }
    pub(super) fn type_id(&self) -> TypeId {
        TypeId::from_u32(self.metadata.context_type.get()).unwrap()
    }
    pub(super) fn low_level(&self) -> &MlsLevel {
        self.mls_range.low()
    }
    pub(super) fn high_level(&self) -> &Option<MlsLevel> {
        self.mls_range.high()
    }
}

impl Parse for Context
where
    MlsRange: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) =
            PolicyCursor::parse::<ContextMetadata>(tail).context("parsing metadata for context")?;

        let (mls_range, tail) = MlsRange::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing mls range for context")?;

        Ok((Self { metadata, mls_range }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct ContextMetadata {
    user: le::U32,
    role: le::U32,
    context_type: le::U32,
}

impl Validate for NamedContextPair {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`NamedContextPairs`] objects.
    ///
    /// TODO: Is different validation required for `filesystems` and `network_interfaces`? If so,
    /// create wrapper types with different [`Validate`] implementations.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct NamedContextPair {
    name: SimpleArray<u8>,
    context1: Context,
    context2: Context,
}

impl Parse for NamedContextPair
where
    SimpleArray<u8>: Parse,
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (name, tail) = SimpleArray::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing filesystem context name")?;

        let (context1, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing first context for filesystem context")?;

        let (context2, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing second context for filesystem context")?;

        Ok((Self { name, context1, context2 }, tail))
    }
}

impl Validate for Port {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`Ports`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Port {
    metadata: PortMetadata,
    context: Context,
}

impl Parse for Port
where
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) =
            PolicyCursor::parse::<PortMetadata>(tail).context("parsing metadata for context")?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for port")?;

        Ok((Self { metadata, context }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct PortMetadata {
    protocol: le::U32,
    low_port: le::U32,
    high_port: le::U32,
}

impl Validate for Node {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`Node`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Node {
    address: le::U32,
    mask: le::U32,
    context: Context,
}

impl Parse for Node
where
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (address, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (mask, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for node")?;

        Ok((Self { address, mask, context }, tail))
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct FsUse {
    behavior_and_name: Array<FsUseMetadata, u8>,
    context: Context,
}

impl FsUse {
    pub fn fs_type(&self) -> &[u8] {
        &self.behavior_and_name.data
    }

    pub(super) fn behavior(&self) -> FsUseType {
        FsUseType::try_from(self.behavior_and_name.metadata.behavior).unwrap()
    }

    pub(super) fn context(&self) -> &Context {
        &self.context
    }
}

impl Parse for FsUse
where
    Array<FsUseMetadata, u8>: Parse,
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (behavior_and_name, tail) = Array::<FsUseMetadata, u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing fs use metadata")?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for fs use")?;

        Ok((Self { behavior_and_name, context }, tail))
    }
}

impl Validate for FsUse {
    type Error = anyhow::Error;

    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        FsUseType::try_from(self.behavior_and_name.metadata.behavior)?;

        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct FsUseMetadata {
    /// The type of `fs_use` statement.
    behavior: le::U32,
    /// The length of the name in the name_and_behavior field of FsUse.
    name_length: le::U32,
}

impl Counted for FsUseMetadata {
    fn count(&self) -> u32 {
        self.name_length.get()
    }
}

/// Discriminates among the different kinds of "fs_use_*" labeling statements in the policy; see
/// https://selinuxproject.org/page/FileStatements.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum FsUseType {
    Xattr = 1,
    Trans = 2,
    Task = 3,
}

impl TryFrom<le::U32> for FsUseType {
    type Error = anyhow::Error;

    fn try_from(value: le::U32) -> Result<Self, Self::Error> {
        match value.get() {
            1 => Ok(FsUseType::Xattr),
            2 => Ok(FsUseType::Trans),
            3 => Ok(FsUseType::Task),
            _ => Err(ValidateError::InvalidFsUseType { value: value.get() }.into()),
        }
    }
}

impl Validate for IPv6Node {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`IPv6Node`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct IPv6Node {
    address: [le::U32; 4],
    mask: [le::U32; 4],
    context: Context,
}

impl Parse for IPv6Node
where
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (address, tail) = PolicyCursor::parse::<[le::U32; 4]>(tail)?;

        let (mask, tail) = PolicyCursor::parse::<[le::U32; 4]>(tail)?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for ipv6 node")?;

        Ok((Self { address, mask, context }, tail))
    }
}

impl Validate for InfinitiBandPartitionKey {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`InfinitiBandPartitionKey`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct InfinitiBandPartitionKey {
    low: le::U32,
    high: le::U32,
    context: Context,
}

impl Parse for InfinitiBandPartitionKey
where
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (low, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (high, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for infiniti band partition key")?;

        Ok((Self { low, high, context }, tail))
    }
}

impl Validate for InfinitiBandEndPort {
    type Error = anyhow::Error;

    /// TODO: Validate sequence of [`InfinitiBandEndPort`] objects.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct InfinitiBandEndPort {
    port_and_name: Array<InfinitiBandEndPortMetadata, u8>,
    context: Context,
}

impl Parse for InfinitiBandEndPort
where
    Array<InfinitiBandEndPortMetadata, u8>: Parse,
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (port_and_name, tail) = Array::<InfinitiBandEndPortMetadata, u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing infiniti band end port metadata")?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for infiniti band end port")?;

        Ok((Self { port_and_name, context }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct InfinitiBandEndPortMetadata {
    length: le::U32,
    port: le::U32,
}

impl Counted for InfinitiBandEndPortMetadata {
    fn count(&self) -> u32 {
        self.length.get()
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
    Context: Parse,
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
