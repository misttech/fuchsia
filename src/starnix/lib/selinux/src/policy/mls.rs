// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cmp::Ordering;
use std::num::NonZeroU16;

use selinux_policy_derive::{HasName, HasPolicyId, Parse, Serialize, Validate};

use super::bitmap::{IdSet, IdSetBuilder, IdSpan};
use super::classes::ClassId;
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::parser::{PolicyCursor, PolicyWriter};
use super::security_context::SecurityContextError;
use super::traits::{HasName, HasPolicyId, Parse, PolicyId, Serialize, Validate};
use super::{Policy, TypeId};

/// Tag type for type safety of policy sensitivity identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct SensitivityTag;

/// Identifies a sensitivity level within a policy.
pub type SensitivityId = IdType<NonZeroU16, SensitivityTag>;

/// Tag type for type safety of policy category identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct CategoryTag;

/// Identifies a security category within a policy.
pub type CategoryId = IdType<NonZeroU16, CategoryTag>;

/// Set of security categories.
pub type CategorySet = IdSet<CategoryId>;

/// Builder for constructing [`CategorySet`]s dynamically.
pub type CategorySetBuilder = IdSetBuilder<CategoryId>;

/// Entry in a category specification, which may be a single category (in which case
/// [`IdSpan::low`] = [`IdSpan::high`]) or a span of consecutive categories. Bounds are included in
/// the span.
pub type CategorySpan = IdSpan<CategoryId>;

impl CategorySpan {
    /// Serializes the category or category range to a byte vector.
    fn serialize_str(&self, policy: &Policy) -> Vec<u8> {
        let low = policy.categories.get_by_id(self.low()).unwrap().name();
        if self.low() == self.high() {
            return low.into();
        }
        let high = policy.categories.get_by_id(self.high()).unwrap().name();
        // A span of just two categories is described as a pair of individual categories, rather
        // than as a range.
        let separator: &[u8] =
            if self.high().as_u32() == self.low().as_u32() + 1 { b"," } else { b"." };
        [low, high].join(separator)
    }
}

/// Security level in Multi-Level Security (MLS), consisting of a sensitivity and a set of
/// categories.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Parse, Serialize, Validate)]
pub struct MlsLevel {
    sensitivity: SensitivityId,
    categories: CategorySet,
}

impl MlsLevel {
    /// Constructs a new [`MlsLevel`].
    pub fn new(sensitivity: SensitivityId, categories: CategorySet) -> Self {
        Self { sensitivity, categories }
    }

    /// Returns the sensitivity level.
    pub fn sensitivity(&self) -> SensitivityId {
        self.sensitivity
    }

    /// Returns the set of categories.
    pub fn categories(&self) -> &CategorySet {
        &self.categories
    }

