// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::extensible_bitmap::ExtensibleBitmap;
use super::security_context::{Level, SecurityContext, SecurityLevel};
use super::symbols::{
    CONSTRAINT_EXPR_OPERAND_TYPE_H1_H2, CONSTRAINT_EXPR_OPERAND_TYPE_H1_L2,
    CONSTRAINT_EXPR_OPERAND_TYPE_L1_H1, CONSTRAINT_EXPR_OPERAND_TYPE_L1_H2,
    CONSTRAINT_EXPR_OPERAND_TYPE_L1_L2, CONSTRAINT_EXPR_OPERAND_TYPE_L2_H2,
    CONSTRAINT_EXPR_OPERAND_TYPE_ROLE, CONSTRAINT_EXPR_OPERAND_TYPE_TYPE,
    CONSTRAINT_EXPR_OPERAND_TYPE_USER, CONSTRAINT_EXPR_OPERATOR_TYPE_DOM,
    CONSTRAINT_EXPR_OPERATOR_TYPE_DOMBY, CONSTRAINT_EXPR_OPERATOR_TYPE_EQ,
    CONSTRAINT_EXPR_OPERATOR_TYPE_INCOMP, CONSTRAINT_EXPR_OPERATOR_TYPE_NE,
    CONSTRAINT_EXPR_WITH_NAMES_OPERAND_TYPE_TARGET_MASK, CONSTRAINT_TERM_TYPE_AND_OPERATOR,
    CONSTRAINT_TERM_TYPE_EXPR, CONSTRAINT_TERM_TYPE_EXPR_WITH_NAMES,
    CONSTRAINT_TERM_TYPE_NOT_OPERATOR, CONSTRAINT_TERM_TYPE_OR_OPERATOR, ConstraintExpr,
    ConstraintTerm,
};
use super::{RoleId, TypeId, UserId};
use crate::new_policy::traits::PolicyId;

use std::cmp::Ordering;
use std::collections::HashSet;
use std::num::NonZeroU32;
use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum ConstraintError {
    #[error("missing names for constraint term")]
    MissingNames,
    #[error("invalid constraint term type {type_:?}")]
    InvalidTermType { type_: u32 },
    #[error("invalid operator type for context expression: {type_:?}")]
    InvalidContextOperatorType { type_: u32 },
    #[error("invalid operand type for context expression: {type_:?}")]
    InvalidContextOperandType { type_: u32 },
    #[error("invalid operand type for context expression with names: {type_:?}")]
    InvalidContextWithNamesOperandType { type_: u32 },
    #[error("invalid operator type {operator:?} for operands ({left:?}, {right:?})")]
    InvalidContextOperatorForOperands {
        operator: ContextOperator,
        left: ContextOperand,
        right: ContextOperand,
    },
    #[error("invalid pair of context operands: ({left:?}, {right:?})")]
    InvalidContextOperands { left: ContextOperand, right: ContextOperand },
    #[error("invalid constraint term sequence")]
    InvalidTermSequence,
}

/// Given a [`ConstraintExpr`] and source and target [`SecurityContext`]s,
/// decode the constraint expression and evaluate it for the security contexts.
///
/// Assumes that the terms of the [`ConstraintExpr`] were sequenced in postfix
/// order by the policy compiler.
///
/// This implementation deliberately avoids shortcuts, since it is used to
/// validate that constraint expressions are well-formed as well as for
/// access decisions.
pub(super) fn evaluate_constraint(
    constraint_expr: &ConstraintExpr,
    source: &SecurityContext,
    target: &SecurityContext,
) -> Result<bool, ConstraintError> {
    let terms = constraint_expr.constraint_terms();
    // The stack depth is at most the number of terms.
    let mut stack = Vec::with_capacity(terms.len());
    for term in terms {
        let node = ConstraintNode::try_from_constraint_term(term, source, target)?;
        match node {
            ConstraintNode::Leaf(expr) => stack.push(expr.evaluate()?),
            ConstraintNode::Branch(op) => match op {
                BooleanOperator::Not => {
                    let arg = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                    *arg = !*arg;
                }
                BooleanOperator::And => {
                    let right = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
                    let left = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                    *left = *left && right;
                }
                BooleanOperator::Or => {
                    let right = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
                    let left = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                    *left = *left || right;
                }
            },
        }
    }
    let result = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
    if !stack.is_empty() {
        return Err(ConstraintError::InvalidTermSequence);
    }
    Ok(result)
}

