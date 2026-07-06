// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::security_context::SecurityContext;
use super::{MlsLevel, PolicyId};

use crate::new_policy::bitmap::IdSet;
use crate::new_policy::{
    ConstraintNames, ConstraintOperand, ConstraintOperator, ConstraintSubject, ConstraintTerm,
};

use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum ConstraintError {
    #[error("invalid operand type for context expression: {type_:?}")]
    InvalidContextOperandType { type_: u32 },
    #[error("invalid operand type for context expression with names: {type_:?}")]
    InvalidContextWithNamesOperandType { type_: u32 },
    #[error("invalid operator type {operator:?} for operands ({left}, {right})")]
    InvalidOperatorForOperandTypes {
        operator: ConstraintOperator,
        left: &'static str,
        right: &'static str,
    },
    #[error("invalid constraint term sequence")]
    InvalidTermSequence,
}

/// Evaluates constraint expression for the given source and target [`SecurityContext`]s.
///
/// Assumes that the terms of the constraint expression were sequenced in postfix
/// order by the policy compiler.
///
/// This implementation deliberately avoids shortcuts, since it is used to
/// validate that constraint expressions are well-formed as well as for
/// access decisions.
pub(super) fn evaluate_constraint(
    constraint_expr: &[ConstraintTerm],
    source: &SecurityContext,
    target: &SecurityContext,
) -> Result<bool, ConstraintError> {
    let mut stack: Vec<bool> = Vec::with_capacity(constraint_expr.len());
    for term in constraint_expr {
        match term {
            ConstraintTerm::Not => {
                let arg = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                *arg = !*arg;
            }
            ConstraintTerm::And => {
                let right = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
                let left = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                *left = *left && right;
            }
            ConstraintTerm::Or => {
                let right = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
                let left = stack.last_mut().ok_or(ConstraintError::InvalidTermSequence)?;
                *left = *left || right;
            }
            ConstraintTerm::Expression { operand, operator } => {
                stack.push(evaluate_expression(*operand, *operator, source, target)?);
            }
            ConstraintTerm::ExpressionWithNames { operand, operator, names } => {
                stack.push(evaluate_expression_with_names(
                    *operand, *operator, names, source, target,
                )?);
            }
        }
    }
    let result = stack.pop().ok_or(ConstraintError::InvalidTermSequence)?;
    if !stack.is_empty() {
        return Err(ConstraintError::InvalidTermSequence);
    }
    Ok(result)
}

fn evaluate_simple<T>(
    left: T,
    right: T,
    operator: ConstraintOperator,
    type_name: &'static str,
) -> Result<bool, ConstraintError>
where
    T: Eq,
{
    if operator == ConstraintOperator::Eq {
        Ok(left == right)
    } else if operator == ConstraintOperator::Ne {
        Ok(left != right)
    } else {
        Err(ConstraintError::InvalidOperatorForOperandTypes {
            operator,
            left: type_name,
            right: type_name,
        })
    }
}

fn evaluate_with_names<T>(
    val: T,
    ids: &IdSet<T>,
    operator: ConstraintOperator,
    val_type_name: &'static str,
    ids_type_name: &'static str,
) -> Result<bool, ConstraintError>
where
    T: PolicyId,
{
    if operator == ConstraintOperator::Eq {
        Ok(ids.contains(val))
    } else if operator == ConstraintOperator::Ne {
        Ok(!ids.contains(val))
    } else {
        Err(ConstraintError::InvalidOperatorForOperandTypes {
            operator,
            left: val_type_name,
            right: ids_type_name,
        })
    }
}

fn evaluate_expression(
    operand: ConstraintOperand,
    operator: ConstraintOperator,
    source: &SecurityContext,
    target: &SecurityContext,
) -> Result<bool, ConstraintError> {
    match operand {
        ConstraintOperand::User(ConstraintSubject::Source) => {
            evaluate_simple(source.user(), target.user(), operator, "UserId")
        }
        ConstraintOperand::Role(ConstraintSubject::Source) => {
            evaluate_simple(source.role(), target.role(), operator, "RoleId")
        }
        ConstraintOperand::Type(ConstraintSubject::Source) => {
            evaluate_simple(source.type_(), target.type_(), operator, "TypeId")
        }
        ConstraintOperand::L1L2 => {
            Ok(evaluate_levels(source.low_level(), target.low_level(), operator))
        }
        ConstraintOperand::L1H2 => {
            Ok(evaluate_levels(source.low_level(), target.effective_high_level(), operator))
        }
        ConstraintOperand::H1L2 => {
            Ok(evaluate_levels(source.effective_high_level(), target.low_level(), operator))
        }
        ConstraintOperand::H1H2 => Ok(evaluate_levels(
            source.effective_high_level(),
            target.effective_high_level(),
            operator,
        )),
        ConstraintOperand::L1H1 => {
            Ok(evaluate_levels(source.low_level(), source.effective_high_level(), operator))
        }
        ConstraintOperand::L2H2 => {
            Ok(evaluate_levels(target.low_level(), target.effective_high_level(), operator))
        }
        _ => Err(ConstraintError::InvalidContextOperandType { type_: operand.as_u32() }),
    }
}