    /// Compares two [`MlsLevel`]s, returning [`None`] if they are incomparable.
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        let s_order = self.sensitivity().cmp(&other.sensitivity());
        let c_order = self.categories().compare(other.categories())?;
        if s_order == c_order {
            return Some(s_order);
        } else if c_order == Ordering::Equal {
            return Some(s_order);
        } else if s_order == Ordering::Equal {
            return Some(c_order);
        }
        None
    }

    /// Returns `true` if `self` dominates `other` (i.e. is greater than or equal to).
    pub fn dominates(&self, other: &Self) -> bool {
        self.sensitivity() >= other.sensitivity()
            && self.categories().is_superset(other.categories())
    }

    /// Parses [`MlsLevel`] from the supplied string slice.
    pub fn parse_str(policy: &Policy, level: &str) -> Result<Self, SecurityContextError> {
        if level.is_empty() {
            return Err(SecurityContextError::InvalidSyntax);
        }

        // Parse the parts before looking up values, to catch invalid syntax.
        let mut items = level.split(':');
        let sensitivity = items.next().ok_or(SecurityContextError::InvalidSyntax)?;
        let categories_item = items.next();
        if items.next().is_some() {
            return Err(SecurityContextError::InvalidSyntax);
        }

        // Lookup the sensitivity, and associated categories/ranges, if any.
        let sensitivity = policy
            .sensitivities
            .get_by_name(sensitivity.as_bytes())
            .ok_or_else(|| SecurityContextError::UnknownSensitivity { name: sensitivity.into() })?
            .id();

        let mut categories = CategorySetBuilder::new();
        if let Some(categories_str) = categories_item {
            for entry in categories_str.split(',') {
                if let Some((low_str, high_str)) = entry.split_once('.') {
                    let low = Self::category_id_by_name(policy, low_str)?;
                    let high = Self::category_id_by_name(policy, high_str)?;
                    if high <= low {
                        return Err(SecurityContextError::InvalidSyntax);
                    }
                    categories.insert_range(low, high);
                } else {
                    let id = Self::category_id_by_name(policy, entry)?;
                    categories.insert(id);
                };
            }
        }

        Ok(Self::new(sensitivity, categories.build()))
    }

    fn category_id_by_name(
        policy: &Policy,
        name: &str,
    ) -> Result<CategoryId, SecurityContextError> {
        Ok(policy
            .categories
            .get_by_name(name.as_bytes())
            .ok_or_else(|| SecurityContextError::UnknownCategory { name: name.into() })?
            .id())
    }

    /// Serializes this [`MlsLevel`] to a canonical security level byte string.
    pub fn serialize_str(&self, policy: &Policy) -> Vec<u8> {
        let sensitivity = policy.sensitivities.get_by_id(self.sensitivity()).unwrap().name();
        let categories = self
            .categories
            .spans()
            .map(|x| x.serialize_str(policy))
            .collect::<Vec<Vec<u8>>>()
            .join(b",".as_ref());

        if categories.is_empty() {
            sensitivity.to_vec()
        } else {
            [sensitivity, categories.as_slice()].join(b":".as_ref())
        }
    }
}

/// Returns the set of categories present in both `left` and `right`.
fn intersect_categories(left: &MlsLevel, right: &MlsLevel) -> CategorySet {
    let right_categories = right.categories();
    CategorySet::from_ids(left.categories().iter().filter(|id| right_categories.contains(*id)))
}

/// Security range in Multi-Level Security (MLS), consisting of a low level and an optional
/// high level.
///
/// Ranges whose high level equals their low level are normalized to omit `high`, ensuring that
/// equivalent single-level ranges compare and serialize identically.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MlsRange {
    low: MlsLevel,
    high: Option<MlsLevel>,
}

impl MlsRange {
    /// Constructs a new [`MlsRange`], normalizing equal low and high levels to omit `high`.
    pub fn new(low: MlsLevel, high: Option<MlsLevel>) -> Self {
        let high = high.filter(|high| *high != low);
        Self { low, high }
    }

    /// Returns the low security level.
    pub fn low(&self) -> &MlsLevel {
        &self.low
    }

    /// Returns the high security level, if distinct from [`Self::low`].
    pub fn high(&self) -> Option<&MlsLevel> {
        self.high.as_ref()
    }

    /// Returns the high security level if distinct from [`Self::low`], or [`Self::low`]
    /// otherwise.
    pub fn effective_high(&self) -> &MlsLevel {
        self.high.as_ref().unwrap_or(&self.low)
    }

    /// Returns `true` if the range is internally consistent (i.e., [`Self::high`], if present,
    /// dominates [`Self::low`]).
    pub fn is_consistent(&self) -> bool {
        self.high.as_ref().is_none_or(|high| high.dominates(&self.low))
    }

    /// Returns `true` if `level` is within this range (i.e., `level` dominates [`Self::low`] and
    /// [`Self::effective_high`] dominates `level`).
    pub fn contains_level(&self, level: &MlsLevel) -> bool {
        level.dominates(&self.low) && self.effective_high().dominates(level)
    }

    /// Computes the `glblub` range from `self` and `other`, with low sensitivity `max(lows)`,
    /// high sensitivity `min(highs)`, and low and high category sets intersected at each endpoint.
    ///
    /// If the two sensitivity ranges do not overlap then the resulting range is inconsistent (i.e.
    /// its high level does not dominate its low level), and is rejected when the resulting
    /// [`super::SecurityContext`] is validated.
    pub fn glblub(&self, other: &Self) -> Self {
        let self_high = self.effective_high();
        let other_high = other.effective_high();

        let low = MlsLevel::new(
            std::cmp::max(self.low.sensitivity(), other.low.sensitivity()),
            intersect_categories(&self.low, &other.low),
        );
        let high = MlsLevel::new(
            std::cmp::min(self_high.sensitivity(), other_high.sensitivity()),
            intersect_categories(self_high, other_high),
        );
        Self::new(low, Some(high))
    }

