// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::ValidateError;
use super::extensible_bitmap::ExtensibleBitmap;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};

use super::view::U24;
use super::{
    Array, CategoryId, Counted, MlsLevel, MlsRange, Parse, PolicyValidationContext, RoleId,
    SensitivityId, TypeId, UserId, Validate, ValidateArray, array_type,
    array_type_validate_deref_both,
};

use crate::new_policy::traits::PolicyId;
use anyhow::{Context as _, anyhow};
use hashbrown::hash_table::HashTable;
use rapidhash::RapidHasher;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

/// Exact value of [`Type`] `properties` when the underlying data refers to an SELinux type.
const TYPE_PROPERTIES_TYPE: u32 = 1;

/// Exact value of [`Type`] `properties` when the underlying data refers to an SELinux alias.
const TYPE_PROPERTIES_ALIAS: u32 = 0;

/// Exact value of [`Type`] `properties` when the underlying data refers to an SELinux attribute.
const TYPE_PROPERTIES_ATTRIBUTE: u32 = 3;

/// [`SymbolList`] is an [`Array`] of items with the count of items determined by [`Metadata`] as
/// [`Counted`].
#[derive(Debug, PartialEq)]
pub(super) struct SymbolList<T>(Array<Metadata, T>);

impl<T> Deref for SymbolList<T> {
    type Target = Array<Metadata, T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Parse> Parse for SymbolList<T> {
    type Error = <Array<Metadata, T> as Parse>::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let (array, tail) = Array::<Metadata, T>::parse(bytes)?;
        Ok((Self(array), tail))
    }
}

impl<T: Validate> Validate for SymbolList<T> {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        self.0.metadata.validate(context)?;
        self.0.data.validate(context).map_err(Into::<anyhow::Error>::into)?;

        Ok(())
    }
}

/// Binary metadata prefix to [`SymbolList`] objects.
#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct Metadata {
    /// The number of primary names referred to in the associated [`SymbolList`].
    primary_names_count: le::U32,
    /// The number of objects in the associated [`SymbolList`] [`Array`].
    count: le::U32,
}

impl Metadata {
    pub fn primary_names_count(&self) -> u32 {
        self.primary_names_count.get()
    }
}

impl Counted for Metadata {
    /// The number of items that follow a [`Metadata`] is the value stored in the `metadata.count`
    /// field.
    fn count(&self) -> u32 {
        self.count.get()
    }
}

impl Validate for Metadata {
    type Error = anyhow::Error;

    /// TODO: Should there be an upper bound on `primary_names_count` or `count`?
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Validate for Role {
    type Error = anyhow::Error;

    /// TODO: Validate [`Role`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Role {
    metadata: RoleMetadata,
    role_dominates: ExtensibleBitmap,
    role_types: ExtensibleBitmap,
}

impl Role {
    pub(super) fn id(&self) -> RoleId {
        RoleId::from_u32(self.metadata.metadata.id.get()).unwrap()
    }

    pub(super) fn name_bytes(&self) -> &[u8] {
        &self.metadata.data
    }

    pub(super) fn types(&self) -> &ExtensibleBitmap {
        &self.role_types
    }
}

impl Parse for Role {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) = RoleMetadata::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing role metadata")?;

        let (role_dominates, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing role dominates")?;

        let (role_types, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing role types")?;

        Ok((Self { metadata, role_dominates, role_types }, tail))
    }
}

array_type!(RoleMetadata, RoleStaticMetadata, u8);

array_type_validate_deref_both!(RoleMetadata);

impl ValidateArray<RoleStaticMetadata, u8> for RoleMetadata {
    type Error = anyhow::Error;

