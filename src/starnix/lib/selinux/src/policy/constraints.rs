// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use selinux_policy_derive::{Parse, Serialize, Validate};

use super::bitmap::IdSet;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::{Array, PolicyCursor, PolicyWriter};
use super::traits::{Parse, PolicyId, Serialize, Validate};
use super::{AccessVector, MlsLevel, Policy, RoleId, SecurityContext, TypeId, UserId};

/// Set of identifiers (Users, Roles, or Types) with negative matching and flags used in constraints.
/// Note that in practice this structure's fields are always empty for User and Role expressions,
/// and only set for Type expressions. Even if present for Type expressions, the contents are not
/// currently used at run-time, only to userspace tooling to reconstruct the original statement.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
struct ConstraintSets<T: PolicyId> {
    include_set: IdSet<T>,
    exclude_set: IdSet<T>,
    flags: u32,
}

impl<T: PolicyId> ConstraintSets<T> {
    fn empty() -> Self {
        Self { include_set: IdSet::empty(), exclude_set: IdSet::empty(), flags: 0 }
    }

    /// Returns true if `include_set`, `exclude_set`, and `flags` are all empty or zero.
    fn is_empty(&self) -> bool {
        self.include_set.is_empty() && self.exclude_set.is_empty() && self.flags == 0
    }
}

const TARGET_FLAG: u32 = 0x8;
const USER: u32 = 0x1;
const ROLE: u32 = 0x2;
const TYPE: u32 = 0x4;
const L1_L2: u32 = 0x20;
const L1_H2: u32 = 0x40;
const H1_L2: u32 = 0x80;
const H1_H2: u32 = 0x100;
const L1_H1: u32 = 0x200;
const L2_H2: u32 = 0x400;

const VALID_MLS_FLAGS: u32 = L1_L2 | L1_H2 | H1_L2 | H1_H2 | L1_H1 | L2_H2;
const VALID_NON_MLS_FLAGS: u32 = USER | ROLE | TYPE;

/// Pair of left and right operands for an MLS level constraint comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MlsOperands {
    L1L2,
    L1H2,
    H1L2,
    H1H2,
    L1H1,
    L2H2,
}

impl MlsOperands {
    fn from_u32(val: u32) -> Option<Self> {
        match val {
            L1_L2 => Some(Self::L1L2),
            L1_H2 => Some(Self::L1H2),
            H1_L2 => Some(Self::H1L2),
            H1_H2 => Some(Self::H1H2),
            L1_H1 => Some(Self::L1H1),
            L2_H2 => Some(Self::L2H2),
            _ => None,
        }
    }

    fn as_u32(&self) -> u32 {
        match self {
            Self::L1L2 => L1_L2,
            Self::L1H2 => L1_H2,
            Self::H1L2 => H1_L2,
            Self::H1H2 => H1_H2,
            Self::L1H1 => L1_H1,
            Self::L2H2 => L2_H2,
        }
    }
}

/// Subject of a constraint operand (source or target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintSubject {
    Source,
    Target,
}

impl ConstraintSubject {
    fn as_u32(&self) -> u32 {
        match self {
            Self::Source => 0,
            Self::Target => TARGET_FLAG,
        }
    }
}

impl Validate for ConstraintSubject {
    fn validate(&self, _policy: &Policy) -> Result<(), ValidateError> {
        Ok(())
    }
}

/// Equality operator in a non-MLS constraint expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum ConstraintOperator {
    Eq = 1,
    Ne = 2,
}

/// Operator in an MLS constraint expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum MlsOperator {
    Eq = 1,
    Ne = 2,
    Dom = 3,
    DomBy = 4,
    Incomp = 5,
}

/// Binary representation of a [`NameExpression`] comparing against a set of names.
#[derive(Debug, Parse, Serialize, Validate)]
struct BinaryNameExpression<T: PolicyId> {
    operator: ConstraintOperator,
    names: IdSet<T>,
    constraint_sets: ConstraintSets<T>,
}

/// Constraint expression comparing a subject attribute against a set of names.
#[derive(Debug, Clone, PartialEq, Eq, Validate)]
pub struct NameExpression<T: PolicyId> {
    subject: ConstraintSubject,
    operator: ConstraintOperator,
    names: IdSet<T>,
    constraint_sets: Option<Box<ConstraintSets<T>>>,
}