    /// Parses an [`MlsRange`] from `<level>[-<level>]` string syntax.
    pub fn parse_str(policy: &Policy, range_str: &str) -> Result<Self, SecurityContextError> {
        let mut levels = range_str.split('-');
        let low_str = levels.next().ok_or(SecurityContextError::InvalidSyntax)?;
        if low_str.is_empty() {
            return Err(SecurityContextError::InvalidSyntax);
        }
        let high_str = levels.next();
        if high_str.is_some_and(|s| s.is_empty()) || levels.next().is_some() {
            return Err(SecurityContextError::InvalidSyntax);
        }

        let low = MlsLevel::parse_str(policy, low_str)?;
        let high = high_str.map(|s| MlsLevel::parse_str(policy, s)).transpose()?;
        Ok(Self::new(low, high))
    }

    /// Serializes this [`MlsRange`] to a canonical `<level>[-<level>]` byte string.
    pub fn serialize_str(&self, policy: &Policy) -> Vec<u8> {
        let mut levels = self.low.serialize_str(policy);
        if let Some(ref high) = self.high {
            levels.push(b'-');
            levels.extend(high.serialize_str(policy));
        }
        levels
    }
}

impl Parse for MlsRange {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let levels_count = u32::parse(cursor)?;
        let sensitivity_low = SensitivityId::parse(cursor)?;

        // If levels_count > 1, the MLS range contains both low and high security levels.
        if levels_count > 1 {
            let sensitivity_high = SensitivityId::parse(cursor)?;
            let low_categories = CategorySet::parse(cursor)?;
            let high_categories = CategorySet::parse(cursor)?;

            Ok(Self::new(
                MlsLevel::new(sensitivity_low, low_categories),
                Some(MlsLevel::new(sensitivity_high, high_categories)),
            ))
        } else {
            let low_categories = CategorySet::parse(cursor)?;
            Ok(Self::new(MlsLevel::new(sensitivity_low, low_categories), /* high = */ None))
        }
    }
}

impl Serialize for MlsRange {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        if let Some(ref high) = self.high {
            2u32.serialize(writer)?;
            self.low.sensitivity.serialize(writer)?;
            high.sensitivity.serialize(writer)?;
            self.low.categories.serialize(writer)?;
            high.categories.serialize(writer)?;
        } else {
            1u32.serialize(writer)?;
            self.low.sensitivity.serialize(writer)?;
            self.low.categories.serialize(writer)?;
        }
        Ok(())
    }
}

impl Validate for MlsRange {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        self.low.validate(policy)?;
        if let Some(ref high) = self.high {
            high.validate(policy)?;
        }
        if !self.is_consistent() {
            return Err(ValidateError::InvalidMlsRange);
        }
        Ok(())
    }
}

/// MLS sensitivity level definition in an SELinux policy.
#[derive(Debug, Validate, HasName, HasPolicyId)]
pub struct Sensitivity {
    id: SensitivityId,
    name: Box<[u8]>,
    is_alias: bool,
    level: MlsLevel,
}

impl Sensitivity {
    /// Returns whether this sensitivity was declared as an alias in policy.
    #[cfg(test)]
    pub fn is_alias(&self) -> bool {
        self.is_alias
    }

    /// Default [`MlsLevel`] associated with this sensitivity.
    #[cfg(test)]
    pub fn level(&self) -> &MlsLevel {
        &self.level
    }
}

#[derive(Parse, Serialize)]
struct BinarySensitivityMetadata {
    length: u32,
    is_alias: u32,
}

impl Parse for Sensitivity {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinarySensitivityMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.length as usize)?);
        let level = MlsLevel::parse(cursor)?;
        Ok(Self { id: level.sensitivity(), name, is_alias: metadata.is_alias != 0, level })
    }
}

