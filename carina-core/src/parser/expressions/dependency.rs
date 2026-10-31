//! Parser-side lowering for dependency operands shared by resource directives
//! and waits.

use pest::iterators::Pair;

use crate::eval_value::EvalValue;
use crate::parser::context::{ParseBindingRef, ParseContext};
use crate::parser::{ParseError, Rule, TypeExpr, UnexpandedDependencyOperand};
use crate::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};

fn invalid_operand(line: usize, boundary: &str, actual: impl std::fmt::Display) -> ParseError {
    ParseError::InvalidExpression {
        line,
        message: format!("{boundary}: expected a managed resource handle; got {actual}",),
    }
}

/// Lower one authored dependency expression without turning a value into an
/// address. Bootstrap-only unresolved dotted argument types remain
/// provisional; every known value type is rejected at this boundary.
pub(in crate::parser) fn parse_dependency_operand(
    expression: Pair<'_, Rule>,
    ctx: &ParseContext<'_>,
    boundary: &str,
) -> Result<UnexpandedDependencyOperand, ParseError> {
    let line = expression.line_col().0;
    match super::super::parse_expression_eval(expression, ctx)? {
        EvalValue::UnexpandedStructuralHandle(handle) => {
            let Some(handle) = handle.resource_handle() else {
                return Err(invalid_operand(line, boundary, handle.kind().description()));
            };
            Ok(UnexpandedDependencyOperand::Handle(handle))
        }
        EvalValue::User(Value::Deferred(DeferredValue::ResourceRef { path }))
            if path.segments().is_empty() =>
        {
            Ok(UnexpandedDependencyOperand::AttributeSelection {
                binding: path.binding().into(),
                attribute: path.attribute().to_string(),
            })
        }
        EvalValue::User(Value::Deferred(DeferredValue::BindingRef { binding }))
        | EvalValue::ProvisionalReference { binding } => {
            Ok(UnexpandedDependencyOperand::BareBinding(binding.into()))
        }
        EvalValue::User(Value::Concrete(ConcreteValue::EnumIdentifier(name))) => Ok(
            UnexpandedDependencyOperand::BareBinding(name.as_str().into()),
        ),
        EvalValue::User(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ModuleArgument { name, .. },
        ))) => match ctx.lookup_binding(&name) {
            ParseBindingRef::Value {
                declared_type: Some(declared_type),
                ..
            } => match declared_type.as_ref() {
                TypeExpr::DottedUnresolved(_) => {
                    Ok(UnexpandedDependencyOperand::BareBinding(name.into()))
                }
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::List(_)
                | TypeExpr::Map(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_) => {
                    Err(invalid_operand(line, boundary, declared_type.as_ref()))
                }
                TypeExpr::Ref(_) => unreachable!(
                    "resource-typed module arguments are registered as structural bindings"
                ),
            },
            ParseBindingRef::Value {
                declared_type: None,
                ..
            }
            | ParseBindingRef::Unbound => Err(invalid_operand(line, boundary, "unknown value")),
            ParseBindingRef::Structural(_) => unreachable!(
                "a structural binding cannot evaluate to a value-shaped module placeholder"
            ),
        },
        EvalValue::User(value) => Err(invalid_operand(
            line,
            boundary,
            crate::validation::typecheck::value_kind_name(&value),
        )),
        EvalValue::PendingValue(_)
        | EvalValue::ComputedWithUnexpandedStructuralHandles { .. }
        | EvalValue::DeferredForValue { .. }
        | EvalValue::Closure { .. } => Err(ParseError::InvalidExpression {
            line,
            message: format!("{boundary}: expected a managed resource handle"),
        }),
    }
}