impl<T: PolicyId> NameExpression<T> {
    /// Returns the subject ([`ConstraintSubject::Source`] or [`ConstraintSubject::Target`]) of the expression.
    fn subject(&self) -> ConstraintSubject {
        self.subject
    }

    /// Returns the equality operator ([`ConstraintOperator::Eq`] or [`ConstraintOperator::Ne`]) of the expression.
    fn operator(&self) -> ConstraintOperator {
        self.operator
    }

    /// Returns the set of names compared against.
    fn names(&self) -> &IdSet<T> {
        &self.names
    }
}

impl<T: PolicyId + Parse> NameExpression<T> {
    fn parse(
        cursor: &mut PolicyCursor<'_>,
        subject: ConstraintSubject,
        expect_empty_constraint_sets: bool,
    ) -> Result<Self, ParseError> {
        let binary = BinaryNameExpression::<T>::parse(cursor)?;
        if expect_empty_constraint_sets && !binary.constraint_sets.is_empty() {
            return Err(ParseError::UnexpectedConstraintTypeSet);
        }
        let constraint_sets = if binary.constraint_sets.is_empty() {
            None
        } else {
            Some(Box::new(binary.constraint_sets))
        };
        Ok(Self { subject, operator: binary.operator, names: binary.names, constraint_sets })
    }
}

impl<T: PolicyId + Serialize> NameExpression<T> {
    fn serialize(
        &self,
        writer: &mut PolicyWriter<'_>,
        operand_type: u32,
    ) -> Result<(), SerializeError> {
        ConstraintTerm::NAME_EXPR.serialize(writer)?;
        (operand_type | self.subject.as_u32()).serialize(writer)?;
        let binary = BinaryNameExpression {
            operator: self.operator,
            names: self.names.clone(),
            constraint_sets: match self.constraint_sets {
                Some(ref sets) => (**sets).clone(),
                None => ConstraintSets::empty(),
            },
        };
        binary.serialize(writer)?;
        Ok(())
    }
}

/// Node in a constraint expression tree.
///
/// Constraint expressions evaluate whether an operation is permitted between a source and
/// target security context. Terms are evaluated in postfix order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintTerm {
    /// Logical NOT operator.
    Not,
    /// Logical AND operator.
    And,
    /// Logical OR operator.
    Or,
    /// Comparison between source and target [`UserId`] attributes.
    UserAttributeOp(ConstraintOperator),
    /// Comparison between source and target [`RoleId`] attributes.
    RoleAttributeOp(ConstraintOperator),
    /// Comparison between source and target [`TypeId`] attributes.
    TypeAttributeOp(ConstraintOperator),
    /// Comparison between high and low MLS level attributes of source and target contexts.
    MlsOp(MlsOperands, MlsOperator),
    /// Comparison of a [`UserId`] attribute against a specific set of user names.
    UserNameOp(NameExpression<UserId>),
    /// Comparison of a [`RoleId`] attribute against a specific set of role names.
    RoleNameOp(NameExpression<RoleId>),
    /// Comparison of a [`TypeId`] attribute against a specific set of type names or attributes.
    TypeNameOp(NameExpression<TypeId>),
}

impl ConstraintTerm {
    // Term types
    const NOT_OPERATOR: u32 = 1;
    const AND_OPERATOR: u32 = 2;
    const OR_OPERATOR: u32 = 3;
    const ATTR_EXPR: u32 = 4;
    const NAME_EXPR: u32 = 5;
}