/// A node in the parse tree of a [`ConstraintExpr`].
#[derive(Debug)]
enum ConstraintNode<'a> {
    Branch(BooleanOperator),
    Leaf(ContextExpression<'a>),
}

impl<'a> ConstraintNode<'a> {
    fn try_from_constraint_term(
        value: &'a ConstraintTerm,
        source: &'a SecurityContext,
        target: &'a SecurityContext,
    ) -> Result<ConstraintNode<'a>, ConstraintError> {
        if let Ok(op) = BooleanOperator::try_from_constraint_term(value) {
            Ok(ConstraintNode::Branch(op))
        } else {
            Ok(ConstraintNode::Leaf(ContextExpression::try_from_constraint_term(
                value, source, target,
            )?))
        }
    }
}

/// A branch node in the parse tree of a [`ConstraintExpr`],
/// representing an operator on the boolean values of the subtree(s)
/// below that node.
#[derive(Debug, Eq, PartialEq)]
enum BooleanOperator {
    Not,
    And,
    Or,
}

impl BooleanOperator {
    fn try_from_constraint_term(
        value: &ConstraintTerm,
    ) -> Result<BooleanOperator, ConstraintError> {
        match value.constraint_term_type() {
            CONSTRAINT_TERM_TYPE_NOT_OPERATOR => Ok(BooleanOperator::Not),
            CONSTRAINT_TERM_TYPE_AND_OPERATOR => Ok(BooleanOperator::And),
            CONSTRAINT_TERM_TYPE_OR_OPERATOR => Ok(BooleanOperator::Or),
            _ => Err(ConstraintError::InvalidTermType { type_: value.constraint_term_type() }),
        }
    }
}

/// An operator on [`SecurityContext`] fields in a
/// [`ContextExpression`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContextOperator {
    Equal,        // `eq` or `==` in policy language
    NotEqual,     // `ne` or `!=` in policy language
    Dominates,    // `dom` in policy language
    DominatedBy,  // `domby` in policy language
    Incomparable, // `incomp` in policy language
}

/// An operand in a [`ContextExpression`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContextOperand {
    UserId(UserId),
    RoleId(RoleId),
    TypeId(TypeId),
    Level(SecurityLevel),
    UserIds(HashSet<UserId>),
    RoleIds(HashSet<RoleId>),
    TypeIds(HashSet<TypeId>),
}

#[derive(Clone, Debug)]
pub(super) struct TypeIds<'a> {
    underlying: &'a ExtensibleBitmap,
}

impl<'a> TypeIds<'a> {
    fn contains(&self, id: &TypeId) -> bool {
        self.underlying.is_set(id.as_u32() - 1)
    }
}

/// Like [`ContextOperand`] but lifetime-bound to a [`TypeIds`].
#[derive(Clone, Debug)]
enum Operand<'a> {
    UserId(UserId),
    RoleId(RoleId),
    TypeId(TypeId),
    Level(&'a SecurityLevel),
    UserIds(HashSet<UserId>),
    RoleIds(HashSet<RoleId>),
    TypeIds(TypeIds<'a>),
}

impl From<&Operand<'_>> for ContextOperand {
    fn from(value: &Operand<'_>) -> Self {
        match value {
            Operand::UserId(user_id) => Self::UserId(user_id.clone()),
            Operand::RoleId(role_id) => Self::RoleId(role_id.clone()),
            Operand::TypeId(type_id) => Self::TypeId(type_id.clone()),
            Operand::Level(security_level) => Self::Level((*security_level).clone()),
            Operand::UserIds(user_ids) => Self::UserIds(user_ids.clone()),
            Operand::RoleIds(role_ids) => Self::RoleIds(role_ids.clone()),
            Operand::TypeIds(type_ids) => Self::TypeIds(
                type_ids
                    .underlying
                    .indices_of_set_bits()
                    .map(|i| TypeId::from_u32(i + 1).unwrap())
                    .collect(),
            ),
        }
    }
}

/// A leaf node in the parse tree of a [`ConstraintExpr`]. Represents
/// a boolean expression in terms of source and target
/// [`SecurityContext`]s.
#[derive(Debug)]
struct ContextExpression<'a> {
    left: Operand<'a>,
    right: Operand<'a>,
    operator: ContextOperator,
}

