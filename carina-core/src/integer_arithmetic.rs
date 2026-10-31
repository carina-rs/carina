//! Typed integer arithmetic shared by parsing, validation, and deferred
//! resolution.

use std::fmt;
use std::num::NonZeroUsize;

use serde::{Deserialize, Deserializer, Serialize};

use crate::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};

/// The closed set of binary integer operators in the Carina DSL.
///
/// Variant names are part of the saved-plan wire format. Keep them stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IntegerOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl IntegerOp {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
        }
    }

    /// Precedence used by structural value rendering. Larger binds tighter.
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Add | Self::Subtract => 1,
            Self::Multiply | Self::Divide | Self::Remainder => 2,
        }
    }
}

impl fmt::Display for IntegerOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol())
    }
}

/// A precise checked-evaluation failure. This type intentionally carries no
/// operand values so diagnostics cannot expose secret integer inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerArithmeticError {
    DivideByZero,
    RemainderByZero,
    Overflow,
}

impl fmt::Display for IntegerArithmeticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DivideByZero => f.write_str("integer division by zero"),
            Self::RemainderByZero => f.write_str("integer remainder by zero"),
            Self::Overflow => f.write_str("integer arithmetic overflow"),
        }
    }
}

impl std::error::Error for IntegerArithmeticError {}

/// The one checked evaluator used at every lifecycle stage.
pub fn evaluate_checked_integer(
    lhs: i64,
    op: IntegerOp,
    rhs: i64,
) -> Result<i64, IntegerArithmeticError> {
    if rhs == 0
        && let Some(error) = zero_divisor_error(op)
    {
        return Err(error);
    }
    let value = match op {
        IntegerOp::Add => lhs.checked_add(rhs),
        IntegerOp::Subtract => lhs.checked_sub(rhs),
        IntegerOp::Multiply => lhs.checked_mul(rhs),
        IntegerOp::Divide => lhs.checked_div(rhs),
        IntegerOp::Remainder => lhs.checked_rem(rhs),
    };
    value.ok_or(IntegerArithmeticError::Overflow)
}

fn zero_divisor_error(op: IntegerOp) -> Option<IntegerArithmeticError> {
    match op {
        IntegerOp::Divide => Some(IntegerArithmeticError::DivideByZero),
        IntegerOp::Remainder => Some(IntegerArithmeticError::RemainderByZero),
        IntegerOp::Add | IntegerOp::Subtract | IntegerOp::Multiply => None,
    }
}

/// Authored location of an arithmetic operator. It participates in
/// diagnostics but deliberately not semantic equality or persistence.
#[derive(Debug, Clone, Copy)]
pub struct IntegerOperatorSpan {
    line: Option<NonZeroUsize>,
    column: Option<NonZeroUsize>,
}

impl IntegerOperatorSpan {
    pub(crate) const fn from_original_position(position: crate::heredoc::OriginalPosition) -> Self {
        Self {
            line: NonZeroUsize::new(position.line()),
            column: NonZeroUsize::new(position.column()),
        }
    }

    pub const fn synthetic() -> Self {
        Self {
            line: None,
            column: None,
        }
    }

    pub fn line(self) -> Option<usize> {
        self.line.map(NonZeroUsize::get)
    }

    pub fn column(self) -> Option<usize> {
        self.column.map(NonZeroUsize::get)
    }
}

impl PartialEq for IntegerOperatorSpan {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for IntegerOperatorSpan {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegerOperandError {
    NonInteger {
        side: IntegerOperandSide,
        actual: &'static str,
    },
    UntypedUnknown {
        side: IntegerOperandSide,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerOperandSide {
    Left,
    Right,
}

impl fmt::Display for IntegerOperandSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left => f.write_str("left"),
            Self::Right => f.write_str("right"),
        }
    }
}

impl fmt::Display for IntegerOperandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonInteger { side, actual } => {
                write!(f, "{side} operand must be Int, got {actual}")
            }
            Self::UntypedUnknown { side } => {
                write!(f, "{side} operand is unknown without a proven Int type")
            }
        }
    }
}

