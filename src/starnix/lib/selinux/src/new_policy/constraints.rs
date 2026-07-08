// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::bitmap::IdSet;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::{Array, PolicyCursor};
use super::traits::{Parse, PolicyId, Serialize, Validate};
use super::{AccessVector, NewPolicy, RoleId, TypeId, UserId};

pub use selinux_policy_derive::{Parse, Serialize, Validate};

/// Set of identifiers (Users, Roles, or Types) with negative matching and flags used in constraints.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
pub struct ConstraintSet<T: PolicyId + Validate> {
    types: IdSet<T>,
    negative_set: IdSet<T>,
    flags: u32,
}

impl<T: PolicyId + Validate> ConstraintSet<T> {
    /// Returns true if the type set, negative set, and flags are all empty/zero.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.negative_set.is_empty() && self.flags == 0
    }

    pub fn types(&self) -> &IdSet<T> {
        &self.types
    }

    pub fn negative_set(&self) -> &IdSet<T> {
        &self.negative_set
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }
}

/// Subject of a constraint operand (source or target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintSubject {
    Source,
    Target,
}

/// Operand in a constraint expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintOperand {
    User(ConstraintSubject),
    Role(ConstraintSubject),
    Type(ConstraintSubject),
    L1L2,
    L1H2,
    H1L2,
    H1H2,
    L1H1,
    L2H2,
}

impl ConstraintOperand {
    const USER: u32 = 0x1;
    const ROLE: u32 = 0x2;
    const TYPE: u32 = 0x4;
    const L1_L2: u32 = 0x20;
    const L1_H2: u32 = 0x40;
    const H1_L2: u32 = 0x80;
    const H1_H2: u32 = 0x100;
    const L1_H1: u32 = 0x200;
    const L2_H2: u32 = 0x400;

    const TARGET_FLAG: u32 = 0x8;

    fn from_u32(value: u32) -> Result<Self, ParseError> {
        let subject = if value & Self::TARGET_FLAG != 0 {
            ConstraintSubject::Target
        } else {
            ConstraintSubject::Source
        };

        match value & !Self::TARGET_FLAG {
            Self::USER => Ok(Self::User(subject)),
            Self::ROLE => Ok(Self::Role(subject)),
            Self::TYPE => Ok(Self::Type(subject)),
            _ => match value {
                Self::L1_L2 => Ok(Self::L1L2),
                Self::L1_H2 => Ok(Self::L1H2),
                Self::H1_L2 => Ok(Self::H1L2),
                Self::H1_H2 => Ok(Self::H1H2),
                Self::L1_H1 => Ok(Self::L1H1),
                Self::L2_H2 => Ok(Self::L2H2),
                _ => Err(ParseError::InvalidConstraintOperandType { value }),
            },
        }
    }

    pub fn as_u32(&self) -> u32 {
        match self {
            Self::User(subject) => Self::USER | subject.as_u32(),
            Self::Role(subject) => Self::ROLE | subject.as_u32(),
            Self::Type(subject) => Self::TYPE | subject.as_u32(),
            Self::L1L2 => Self::L1_L2,
            Self::L1H2 => Self::L1_H2,
            Self::H1L2 => Self::H1_L2,
            Self::H1H2 => Self::H1_H2,
            Self::L1H1 => Self::L1_H1,
            Self::L2H2 => Self::L2_H2,
        }
    }
}

impl ConstraintSubject {
    fn as_u32(&self) -> u32 {
        match self {
            Self::Source => 0,
            Self::Target => ConstraintOperand::TARGET_FLAG,
        }
    }
}

/// Operator in a constraint expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum ConstraintOperator {
    Eq = 1,
    Ne = 2,
    Dom = 3,
    DomBy = 4,
    Incomp = 5,
}

/// Leaf expression in a constraint expression tree, containing the identifiers targeted by the term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintNames {
    Users(IdSet<UserId>, ConstraintSet<UserId>),
    Roles(IdSet<RoleId>, ConstraintSet<RoleId>),
    Types(IdSet<TypeId>, ConstraintSet<TypeId>),
}

/// Term in a constraint expression tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintTerm {
    Not,
    And,
    Or,
    Expression {
        operand: ConstraintOperand,
        operator: ConstraintOperator,
    },
    ExpressionWithNames {
        operand: ConstraintOperand,
        operator: ConstraintOperator,
        names: Box<ConstraintNames>,
    },
}