impl<'a> ContextExpression<'a> {
    fn evaluate(&self) -> Result<bool, ConstraintError> {
        match (&self.left, &self.right) {
            (Operand::UserId(left_id), Operand::UserId(right_id)) => match self.operator {
                ContextOperator::Equal => Ok(left_id == right_id),
                ContextOperator::NotEqual => Ok(left_id != right_id),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::RoleId(left_id), Operand::RoleId(right_id)) => match self.operator {
                ContextOperator::Equal => Ok(left_id == right_id),
                ContextOperator::NotEqual => Ok(left_id != right_id),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::TypeId(left_id), Operand::TypeId(right_id)) => match self.operator {
                ContextOperator::Equal => Ok(left_id == right_id),
                ContextOperator::NotEqual => Ok(left_id != right_id),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::UserId(id), Operand::UserIds(ids)) => match self.operator {
                ContextOperator::Equal => Ok(ids.contains(id)),
                ContextOperator::NotEqual => Ok(!ids.contains(id)),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::RoleId(id), Operand::RoleIds(ids)) => match self.operator {
                ContextOperator::Equal => Ok(ids.contains(id)),
                ContextOperator::NotEqual => Ok(!ids.contains(id)),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::TypeId(id), Operand::TypeIds(ids)) => match self.operator {
                ContextOperator::Equal => Ok(ids.contains(id)),
                ContextOperator::NotEqual => Ok(!ids.contains(id)),
                _ => Err(ConstraintError::InvalidContextOperatorForOperands {
                    operator: self.operator.clone(),
                    left: ContextOperand::from(&self.left),
                    right: ContextOperand::from(&self.right),
                }),
            },
            (Operand::Level(left), Operand::Level(right)) => match self.operator {
                ContextOperator::Equal => Ok((*left).compare(*right) == Some(Ordering::Equal)),
                ContextOperator::NotEqual => Ok((*left).compare(*right) != Some(Ordering::Equal)),
                ContextOperator::Dominates => Ok((*left).dominates(*right)),
                ContextOperator::DominatedBy => Ok((*right).dominates(*left)),
                ContextOperator::Incomparable => Ok((*left).compare(*right).is_none()),
            },
            _ => Err(ConstraintError::InvalidContextOperands {
                left: ContextOperand::from(&self.left),
                right: ContextOperand::from(&self.right),
            }),
        }
    }