    /// [`RoleMetadata`] has no internal constraints beyond those imposed by [`Array`].
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &RoleStaticMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct RoleStaticMetadata {
    length: le::U32,
    id: le::U32,
    bounds: le::U32,
}

impl Counted for RoleStaticMetadata {
    /// [`RoleStaticMetadata`] serves as [`Counted`] for a length-encoded `[u8]`.
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for RoleStaticMetadata {
    type Error = anyhow::Error;

    /// TODO: Should there be any constraints on `length`, `value`, or `bounds`?
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

array_type!(Type, TypeMetadata, u8);

array_type_validate_deref_both!(Type);

impl Type {
    /// Returns the name of this type.
    pub fn name_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Returns the id associated with this type. The id is used to index into collections and
    /// bitmaps associated with this type. The id is 1-indexed, whereas most collections and
    /// bitmaps are 0-indexed, so clients of this API will usually use `id - 1`.
    pub fn id(&self) -> TypeId {
        TypeId::from_u32(self.metadata.id.get()).unwrap()
    }

    /// Returns the Id of the bounding type, if any.
    pub fn bounded_by(&self) -> Option<TypeId> {
        TypeId::from_u32(self.metadata.bounds.get())
    }
}

impl ValidateArray<TypeMetadata, u8> for Type {
    type Error = anyhow::Error;

    /// TODO: Validate that `PS::deref(&self.data)` is an ascii string that contains a valid type name.
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &TypeMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct TypeMetadata {
    length: le::U32,
    id: le::U32,
    properties: le::U32,
    bounds: le::U32,
}

impl Counted for TypeMetadata {
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for TypeMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate [`TypeMetadata`] internals.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn name_hash(name: &[u8]) -> u64 {
    let mut hasher = RapidHasher::default();
    name.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug)]
pub(super) struct TypeIndex {
    // TODO: https://fxbug.dev/483930877 - we don't use or need this after validation; it
    // would be nice to avoid having it continue to be stored here after its last use.
    primary_names_count: u32,

    /// A mapping from [`TypeId`] (represented as position-in-the-array-plus-one) to
    /// the corresponding [`Type`] (represented as an offset into the policy bytes).
    /// If zero is the value at index `i` of this structure, that indicates that the
    /// binary policy has no type with type ID `i + 1`. Only types ([`Type`]s matching
    /// `TYPE_PROPERTIES_TYPE`) and attributes ([`Type`]s matching
    /// `TYPE_PROPERTIES_ATTRIBUTE`) are included in this structure; type aliases are
    /// excluded (were we to want to include them, they would "claim" the ID properly
    /// belonging to exactly one non-alias [`Type`]).
    //
    // TODO: https://fxbug.dev/479180246 - we currently allow for "holes" (integer type IDs
    // that do not correspond to any type) in this array, but do we need to? Will all the
    // binary policies that we encounter be "packed" such that they use every integer
    // between one and the largest integer that they use?
    offsets_by_id_minus_one: Box<[U24]>,

    /// A mapping from the string name of a [`Type`] to that type's location in the policy
    /// bytes. This structure contains entries for all types ([`Type`]s matching
    /// `TYPE_PROPERTIES_TYPE`) and type aliases ([`Type`]s matching
    /// `TYPE_PROPERTIES_ALIAS`) but not attributes ([`Type`]s matching
    /// `TYPE_PROPERTIES_ATTRIBUTE`); attributes are never looked up by name.
    offsets_by_name: HashTable<U24>,
}

impl TypeIndex {
    fn parse_type_at(policy_bytes: &PolicyData, offset: U24) -> Type {
        Type::parse(PolicyCursor::new_at(policy_bytes, PolicyOffset::from(offset)))
            .expect("These bytes already successfully parsed")
            .0
    }

    pub(super) fn primary_names_count(&self) -> u32 {
        self.primary_names_count
    }

    pub(super) fn type_id_by_name(&self, name: &str, data: &PolicyData) -> Option<TypeId> {
        let name_bytes = name.as_bytes();
        self.offsets_by_name
            .find(name_hash(name_bytes), |&other_offset| {
                Self::parse_type_at(data, other_offset).name_bytes() == name_bytes
            })
            .map(|&offset| Self::parse_type_at(data, offset).id())
    }

    pub(super) fn type_by_type_id(&self, id: TypeId, data: &PolicyData) -> Type {
        Self::parse_type_at(data, self.offsets_by_id_minus_one[(id.as_u32() - 1) as usize])
    }

    /// Returns an iterator over all the type-Ids, for use by the post-parse validation.
    pub(super) fn all_type_ids(&self) -> impl Iterator<Item = TypeId> {
        self.offsets_by_id_minus_one.iter().enumerate().filter_map(
            |(index, offset)| match u32::from(*offset) {
                0 => None,
                _ => Some(TypeId::from_u32((index + 1) as u32).unwrap()),
            },
        )
    }
}

impl Parse for TypeIndex {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let policy_data = bytes.data();
        let (metadata, mut tail) = Metadata::parse(bytes)?;
        let type_count = usize::try_from(metadata.count()).unwrap();
        let mut offsets_by_id_minus_one = Vec::with_capacity(type_count);
        let mut offsets_by_name = HashTable::with_capacity(type_count);
        for _ in 0..type_count {
            let offset = U24::try_from(tail.offset()).unwrap();
            let (type_, next_tail) = Type::parse(tail)?;

            let will_be_looked_up_by_id;
            let will_be_looked_up_by_name;
            match type_.metadata.properties.get() {
                TYPE_PROPERTIES_TYPE => {
                    will_be_looked_up_by_id = true;
                    will_be_looked_up_by_name = true;
                }
                TYPE_PROPERTIES_ATTRIBUTE => {
                    will_be_looked_up_by_id = true;
                    will_be_looked_up_by_name = false;
                }
                TYPE_PROPERTIES_ALIAS => {
                    will_be_looked_up_by_id = false;
                    will_be_looked_up_by_name = true;
                }
                unrecognized => {
                    return Err(anyhow!(
                        "Can't parse \"type\" element with \"properties\" value {:?}",
                        unrecognized
                    ));
                }
            }

            if will_be_looked_up_by_id {
                let type_id_as_usize = type_.id().as_u32() as usize;
                if offsets_by_id_minus_one.len() < type_id_as_usize {
                    offsets_by_id_minus_one.resize(type_id_as_usize, U24::try_from(0).unwrap());
                }
                offsets_by_id_minus_one[type_id_as_usize - 1] = offset;
            }

            if will_be_looked_up_by_name {
                let name_bytes = type_.name_bytes();
                offsets_by_name
                    .entry(
                        name_hash(name_bytes),
                        |&other_offset| {
                            Self::parse_type_at(&policy_data, other_offset).name_bytes()
                                == name_bytes
                        },
                        |&other_offset| {
                            name_hash(Self::parse_type_at(&policy_data, other_offset).name_bytes())
                        },
                    )
                    .insert(offset);
            }

            tail = next_tail;
        }
        let offsets_by_id_minus_one = Box::<[U24]>::from(offsets_by_id_minus_one);
        offsets_by_name.shrink_to_fit(|&other_offset| {
            name_hash(Self::parse_type_at(&policy_data, other_offset).name_bytes())
        });

        Ok((
            Self {
                primary_names_count: metadata.primary_names_count(),
                offsets_by_id_minus_one,
                offsets_by_name,
            },
            tail,
        ))
    }
}

impl Validate for TypeIndex {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency between consecutive [`Type`] instances.
    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        let data = context.data.clone();
        let mut primary_names_count = 0u32;
        for offset in &self.offsets_by_id_minus_one {
            if PolicyOffset::from(*offset) != 0 {
                Self::parse_type_at(&data, *offset).validate(context)?;
                primary_names_count += 1;
            }
        }

        if self.primary_names_count != primary_names_count {
            return Err(anyhow!(
                "Expected {:?} primary names but found {:?}",
                self.primary_names_count,
                primary_names_count
            ));
        }

        Ok(())
    }
}

impl Validate for User {
    type Error = anyhow::Error;

