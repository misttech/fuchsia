// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::ValidateError;

use super::parser::{PolicyCursor, PolicyData, PolicyOffset};

use super::view::U24;
use super::{
    Array, CategoryId, Counted, MlsLevel, MlsRange, Parse, PolicyValidationContext, SensitivityId,
    Validate, ValidateArray, array_type, array_type_validate_deref_both,
};

use crate::new_policy::traits::PolicyId;
use anyhow::Context as _;
use hashbrown::hash_table::HashTable;
use rapidhash::RapidHasher;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

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

fn name_hash(name: &[u8]) -> u64 {
    let mut hasher = RapidHasher::default();
    name.hash(&mut hasher);
    hasher.finish()
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
        ConstraintOperator, ConstraintSubject, ConstraintTerm, MlsOperands, MlsOperator,
    };

    #[test]
    fn mls_levels_for_user_context() {
        const TEST_POLICY: &[u8] =
            include_bytes! {"../../testdata/micro_policies/multiple_levels_and_categories_policy"};
        let policy = parse_policy_by_value(TEST_POLICY.to_vec()).unwrap();
        let policy = policy.validate().unwrap();

        let user = policy.users().get_by_id(UserId::for_test(1)).unwrap();
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
            ConstraintTerm::MlsOp(MlsOperands::L1H1, MlsOperator::Incomp),
            ConstraintTerm::MlsOp(MlsOperands::H1H2, MlsOperator::Incomp),
            ConstraintTerm::MlsOp(MlsOperands::L1H2, MlsOperator::DomBy),
            ConstraintTerm::MlsOp(MlsOperands::H1L2, MlsOperator::Dom),
            ConstraintTerm::MlsOp(MlsOperands::L2H2, MlsOperator::Ne),
            ConstraintTerm::MlsOp(MlsOperands::L1L2, MlsOperator::Eq),
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

        assert_eq!(terms[4], ConstraintTerm::TypeAttributeOp(ConstraintOperator::Eq));

        assert_eq!(terms[3], ConstraintTerm::UserAttributeOp(ConstraintOperator::Eq));

        assert_eq!(terms[2], ConstraintTerm::And);

        assert_eq!(terms[1], ConstraintTerm::RoleAttributeOp(ConstraintOperator::Eq));

        match &terms[0] {
            ConstraintTerm::UserNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Target);
                assert_eq!(expr.operator(), ConstraintOperator::Eq);
                assert!(!expr.names().is_empty());
            }
            _ => panic!("expected UserNameOp"),
        }
    }

    #[test]
    fn parse_constrain_user_names_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_constraint_with_names").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let terms = constraints[0].constraint_expr();
        assert_eq!(terms.len(), 1);

        match &terms[0] {
            ConstraintTerm::UserNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Source);
                assert_eq!(expr.operator(), ConstraintOperator::Ne);
                assert_eq!(expr.names().iter().count(), 2);
            }
            _ => panic!("expected UserNameOp"),
        }
    }

    #[test]
    fn parse_constrain_role_names_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_constraint_role_names").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let terms = constraints[0].constraint_expr();
        assert_eq!(terms.len(), 1);

        match &terms[0] {
            ConstraintTerm::RoleNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Source);
                assert_eq!(expr.operator(), ConstraintOperator::Eq);
                assert_eq!(expr.names().iter().count(), 1);
            }
            _ => panic!("expected RoleNameOp"),
        }
    }

    #[test]
    fn parse_constrain_type_names_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_constraint_type_names").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let terms = constraints[0].constraint_expr();
        assert_eq!(terms.len(), 1);

        match &terms[0] {
            ConstraintTerm::TypeNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Source);
                assert_eq!(expr.operator(), ConstraintOperator::Eq);
                // domain attribute expands to type0, type1, and type2 in `names`
                assert_eq!(expr.names().iter().count(), 3);
                // constraint_sets should contain the un-expanded attribute ID in include_set(), empty exclude_set, and 0 flags
                let constraint_sets = expr.constraint_sets().expect("constraint sets");
                assert_eq!(constraint_sets.include_set().iter().count(), 1);
                assert!(constraint_sets.exclude_set().is_empty());
                assert_eq!(constraint_sets.flags(), 0);
            }
            _ => panic!("expected TypeNameOp"),
        }
    }

    #[test]
    fn parse_constrain_type_list_statement() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec()).expect("parse policy");
        let policy = policy.validate().expect("validate policy");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_constraint_type_list").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let terms = constraints[0].constraint_expr();
        assert_eq!(terms.len(), 1);

        match &terms[0] {
            ConstraintTerm::TypeNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Source);
                assert_eq!(expr.operator(), ConstraintOperator::Eq);
                // list { type0 security_t } expands to type0 and security_t in `names`
                assert_eq!(expr.names().iter().count(), 2);
                // constraint_sets should contain type0 and security_t in include_set(), empty exclude_set, and 0 flags
                let constraint_sets = expr.constraint_sets().expect("constraint sets");
                assert_eq!(constraint_sets.include_set().iter().count(), 2);
                assert!(constraint_sets.exclude_set().is_empty());
                assert_eq!(constraint_sets.flags(), 0);
            }
            _ => panic!("expected TypeNameOp"),
        }
    }
}