impl Serialize for Sensitivity {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        let metadata = BinarySensitivityMetadata {
            length: self.name.len() as u32,
            is_alias: if self.is_alias { 1 } else { 0 },
        };
        metadata.serialize(writer)?;
        writer.write_bytes(&self.name);
        self.level.serialize(writer)?;
        Ok(())
    }
}

impl Validate for SensitivityId {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        policy
            .sensitivities
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "sensitivity", id: self.as_u32() })
    }
}

/// MLS category definition in an SELinux policy.
#[derive(Debug, Validate, HasName, HasPolicyId)]
pub struct Category {
    id: CategoryId,
    name: Box<[u8]>,
    is_alias: bool,
}

impl Category {
    /// Returns whether this category was declared as an alias in policy.
    #[cfg(test)]
    pub fn is_alias(&self) -> bool {
        self.is_alias
    }
}

#[derive(Parse, Serialize)]
struct BinaryCategoryMetadata {
    length: u32,
    id: u32,
    is_alias: u32,
}

impl Parse for Category {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryCategoryMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.length as usize)?);
        let id = CategoryId::from_u32(metadata.id)
            .ok_or(ParseError::InvalidId { value: metadata.id })?;
        Ok(Self { id, name, is_alias: metadata.is_alias != 0 })
    }
}

impl Serialize for Category {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        let metadata = BinaryCategoryMetadata {
            length: self.name.len() as u32,
            id: self.id.as_u32(),
            is_alias: if self.is_alias { 1 } else { 0 },
        };
        metadata.serialize(writer)?;
        writer.write_bytes(&self.name);
        Ok(())
    }
}

impl Validate for CategoryId {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        policy
            .categories
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "category", id: self.as_u32() })
    }
}

/// Range transition rule for target object creation under MLS.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
pub struct RangeTransition {
    source_type: TypeId,
    target_type: TypeId,
    target_class: ClassId,
    mls_range: MlsRange,
}

impl RangeTransition {
    /// Returns the source [`TypeId`].
    pub fn source_type(&self) -> TypeId {
        self.source_type
    }

    /// Returns the target [`TypeId`].
    pub fn target_type(&self) -> TypeId {
        self.target_type
    }

    /// Returns the target [`ClassId`].
    pub fn target_class(&self) -> ClassId {
        self.target_class
    }