    fn try_from_constraint_term(
        value: &'a ConstraintTerm,
        source: &'a SecurityContext,
        target: &'a SecurityContext,
    ) -> Result<ContextExpression<'a>, ConstraintError> {
        let (left, right) = match value.constraint_term_type() {
            CONSTRAINT_TERM_TYPE_EXPR => {
                ContextExpression::operands_from_expr(value.expr_operand_type(), source, target)
            }
            CONSTRAINT_TERM_TYPE_EXPR_WITH_NAMES => {
                if let Some(names) = value.names() {
                    ContextExpression::operands_from_expr_with_names(
                        value.expr_operand_type(),
                        names,
                        source,
                        target,
                    )
                } else {
                    Err(ConstraintError::MissingNames)
                }
            }
            _ => Err(ConstraintError::InvalidTermType { type_: value.constraint_term_type() }),
        }?;
        let operator = match value.expr_operator_type() {
            CONSTRAINT_EXPR_OPERATOR_TYPE_EQ => Ok(ContextOperator::Equal),
            CONSTRAINT_EXPR_OPERATOR_TYPE_NE => Ok(ContextOperator::NotEqual),
            CONSTRAINT_EXPR_OPERATOR_TYPE_DOM => Ok(ContextOperator::Dominates),
            CONSTRAINT_EXPR_OPERATOR_TYPE_DOMBY => Ok(ContextOperator::DominatedBy),
            CONSTRAINT_EXPR_OPERATOR_TYPE_INCOMP => Ok(ContextOperator::Incomparable),
            _ => Err(ConstraintError::InvalidContextOperatorType {
                type_: value.expr_operator_type(),
            }),
        }?;
        Ok(ContextExpression { left, right, operator })
    }

    fn operands_from_expr(
        operand_type: u32,
        source: &'a SecurityContext,
        target: &'a SecurityContext,
    ) -> Result<(Operand<'a>, Operand<'a>), ConstraintError> {
        match operand_type {
            CONSTRAINT_EXPR_OPERAND_TYPE_USER => {
                Ok((Operand::UserId(source.user()), Operand::UserId(target.user())))
            }
            CONSTRAINT_EXPR_OPERAND_TYPE_ROLE => {
                Ok((Operand::RoleId(source.role()), Operand::RoleId(target.role())))
            }
            CONSTRAINT_EXPR_OPERAND_TYPE_TYPE => {
                Ok((Operand::TypeId(source.type_()), Operand::TypeId(target.type_())))
            }
            CONSTRAINT_EXPR_OPERAND_TYPE_L1_L2 => {
                Ok((Operand::Level(source.low_level()), Operand::Level(target.low_level())))
            }
            CONSTRAINT_EXPR_OPERAND_TYPE_L1_H2 => Ok((
                Operand::Level(source.low_level()),
                Operand::Level(target.effective_high_level()),
            )),
            CONSTRAINT_EXPR_OPERAND_TYPE_H1_L2 => Ok((
                Operand::Level(source.effective_high_level()),
                Operand::Level(target.low_level()),
            )),
            CONSTRAINT_EXPR_OPERAND_TYPE_H1_H2 => Ok((
                Operand::Level(source.effective_high_level()),
                Operand::Level(target.effective_high_level()),
            )),
            CONSTRAINT_EXPR_OPERAND_TYPE_L1_H1 => Ok((
                Operand::Level(source.low_level()),
                Operand::Level(source.effective_high_level()),
            )),
            CONSTRAINT_EXPR_OPERAND_TYPE_L2_H2 => Ok((
                Operand::Level(target.low_level()),
                Operand::Level(target.effective_high_level()),
            )),
            _ => Err(ConstraintError::InvalidContextOperandType { type_: operand_type }),
        }
    }

    fn operands_from_expr_with_names(
        operand_type: u32,
        names: &'a ExtensibleBitmap,
        source: &'a SecurityContext,
        target: &'a SecurityContext,
    ) -> Result<(Operand<'a>, Operand<'a>), ConstraintError> {
        let ids = names.indices_of_set_bits().map(|i| NonZeroU32::new(i + 1).unwrap());

        if operand_type & CONSTRAINT_EXPR_WITH_NAMES_OPERAND_TYPE_TARGET_MASK == 0 {
            match operand_type {
                CONSTRAINT_EXPR_OPERAND_TYPE_USER => Ok((
                    Operand::UserId(source.user()),
                    Operand::UserIds(ids.map(|id| UserId(id)).collect()),
                )),
                CONSTRAINT_EXPR_OPERAND_TYPE_ROLE => Ok((
                    Operand::RoleId(source.role()),
                    Operand::RoleIds(ids.map(|id| RoleId(id)).collect()),
                )),
                CONSTRAINT_EXPR_OPERAND_TYPE_TYPE => Ok((
                    Operand::TypeId(source.type_()),
                    Operand::TypeIds(TypeIds { underlying: names }),
                )),
                _ => {
                    Err(ConstraintError::InvalidContextWithNamesOperandType { type_: operand_type })
                }
            }
        } else {
            match operand_type ^ CONSTRAINT_EXPR_WITH_NAMES_OPERAND_TYPE_TARGET_MASK {
                CONSTRAINT_EXPR_OPERAND_TYPE_USER => Ok((
                    Operand::UserId(target.user()),
                    Operand::UserIds(ids.map(|id| UserId(id)).collect()),
                )),
                CONSTRAINT_EXPR_OPERAND_TYPE_ROLE => Ok((
                    Operand::RoleId(target.role()),
                    Operand::RoleIds(ids.map(|id| RoleId(id)).collect()),
                )),
                CONSTRAINT_EXPR_OPERAND_TYPE_TYPE => Ok((
                    Operand::TypeId(target.type_()),
                    Operand::TypeIds(TypeIds { underlying: names }),
                )),
                _ => {
                    Err(ConstraintError::InvalidContextWithNamesOperandType { type_: operand_type })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{find_class_by_name, parse_policy_by_value};
    use super::*;

    impl PartialEq for Operand<'_> {
        fn eq(&self, other: &Self) -> bool {
            ContextOperand::from(self) == ContextOperand::from(other)
        }
    }

    impl Eq for Operand<'_> {}

    impl PartialEq for ContextExpression<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.operator == other.operator && self.left == other.left && self.right == other.right
        }
    }

    impl Eq for ContextExpression<'_> {}

    impl PartialEq for ConstraintNode<'_> {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Branch(self_operator), Self::Branch(other_operator)) => {
                    self_operator == other_operator
                }
                (Self::Leaf(self_expression), Self::Leaf(other_expression)) => {
                    self_expression == other_expression
                }
                _ => false,
            }
        }
    }

    impl Eq for ConstraintNode<'_> {}

    fn normalize_context_expr<'a>(expr: ContextExpression<'a>) -> ContextExpression<'a> {
        let (left, right) = match expr.operator {
            ContextOperator::Dominates | ContextOperator::DominatedBy => (expr.left, expr.right),
            ContextOperator::Equal | ContextOperator::NotEqual | ContextOperator::Incomparable => {
                match (&expr.left, &expr.right) {
                    (Operand::UserId(left), Operand::UserId(right)) => (
                        Operand::UserId(std::cmp::min(*left, *right)),
                        Operand::UserId(std::cmp::max(*left, *right)),
                    ),
                    (Operand::TypeId(left), Operand::TypeId(right)) => (
                        Operand::TypeId(std::cmp::min(*left, *right)),
                        Operand::TypeId(std::cmp::max(*left, *right)),
                    ),
                    (Operand::RoleId(left), Operand::RoleId(right)) => (
                        Operand::RoleId(std::cmp::min(*left, *right)),
                        Operand::RoleId(std::cmp::max(*left, *right)),
                    ),
                    _ => (expr.left, expr.right),
                }
            }
        };
        ContextExpression { operator: expr.operator, left, right }
    }

    fn normalize<'a>(expr: Vec<ConstraintNode<'a>>) -> Vec<ConstraintNode<'a>> {
        expr.into_iter()
            .map(|node| match node {
                ConstraintNode::Leaf(context_expr) => {
                    ConstraintNode::Leaf(normalize_context_expr(context_expr))
                }
                ConstraintNode::Branch(_) => node,
            })
            .collect()
    }

    #[test]
    fn decode_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec())
            .expect("parse policy")
            .validate()
            .expect("validate policy");
        let parsed_policy = policy.0.parsed_policy();

        let source = policy
            .parse_security_context(b"user0:object_r:type0:s0-s0".into())
            .expect("valid source security context");
        let target = policy
            .parse_security_context(b"user1:object_r:security_t:s0:c0-s0:c0".into())
            .expect("valid target security context");

        let classes = parsed_policy.classes();
        let class = find_class_by_name(&classes, "class_constraint_nested").expect("look up class");
        let constraints = class.constraints();
        assert_eq!(constraints.len(), 1);
        let constraint = &constraints[0].constraint_expr();
        let result: Result<Vec<ConstraintNode<'_>>, ConstraintError> = constraint
            .constraint_terms()
            .iter()
            .map(|x| ConstraintNode::try_from_constraint_term(x, &source, &target))
            .collect();
        let constraint_nodes = normalize(result.expect("decode constraint terms"));
        let expected = vec![
            // ( u2 == { user0 user1 } )
            ConstraintNode::Leaf(ContextExpression {
                left: Operand::UserId(UserId(NonZeroU32::new(2).unwrap())),
                right: Operand::UserIds(HashSet::from([
                    UserId(NonZeroU32::new(1).unwrap()),
                    UserId(NonZeroU32::new(2).unwrap()),
                ])),
                operator: ContextOperator::Equal,
            }),
            // ( r1 == r2 )
            ConstraintNode::Leaf(ContextExpression {
                left: Operand::RoleId(RoleId(NonZeroU32::new(1).unwrap())),
                right: Operand::RoleId(RoleId(NonZeroU32::new(1).unwrap())),
                operator: ContextOperator::Equal,
            }),
            // ( (u2 == { user0 user1 }) and (r1 == r2) )
            ConstraintNode::Branch(BooleanOperator::And),
            // (u1 == u2)
            ConstraintNode::Leaf(ContextExpression {
                left: Operand::UserId(UserId(NonZeroU32::new(1).unwrap())),
                right: Operand::UserId(UserId(NonZeroU32::new(2).unwrap())),
                operator: ContextOperator::Equal,
            }),
            // (t1 == t2)
            ConstraintNode::Leaf(ContextExpression {
                left: Operand::TypeId(TypeId::from_u32(1).unwrap()),
                right: Operand::TypeId(TypeId::from_u32(2).unwrap()),
                operator: ContextOperator::Equal,
            }),
            // not (t1 == t2)
            ConstraintNode::Branch(BooleanOperator::Not),
            // (( u1 == u2 ) and ( not (t1 == t2)))
            ConstraintNode::Branch(BooleanOperator::And),
            // ( (u2 == { user0 user1 }) and (r1 == r2) ) or (( u1 == u2 ) and ( not (t1 == t2)))
            ConstraintNode::Branch(BooleanOperator::Or),
        ];

        assert_eq!(constraint_nodes, expected)
    }

    #[test]
    fn evaluate_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec())
            .expect("parse policy")
            .validate()
            .expect("validate policy");
        let parsed_policy = policy.0.parsed_policy();

        let source = policy
            .parse_security_context(b"user0:object_r:type0:s0-s0".into())
            .expect("valid source security context");
        let target = policy
            .parse_security_context(b"user1:object_r:security_t:s0:c0-s0:c0".into())
            .expect("valid target security context");

        let classes = parsed_policy.classes();
        let class_constraint_eq =
            find_class_by_name(&classes, "class_constraint_eq").expect("look up class");
        let class_constraint_eq_constraints = class_constraint_eq.constraints();
        assert_eq!(class_constraint_eq_constraints.len(), 1);
        // ( u1 == u2 )
        let constraint_eq = &class_constraint_eq_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_eq, &source, &target).expect("evaluate constraint"),
            false
        );

        let class_constraint_with_and =
            find_class_by_name(&classes, "class_constraint_with_and").expect("look up class");
        let class_constraint_with_and_constraints = class_constraint_with_and.constraints();
        assert_eq!(class_constraint_with_and_constraints.len(), 1);
        // ( ( u1 == u2 ) and ( t1 == t2 ) )
        let constraint_with_and = &class_constraint_with_and_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_and, &source, &target)
                .expect("evaluate constraint"),
            false
        );

        let class_constraint_with_not =
            find_class_by_name(&classes, "class_constraint_with_not").expect("look up class");
        let class_constraint_with_not_constraints = class_constraint_with_not.constraints();
        assert_eq!(class_constraint_with_not_constraints.len(), 1);
        // ( not ( ( u1 == u2 ) and ( t1 == t2 ) )
        let constraint_with_not = &class_constraint_with_not_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_not, &source, &target)
                .expect("evaluate constraint"),
            true
        );

        let class_constraint_with_names =
            find_class_by_name(&classes, "class_constraint_with_names").expect("look up class");
        let class_constraint_with_names_constraints = class_constraint_with_names.constraints();
        assert_eq!(class_constraint_with_names_constraints.len(), 1);
        // ( u1 != { user0 user1 })
        let constraint_with_names = &class_constraint_with_names_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_names, &source, &target)
                .expect("evaluate constraint"),
            false
        );

        let class_constraint_nested =
            find_class_by_name(&classes, "class_constraint_nested").expect("look up class");
        let class_constraint_nested_constraints = class_constraint_nested.constraints();
        assert_eq!(class_constraint_nested_constraints.len(), 1);
        // ( ( ( u2 == { user0 user1} ) and ( r1 == r2 ) ) or ( ( u1 == u2 ) and ( not (t1 == t2 ) ) ) )
        let constraint_nested = &class_constraint_nested_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_nested, &source, &target).expect("evaluate constraint"),
            true
        )
    }

    #[test]
    fn evaluate_mls_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec())
            .expect("parse policy")
            .validate()
            .expect("validate policy");
        let parsed_policy = policy.0.parsed_policy();

        let source = policy
            .parse_security_context(b"user0:object_r:type0:s0-s0".into())
            .expect("valid source security context");
        let target = policy
            .parse_security_context(b"user1:object_r:security_t:s0:c0-s0:c0".into())
            .expect("valid target security context");

        let classes = parsed_policy.classes();
        let class = find_class_by_name(&classes, "class_mls_constraints").expect("look up class");
        let constraints = class.constraints();
        // Constraints appear in reverse order in parsed policy.
        let expected = vec![
            false, // l1 incomp h1
            false, // h1 incomp h2
            true,  // l1 domby h2
            false, // h1 dom l2
            false, // l2 != h2
            false, // l1 == l2
        ];
        for (i, constraint) in constraints.iter().enumerate() {
            assert_eq!(
                evaluate_constraint(constraint.constraint_expr(), &source, &target)
                    .expect("evaluate constraint",),
                expected[i],
                "constraint {}",
                i
            );
        }
    }
}