impl Parse for ConstraintTerm {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let constraint_term_type = u32::parse(cursor)?;
        match constraint_term_type {
            Self::NOT_OPERATOR => {
                let _expr_operand_type = u32::parse(cursor)?;
                let _expr_operator_type = u32::parse(cursor)?;
                Ok(Self::Not)
            }
            Self::AND_OPERATOR => {
                let _expr_operand_type = u32::parse(cursor)?;
                let _expr_operator_type = u32::parse(cursor)?;
                Ok(Self::And)
            }
            Self::OR_OPERATOR => {
                let _expr_operand_type = u32::parse(cursor)?;
                let _expr_operator_type = u32::parse(cursor)?;
                Ok(Self::Or)
            }
            Self::ATTR_EXPR => {
                let operand_val = u32::parse(cursor)?;
                if (operand_val & VALID_NON_MLS_FLAGS) != 0 {
                    if (operand_val & !VALID_NON_MLS_FLAGS) != 0 {
                        return Err(ParseError::InvalidConstraintOperandType {
                            value: operand_val,
                        });
                    }
                    let operator = ConstraintOperator::parse(cursor)?;
                    match operand_val {
                        USER => Ok(Self::UserAttributeOp(operator)),
                        ROLE => Ok(Self::RoleAttributeOp(operator)),
                        TYPE => Ok(Self::TypeAttributeOp(operator)),
                        _ => Err(ParseError::InvalidConstraintOperandType { value: operand_val }),
                    }
                } else {
                    if (operand_val & !VALID_MLS_FLAGS) != 0 {
                        return Err(ParseError::InvalidConstraintOperandType {
                            value: operand_val,
                        });
                    }
                    let operator = MlsOperator::parse(cursor)?;
                    let operand = MlsOperands::from_u32(operand_val)
                        .ok_or(ParseError::InvalidConstraintOperandType { value: operand_val })?;
                    Ok(Self::MlsOp(operand, operator))
                }
            }
            Self::NAME_EXPR => {
                let operand_val = u32::parse(cursor)?;

                if (operand_val & VALID_NON_MLS_FLAGS) == 0 {
                    return Err(ParseError::InvalidConstraintOperandType { value: operand_val });
                }
                if (operand_val & !(VALID_NON_MLS_FLAGS | TARGET_FLAG)) != 0 {
                    return Err(ParseError::InvalidConstraintOperandType { value: operand_val });
                }

                let subject = if operand_val & TARGET_FLAG != 0 {
                    ConstraintSubject::Target
                } else {
                    ConstraintSubject::Source
                };

                match operand_val & !TARGET_FLAG {
                    USER => Ok(Self::UserNameOp(NameExpression::parse(cursor, subject, true)?)),
                    ROLE => Ok(Self::RoleNameOp(NameExpression::parse(cursor, subject, true)?)),
                    TYPE => Ok(Self::TypeNameOp(NameExpression::parse(cursor, subject, false)?)),
                    _ => Err(ParseError::InvalidConstraintOperandType { value: operand_val }),
                }
            }
            _ => Err(ParseError::InvalidConstraintTermType { value: constraint_term_type }),
        }
    }
}

impl Serialize for ConstraintTerm {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        match self {
            Self::Not => {
                Self::NOT_OPERATOR.serialize(writer)?;
                0u32.serialize(writer)?;
                0u32.serialize(writer)?;
            }
            Self::And => {
                Self::AND_OPERATOR.serialize(writer)?;
                0u32.serialize(writer)?;
                0u32.serialize(writer)?;
            }
            Self::Or => {
                Self::OR_OPERATOR.serialize(writer)?;
                0u32.serialize(writer)?;
                0u32.serialize(writer)?;
            }
            Self::UserAttributeOp(operator) => {
                Self::ATTR_EXPR.serialize(writer)?;
                USER.serialize(writer)?;
                operator.serialize(writer)?;
            }
            Self::RoleAttributeOp(operator) => {
                Self::ATTR_EXPR.serialize(writer)?;
                ROLE.serialize(writer)?;
                operator.serialize(writer)?;
            }
            Self::TypeAttributeOp(operator) => {
                Self::ATTR_EXPR.serialize(writer)?;
                TYPE.serialize(writer)?;
                operator.serialize(writer)?;
            }
            Self::MlsOp(operand, operator) => {
                Self::ATTR_EXPR.serialize(writer)?;
                operand.as_u32().serialize(writer)?;
                operator.serialize(writer)?;
            }
            Self::UserNameOp(expr) => expr.serialize(writer, USER)?,
            Self::RoleNameOp(expr) => expr.serialize(writer, ROLE)?,
            Self::TypeNameOp(expr) => expr.serialize(writer, TYPE)?,
        }
        Ok(())
    }
}

