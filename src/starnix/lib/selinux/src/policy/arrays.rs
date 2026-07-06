// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::policy::view::Hashable;

use super::error::ValidateError;
use super::extensible_bitmap::ExtensibleBitmap;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};
use super::view::{ArrayView, HasMetadata, Walk};
use super::{
    AccessVector, Array, ClassId, Counted, MlsLevel, MlsRange, Parse, PolicyValidationContext,
    RoleId, TypeId, UserId, Validate, ValidateArray, array_type, array_type_validate_deref_both,
};

use crate::new_policy::traits::PolicyId;
use anyhow::Context as _;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::ops::Shl;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

pub(super) const MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY: u32 = 31;

/// Mask for [`AccessVectorRuleMetadata`]'s `access_vector_rule_type` that
/// indicates that the access vector rule's associated data is a type ID.
pub(super) const ACCESS_VECTOR_RULE_DATA_IS_TYPE_ID_MASK: u16 = 0x070;
/// Mask for [`AccessVectorRuleMetadata`]'s `access_vector_rule_type` that
/// indicates that the access vector rule's associated data is an extended
/// permission.
pub(super) const ACCESS_VECTOR_RULE_DATA_IS_XPERM_MASK: u16 = 0x0700;

/// ** Access vector rule types ***
///
/// Although these values each have a single bit set, they appear to be
/// used as enum values rather than as bit masks: i.e., the policy compiler
/// does not produce access vector rule structures that have more than
/// one of these types.
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from an `allow [source]
/// [target]:[class] { [permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_ALLOW: u16 = 0x1;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from an `auditallow [source]
/// [target]:[class] { [permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_AUDITALLOW: u16 = 0x2;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from a `dontaudit [source]
/// [target]:[class] { [permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_DONTAUDIT: u16 = 0x4;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from a `type_transition
/// [source] [target]:[class] [new_type];` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_TYPE_TRANSITION: u16 = 0x10;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from a `type_member
/// [source] [target]:[class] [member_type];` policy statement.
#[allow(dead_code)]
pub(super) const ACCESS_VECTOR_RULE_TYPE_TYPE_MEMBER: u16 = 0x20;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type` that
/// indicates that the access vector rule comes from a `type_change
/// [source] [target]:[class] [change_type];` policy statement.
#[allow(dead_code)]
pub(super) const ACCESS_VECTOR_RULE_TYPE_TYPE_CHANGE: u16 = 0x40;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type`
/// that indicates that the access vector rule comes from an
/// `allowxperm [source] [target]:[class] [permission] {
/// [extended_permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_ALLOWXPERM: u16 = 0x100;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type`
/// that indicates that the access vector rule comes from an
/// `auditallowxperm [source] [target]:[class] [permission] {
/// [extended_permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_AUDITALLOWXPERM: u16 = 0x200;
/// Value for [`AccessVectorRuleMetadata`] `access_vector_rule_type`
/// that indicates that the access vector rule comes from an
/// `dontauditxperm [source] [target]:[class] [permission] {
/// [extended_permissions] };` policy statement.
pub(super) const ACCESS_VECTOR_RULE_TYPE_DONTAUDITXPERM: u16 = 0x400;

/// ** Extended permissions types ***
///
/// Value for [`ExtendedPermissions`] `xperms_type` that indicates
/// that the xperms set is a proper subset of the 16-bit ioctl
/// xperms with a given high byte value.
pub(super) const XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES: u8 = 1;
/// Value for [`ExtendedPermissions`] `xperms_type` that indicates
/// that the xperms set consists of all 16-bit ioctl xperms with a
/// given high byte, for one or more high byte values.
pub(super) const XPERMS_TYPE_IOCTL_PREFIXES: u8 = 2;
/// Value for [`ExtendedPermissions`] `xperms_type` that indicates
/// that the xperms set consists of 16-bit `nlmsg` xperms with a given
/// high byte value in common. The xperms set may be the full set of
/// xperms with that high byte value (unlike a set of type
/// `XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES`).
pub(super) const XPERMS_TYPE_NLMSG: u8 = 3;

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

impl Validate for ConditionalNode {
    type Error = anyhow::Error;

    // TODO: Validate [`ConditionalNodeMetadata`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

array_type!(ConditionalNodeItems, ConditionalNodeMetadata, ConditionalNodeDatum);

array_type_validate_deref_both!(ConditionalNodeItems);

impl ValidateArray<ConditionalNodeMetadata, ConditionalNodeDatum> for ConditionalNodeItems {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency between [`ConditionalNodeMetadata`] consecutive
    /// [`ConditionalNodeDatum`].
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &ConditionalNodeMetadata,
        _items: &[ConditionalNodeDatum],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct ConditionalNode {
    items: ConditionalNodeItems,
    true_list: SimpleArray<AccessVectorRule>,
    false_list: SimpleArray<AccessVectorRule>,
}

impl Parse for ConditionalNode
where
    ConditionalNodeItems: Parse,
    SimpleArray<AccessVectorRule>: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (items, tail) = ConditionalNodeItems::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing conditional node items")?;

        let (true_list, tail) = SimpleArray::<AccessVectorRule>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing conditional node true list")?;

        let (false_list, tail) = SimpleArray::<AccessVectorRule>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing conditional node false list")?;

        Ok((Self { items, true_list, false_list }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct ConditionalNodeMetadata {
    state: le::U32,
    count: le::U32,
}

impl Counted for ConditionalNodeMetadata {
    fn count(&self) -> u32 {
        self.count.get()
    }
}

impl Validate for ConditionalNodeMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate [`ConditionalNodeMetadata`] internals.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct ConditionalNodeDatum {
    node_type: le::U32,
    boolean: le::U32,
}

impl Validate for ConditionalNodeDatum {
    type Error = anyhow::Error;

    /// TODO: Validate sequence of [`ConditionalNodeDatum`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// An access control rule defined by a policy statement of one of the
/// following kinds:
/// - `allow`, `dontaudit`, `auditallow`, and `neverallow`, which specify
///   an access vector describing a permission set.
/// - `allowxperm`, `auditallowxperm`, `dontaudit`, which specify a set
///   of extended permissions.
/// - `type_transition`, `type_change`, and `type_member`, which include
///   a type id describing a permitted new type.
#[derive(Debug, PartialEq)]
pub(super) struct AccessVectorRule {
    metadata: AccessVectorRuleMetadata,
    permission_data: PermissionData,
}

impl AccessVectorRule {
    /// An access vector that corresponds to the `[access_vector]` in an
    /// `allow [source] [target]:[class] [access_vector]` policy statement,
    /// or similarly for an `auditallow` or `dontaudit` policy statement.
    /// Return value is `None` if this access vector rule corresponds to a
    /// different kind of policy statement.
    pub fn access_vector(&self) -> Option<AccessVector> {
        match &self.permission_data {
            PermissionData::AccessVector(access_vector_raw) => {
                Some(AccessVector::from(access_vector_raw.get()))
            }
            _ => None,
        }
    }

    /// A numeric type id that corresponds to the `[new_type]` in a
    /// `type_transition [source] [target]:[class] [new_type];` policy statement,
    /// or similarly for a `type_member` or `type_change` policy statement.
    /// Return value is `None` if this access vector rule corresponds to a
    /// different kind of policy statement.
    pub fn new_type(&self) -> Option<TypeId> {
        match &self.permission_data {
            PermissionData::NewType(new_type) => {
                Some(TypeId::from_u32(new_type.get().into()).unwrap())
            }
            _ => None,
        }
    }

    /// A set of extended permissions that corresponds to the `[xperms]` in an
    /// `allowxperm [source][target]:[class] [permission] [xperms]` policy
    /// statement, or similarly for an `auditallowxperm` or `dontauditxperm`
    /// policy statement. Return value is `None` if this access vector rule
    /// corresponds to a different kind of policy statement.
    pub fn extended_permissions(&self) -> Option<&ExtendedPermissions> {
        match &self.permission_data {
            PermissionData::ExtendedPermissions(xperms) => Some(xperms),
            _ => None,
        }
    }
}

impl Walk for AccessVectorRule {
    fn walk(policy_data: &PolicyData, offset: PolicyOffset) -> PolicyOffset {
        const METADATA_SIZE: u32 = std::mem::size_of::<AccessVectorRuleMetadata>() as u32;
        let bytes = &policy_data[offset as usize..(offset + METADATA_SIZE) as usize];
        let metadata = AccessVectorRuleMetadata::read_from_bytes(bytes).unwrap();
        let permission_data_size = metadata.permission_data_size() as u32;
        offset + METADATA_SIZE + permission_data_size
    }
}

impl HasMetadata for AccessVectorRule {
    type Metadata = AccessVectorRuleMetadata;
}

impl Parse for AccessVectorRule {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) = PolicyCursor::parse::<AccessVectorRuleMetadata>(tail)?;
        let access_vector_rule_type = metadata.access_vector_rule_type;
        let (permission_data, tail) =
            if (access_vector_rule_type & ACCESS_VECTOR_RULE_DATA_IS_XPERM_MASK) != 0 {
                let (xperms, tail) = ExtendedPermissions::parse(tail)
                    .map_err(Into::<anyhow::Error>::into)
                    .context("parsing extended permissions")?;
                (PermissionData::ExtendedPermissions(xperms), tail)
            } else if (access_vector_rule_type & ACCESS_VECTOR_RULE_DATA_IS_TYPE_ID_MASK) != 0 {
                let (new_type, tail) = PolicyCursor::parse::<le::U32>(tail)?;
                (PermissionData::NewType(new_type), tail)
            } else {
                let (access_vector, tail) = PolicyCursor::parse::<le::U32>(tail)?;
                (PermissionData::AccessVector(access_vector), tail)
            };
        Ok((Self { metadata, permission_data }, tail))
    }
}

impl Validate for AccessVectorRule {
    type Error = anyhow::Error;

    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        if self.metadata.class.get() == 0 {
            return Err(ValidateError::NonOptionalIdIsZero.into());
        }
        if let PermissionData::ExtendedPermissions(xperms) = &self.permission_data {
            let xperms_type = xperms.xperms_type;
            if !(xperms_type == XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES
                || xperms_type == XPERMS_TYPE_IOCTL_PREFIXES
                || xperms_type == XPERMS_TYPE_NLMSG)
            {
                return Err(
                    ValidateError::InvalidExtendedPermissionsType { type_: xperms_type }.into()
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, Eq, PartialEq, Unaligned, Hash)]
#[repr(C, packed)]
pub(super) struct AccessVectorRuleMetadata {
    source_type: le::U16,
    target_type: le::U16,
    class: le::U16,
    access_vector_rule_type: le::U16,
}

impl AccessVectorRuleMetadata {
    pub fn for_query(source: TypeId, target: TypeId, class: ClassId, rule_type: u16) -> Self {
        let source_type = le::U16::new(source.as_u32() as u16);
        let target_type = le::U16::new(target.as_u32() as u16);
        let class = le::U16::new(class.as_u32() as u16);
        let access_vector_rule_type = le::U16::new(rule_type);
        Self { source_type, target_type, class, access_vector_rule_type }
    }

    fn permission_data_size(&self) -> usize {
        if (self.access_vector_rule_type & ACCESS_VECTOR_RULE_DATA_IS_XPERM_MASK) != 0 {
            std::mem::size_of::<ExtendedPermissions>()
        } else if (self.access_vector_rule_type & ACCESS_VECTOR_RULE_DATA_IS_TYPE_ID_MASK) != 0 {
            std::mem::size_of::<le::U32>()
        } else {
            std::mem::size_of::<le::U32>()
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum PermissionData {
    AccessVector(le::U32),
    NewType(le::U32),
    ExtendedPermissions(ExtendedPermissions),
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct ExtendedPermissions {
    pub(super) xperms_type: u8,
    // xperms_optional_prefix is meaningful when xperms_type is
    // XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES or XPERMS_TYPE_NLMSG and
    // meaningless when xperms_type is XPERMS_TYPE_IOCTL_PREFIXES.
    pub(super) xperms_optional_prefix: u8,
    pub(super) xperms_bitmap: XpermsBitmap,
}

impl ExtendedPermissions {
    #[cfg(test)]
    fn count(&self) -> u64 {
        let count = self
            .xperms_bitmap
            .0
            .iter()
            .fold(0, |count, block| (count as u64) + (block.get().count_ones() as u64));
        match self.xperms_type {
            XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES | XPERMS_TYPE_NLMSG => count,
            XPERMS_TYPE_IOCTL_PREFIXES => count * 0x100,
            _ => unreachable!("invalid xperms_type in validated ExtendedPermissions"),
        }
    }

    #[cfg(test)]
    fn contains(&self, xperm: u16) -> bool {
        let [postfix, prefix] = xperm.to_le_bytes();
        if (self.xperms_type == XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES
            || self.xperms_type == XPERMS_TYPE_NLMSG)
            && self.xperms_optional_prefix != prefix
        {
            return false;
        }
        let value = match self.xperms_type {
            XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES | XPERMS_TYPE_NLMSG => postfix,
            XPERMS_TYPE_IOCTL_PREFIXES => prefix,
            _ => unreachable!("invalid xperms_type in validated ExtendedPermissions"),
        };
        self.xperms_bitmap.contains(value)
    }
}

// A bitmap representing a subset of `{0x0,...,0xff}`.
#[derive(Clone, Copy, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub struct XpermsBitmap([le::U32; 8]);

impl XpermsBitmap {
    const BITMAP_BLOCKS: usize = 8;
    pub const ALL: Self = Self([le::U32::MAX_VALUE; Self::BITMAP_BLOCKS]);
    pub const NONE: Self = Self([le::U32::ZERO; Self::BITMAP_BLOCKS]);

    #[cfg(test)]
    pub fn new(elements: [le::U32; 8]) -> Self {
        Self(elements)
    }

    pub fn contains(&self, value: u8) -> bool {
        let block_index = (value as usize) / 32;
        let bit_index = ((value as usize) % 32) as u32;
        self.0[block_index] & le::U32::new(1).shl(bit_index) != 0
    }
}

/// The xperms cache uses a u64-based representation.
impl From<[u64; 4]> for XpermsBitmap {
    fn from(v: [u64; 4]) -> Self {
        let mut elements = [le::U32::ZERO; 8];
        for (i, &val) in v.iter().enumerate() {
            elements[i * 2] = le::U32::new(val as u32);
            elements[i * 2 + 1] = le::U32::new((val >> 32) as u32);
        }
        XpermsBitmap(elements)
    }
}

impl From<XpermsBitmap> for [u64; 4] {
    fn from(v: XpermsBitmap) -> Self {
        let mut result = [0u64; 4];
        for i in 0..4 {
            let low = v.0[i * 2].get() as u64;
            let high = v.0[i * 2 + 1].get() as u64;
            result[i] = low | (high << 32);
        }
        result
    }
}

impl std::ops::BitAnd for XpermsBitmap {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        let mut result = self;
        (0..Self::BITMAP_BLOCKS).for_each(|i| result.0[i] &= rhs.0[i]);
        result
    }
}

impl std::ops::BitOr for XpermsBitmap {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        let mut result = self;
        (0..Self::BITMAP_BLOCKS).for_each(|i| result.0[i] |= rhs.0[i]);
        result
    }
}

impl std::ops::Not for XpermsBitmap {
    type Output = Self;
    fn not(self) -> Self {
        let mut result = self;
        (0..Self::BITMAP_BLOCKS).for_each(|i| result.0[i] = !result.0[i]);
        result
    }
}

impl std::ops::BitOrAssign<&Self> for XpermsBitmap {
    fn bitor_assign(&mut self, rhs: &Self) {
        (0..Self::BITMAP_BLOCKS).for_each(|i| self.0[i] |= rhs.0[i])
    }
}

impl std::ops::SubAssign<&Self> for XpermsBitmap {
    fn sub_assign(&mut self, rhs: &Self) {
        (0..Self::BITMAP_BLOCKS).for_each(|i| self.0[i] = self.0[i] ^ (self.0[i] & rhs.0[i]))
    }
}

array_type!(RoleTransitions, le::U32, RoleTransition);

array_type_validate_deref_both!(RoleTransitions);

impl ValidateArray<le::U32, RoleTransition> for RoleTransitions {
    type Error = anyhow::Error;

    /// [`RoleTransitions`] have no additional metadata (beyond length encoding).
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &le::U32,
        _items: &[RoleTransition],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct RoleTransition {
    role: le::U32,
    role_type: le::U32,
    new_role: le::U32,
    tclass: le::U32,
}

impl RoleTransition {
    pub(super) fn current_role(&self) -> RoleId {
        RoleId::from_u32(self.role.get()).unwrap()
    }

    pub(super) fn type_(&self) -> TypeId {
        TypeId::from_u32(self.role_type.get()).unwrap()
    }

    pub(super) fn class(&self) -> ClassId {
        ClassId::from_u32(self.tclass.get()).unwrap()
    }

    pub(super) fn new_role(&self) -> RoleId {
        RoleId::from_u32(self.new_role.get()).unwrap()
    }
}

impl Validate for RoleTransition {
    type Error = anyhow::Error;

    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        NonZeroU32::new(self.role.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        NonZeroU32::new(self.role_type.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        NonZeroU32::new(self.tclass.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        NonZeroU32::new(self.new_role.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        Ok(())
    }
}

array_type!(RoleAllows, le::U32, RoleAllow);

array_type_validate_deref_both!(RoleAllows);

impl ValidateArray<le::U32, RoleAllow> for RoleAllows {
    type Error = anyhow::Error;

    /// [`RoleAllows`] have no additional metadata (beyond length encoding).
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &le::U32,
        _items: &[RoleAllow],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct RoleAllow {
    role: le::U32,
    new_role: le::U32,
}

impl RoleAllow {
    pub(super) fn source_role(&self) -> RoleId {
        RoleId::from_u32(self.role.get()).unwrap()
    }

    pub(super) fn new_role(&self) -> RoleId {
        RoleId::from_u32(self.new_role.get()).unwrap()
    }
}

impl Validate for RoleAllow {
    type Error = anyhow::Error;

    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        NonZeroU32::new(self.role.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        NonZeroU32::new(self.new_role.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum FilenameTransitionList {
    PolicyVersionGeq33(SimpleArray<FilenameTransition>),
    PolicyVersionLeq32(SimpleArray<DeprecatedFilenameTransition>),
}

impl Validate for FilenameTransitionList {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        match self {
            Self::PolicyVersionLeq32(list) => {
                list.validate(context).map_err(Into::<anyhow::Error>::into)
            }
            Self::PolicyVersionGeq33(list) => {
                list.validate(context).map_err(Into::<anyhow::Error>::into)
            }
        }
    }
}

impl Validate for FilenameTransition {
    type Error = anyhow::Error;
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct FilenameTransition {
    filename: SimpleArray<u8>,
    transition_type: le::U32,
    transition_class: le::U32,
    items: SimpleArray<FilenameTransitionItem>,
}

impl FilenameTransition {
    pub(super) fn name_bytes(&self) -> &[u8] {
        &self.filename.data
    }

    pub(super) fn target_type(&self) -> TypeId {
        TypeId::from_u32(self.transition_type.get()).unwrap()
    }

    pub(super) fn target_class(&self) -> ClassId {
        ClassId::from_u32(self.transition_class.get()).unwrap()
    }

    pub(super) fn outputs(&self) -> &[FilenameTransitionItem] {
        &self.items.data
    }
}

impl Parse for FilenameTransition
where
    SimpleArray<u8>: Parse,
    SimpleArray<FilenameTransitionItem>: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (filename, tail) = SimpleArray::<u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing filename for filename transition")?;

        let (transition_type, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (transition_class, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (items, tail) = SimpleArray::<FilenameTransitionItem>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing items for filename transition")?;

        Ok((Self { filename, transition_type, transition_class, items }, tail))
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct FilenameTransitionItem {
    stypes: ExtensibleBitmap,
    out_type: le::U32,
}

impl FilenameTransitionItem {
    pub(super) fn has_source_type(&self, source_type: TypeId) -> bool {
        self.stypes.is_set(source_type.as_u32() - 1)
    }

    pub(super) fn out_type(&self) -> TypeId {
        TypeId::from_u32(self.out_type.get()).unwrap()
    }
}

impl Parse for FilenameTransitionItem
where
    ExtensibleBitmap: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (stypes, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing stypes extensible bitmap for file transition")?;

        let (out_type, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        Ok((Self { stypes, out_type }, tail))
    }
}

impl Validate for DeprecatedFilenameTransition {
    type Error = anyhow::Error;
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct DeprecatedFilenameTransition {
    filename: SimpleArray<u8>,
    metadata: DeprecatedFilenameTransitionMetadata,
}

impl DeprecatedFilenameTransition {
    pub(super) fn name_bytes(&self) -> &[u8] {
        &self.filename.data
    }

    pub(super) fn source_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.source_type.get()).unwrap()
    }

    pub(super) fn target_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.transition_type.get()).unwrap()
    }

    pub(super) fn target_class(&self) -> ClassId {
        ClassId::from_u32(self.metadata.transition_class.get()).unwrap()
    }

    pub(super) fn out_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.out_type.get()).unwrap()
    }
}

impl Parse for DeprecatedFilenameTransition
where
    SimpleArray<u8>: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (filename, tail) = SimpleArray::<u8>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing filename for deprecated filename transition")?;

        let (metadata, tail) = PolicyCursor::parse::<DeprecatedFilenameTransitionMetadata>(tail)?;

        Ok((Self { filename, metadata }, tail))
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct DeprecatedFilenameTransitionMetadata {
    source_type: le::U32,
    transition_type: le::U32,
    transition_class: le::U32,
    out_type: le::U32,
}

impl Validate for SimpleArray<InitialSid> {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        for initial_sid in crate::InitialSid::all_variants() {
            if *initial_sid == crate::InitialSid::Init && !context.need_init_sid {
                continue;
            }
            self.data
                .iter()
                .find(|initial| initial.id().get() == *initial_sid as u32)
                .ok_or(ValidateError::MissingInitialSid { initial_sid: *initial_sid })?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct InitialSid {
    id: le::U32,
    context: Context,
}

impl InitialSid {
    pub(super) fn id(&self) -> le::U32 {
        self.id
    }

    pub(super) fn context(&self) -> &Context {
        &self.context
    }
}

impl Parse for InitialSid
where
    Context: Parse,
{
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (id, tail) = PolicyCursor::parse::<le::U32>(tail)?;

        let (context, tail) = Context::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing context for initial sid")?;

        Ok((Self { id, context }, tail))
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
        ClassId::from_u32(self.class.into())
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

    pub(super) fn target_type(&self) -> TypeId {
        TypeId::from_u32(self.metadata.target_type.get()).unwrap()
    }

    pub fn target_class(&self) -> ClassId {
        ClassId::from_u32(self.metadata.target_class.get()).unwrap()
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

#[cfg(test)]
pub(super) mod testing {
    use super::AccessVectorRule;
    use std::cmp::Ordering;

    pub(in super::super) fn access_vector_rule_ordering(
        left: &AccessVectorRule,
        right: &AccessVectorRule,
    ) -> Ordering {
        (
            left.metadata.source_type,
            left.metadata.target_type,
            left.metadata.class,
            left.metadata.access_vector_rule_type,
        )
            .cmp(&(
                right.metadata.source_type,
                right.metadata.target_type,
                right.metadata.class,
                right.metadata.access_vector_rule_type,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ClassId, find_class_by_name, parse_policy_by_value};
    use super::{
        ACCESS_VECTOR_RULE_TYPE_ALLOWXPERM, ACCESS_VECTOR_RULE_TYPE_AUDITALLOWXPERM,
        ACCESS_VECTOR_RULE_TYPE_DONTAUDITXPERM, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES,
        XPERMS_TYPE_IOCTL_PREFIXES, XPERMS_TYPE_NLMSG,
    };
    use crate::new_policy::traits::PolicyId;

    impl super::AccessVectorRuleMetadata {
        /// Returns whether this access vector rule comes from an
        /// `allowxperm [source] [target]:[class] [permission] {
        /// [extended_permissions] };` policy statement.
        pub fn is_allowxperm(&self) -> bool {
            (self.access_vector_rule_type & ACCESS_VECTOR_RULE_TYPE_ALLOWXPERM) != 0
        }

        /// Returns whether this access vector rule comes from an
        /// `auditallowxperm [source] [target]:[class] [permission] {
        /// [extended_permissions] };` policy statement.
        pub fn is_auditallowxperm(&self) -> bool {
            (self.access_vector_rule_type & ACCESS_VECTOR_RULE_TYPE_AUDITALLOWXPERM) != 0
        }

        /// Returns whether this access vector rule comes from a
        /// `dontauditxperm [source] [target]:[class] [permission] {
        /// [extended_permissions] };` policy statement.
        pub fn is_dontauditxperm(&self) -> bool {
            (self.access_vector_rule_type & ACCESS_VECTOR_RULE_TYPE_DONTAUDITXPERM) != 0
        }

        /// Returns the target class id in this access vector rule. This id
        /// corresponds to the [`super::symbols::Class`] `id()` of some class in the
        /// same policy. Although the index is returned as a 32-bit value, the field
        /// itself is 16-bit
        pub fn target_class(&self) -> ClassId {
            ClassId::from_u32(self.class.into()).unwrap()
        }
    }

    #[test]
    fn parse_allowxperm_one_ioctl() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_one_ioctl")
            .expect("look up class_one_ioctl")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0xabcd));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // `ioctl` extended permissions that are declared in the same rule, and have the same
    // high byte, are stored in the same `AccessVectorRule` in the compiled policy.
    #[test]
    fn parse_allowxperm_two_ioctls_same_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_two_ioctls_same_range")
            .expect("look up class_two_ioctls_same_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x12);
            assert_eq!(xperms.count(), 2);
            assert!(xperms.contains(0x1234));
            assert!(xperms.contains(0x1256));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // `ioctl` extended permissions that are declared in different rules, but that have the same
    // high byte, are stored in the same `AccessVectorRule` in the compiled policy.
    #[test]
    fn parse_allowxperm_two_ioctls_same_range_diff_rules() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id =
            find_class_by_name(&parsed_policy.classes(), "class_four_ioctls_same_range_diff_rules")
                .expect("look up class_four_ioctls_same_range_diff_rules")
                .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x30);
            assert_eq!(xperms.count(), 4);
            assert!(xperms.contains(0x3008));
            assert!(xperms.contains(0x3009));
            assert!(xperms.contains(0x3011));
            assert!(xperms.contains(0x3013));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // `ioctl` extended permissions that are declared in the same rule, and have different
    // high bytes, are stored in different `AccessVectorRule`s in the compiled policy.
    #[test]
    fn parse_allowxperm_two_ioctls_different_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_two_ioctls_diff_range")
            .expect("look up class_two_ioctls_diff_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x56);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x5678));
        } else {
            panic!("unexpected permission data type")
        }
        assert!(rules[1].metadata.is_allowxperm());
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x12);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x1234));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // If a set of `ioctl` extended permissions consists of all xperms with a given high byte,
    // then it is represented by one `AccessVectorRule`.
    #[test]
    fn parse_allowxperm_one_driver_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_one_driver_range")
            .expect("look up class_one_driver_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIXES);
            assert_eq!(xperms.count(), 0x100);
            assert!(xperms.contains(0x1000));
            assert!(xperms.contains(0x10ab));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // If a rule grants `ioctl` extended permissions to a wide range that does not fall cleanly on
    // divisible-by-256 boundaries, it gets represented in the policy as three `AccessVectorRule`s:
    // two for the smaller subranges at the ends and one for the large subrange in the middle.
    #[test]
    fn parse_allowxperm_most_ioctls() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_most_ioctls")
            .expect("look up class_most_ioctls")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 3);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0xff);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0xff00..0xfffd {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0x0002..0x0100 {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[2].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIXES);
            assert_eq!(xperms.count(), 0xfe00);
            for xperm in 0x0100..0xff00 {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // If a rule grants `ioctl` extended permissions to two wide ranges that do not fall cleanly on
    // divisible-by-256 boundaries, they get represented in the policy as five `AccessVectorRule`s:
    // four for the smaller subranges at the ends and one for the two large subranges.
    #[test]
    fn parse_allowxperm_most_ioctls_with_hole() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_most_ioctls_with_hole")
            .expect("look up class_most_ioctls_with_hole")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 5);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0xff);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0xff00..0xfffd {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x40);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0x4002..0x4100 {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[2].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x2f);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0x2f00..0x2ffd {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[3].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0xfe);
            for xperm in 0x0002..0x0100 {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[4].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIXES);
            assert_eq!(xperms.count(), 0xec00);
            for xperm in 0x0100..0x2f00 {
                assert!(xperms.contains(xperm));
            }
            for xperm in 0x4100..0xff00 {
                assert!(xperms.contains(xperm));
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // If a set of `ioctl` extended permissions contains all 16-bit xperms, then it is
    // then it is represented by one `AccessVectorRule`. (More generally, the representation
    // is a single `AccessVectorRule` as long as the set either fully includes or fully
    // excludes each 8-bit prefix range.)
    #[test]
    fn parse_allowxperm_all_ioctls() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_all_ioctls")
            .expect("look up class_all_ioctls")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIXES);
            assert_eq!(xperms.count(), 0x10000);
        } else {
            panic!("unexpected permission data type")
        }
    }

    #[test]
    fn parse_allowxperm_one_nlmsg() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_one_nlmsg")
            .expect("look up class_one_nlmsg")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x12));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // `nlmsg` extended permissions that are declared in the same rule, and have the same
    // high byte, are stored in the same `AccessVectorRule` in the compiled policy.
    #[test]
    fn parse_allowxperm_two_nlmsg_same_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_two_nlmsg_same_range")
            .expect("look up class_two_nlmsg_same_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 2);
            assert!(xperms.contains(0x12));
            assert!(xperms.contains(0x24));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // `nlmsg` extended permissions that are declared in the same rule, and have different
    // high bytes, are stored in different `AccessVectorRule`s in the compiled policy.
    #[test]
    fn parse_allowxperm_two_nlmsg_different_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_two_nlmsg_diff_range")
            .expect("look up class_two_nlmsg_diff_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x10);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x1024));
        } else {
            panic!("unexpected permission data type")
        }
        assert!(rules[1].metadata.is_allowxperm());
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x12));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // The set of `nlmsg` extended permissions with a given high byte is represented by
    // a single `AccessVectorRule` in the compiled policy.
    #[test]
    fn parse_allowxperm_one_nlmsg_range() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_one_nlmsg_range")
            .expect("look up class_one_nlmsg_range")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 1);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0..0xff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // A set of `nlmsg` extended permissions consisting of all 16-bit integers with one
    // of 2 given prefix bytes is represented by 2 `AccessVectorRule`s in the compiled policy.
    //
    // The policy compiler allows `nlmsg` extended permission sets of this form, but they
    // are not expected to appear in policies.
    #[test]
    fn parse_allowxperm_two_nlmsg_ranges() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_two_nlmsg_ranges")
            .expect("look up class_two_nlmsg_ranges")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x01);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0100..0x01ff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0..0xff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // A set of `nlmsg` extended permissions consisting of all 16-bit integers with one
    // of 3 non-consecutive prefix bytes is represented by 3 `AccessVectorRule`s in the
    // compiled policy.
    //
    // The policy compiler allows `nlmsg` extended permission sets of this form, but they
    // are not expected to appear in policies.
    #[test]
    fn parse_allowxperm_three_separate_nlmsg_ranges() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id =
            find_class_by_name(&parsed_policy.classes(), "class_three_separate_nlmsg_ranges")
                .expect("look up class_three_separate_nlmsg_ranges")
                .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 3);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x20);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x2000..0x20ff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x10);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x1000..0x10ff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[2].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0..0xff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // A set of `nlmsg` extended permissions consisting of all 16-bit integers with one
    // of 3 (or more) consecutive prefix bytes is represented by 2 `AccessVectorRule`s in the
    // compiled policy, one for the smallest prefix byte and one for the largest.
    //
    // The policy compiler allows `nlmsg` extended permission sets of this form, but they
    // are not expected to appear in policies.
    #[test]
    fn parse_allowxperm_three_contiguous_nlmsg_ranges() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id =
            find_class_by_name(&parsed_policy.classes(), "class_three_contiguous_nlmsg_ranges")
                .expect("look up class_three_contiguous_nlmsg_ranges")
                .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_allowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x02);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0200..0x02ff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 0x100);
            for i in 0x0..0xff {
                assert!(xperms.contains(i), "{i}");
            }
        } else {
            panic!("unexpected permission data type")
        }
    }

    // The representation of extended permissions for `auditallowxperm` rules is
    // the same as for `allowxperm` rules.
    #[test]
    fn parse_auditallowxperm() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_auditallowxperm")
            .expect("look up class_auditallowxperm")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_auditallowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x10));
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x10);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x1000));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // The representation of extended permissions for `dontauditxperm` rules is
    // the same as for `allowxperm` rules. In particular, the `AccessVectorRule`
    // contains the same set of extended permissions that appears in the text
    // policy. (This differs from the representation of the access vector in
    // `AccessVectorRule`s for `dontaudit` rules, where the `AccessVectorRule`
    // contains the complement of the access vector that appears in the text
    // policy.)
    #[test]
    fn parse_dontauditxperm() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id = find_class_by_name(&parsed_policy.classes(), "class_dontauditxperm")
            .expect("look up class_dontauditxperm")
            .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_dontauditxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_NLMSG);
            assert_eq!(xperms.xperms_optional_prefix, 0x00);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x11));
        } else {
            panic!("unexpected permission data type")
        }
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.xperms_type, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES);
            assert_eq!(xperms.xperms_optional_prefix, 0x10);
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0x1000));
        } else {
            panic!("unexpected permission data type")
        }
    }

    // If an allowxperm rule and an auditallowxperm rule specify exactly the same permissions, they
    // are not coalesced into a single `AccessVectorRule` in the policy; two rules appear in the
    // policy.
    #[test]
    fn parse_auditallowxperm_not_coalesced() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/allowxperm_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let parsed_policy = &policy.0;
        parsed_policy.validate().expect("validate policy");

        let class_id =
            find_class_by_name(&parsed_policy.classes(), "class_auditallowxperm_not_coalesced")
                .expect("class_auditallowxperm_not_coalesced")
                .id();

        let rules: Vec<_> = parsed_policy
            .access_vector_rules_for_test()
            .filter(|rule| rule.metadata.target_class() == class_id)
            .collect();

        assert_eq!(rules.len(), 2);
        assert!(rules[0].metadata.is_allowxperm());
        assert!(!rules[0].metadata.is_auditallowxperm());
        if let Some(xperms) = rules[0].extended_permissions() {
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0xabcd));
        } else {
            panic!("unexpected permission data type")
        }
        assert!(!rules[1].metadata.is_allowxperm());
        assert!(rules[1].metadata.is_auditallowxperm());
        if let Some(xperms) = rules[1].extended_permissions() {
            assert_eq!(xperms.count(), 1);
            assert!(xperms.contains(0xabcd));
        } else {
            panic!("unexpected permission data type")
        }
    }
}