impl std::error::Error for IntegerOperandError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegerExpressionError {
    Operand(IntegerOperandError),
    Evaluation(IntegerArithmeticError),
    SecretEvaluation(IntegerArithmeticError),
}

impl fmt::Display for IntegerExpressionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operand(error) => error.fmt(f),
            Self::Evaluation(error) => error.fmt(f),
            Self::SecretEvaluation(_error) => {
                f.write_str("integer arithmetic on a secret operand failed")
            }
        }
    }
}

impl std::error::Error for IntegerExpressionError {}

/// A sealed, structurally valid deferred integer expression.
///
/// Fields are private so all construction and rebuilding passes through
/// [`Self::new`]. Schema-aware validation separately proves every deferred
/// leaf is Int before a parsed configuration may cross the validated-config
/// boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedIntegerExpr {
    lhs: Box<Value>,
    op: IntegerOp,
    rhs: Box<Value>,
    operator_span: IntegerOperatorSpan,
}

impl ValidatedIntegerExpr {
    pub fn new(
        lhs: Value,
        op: IntegerOp,
        rhs: Value,
        operator_span: IntegerOperatorSpan,
    ) -> Result<Self, IntegerOperandError> {
        validate_operand_shape(&lhs, IntegerOperandSide::Left)?;
        validate_operand_shape(&rhs, IntegerOperandSide::Right)?;
        Ok(Self {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
            operator_span,
        })
    }

    fn from_post_validation(
        lhs: Value,
        op: IntegerOp,
        rhs: Value,
        operator_span: IntegerOperatorSpan,
    ) -> Result<Self, IntegerOperandError> {
        validate_post_validation_operand_shape(&lhs, IntegerOperandSide::Left)?;
        validate_post_validation_operand_shape(&rhs, IntegerOperandSide::Right)?;
        Ok(Self {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
            operator_span,
        })
    }

    fn from_saved_plan(
        lhs: Value,
        op: IntegerOp,
        rhs: Value,
        operator_span: IntegerOperatorSpan,
    ) -> Result<Self, IntegerOperandError> {
        validate_saved_plan_operand_shape(&lhs, IntegerOperandSide::Left)?;
        validate_saved_plan_operand_shape(&rhs, IntegerOperandSide::Right)?;
        Ok(Self {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
            operator_span,
        })
    }

    pub fn lhs(&self) -> &Value {
        &self.lhs
    }

    pub const fn op(&self) -> IntegerOp {
        self.op
    }

    pub fn rhs(&self) -> &Value {
        &self.rhs
    }

    pub const fn operator_span(&self) -> IntegerOperatorSpan {
        self.operator_span
    }

    /// Rebuild a previously validated expression after recursively rewriting
    /// its operands.
    ///
    /// Runtime unknowns are valid here because the original expression has
    /// already proved that each position is Int. The public constructor still
    /// rejects an unknown that has no such type authority.
    pub fn rebuild(&self, lhs: Value, rhs: Value) -> Result<Self, IntegerOperandError> {
        Self::from_post_validation(lhs, self.op, rhs, self.operator_span)
    }