impl Validate for ConstraintTerm {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        match self {
            Self::UserNameOp(expr) => expr.validate(policy)?,
            Self::RoleNameOp(expr) => expr.validate(policy)?,
            Self::TypeNameOp(expr) => expr.validate(policy)?,
            _ => {}
        }
        Ok(())
    }
}

/// Sequence of terms in a constraint expression tree, in postfix order.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize)]
struct ConstraintNode {
    terms: Array<ConstraintTerm>,
}

impl ConstraintNode {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

impl Validate for ConstraintNode {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        self.terms.validate(policy)?;
        let mut stack_depth: usize = 0;
        for term in self.terms.iter() {
            match term {
                ConstraintTerm::Not => {
                    if stack_depth < 1 {
                        return Err(ValidateError::InvalidConstraintTermSequence);
                    }
                }
                ConstraintTerm::And | ConstraintTerm::Or => {
                    if stack_depth < 2 {
                        return Err(ValidateError::InvalidConstraintTermSequence);
                    }
                    stack_depth -= 1;
                }
                _ => {
                    stack_depth += 1;
                    if stack_depth > BitStack::MAX_DEPTH {
                        return Err(ValidateError::InvalidConstraintTermSequence);
                    }
                }
            }
        }
        if stack_depth != 1 {
            return Err(ValidateError::InvalidConstraintTermSequence);
        }
        Ok(())
    }
}

/// Security policy constraint restricting permissions based on context attributes.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
pub struct Constraint {
    access_vector: AccessVector,
    constraint_expr: ConstraintNode,
}

impl Constraint {
    pub fn access_vector(&self) -> AccessVector {
        self.access_vector
    }

    #[cfg(test)]
    fn constraint_expr(&self) -> &ConstraintNode {
        &self.constraint_expr
    }

    /// Evaluates whether `source` and `target` satisfy this constraint.
    ///
    /// Assumes that the constraint expression terms were sequenced in postfix order and validated
    /// at policy load time by [`ConstraintNode::validate`].
    pub fn evaluate(&self, source: &SecurityContext, target: &SecurityContext) -> bool {
        let mut stack = BitStack::default();
        for term in self.constraint_expr.terms.iter() {
            match term {
                ConstraintTerm::Not => {
                    let arg = stack.pop();
                    stack.push(!arg);
                }
                ConstraintTerm::And => {
                    let right = stack.pop();
                    let left = stack.pop();
                    stack.push(left && right);
                }
                ConstraintTerm::Or => {
                    let right = stack.pop();
                    let left = stack.pop();
                    stack.push(left || right);
                }
                ConstraintTerm::UserAttributeOp(operator) => {
                    stack.push(evaluate_simple(source.user(), target.user(), *operator));
                }
                ConstraintTerm::RoleAttributeOp(operator) => {
                    stack.push(evaluate_simple(source.role(), target.role(), *operator));
                }
                ConstraintTerm::TypeAttributeOp(operator) => {
                    stack.push(evaluate_simple(source.type_(), target.type_(), *operator));
                }
                ConstraintTerm::UserNameOp(expr) => {
                    stack.push(evaluate_name_expr(expr, source.user(), target.user()));
                }
                ConstraintTerm::RoleNameOp(expr) => {
                    stack.push(evaluate_name_expr(expr, source.role(), target.role()));
                }
                ConstraintTerm::TypeNameOp(expr) => {
                    stack.push(evaluate_name_expr(expr, source.type_(), target.type_()));
                }
                ConstraintTerm::MlsOp(operands, operator) => {
                    let (source_range, target_range) = (source.mls_range(), target.mls_range());
                    let (left, right) = match operands {
                        MlsOperands::L1L2 => (source_range.low(), target_range.low()),
                        MlsOperands::L1H2 => (source_range.low(), target_range.effective_high()),
                        MlsOperands::H1L2 => (source_range.effective_high(), target_range.low()),
                        MlsOperands::H1H2 => {
                            (source_range.effective_high(), target_range.effective_high())
                        }
                        MlsOperands::L1H1 => (source_range.low(), source_range.effective_high()),
                        MlsOperands::L2H2 => (target_range.low(), target_range.effective_high()),
                    };
                    stack.push(evaluate_mls(left, right, *operator));
                }
            }
        }
        let result = stack.pop();
        #[cfg(debug_assertions)]
        debug_assert!(stack.is_empty(), "validated term sequence leaves stack empty");
        result
    }
}