fn evaluate_levels(left: &MlsLevel, right: &MlsLevel, operator: ConstraintOperator) -> bool {
    match operator {
        ConstraintOperator::Eq => left == right,
        ConstraintOperator::Ne => left != right,
        ConstraintOperator::Dom => left.dominates(right),
        ConstraintOperator::DomBy => right.dominates(left),
        ConstraintOperator::Incomp => !left.dominates(right) && !right.dominates(left),
    }
}

fn evaluate_expression_with_names(
    operand: ConstraintOperand,
    operator: ConstraintOperator,
    names: &ConstraintNames,
    source: &SecurityContext,
    target: &SecurityContext,
) -> Result<bool, ConstraintError> {
    match (operand, names) {
        (ConstraintOperand::User(subject), ConstraintNames::Users(ids, _)) => {
            let val = match subject {
                ConstraintSubject::Source => source.user(),
                ConstraintSubject::Target => target.user(),
            };
            evaluate_with_names(val, ids, operator, "UserId", "IdSet<UserId>")
        }
        (ConstraintOperand::Role(subject), ConstraintNames::Roles(ids, _)) => {
            let val = match subject {
                ConstraintSubject::Source => source.role(),
                ConstraintSubject::Target => target.role(),
            };
            evaluate_with_names(val, ids, operator, "RoleId", "IdSet<RoleId>")
        }
        (ConstraintOperand::Type(subject), ConstraintNames::Types(ids, _)) => {
            let val = match subject {
                ConstraintSubject::Source => source.type_(),
                ConstraintSubject::Target => target.type_(),
            };
            evaluate_with_names(val, ids, operator, "TypeId", "IdSet<TypeId>")
        }
        _ => Err(ConstraintError::InvalidContextWithNamesOperandType { type_: operand.as_u32() }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse_policy_by_value;
    use super::*;

    #[test]
    fn evaluate_constraint_expr() {
        let policy_bytes = include_bytes!("../../testdata/micro_policies/constraints_policy");
        let policy = parse_policy_by_value(policy_bytes.to_vec())
            .expect("parse policy")
            .validate()
            .expect("validate policy");

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
        let constraint_eq = class_constraint_eq_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_eq, &source, &target).expect("evaluate constraint"),
            false
        );

        let class_constraint_with_and =
            classes.get_by_name(b"class_constraint_with_and").expect("look up class");
        let class_constraint_with_and_constraints = class_constraint_with_and.constraints();
        assert_eq!(class_constraint_with_and_constraints.len(), 1);
        // ( ( u1 == u2 ) and ( t1 == t2 ) )
        let constraint_with_and = class_constraint_with_and_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_and, &source, &target)
                .expect("evaluate constraint"),
            false
        );

        let class_constraint_with_not =
            classes.get_by_name(b"class_constraint_with_not").expect("look up class");
        let class_constraint_with_not_constraints = class_constraint_with_not.constraints();
        assert_eq!(class_constraint_with_not_constraints.len(), 1);
        // ( not ( ( u1 == u2 ) and ( t1 == t2 ) )
        let constraint_with_not = class_constraint_with_not_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_not, &source, &target)
                .expect("evaluate constraint"),
            true
        );

        let class_constraint_with_names =
            classes.get_by_name(b"class_constraint_with_names").expect("look up class");
        let class_constraint_with_names_constraints = class_constraint_with_names.constraints();
        assert_eq!(class_constraint_with_names_constraints.len(), 1);
        // ( u1 != { user0 user1 })
        let constraint_with_names = class_constraint_with_names_constraints[0].constraint_expr();
        assert_eq!(
            evaluate_constraint(constraint_with_names, &source, &target)
                .expect("evaluate constraint"),
            false
        );

        let class_constraint_nested =
            classes.get_by_name(b"class_constraint_nested").expect("look up class");
        let class_constraint_nested_constraints = class_constraint_nested.constraints();
        assert_eq!(class_constraint_nested_constraints.len(), 1);
        // ( ( ( u2 == { user0 user1} ) and ( r1 == r2 ) ) or ( ( u1 == u2 ) and ( not (t1 == t2 ) ) ) )
        let constraint_nested = class_constraint_nested_constraints[0].constraint_expr();
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