    /// TODO: Validate [`User`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct User {
    user_data: UserData,
    roles: ExtensibleBitmap,
    expanded_range: MlsRange,
    default_level: MlsLevel,
}

impl User {
    pub(super) fn id(&self) -> UserId {
        UserId::from_u32(self.user_data.metadata.id.get()).unwrap()
    }

    pub(super) fn name_bytes(&self) -> &[u8] {
        &self.user_data.data
    }

    pub(super) fn roles(&self) -> &ExtensibleBitmap {
        &self.roles
    }

    pub(super) fn mls_range(&self) -> &MlsRange {
        &self.expanded_range
    }
}

impl Parse for User {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (user_data, tail) = UserData::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing user data")?;

        let (roles, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing user roles")?;

        let (expanded_range, tail) =
            MlsRange::parse(tail).context("parsing user expanded range")?;

        let (default_level, tail) = MlsLevel::parse(tail).context("parsing user default level")?;

        Ok((Self { user_data, roles, expanded_range, default_level }, tail))
    }
}

array_type!(UserData, UserMetadata, u8);

array_type_validate_deref_both!(UserData);

impl ValidateArray<UserMetadata, u8> for UserData {
    type Error = anyhow::Error;

    /// TODO: Validate consistency between [`UserMetadata`] in `self.metadata` and `[u8]` key in `self.data`.
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &UserMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct UserMetadata {
    length: le::U32,
    id: le::U32,
    bounds: le::U32,
}

impl Counted for UserMetadata {
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for UserMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate [`UserMetadata`] internals.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Parse for MlsLevel {
    type Error = anyhow::Error;

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let offset = cursor.offset() as usize;
        let slice = &cursor.data().as_ref()[offset..];
        let mut new_cursor = crate::new_policy::parser::PolicyCursor::new(slice);
        let level = <Self as crate::new_policy::traits::Parse>::parse(&mut new_cursor)
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;
        let bytes_parsed = new_cursor.offset();
        let new_offset = cursor.offset() + bytes_parsed as u32;
        Ok((level, PolicyCursor::new_at(cursor.data(), new_offset)))
    }
}

impl Validate for MlsLevel {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        crate::new_policy::traits::Validate::validate(self, &context.new_policy).map_err(Into::into)
    }
}

impl Parse for MlsRange {
    type Error = anyhow::Error;

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let offset = cursor.offset() as usize;
        let slice = &cursor.data().as_ref()[offset..];
        let mut new_cursor = crate::new_policy::parser::PolicyCursor::new(slice);
        let range = <Self as crate::new_policy::traits::Parse>::parse(&mut new_cursor)
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;
        let bytes_parsed = new_cursor.offset();
        let new_offset = cursor.offset() + bytes_parsed as u32;
        Ok((range, PolicyCursor::new_at(cursor.data(), new_offset)))
    }
}

impl Validate for MlsRange {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        crate::new_policy::traits::Validate::validate(self, &context.new_policy).map_err(Into::into)
    }
}

array_type!(ConditionalBoolean, ConditionalBooleanMetadata, u8);

array_type_validate_deref_both!(ConditionalBoolean);

impl ValidateArray<ConditionalBooleanMetadata, u8> for ConditionalBoolean {
    type Error = anyhow::Error;