/// Fixed-capacity boolean operand stack backed by a single `u64` bitmask.
#[derive(Default)]
struct BitStack {
    bits: u64,
    /// Expression term sequences are validated at policy load time by [`ConstraintNode::validate`],
    /// so stack depth is only tracked in debug builds to avoid runtime underflow checks in `pop()`.
    #[cfg(debug_assertions)]
    len: u32,
}

impl BitStack {
    const MAX_DEPTH: usize = u64::BITS as usize;

    #[inline(always)]
    fn push(&mut self, val: bool) {
        #[cfg(debug_assertions)]
        {
            debug_assert!((self.len as usize) < Self::MAX_DEPTH);
            self.len += 1;
        }
        self.bits = (self.bits << 1) | (val as u64);
    }

    #[inline(always)]
    fn pop(&mut self) -> bool {
        #[cfg(debug_assertions)]
        {
            debug_assert!(self.len > 0);
            self.len -= 1;
        }
        let val = (self.bits & 1) != 0;
        self.bits >>= 1;
        val
    }

    #[cfg(debug_assertions)]
    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

fn evaluate_simple<T: Eq>(left: T, right: T, operator: ConstraintOperator) -> bool {
    match operator {
        ConstraintOperator::Eq => left == right,
        ConstraintOperator::Ne => left != right,
    }
}

fn evaluate_name_expr<T: PolicyId>(expr: &NameExpression<T>, source: T, target: T) -> bool {
    let val = match expr.subject() {
        ConstraintSubject::Source => source,
        ConstraintSubject::Target => target,
    };
    evaluate_simple(expr.names().contains(val), /* right = */ true, expr.operator())
}

fn evaluate_mls(left: &MlsLevel, right: &MlsLevel, operator: MlsOperator) -> bool {
    match operator {
        MlsOperator::Eq => left == right,
        MlsOperator::Ne => left != right,
        MlsOperator::Dom => left.dominates(right),
        MlsOperator::DomBy => right.dominates(left),
        MlsOperator::Incomp => left.compare(right).is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::{PolicyCursor, *};
    use crate::policy::metadata::PolicyVersion;
    use crate::policy::parser::PolicyWriter;

    #[test]
    fn test_minimal_constraint_parse_and_serialize() {
        let data = [
            1, 0, 0, 0, // access_vector = 1
            0, 0, 0, 0, // constraint_expr (Array count = 0)
        ];
        let mut cursor = PolicyCursor::new(&data);
        let constraint = Constraint::parse(&mut cursor).unwrap();
        assert_eq!(constraint.access_vector(), AccessVector::from(1));
        assert!(constraint.constraint_expr().is_empty());

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        constraint.serialize(&mut policy_writer).unwrap();
        assert_eq!(writer, data);
    }

    #[test]
    fn test_constraint_term_with_users() {
        let data = [
            // metadata (NAME_EXPR, OPERAND_USER, OPERATOR_EQ)
            5, 0, 0, 0, // constraint_term_type = 5 (NAME_EXPR)
            1, 0, 0, 0, // expr_operand_type = 1 (OPERAND_USER)
            1, 0, 0, 0, // expr_operator_type = 1 (OPERATOR_EQ)
            // ids (IdSet<UserId> containing user 1)
            64, 0, 0, 0, // map_item_size_bits = 64
            64, 0, 0, 0, // high_bit = 64
            1, 0, 0, 0, // count = 1
            0, 0, 0, 0, // start_bit = 0
            1, 0, 0, 0, 0, 0, 0, 0, // map = 1 (bit 0 set -> ID 1)
            // type_set (empty ConstraintSet<UserId>: 28 bytes)
            64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // types (empty bitmap)
            64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // negative_set (empty bitmap)
            0, 0, 0, 0, // flags = 0
        ];
        let mut cursor = PolicyCursor::new(&data);
        let term = ConstraintTerm::parse(&mut cursor).unwrap();

        match &term {
            ConstraintTerm::UserNameOp(expr) => {
                assert_eq!(expr.subject(), ConstraintSubject::Source);
                assert_eq!(expr.operator(), ConstraintOperator::Eq);
                assert!(expr.names().contains(UserId::for_test(1)));
                assert!(!expr.names().contains(UserId::for_test(2)));
            }
            _ => panic!("expected UserNameOp"),
        }

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        term.serialize(&mut policy_writer).unwrap();
        assert_eq!(writer, data);
    }

    #[test]
    fn test_constraint_node_validate_term_sequence() {
        let policy_bytes = include_bytes!("../../testdata/policies/selinux_testsuite");
        let policy = Policy::parse(policy_bytes).unwrap();

        // A valid single-term sequence
        let valid_data = [
            1, 0, 0, 0, // count = 1
            4, 0, 0, 0, // ATTR_EXPR
            1, 0, 0, 0, // USER
            1, 0, 0, 0, // EQ
        ];
        let mut cursor = PolicyCursor::new(&valid_data);
        let node = ConstraintNode::parse(&mut cursor).unwrap();
        assert!(node.validate(&policy).is_ok());

        // An invalid sequence (stack underflow on NOT)
        let invalid_data = [
            1, 0, 0, 0, // count = 1
            1, 0, 0, 0, // NOT
            0, 0, 0, 0, // unused operand_type
            0, 0, 0, 0, // unused operator_type
        ];
        let mut cursor = PolicyCursor::new(&invalid_data);
        let node = ConstraintNode::parse(&mut cursor).unwrap();
        assert_eq!(node.validate(&policy), Err(ValidateError::InvalidConstraintTermSequence));

        // A valid sequence reaching `BitStack::MAX_DEPTH` (64 leaf terms + 63 AND terms)
        let leaf_term = [
            4, 0, 0, 0, // ATTR_EXPR
            1, 0, 0, 0, // USER
            1, 0, 0, 0, // EQ
        ];
        let and_term = [
            2, 0, 0, 0, // AND
            0, 0, 0, 0, // unused operand_type
            0, 0, 0, 0, // unused operator_type
        ];
        let max_depth_count = (BitStack::MAX_DEPTH * 2 - 1) as u32;
        let mut max_depth_data = max_depth_count.to_le_bytes().to_vec();
        for _ in 0..BitStack::MAX_DEPTH {
            max_depth_data.extend_from_slice(&leaf_term);
        }
        for _ in 0..(BitStack::MAX_DEPTH - 1) {
            max_depth_data.extend_from_slice(&and_term);
        }
        let mut cursor = PolicyCursor::new(&max_depth_data);
        let node = ConstraintNode::parse(&mut cursor).unwrap();
        assert!(node.validate(&policy).is_ok());

        // An invalid sequence exceeding `BitStack::MAX_DEPTH` (65 leaf terms + 64 AND terms)
        let overflow_count = ((BitStack::MAX_DEPTH + 1) * 2 - 1) as u32;
        let mut overflow_data = overflow_count.to_le_bytes().to_vec();
        for _ in 0..=BitStack::MAX_DEPTH {
            overflow_data.extend_from_slice(&leaf_term);
        }
        for _ in 0..BitStack::MAX_DEPTH {
            overflow_data.extend_from_slice(&and_term);
        }
        let mut cursor = PolicyCursor::new(&overflow_data);
        let node = ConstraintNode::parse(&mut cursor).unwrap();
        assert_eq!(node.validate(&policy), Err(ValidateError::InvalidConstraintTermSequence));
    }

    #[test]
    fn evaluate_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = Policy::parse_and_validate(policy_bytes);

        let source = policy
            .parse_security_context(b"user0:object_r:type0:s0-s0".into())
            .expect("valid source security context");
        let target = policy
            .parse_security_context(b"user1:object_r:security_t:s0:c0-s0:c0".into())
            .expect("valid target security context");

        let classes = policy.classes();
        let class_constraint_eq =
            classes.get_by_name(b"class_constraint_eq").expect("look up class");
        let class_constraint_eq_constraints = class_constraint_eq.constraints();
        assert_eq!(class_constraint_eq_constraints.len(), 1);
        // ( u1 == u2 )
        assert!(!class_constraint_eq_constraints[0].evaluate(&source, &target));

        let class_constraint_with_and =
            classes.get_by_name(b"class_constraint_with_and").expect("look up class");
        let class_constraint_with_and_constraints = class_constraint_with_and.constraints();
        assert_eq!(class_constraint_with_and_constraints.len(), 1);
        // ( ( u1 == u2 ) and ( t1 == t2 ) )
        assert!(!class_constraint_with_and_constraints[0].evaluate(&source, &target));

        let class_constraint_with_not =
            classes.get_by_name(b"class_constraint_with_not").expect("look up class");
        let class_constraint_with_not_constraints = class_constraint_with_not.constraints();
        assert_eq!(class_constraint_with_not_constraints.len(), 1);
        // ( not ( ( u1 == u2 ) and ( t1 == t2 ) )
        assert!(class_constraint_with_not_constraints[0].evaluate(&source, &target));

        let class_constraint_with_names =
            classes.get_by_name(b"class_constraint_with_names").expect("look up class");
        let class_constraint_with_names_constraints = class_constraint_with_names.constraints();
        assert_eq!(class_constraint_with_names_constraints.len(), 1);
        // ( u1 != { user0 user1 })
        assert!(!class_constraint_with_names_constraints[0].evaluate(&source, &target));

        let class_constraint_nested =
            classes.get_by_name(b"class_constraint_nested").expect("look up class");
        let class_constraint_nested_constraints = class_constraint_nested.constraints();
        assert_eq!(class_constraint_nested_constraints.len(), 1);
        // ( ( ( u2 == { user0 user1} ) and ( r1 == r2 ) ) or ( ( u1 == u2 ) and ( not (t1 == t2 ) ) ) )
        assert!(class_constraint_nested_constraints[0].evaluate(&source, &target));

        let class_constraint_role_names =
            classes.get_by_name(b"class_constraint_role_names").expect("look up class");
        let class_constraint_role_names_constraints = class_constraint_role_names.constraints();
        assert_eq!(class_constraint_role_names_constraints.len(), 1);
        // ( r1 == { object_r } ) where source is object_r
        assert!(class_constraint_role_names_constraints[0].evaluate(&source, &target));

        let class_constraint_type_names =
            classes.get_by_name(b"class_constraint_type_names").expect("look up class");
        let class_constraint_type_names_constraints = class_constraint_type_names.constraints();
        assert_eq!(class_constraint_type_names_constraints.len(), 1);
        // ( t1 == { domain } ) where source type0 is in domain
        assert!(class_constraint_type_names_constraints[0].evaluate(&source, &target));

        let class_constraint_type_list =
            classes.get_by_name(b"class_constraint_type_list").expect("look up class");
        let class_constraint_type_list_constraints = class_constraint_type_list.constraints();
        assert_eq!(class_constraint_type_list_constraints.len(), 1);
        // ( t1 == { type0 security_t } ) where source type is type0
        assert!(class_constraint_type_list_constraints[0].evaluate(&source, &target));
    }

    #[test]
    fn evaluate_mls_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = Policy::parse_and_validate(policy_bytes);

        let source = policy
            .parse_security_context(b"user0:object_r:type0:s0-s0".into())
            .expect("valid source security context");
        let target = policy
            .parse_security_context(b"user1:object_r:security_t:s0:c0-s0:c0".into())
            .expect("valid target security context");

        let classes = policy.classes();
        let class = classes.get_by_name(b"class_mls_constraints").expect("look up class");
        let constraints = class.constraints();
        // Constraints appear in reverse order in parsed policy.
        let expected = [
            false, // l1 incomp h1
            false, // h1 incomp h2
            true,  // l1 domby h2
            false, // h1 dom l2
            false, // l2 != h2
            false, // l1 == l2
        ];
        for (i, constraint) in constraints.iter().enumerate() {
            assert_eq!(constraint.evaluate(&source, &target), expected[i], "constraint {i}");
        }
    }
}