impl ConstraintTerm {
    // Term types
    const NOT_OPERATOR: u32 = 1;
    const AND_OPERATOR: u32 = 2;
    const OR_OPERATOR: u32 = 3;
    const EXPR: u32 = 4;
    const EXPR_WITH_NAMES: u32 = 5;
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
            Self::EXPR => {
                let operand = ConstraintOperand::from_u32(u32::parse(cursor)?)?;
                let operator = ConstraintOperator::parse(cursor)?;
                Ok(Self::Expression { operand, operator })
            }
            Self::EXPR_WITH_NAMES => {
                let operand = ConstraintOperand::from_u32(u32::parse(cursor)?)?;
                let operator = ConstraintOperator::parse(cursor)?;
                let names = match operand {
                    ConstraintOperand::User(_) => {
                        let ids = IdSet::parse(cursor)?;
                        let set = ConstraintSet::parse(cursor)?;
                        if !set.is_empty() {
                            return Err(ParseError::UnexpectedConstraintTypeSet);
                        }
                        ConstraintNames::Users(ids, set)
                    }
                    ConstraintOperand::Role(_) => {
                        let ids = IdSet::parse(cursor)?;
                        let set = ConstraintSet::parse(cursor)?;
                        if !set.is_empty() {
                            return Err(ParseError::UnexpectedConstraintTypeSet);
                        }
                        ConstraintNames::Roles(ids, set)
                    }
                    ConstraintOperand::Type(_) => {
                        let ids = IdSet::parse(cursor)?;
                        let set = ConstraintSet::parse(cursor)?;
                        ConstraintNames::Types(ids, set)
                    }
                    _ => {
                        return Err(ParseError::InvalidConstraintOperandType {
                            value: operand.as_u32(),
                        });
                    }
                };
                Ok(Self::ExpressionWithNames { operand, operator, names: Box::new(names) })
            }
            _ => Err(ParseError::InvalidConstraintTermType { value: constraint_term_type }),
        }
    }
}

impl Serialize for ConstraintTerm {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
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
            Self::Expression { operand, operator } => {
                Self::EXPR.serialize(writer)?;
                operand.as_u32().serialize(writer)?;
                operator.serialize(writer)?;
            }
            Self::ExpressionWithNames { operand, operator, names } => {
                Self::EXPR_WITH_NAMES.serialize(writer)?;
                operand.as_u32().serialize(writer)?;
                operator.serialize(writer)?;
                match names.as_ref() {
                    ConstraintNames::Users(ids, set) => {
                        ids.serialize(writer)?;
                        set.serialize(writer)?;
                    }
                    ConstraintNames::Roles(ids, set) => {
                        ids.serialize(writer)?;
                        set.serialize(writer)?;
                    }
                    ConstraintNames::Types(ids, set) => {
                        ids.serialize(writer)?;
                        set.serialize(writer)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Validate for ConstraintTerm {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        match self {
            Self::ExpressionWithNames { names, .. } => match names.as_ref() {
                ConstraintNames::Users(ids, set) => {
                    ids.validate(policy)?;
                    set.validate(policy)?;
                }
                ConstraintNames::Roles(ids, set) => {
                    ids.validate(policy)?;
                    set.validate(policy)?;
                }
                ConstraintNames::Types(ids, set) => {
                    ids.validate(policy)?;
                    set.validate(policy)?;
                }
            },
            _ => {}
        }
        Ok(())
    }
}

/// Security policy constraint restricting permissions based on context attributes.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
pub struct Constraint {
    access_vector: AccessVector,
    constraint_expr: Array<ConstraintTerm>,
}

impl Constraint {
    pub fn access_vector(&self) -> AccessVector {
        self.access_vector
    }

    pub fn constraint_expr(&self) -> &[ConstraintTerm] {
        &self.constraint_expr
    }
}

#[cfg(test)]
mod tests {
    use super::{PolicyCursor, *};

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
        constraint.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }

    #[test]
    fn test_constraint_term_with_users() {
        let data = [
            // metadata (EXPR_WITH_NAMES, OPERAND_USER, OPERATOR_EQ)
            5, 0, 0, 0, // constraint_term_type = 5 (EXPR_WITH_NAMES)
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
            ConstraintTerm::ExpressionWithNames { operand, operator, names } => {
                assert_eq!(*operand, ConstraintOperand::User(ConstraintSubject::Source));
                assert_eq!(*operator, ConstraintOperator::Eq);
                match names.as_ref() {
                    ConstraintNames::Users(ids, set) => {
                        assert!(ids.contains(UserId::for_test(1)));
                        assert!(!ids.contains(UserId::for_test(2)));
                        assert!(set.is_empty());
                    }
                    _ => panic!("expected Users"),
                }
            }
            _ => panic!("expected ExpressionWithNames"),
        }

        let mut writer = Vec::new();
        term.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }
}