    /// Resolve or partially resolve a previously validated expression.
    /// Known operands fold through the shared checked evaluator; a runtime
    /// unknown remains a typed deferred Int expression.
    pub fn evaluate_with_operands(
        &self,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value, IntegerExpressionError> {
        evaluate_integer_values_inner(lhs, self.op, rhs, self.operator_span, |value, side| {
            validate_post_validation_operand_shape(value, side)
        })
    }
}

/// Evaluate a pair of Int operands as far as their current availability
/// allows. Known operands fold to a concrete Int; otherwise this rebuilds a
/// validated deferred node. A secret on either operand makes the whole result
/// secret, normalized to one wrapper.
pub fn evaluate_integer_values(
    lhs: Value,
    op: IntegerOp,
    rhs: Value,
    operator_span: IntegerOperatorSpan,
) -> Result<Value, IntegerExpressionError> {
    evaluate_integer_values_inner(lhs, op, rhs, operator_span, validate_operand_shape)
}

fn evaluate_integer_values_inner(
    lhs: Value,
    op: IntegerOp,
    rhs: Value,
    operator_span: IntegerOperatorSpan,
    validate: impl Fn(&Value, IntegerOperandSide) -> Result<(), IntegerOperandError>,
) -> Result<Value, IntegerExpressionError> {
    let (lhs, lhs_secret_depth) = crate::resource::peel_secrets(lhs);
    let (rhs, rhs_secret_depth) = crate::resource::peel_secrets(rhs);
    let secret = lhs_secret_depth > 0 || rhs_secret_depth > 0;

    validate(&lhs, IntegerOperandSide::Left).map_err(IntegerExpressionError::Operand)?;
    validate(&rhs, IntegerOperandSide::Right).map_err(IntegerExpressionError::Operand)?;

    // A known-zero divisor fails even while the left operand remains deferred.
    // Type admission deliberately runs first so e.g. `'abc' / 0` reports the
    // invalid operand rather than hiding it behind this evaluation failure.
    if matches!(&rhs, Value::Concrete(ConcreteValue::Int(0)))
        && let Some(error) = zero_divisor_error(op)
    {
        return Err(if secret {
            IntegerExpressionError::SecretEvaluation(error)
        } else {
            IntegerExpressionError::Evaluation(error)
        });
    }

    let value = match (&lhs, &rhs) {
        (Value::Concrete(ConcreteValue::Int(lhs)), Value::Concrete(ConcreteValue::Int(rhs))) => {
            Value::Concrete(ConcreteValue::Int(
                evaluate_checked_integer(*lhs, op, *rhs).map_err(|error| {
                    if secret {
                        IntegerExpressionError::SecretEvaluation(error)
                    } else {
                        IntegerExpressionError::Evaluation(error)
                    }
                })?,
            ))
        }
        _ => Value::Deferred(DeferredValue::IntegerArithmetic(ValidatedIntegerExpr {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
            operator_span,
        })),
    };
    Ok(if secret {
        Value::Deferred(DeferredValue::Secret(Box::new(value)))
    } else {
        value
    })
}

fn validate_post_validation_operand_shape(
    value: &Value,
    side: IntegerOperandSide,
) -> Result<(), IntegerOperandError> {
    validate_operand_shape_at_stage(value, side, IntegerOperandAdmission::PostValidation)
}

fn validate_operand_shape(
    value: &Value,
    side: IntegerOperandSide,
) -> Result<(), IntegerOperandError> {
    validate_operand_shape_at_stage(value, side, IntegerOperandAdmission::Parser)
}

fn validate_saved_plan_operand_shape(
    value: &Value,
    side: IntegerOperandSide,
) -> Result<(), IntegerOperandError> {
    validate_operand_shape_at_stage(value, side, IntegerOperandAdmission::SavedPlan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegerOperandAdmission {
    Parser,
    PostValidation,
    SavedPlan,
}

fn validate_operand_shape_at_stage(
    value: &Value,
    side: IntegerOperandSide,
    admission: IntegerOperandAdmission,
) -> Result<(), IntegerOperandError> {
    match value {
        Value::Concrete(ConcreteValue::Int(_)) => Ok(()),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            validate_operand_shape_at_stage(inner, side, admission)
        }
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::IntegerArithmetic(_),
        ) => Ok(()),
        Value::Deferred(DeferredValue::Unknown(
            UnknownReason::FnParam { .. }
            | UnknownReason::FnLocal { .. }
            | UnknownReason::ModuleArgument { .. }
            | UnknownReason::ForKey
            | UnknownReason::ForIndex
            | UnknownReason::ForValue,
        )) if matches!(
            admission,
            IntegerOperandAdmission::Parser | IntegerOperandAdmission::PostValidation
        ) =>
        {
            Ok(())
        }
        Value::Deferred(DeferredValue::Unknown(
            UnknownReason::UpstreamExport { .. } | UnknownReason::PendingReRead(_),
        )) if matches!(
            admission,
            IntegerOperandAdmission::PostValidation | IntegerOperandAdmission::SavedPlan
        ) =>
        {
            Ok(())
        }
        Value::Deferred(DeferredValue::Unknown(_)) => {
            Err(IntegerOperandError::UntypedUnknown { side })
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Deferred(DeferredValue::Interpolation(_)) => {
            Err(IntegerOperandError::NonInteger {
                side,
                actual: "String",
            })
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_)) => {
            Err(IntegerOperandError::NonInteger {
                side,
                actual: "enum identifier",
            })
        }
        Value::Concrete(ConcreteValue::Float(_)) => Err(IntegerOperandError::NonInteger {
            side,
            actual: "Float",
        }),
        Value::Concrete(ConcreteValue::Bool(_)) => Err(IntegerOperandError::NonInteger {
            side,
            actual: "Bool",
        }),
        Value::Concrete(ConcreteValue::Duration(_)) => Err(IntegerOperandError::NonInteger {
            side,
            actual: "Duration",
        }),
        Value::Concrete(ConcreteValue::List(_) | ConcreteValue::StringList(_)) => {
            Err(IntegerOperandError::NonInteger {
                side,
                actual: "List",
            })
        }
        Value::Concrete(ConcreteValue::Map(_)) => Err(IntegerOperandError::NonInteger {
            side,
            actual: "Map",
        }),
        Value::Deferred(DeferredValue::RedactedSecret { .. })
            if matches!(
                admission,
                IntegerOperandAdmission::PostValidation | IntegerOperandAdmission::SavedPlan
            ) =>
        {
            Ok(())
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(IntegerOperandError::NonInteger {
                side,
                actual: "redacted Secret",
            })
        }
    }
}

impl Serialize for ValidatedIntegerExpr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            lhs: &'a Value,
            op: IntegerOp,
            rhs: &'a Value,
        }

        Wire {
            lhs: self.lhs(),
            op: self.op(),
            rhs: self.rhs(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ValidatedIntegerExpr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            lhs: Value,
            op: IntegerOp,
            rhs: Value,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::from_saved_plan(
            wire.lhs,
            wire.op,
            wire.rhs,
            IntegerOperatorSpan::synthetic(),
        )
        .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AccessPath, UnknownReason};

    #[test]
    fn checked_evaluator_covers_zero_sign_and_overflow_edges() {
        for (lhs, op, rhs, expected) in [
            (i64::MAX, IntegerOp::Add, 0, i64::MAX),
            (i64::MIN, IntegerOp::Add, 0, i64::MIN),
            (i64::MAX, IntegerOp::Subtract, 0, i64::MAX),
            (i64::MIN, IntegerOp::Subtract, 0, i64::MIN),
            (i64::MAX, IntegerOp::Multiply, 1, i64::MAX),
            (i64::MIN, IntegerOp::Multiply, 1, i64::MIN),
            (i64::MAX, IntegerOp::Divide, 1, i64::MAX),
            (i64::MIN, IntegerOp::Divide, 1, i64::MIN),
            (i64::MAX, IntegerOp::Remainder, 2, 1),
            (i64::MIN, IntegerOp::Remainder, 2, 0),
        ] {
            assert_eq!(
                evaluate_checked_integer(lhs, op, rhs),
                Ok(expected),
                "{lhs} {op} {rhs}",
            );
        }
        assert_eq!(evaluate_checked_integer(-5, IntegerOp::Divide, 2), Ok(-2));
        assert_eq!(
            evaluate_checked_integer(-5, IntegerOp::Remainder, 2),
            Ok(-1)
        );
        assert_eq!(evaluate_checked_integer(5, IntegerOp::Divide, -2), Ok(-2));
        assert_eq!(evaluate_checked_integer(5, IntegerOp::Remainder, -2), Ok(1));
        assert_eq!(
            evaluate_checked_integer(1, IntegerOp::Divide, 0),
            Err(IntegerArithmeticError::DivideByZero)
        );
        assert_eq!(
            evaluate_checked_integer(1, IntegerOp::Remainder, 0),
            Err(IntegerArithmeticError::RemainderByZero)
        );
        for (lhs, op, rhs) in [
            (i64::MAX, IntegerOp::Add, 1),
            (i64::MIN, IntegerOp::Add, -1),
            (i64::MAX, IntegerOp::Subtract, -1),
            (i64::MIN, IntegerOp::Subtract, 1),
            (i64::MAX, IntegerOp::Multiply, 2),
            (i64::MIN, IntegerOp::Multiply, 2),
            (i64::MIN, IntegerOp::Divide, -1),
            (i64::MIN, IntegerOp::Remainder, -1),
        ] {
            assert_eq!(
                evaluate_checked_integer(lhs, op, rhs),
                Err(IntegerArithmeticError::Overflow),
                "{lhs} {op} {rhs}"
            );
        }
    }

    #[test]
    fn deferred_construction_is_structural_and_secret_taint_is_preserved() {
        let reference = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("source", "seconds"),
        });
        let deferred = evaluate_integer_values(
            reference.clone(),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap();
        let nested = evaluate_integer_values(
            deferred,
            IntegerOp::Multiply,
            Value::Concrete(ConcreteValue::Int(2)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap();
        assert!(matches!(
            nested,
            Value::Deferred(DeferredValue::IntegerArithmetic(_))
        ));
        assert_eq!(nested.root_bindings(), ["source".to_string()].into());

        let secret = Value::Deferred(DeferredValue::Secret(Box::new(reference)));
        let result = evaluate_integer_values(
            secret,
            IntegerOp::Subtract,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap();
        assert!(matches!(
            result,
            Value::Deferred(DeferredValue::Secret(inner))
                if matches!(*inner, Value::Deferred(DeferredValue::IntegerArithmetic(_)))
        ));
    }

    #[test]
    fn known_zero_rhs_and_untyped_runtime_unknowns_are_rejected_before_resolution() {
        let reference = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("source", "seconds"),
        });
        assert_eq!(
            evaluate_integer_values(
                reference,
                IntegerOp::Divide,
                Value::Concrete(ConcreteValue::Int(0)),
                IntegerOperatorSpan::synthetic(),
            ),
            Err(IntegerExpressionError::Evaluation(
                IntegerArithmeticError::DivideByZero
            ))
        );

        let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamAllExports {
            binding: "upstream".to_string(),
        }));
        assert!(matches!(
            evaluate_integer_values(
                unknown,
                IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                IntegerOperatorSpan::synthetic(),
            ),
            Err(IntegerExpressionError::Operand(
                IntegerOperandError::UntypedUnknown { .. }
            ))
        ));
    }

    #[test]
    fn a_validated_integer_slot_preserves_a_runtime_unknown_structurally() {
        let expression = evaluate_integer_values(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("source", "seconds"),
            }),
            IntegerOp::Divide,
            Value::Concrete(ConcreteValue::Int(60)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap();
        let Value::Deferred(DeferredValue::IntegerArithmetic(expression)) = expression else {
            panic!("resource arithmetic should remain deferred");
        };

        let resolved = expression
            .evaluate_with_operands(
                Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
                    path: AccessPath::new("source", "seconds"),
                })),
                Value::Concrete(ConcreteValue::Int(60)),
            )
            .unwrap();

        assert!(matches!(
            resolved,
            Value::Deferred(DeferredValue::IntegerArithmetic(_))
        ));
        assert_eq!(
            crate::value::format_value(&resolved),
            "(known after upstream apply: source.seconds) / 60"
        );
    }

    #[test]
    fn post_validation_runtime_operands_round_trip_through_serde() {
        let expression = ValidatedIntegerExpr::new(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("source", "seconds"),
            }),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .expect("resource arithmetic is valid before stamping");

        for runtime_operand in [
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
                path: AccessPath::new("source", "seconds"),
            })),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::PendingReRead(
                crate::resource::PendingReRead::incomplete("test pending reread"),
            ))),
            Value::Deferred(DeferredValue::RedactedSecret {
                hash: crate::resource::RedactedSecretHash::generate(b"arithmetic operand"),
            }),
        ] {
            let stamped_value = expression
                .evaluate_with_operands(runtime_operand, Value::Concrete(ConcreteValue::Int(1)))
                .expect("a validated slot admits plan-time runtime markers");
            let Value::Deferred(DeferredValue::IntegerArithmetic(stamped)) = stamped_value else {
                panic!("a runtime operand must keep the expression deferred");
            };
            let encoded = serde_json::to_string(&stamped).expect("expression serializes");
            let decoded: ValidatedIntegerExpr =
                serde_json::from_str(&encoded).expect("saved expression must decode");
            assert_eq!(
                serde_json::to_value(decoded).expect("decoded expression reserializes"),
                serde_json::to_value(stamped).expect("stamped expression reserializes"),
            );
        }
    }

    #[test]
    fn saved_expression_decode_rejects_parser_stage_operands() {
        let parser_expression = ValidatedIntegerExpr::new(
            Value::Deferred(DeferredValue::Unknown(UnknownReason::FnParam {
                name: "count".to_string(),
            })),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .expect("a typed function placeholder is valid while parsing");
        let encoded = serde_json::to_string(&parser_expression).expect("expression serializes");

        let error = serde_json::from_str::<ValidatedIntegerExpr>(&encoded)
            .expect_err("parser-stage obligations must not enter a saved plan");
        assert!(
            error
                .to_string()
                .contains("unknown without a proven Int type"),
            "unexpected decode error: {error}",
        );
    }

    #[test]
    fn operand_type_errors_precede_zero_divisor_errors() {
        let error = evaluate_integer_values(
            Value::Concrete(ConcreteValue::String("not an integer".into())),
            IntegerOp::Divide,
            Value::Concrete(ConcreteValue::Int(0)),
            IntegerOperatorSpan::synthetic(),
        )
        .expect_err("a non-Int left operand must be rejected");

        assert_eq!(
            error,
            IntegerExpressionError::Operand(IntegerOperandError::NonInteger {
                side: IntegerOperandSide::Left,
                actual: "String",
            })
        );
    }

    #[test]
    fn secret_operand_evaluation_failures_are_masked() {
        for (lhs, op, rhs, expected) in [
            (
                1,
                IntegerOp::Divide,
                0,
                IntegerArithmeticError::DivideByZero,
            ),
            (
                1,
                IntegerOp::Remainder,
                0,
                IntegerArithmeticError::RemainderByZero,
            ),
            (
                i64::MAX,
                IntegerOp::Add,
                1,
                IntegerArithmeticError::Overflow,
            ),
        ] {
            let error = evaluate_integer_values(
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::Int(lhs),
                )))),
                op,
                Value::Concrete(ConcreteValue::Int(rhs)),
                IntegerOperatorSpan::synthetic(),
            )
            .expect_err("checked arithmetic failure must remain an error");

            assert_eq!(error, IntegerExpressionError::SecretEvaluation(expected));
            assert_eq!(
                error.to_string(),
                "integer arithmetic on a secret operand failed",
                "{lhs} {op} {rhs} must not reveal the failure kind"
            );
        }
    }
}