    /// TODO: Validate consistency between [`ConditionalBooleanMetadata`] and `[u8]` key.
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &ConditionalBooleanMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct ConditionalBooleanMetadata {
    id: le::U32,
    /// Current active value of this conditional boolean.
    active: le::U32,
    length: le::U32,
}

impl ConditionalBooleanMetadata {
    /// Returns the active value for the boolean.
    pub(super) fn active(&self) -> bool {
        self.active != le::U32::ZERO
    }
}

impl Counted for ConditionalBooleanMetadata {
    /// [`ConditionalBooleanMetadata`] used as `M` in of `Array<PS, PS::Output<M>, PS::Slice<u8>>` with
    /// `self.length` denoting size of inner `[u8]`.
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for ConditionalBooleanMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency of [`ConditionalBooleanMetadata`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Sensitivity {
    metadata: SensitivityMetadata,
    level: MlsLevel,
}

impl Sensitivity {
    pub fn id(&self) -> SensitivityId {
        self.level.sensitivity()
    }

    pub fn name_bytes(&self) -> &[u8] {
        &self.metadata.data
    }
}

impl Parse for Sensitivity {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let tail = bytes;

        let (metadata, tail) = SensitivityMetadata::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing sensitivity metadata")?;

        let (level, tail) = MlsLevel::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing sensitivity mls level")?;

        Ok((Self { metadata, level }, tail))
    }
}

impl Validate for Sensitivity {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency of `self.metadata` and `self.level`.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

array_type!(SensitivityMetadata, SensitivityStaticMetadata, u8);

array_type_validate_deref_both!(SensitivityMetadata);

impl ValidateArray<SensitivityStaticMetadata, u8> for SensitivityMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate consistency between [`SensitivityMetadata`] and `[u8]` key.
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &SensitivityStaticMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct SensitivityStaticMetadata {
    length: le::U32,
    is_alias: le::U32,
}

impl Counted for SensitivityStaticMetadata {
    /// [`SensitivityStaticMetadata`] used as `M` in of `Array<PS, PS::Output<M>, PS::Slice<u8>>` with
    /// `self.length` denoting size of inner `[u8]`.
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for SensitivityStaticMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency of [`SensitivityStaticMetadata`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

array_type!(Category, CategoryMetadata, u8);

array_type_validate_deref_both!(Category);

impl Category {
    pub fn id(&self) -> CategoryId {
        CategoryId::from_u32(self.metadata.id.get()).unwrap()
    }

    pub fn name_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl ValidateArray<CategoryMetadata, u8> for Category {
    type Error = anyhow::Error;

    /// TODO: Validate consistency between [`CategoryMetadata`] and `[u8]` key.
    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &CategoryMetadata,
        _items: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct CategoryMetadata {
    length: le::U32,
    id: le::U32,
    is_alias: le::U32,
}

impl Counted for CategoryMetadata {
    /// [`CategoryMetadata`] used as `M` in of `Array<PS, PS::Output<M>, PS::Slice<u8>>` with
    /// `self.length` denoting size of inner `[u8]`.
    fn count(&self) -> u32 {
        self.length.get()
    }
}

impl Validate for CategoryMetadata {
    type Error = anyhow::Error;

    /// TODO: Validate internal consistency of [`CategoryMetadata`].
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        CategoryId::from_u32(self.id.get()).ok_or(ValidateError::NonOptionalIdIsZero)?;
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct CategoryIndex {
    /// A mapping from [`CategoryId`] (represented as position-in-the-array-plus-one)
    /// to the corresponding [`Category`] (represented as an offset into the policy
    /// bytes). If zero is the value at index `i` of this structure, that indicates that
    /// the policy bytes have no category with ID `i + 1`.
    //
    // TODO: https://fxbug.dev/479180246 - we currently allow for "holes" (integer
    // category IDs that do not correspond to any category) in this array, but do we
    // need to? Will all the binary policies that we encounter be "packed" such that
    // they use every integer between one and the largest integer that they use?
    offsets_by_id_minus_one: Box<[U24]>,

    /// A mapping from category name hash to the offset of that category in the policy.
    offsets_by_name: HashTable<U24>,
}

impl CategoryIndex {
    fn parse_category_at(policy_bytes: &PolicyData, offset: U24) -> Category {
        Category::parse(PolicyCursor::new_at(policy_bytes, PolicyOffset::from(offset)))
            .expect("These bytes already successfully parsed")
            .0
    }

    /// Looks up a [`Category`] by its [`CategoryId`].
    pub fn category(&self, policy_bytes: &PolicyData, category_id: CategoryId) -> Category {
        let offset = self.offsets_by_id_minus_one[(category_id.as_u32() - 1) as usize];
        Self::parse_category_at(policy_bytes, offset)
    }

    /// Looks up all [`Category`]s given in the policy. This is linear in time and
    /// space and inappropriate to call in from a performance-sensitive context, but
    /// may be called during policy parsing/validation, selinuxfs file operations, and
    /// filesystem extended attribute value calculations.
    pub fn categories<'a>(
        &'a self,
        policy_bytes: &'a PolicyData,
    ) -> impl Iterator<Item = Category> + 'a {
        self.offsets_by_id_minus_one.iter().filter_map(|&offset| match PolicyOffset::from(offset) {
            0 => None,
            offset => {
                let (category, _) =
                    Category::parse(PolicyCursor::new_at(policy_bytes, PolicyOffset::from(offset)))
                        .expect("These bytes already successfully parsed");
                Some(category)
            }
        })
    }

    /// Looks up a [`Category`] by its name in constant time.
    pub fn category_by_name(&self, policy_bytes: &PolicyData, name: &str) -> Option<Category> {
        let name_bytes = name.as_bytes();
        self.offsets_by_name
            .find(name_hash(name_bytes), |&other_offset| {
                name_bytes == Self::parse_category_at(policy_bytes, other_offset).name_bytes()
            })
            .map(|&offset| Self::parse_category_at(policy_bytes, offset))
    }
}

impl Parse for CategoryIndex {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let policy_data = bytes.data();
        let (metadata, mut tail) = Metadata::parse(bytes)?;
        let category_count = usize::try_from(metadata.count()).unwrap();
        let mut offsets_by_id_minus_one = vec![U24::ZERO; category_count];
        let mut offsets_by_name = HashTable::with_capacity(category_count);

        for _ in 0..category_count {
            let offset = U24::try_from(tail.offset()).unwrap();
            let (category, next_tail) = Category::parse(tail)?;
            let category_id_as_usize = category.id().as_u32() as usize;

            if offsets_by_id_minus_one.len() < category_id_as_usize {
                offsets_by_id_minus_one.resize(category_id_as_usize, U24::ZERO);
            }
            offsets_by_id_minus_one[category_id_as_usize - 1] = offset;

            offsets_by_name
                .entry(
                    name_hash(category.name_bytes()),
                    |&other_offset| {
                        category.name_bytes()
                            == Self::parse_category_at(&policy_data, other_offset).name_bytes()
                    },
                    |&other_offset| {
                        name_hash(Self::parse_category_at(&policy_data, other_offset).name_bytes())
                    },
                )
                .insert(offset);

            tail = next_tail;
        }

        offsets_by_name.shrink_to_fit(|&other_offset| {
            name_hash(Self::parse_category_at(&policy_data, other_offset).name_bytes())
        });

        Ok((
            Self {
                offsets_by_id_minus_one: Box::<[U24]>::from(offsets_by_id_minus_one),
                offsets_by_name,
            },
            tail,
        ))
    }
}

impl Validate for CategoryIndex {
    type Error = anyhow::Error;

    /// TODO: Validate consistency of sequence of [`Category`].
    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        for offset in &self.offsets_by_id_minus_one {
            let (category, _) =
                Category::parse(PolicyCursor::new_at(&context.data, PolicyOffset::from(*offset)))
                    .expect("These bytes already successfully parsed");

            category.validate(context).context("category defaults")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AccessVector, CategoryId, SensitivityId, UserId, parse_policy_by_value};

    use crate::new_policy::{
        ConstraintNames, ConstraintOperand, ConstraintOperator, ConstraintSubject, ConstraintTerm,
    };

    #[test]
    fn mls_levels_for_user_context() {
        const TEST_POLICY: &[u8] =
            include_bytes! {"../../testdata/micro_policies/multiple_levels_and_categories_policy"};
        let policy = parse_policy_by_value(TEST_POLICY.to_vec()).unwrap();
        let policy = policy.validate().unwrap();

        let user = policy.user(UserId::for_test(1));
        let mls_range = user.mls_range();
        let low_level = mls_range.low();
        let high_level = mls_range.high().as_ref().expect("user 1 has a high mls level");

        assert_eq!(low_level.sensitivity(), SensitivityId::for_test(1));
        assert_eq!(low_level.category_ids().collect::<Vec<_>>(), vec![CategoryId::for_test(1)]);

        assert_eq!(high_level.sensitivity(), SensitivityId::for_test(2));
        assert_eq!(
            high_level.category_ids().collect::<Vec<_>>(),
            vec![
                CategoryId::for_test(1),
                CategoryId::for_test(2),
                CategoryId::for_test(3),
                CategoryId::for_test(4),
                CategoryId::for_test(5),
            ]
        );
    }

    #[test]
    fn parse_mls_constrain_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_mls_constraints").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 6);

        let expected = [
            ConstraintTerm::Expression {
                operand: ConstraintOperand::L1H1,
                operator: ConstraintOperator::Incomp,
            },
            ConstraintTerm::Expression {
                operand: ConstraintOperand::H1H2,
                operator: ConstraintOperator::Incomp,
            },
            ConstraintTerm::Expression {
                operand: ConstraintOperand::L1H2,
                operator: ConstraintOperator::DomBy,
            },
            ConstraintTerm::Expression {
                operand: ConstraintOperand::H1L2,
                operator: ConstraintOperator::Dom,
            },
            ConstraintTerm::Expression {
                operand: ConstraintOperand::L2H2,
                operator: ConstraintOperator::Ne,
            },
            ConstraintTerm::Expression {
                operand: ConstraintOperand::L1L2,
                operator: ConstraintOperator::Eq,
            },
        ];
        for (i, constraint) in constraints.iter().enumerate() {
            assert_eq!(constraint.access_vector(), AccessVector::from(1), "constraint {}", i);
            let terms = constraint.constraint_expr();
            assert_eq!(terms.len(), 1, "constraint {}", i);
            assert_eq!(terms[0], expected[i], "constraint {}", i);
        }
    }

    #[test]
    fn parse_constrain_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_constraint_nested").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let constraint = &constraints[0];
        assert_eq!(constraint.access_vector(), AccessVector::from(1));
        let terms = constraint.constraint_expr();
        assert_eq!(terms.len(), 8);

        assert_eq!(terms[7], ConstraintTerm::Or);
        assert_eq!(terms[6], ConstraintTerm::And);
        assert_eq!(terms[5], ConstraintTerm::Not);

        assert_eq!(
            terms[4],
            ConstraintTerm::Expression {
                operand: ConstraintOperand::Type(ConstraintSubject::Source),
                operator: ConstraintOperator::Eq,
            }
        );

        assert_eq!(
            terms[3],
            ConstraintTerm::Expression {
                operand: ConstraintOperand::User(ConstraintSubject::Source),
                operator: ConstraintOperator::Eq,
            }
        );

        assert_eq!(terms[2], ConstraintTerm::And);

        assert_eq!(
            terms[1],
            ConstraintTerm::Expression {
                operand: ConstraintOperand::Role(ConstraintSubject::Source),
                operator: ConstraintOperator::Eq,
            }
        );

        match &terms[0] {
            ConstraintTerm::ExpressionWithNames { operand, operator, names } => {
                assert_eq!(operand, &ConstraintOperand::User(ConstraintSubject::Target));
                assert_eq!(operator, &ConstraintOperator::Eq);
                match &**names {
                    ConstraintNames::Users(ids, set) => {
                        assert!(!ids.is_empty());
                        assert!(set.is_empty());
                    }
                    _ => panic!("expected Users"),
                }
            }
            _ => panic!("expected ExpressionWithNames"),
        }
    }
}