    /// Returns the target [`MlsRange`].
    pub fn mls_range(&self) -> &MlsRange {
        &self.mls_range
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::metadata::PolicyVersion;
    use crate::policy::parser::PolicyWriter;
    use crate::policy::traits::{HasName, HasPolicyId};

    // Creates a category range for testing.
    fn cat(low: u32, high: u32) -> CategorySpan {
        CategorySpan::new(
            CategoryId::from_u32(low).expect("category ids are nonzero"),
            CategoryId::from_u32(high).expect("category ids are nonzero"),
        )
    }

    // Compares two sets of categories for testing.
    fn compare(lhs: &[CategorySpan], rhs: &[CategorySpan]) -> Option<Ordering> {
        let lhs_set = CategorySet::from_ids(lhs.iter().flat_map(|span| {
            (span.low().as_u32()..=span.high().as_u32()).map(|i| CategoryId::from_u32(i).unwrap())
        }));
        let rhs_set = CategorySet::from_ids(rhs.iter().flat_map(|span| {
            (span.low().as_u32()..=span.high().as_u32()).map(|i| CategoryId::from_u32(i).unwrap())
        }));
        lhs_set.compare(&rhs_set)
    }

    #[test]
    fn category_compare() {
        let cat_1 = cat(1, 1);
        let cat_2 = cat(1, 3);
        let cat_3 = cat(2, 3);
        assert_eq!(compare(&[cat_1], &[cat_1]), Some(Ordering::Equal));
        assert_eq!(compare(&[cat_1], &[cat_2]), Some(Ordering::Less));
        assert_eq!(compare(&[cat_1], &[cat_3]), None);
        assert_eq!(compare(&[cat_2], &[cat_1]), Some(Ordering::Greater));
        assert_eq!(compare(&[cat_2], &[cat_3]), Some(Ordering::Greater));
    }

    #[test]
    fn categories_compare_empty_iter() {
        let cats_0 = &[];
        let cats_1 = &[cat(1, 1)];
        assert_eq!(compare(cats_0, cats_0), Some(Ordering::Equal));
        assert_eq!(compare(cats_0, cats_1), Some(Ordering::Less));
        assert_eq!(compare(cats_1, cats_0), Some(Ordering::Greater));
    }

    #[test]
    fn categories_compare_same_length() {
        let cats_1 = &[cat(1, 1), cat(3, 3)];
        let cats_2 = &[cat(1, 1), cat(4, 4)];
        let cats_3 = &[cat(1, 2), cat(4, 4)];
        let cats_4 = &[cat(1, 2), cat(4, 5)];

        assert_eq!(compare(cats_1, cats_1), Some(Ordering::Equal));
        assert_eq!(compare(cats_1, cats_2), None);
        assert_eq!(compare(cats_1, cats_3), None);
        assert_eq!(compare(cats_1, cats_4), None);

        assert_eq!(compare(cats_2, cats_1), None);
        assert_eq!(compare(cats_2, cats_2), Some(Ordering::Equal));
        assert_eq!(compare(cats_2, cats_3), Some(Ordering::Less));
        assert_eq!(compare(cats_2, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_3, cats_1), None);
        assert_eq!(compare(cats_3, cats_2), Some(Ordering::Greater));
        assert_eq!(compare(cats_3, cats_3), Some(Ordering::Equal));
        assert_eq!(compare(cats_3, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_4, cats_1), None);
        assert_eq!(compare(cats_4, cats_2), Some(Ordering::Greater));
        assert_eq!(compare(cats_4, cats_3), Some(Ordering::Greater));
        assert_eq!(compare(cats_4, cats_4), Some(Ordering::Equal));
    }

    #[test]
    fn categories_compare_different_lengths() {
        let cats_1 = &[cat(1, 1)];
        let cats_2 = &[cat(1, 4)];
        let cats_3 = &[cat(1, 1), cat(4, 4)];
        let cats_4 = &[cat(1, 2), cat(4, 5), cat(7, 7)];

        assert_eq!(compare(cats_1, cats_3), Some(Ordering::Less));
        assert_eq!(compare(cats_1, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_2, cats_3), Some(Ordering::Greater));
        assert_eq!(compare(cats_2, cats_4), None);

        assert_eq!(compare(cats_3, cats_1), Some(Ordering::Greater));
        assert_eq!(compare(cats_3, cats_2), Some(Ordering::Less));
        assert_eq!(compare(cats_3, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_4, cats_1), Some(Ordering::Greater));
        assert_eq!(compare(cats_4, cats_2), None);
        assert_eq!(compare(cats_4, cats_3), Some(Ordering::Greater));
    }

    #[test]
    // Test cases where one interval appears before or after all intervals of the
    // other set, or in a gap between intervals of the other set.
    fn categories_compare_with_gaps() {
        let cats_1 = &[cat(1, 2), cat(4, 5)];
        let cats_2 = &[cat(4, 5)];
        let cats_3 = &[cat(2, 5), cat(10, 11)];
        let cats_4 = &[cat(2, 5), cat(7, 8), cat(10, 11)];

        assert_eq!(compare(cats_1, cats_2), Some(Ordering::Greater));
        assert_eq!(compare(cats_1, cats_3), None);
        assert_eq!(compare(cats_1, cats_4), None);

        assert_eq!(compare(cats_2, cats_1), Some(Ordering::Less));
        assert_eq!(compare(cats_2, cats_3), Some(Ordering::Less));
        assert_eq!(compare(cats_2, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_3, cats_1), None);
        assert_eq!(compare(cats_3, cats_2), Some(Ordering::Greater));
        assert_eq!(compare(cats_3, cats_4), Some(Ordering::Less));

        assert_eq!(compare(cats_4, cats_1), None);
        assert_eq!(compare(cats_4, cats_2), Some(Ordering::Greater));
        assert_eq!(compare(cats_4, cats_3), Some(Ordering::Greater));
    }

    #[test]
    fn test_category_parse_and_serialize() {
        let data = [
            4, 0, 0, 0, // length = 4
            5, 0, 0, 0, // id = 5
            1, 0, 0, 0, // is_alias = 1 (true)
            b't', b'e', b's', b't', // name: "test"
        ];
        let mut cursor = PolicyCursor::new(&data);
        let category = Category::parse(&mut cursor).expect("parse Category");
        assert_eq!(category.id(), CategoryId::from_u32(5).unwrap());
        assert!(category.is_alias());
        assert_eq!(category.name(), b"test");

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        category.serialize(&mut policy_writer).expect("serialize Category");
        assert_eq!(writer.as_slice(), &data);
    }

    #[test]
    fn test_category_invalid_id() {
        let data = [
            4, 0, 0, 0, // length = 4
            0, 0, 0, 0, // id = 0 (invalid)
            0, 0, 0, 0, // is_alias = 0
            b't', b'e', b's', b't', // name: "test"
        ];
        let mut cursor = PolicyCursor::new(&data);
        assert!(matches!(Category::parse(&mut cursor), Err(ParseError::InvalidId { value: 0 })));
    }

    #[test]
    fn test_sensitivity_parse_and_serialize() {
        let data = [
            4, 0, 0, 0, // length = 4
            0, 0, 0, 0, // is_alias = 0 (false)
            b's', b'e', b'n', b's', // name: "sens"
            // Trailing MlsLevel on wire (level_datum->level):
            2, 0, 0, 0, // sensitivity = 2
            // CategorySet (ExtensibleBitmap):
            64, 0, 0, 0, // map_item_size_bits = 64
            0, 0, 0, 0, // high_bit = 0
            0, 0, 0, 0, // categories count = 0
        ];

        let mut cursor = PolicyCursor::new(&data);
        let sensitivity = Sensitivity::parse(&mut cursor).expect("parse Sensitivity");
        assert_eq!(sensitivity.id(), SensitivityId::from_u32(2).unwrap());
        assert!(!sensitivity.is_alias());
        assert_eq!(sensitivity.name(), b"sens");
        assert_eq!(sensitivity.level().sensitivity(), SensitivityId::from_u32(2).unwrap());
        assert_eq!(sensitivity.level().categories(), &CategorySet::from_ids([]));

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        sensitivity.serialize(&mut policy_writer).expect("serialize Sensitivity");
        assert_eq!(writer.as_slice(), &data);
    }

    #[test]
    fn test_range_transition_parse_and_serialize() {
        let data = [
            1, 0, 0, 0, // source_type = 1
            2, 0, 0, 0, // target_type = 2
            3, 0, 0, 0, // target_class = 3
            // MlsRange:
            1, 0, 0, 0, // levels_count = 1
            1, 0, 0, 0, // sensitivity_low = 1
            64, 0, 0, 0, // map_item_size_bits = 64
            0, 0, 0, 0, // high_bit = 0
            0, 0, 0, 0, // categories count = 0
        ];
        let mut cursor = PolicyCursor::new(&data);
        let transition = RangeTransition::parse(&mut cursor).expect("parse RangeTransition");
        assert_eq!(transition.source_type(), TypeId::from_u32(1).unwrap());
        assert_eq!(transition.target_type(), TypeId::from_u32(2).unwrap());
        assert_eq!(transition.target_class(), ClassId::from_u32(3).unwrap());
        assert_eq!(transition.mls_range().low().sensitivity(), SensitivityId::from_u32(1).unwrap());

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        transition.serialize(&mut policy_writer).expect("serialize RangeTransition");
        assert_eq!(writer.as_slice(), &data);
    }

    #[test]
    fn test_mls_range_normalizes_equal_low_and_high_levels() {
        let level = MlsLevel::new(
            SensitivityId::from_u32(1).unwrap(),
            CategorySet::from_ids([CategoryId::from_u32(1).unwrap()]),
        );

        let mut bytes = Vec::new();
        let mut writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        2u32.serialize(&mut writer).unwrap();
        level.sensitivity().serialize(&mut writer).unwrap();
        level.sensitivity().serialize(&mut writer).unwrap();
        level.categories().serialize(&mut writer).unwrap();
        level.categories().serialize(&mut writer).unwrap();

        let parsed_range = MlsRange::parse(&mut PolicyCursor::new(&bytes)).unwrap();
        assert_eq!(parsed_range.high(), None);
        assert_eq!(parsed_range, MlsRange::new(level, /* high = */ None));
    }
}
