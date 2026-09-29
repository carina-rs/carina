//! Expression evaluator for module `validation` and `require` blocks.

use std::collections::{BTreeSet, HashMap};

use crate::binding_index::ResolvedBindings;
use crate::parser::{CompareOp, ValidateExpr};
use crate::resource::{Composition, ConcreteValue, DeferredValue, ResourceId, Value};

/// A failed module value constraint with deterministic, display-safe actuals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConstraintViolation {
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
}

/// A pending module constraint that became invalid at a later resolution
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingModuleConstraintFailure {
    pub composition_id: ResourceId,
    pub module: String,
    pub instance: String,
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
}

/// Result of evaluating a module value constraint at a resolution boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintEvaluation {
    Satisfied,
    Pending,
    Violated(ModuleConstraintViolation),
}

/// Format a value for a constraint error without exposing secret contents.
pub(super) fn format_value_for_error(value: &Value) -> String {
    crate::value::format_value(value)
}

/// Evaluate a module `validation` or `require` expression.
///
/// Every referenced variable is collected before evaluation. If any referenced
/// value still contains a deferred leaf, the whole constraint is pending; no
/// expression branch may accidentally turn an unresolved value into an
/// evaluation error. Concrete collections are ordinary expression values, so
/// functions such as `length()` do not need a variable-only escape hatch.
pub fn evaluate_constraint(
    expr: &ValidateExpr,
    arguments: &HashMap<String, Value>,
    message: impl Into<String>,
) -> Result<ConstraintEvaluation, String> {
    let referenced = referenced_constraint_arguments(expr);
    for name in &referenced {
        let value = arguments
            .get(name)
            .ok_or_else(|| format!("unknown variable '{name}' in constraint expression"))?;
        if !is_recursively_concrete(value) {
            return Ok(ConstraintEvaluation::Pending);
        }
    }

    let result = eval_expr(expr, arguments)?;
    let EvalValue::Value(Value::Concrete(ConcreteValue::Bool(satisfied))) = result else {
        return Err(format!(
            "constraint expression must return a boolean, got {}",
            result.kind_name()
        ));
    };
    if satisfied {
        return Ok(ConstraintEvaluation::Satisfied);
    }

    let actuals = referenced
        .iter()
        .map(|name| {
            let value = arguments
                .get(name)
                .expect("referenced variables were checked above");
            (name.clone(), crate::value::format_value(value))
        })
        .collect();
    Ok(ConstraintEvaluation::Violated(ModuleConstraintViolation {
        arguments: referenced,
        message: message.into(),
        actuals,
    }))
}

/// Return the variables referenced by an expression in stable name order.
pub fn referenced_constraint_arguments(expr: &ValidateExpr) -> Vec<String> {
    fn collect(expr: &ValidateExpr, names: &mut BTreeSet<String>) {
        match expr {
            ValidateExpr::Var(name) => {
                names.insert(name.clone());
            }
            ValidateExpr::Compare { lhs, rhs, .. }
            | ValidateExpr::And(lhs, rhs)
            | ValidateExpr::Or(lhs, rhs) => {
                collect(lhs, names);
                collect(rhs, names);
            }
            ValidateExpr::Not(inner) => collect(inner, names),
            ValidateExpr::FunctionCall { args, .. } => {
                for arg in args {
                    collect(arg, names);
                }
            }
            ValidateExpr::Bool(_)
            | ValidateExpr::Int(_)
            | ValidateExpr::Float(_)
            | ValidateExpr::Duration(_)
            | ValidateExpr::String(_)
            | ValidateExpr::Null => {}
        }
    }

    let mut names = BTreeSet::new();
    collect(expr, &mut names);
    names.into_iter().collect()
}

/// Resolve and evaluate every constraint currently pending on compositions.
///
/// Argument resolution is deliberately temporary: the authored source values
/// stored in [`Composition::signature`] remain untouched so apply can resolve
/// them again against newer bindings. Planning reports violations but keeps
/// every constraint that was pending at expansion so apply can re-evaluate it
/// against values published by upstream effects. This includes constraints
/// satisfied by the pre-apply state: a replacement or update may publish a
/// different value. Violations are returned with display-safe actual values.
pub fn evaluate_pending_constraints(
    compositions: &mut [Composition],
    bindings: &ResolvedBindings,
) -> Vec<PendingModuleConstraintFailure> {
    let mut failures = Vec::new();

    for composition in compositions.iter() {
        let mut resolved_arguments = HashMap::new();
        let mut resolution_errors = HashMap::new();
        for (name, argument) in &composition.signature.arguments {
            match crate::resolver::resolve_ref_value(argument.value(), bindings) {
                Ok(value) => {
                    resolved_arguments.insert(name.clone(), value);
                }
                Err(error) => {
                    resolution_errors.insert(name.clone(), error);
                    resolved_arguments.insert(name.clone(), argument.value().clone());
                }
            }
        }

        for constraint in &composition.signature.pending_constraints {
            let mut arguments = constraint.referenced_arguments().to_vec();
            arguments.sort();
            arguments.dedup();
            let resolution_error = arguments
                .iter()
                .find_map(|name| resolution_errors.get(name).map(|error| (name, error)));
            if let Some((name, error)) = resolution_error {
                failures.push(PendingModuleConstraintFailure {
                    composition_id: composition.id.clone(),
                    module: composition.module_name.clone(),
                    instance: composition.instance.clone(),
                    arguments: arguments.clone(),
                    message: format!(
                        "{} (could not resolve argument '{name}': {error})",
                        constraint.message()
                    ),
                    actuals: constraint_actuals(&arguments, &resolved_arguments),
                });
                continue;
            }

            match evaluate_constraint(
                constraint.expression(),
                &resolved_arguments,
                constraint.message(),
            ) {
                Ok(ConstraintEvaluation::Satisfied | ConstraintEvaluation::Pending) => {}
                Ok(ConstraintEvaluation::Violated(violation)) => {
                    failures.push(PendingModuleConstraintFailure {
                        composition_id: composition.id.clone(),
                        module: composition.module_name.clone(),
                        instance: composition.instance.clone(),
                        arguments: violation.arguments,
                        message: violation.message,
                        actuals: violation.actuals,
                    });
                }
                Err(error) => {
                    failures.push(PendingModuleConstraintFailure {
                        composition_id: composition.id.clone(),
                        module: composition.module_name.clone(),
                        instance: composition.instance.clone(),
                        arguments: arguments.clone(),
                        message: format!("{} ({error})", constraint.message()),
                        actuals: constraint_actuals(&arguments, &resolved_arguments),
                    });
                }
            }
        }
    }

    failures
}

fn constraint_actuals(
    arguments: &[String],
    values: &HashMap<String, Value>,
) -> Vec<(String, String)> {
    arguments
        .iter()
        .map(|name| {
            let rendered = values
                .get(name)
                .map(crate::value::format_value)
                .unwrap_or_else(|| "<missing>".to_string());
            (name.clone(), rendered)
        })
        .collect()
}

fn is_recursively_concrete(value: &Value) -> bool {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => items.iter().all(is_recursively_concrete),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().all(is_recursively_concrete),
        Value::Deferred(DeferredValue::Secret(inner)) => is_recursively_concrete(inner),
        Value::Concrete(_) => true,
        Value::Deferred(_) => false,
    }
}

#[derive(Clone)]
enum EvalValue {
    Value(Value),
    Null,
}

impl EvalValue {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Value(Value::Concrete(value)) => match value {
                ConcreteValue::String(_) => "string",
                ConcreteValue::EnumIdentifier(_) => "enum identifier",
                ConcreteValue::CanonicalEnum(_) => "canonical enum",
                ConcreteValue::Int(_) => "int",
                ConcreteValue::Float(_) => "float",
                ConcreteValue::Bool(_) => "bool",
                ConcreteValue::Duration(_) => "duration",
                ConcreteValue::List(_) | ConcreteValue::StringList(_) => "list",
                ConcreteValue::Map(_) => "map",
            },
            Self::Value(Value::Deferred(DeferredValue::Secret(_))) => "secret",
            Self::Value(Value::Deferred(DeferredValue::ResourceRef { .. })) => "resource reference",
            Self::Value(Value::Deferred(DeferredValue::BindingRef { .. })) => "binding reference",
            Self::Value(Value::Deferred(DeferredValue::Interpolation(_))) => "interpolation",
            Self::Value(Value::Deferred(DeferredValue::FunctionCall { .. })) => "function call",
            Self::Value(Value::Deferred(DeferredValue::Unknown(_))) => "unknown",
        }
    }

    fn bool(self, context: &str) -> Result<bool, String> {
        match self {
            Self::Value(Value::Concrete(ConcreteValue::Bool(value))) => Ok(value),
            other => Err(format!(
                "{context} must be boolean, got {}",
                other.kind_name()
            )),
        }
    }
}

fn eval_expr(expr: &ValidateExpr, arguments: &HashMap<String, Value>) -> Result<EvalValue, String> {
    match expr {
        ValidateExpr::Bool(value) => Ok(value_from(ConcreteValue::Bool(*value))),
        ValidateExpr::Int(value) => Ok(value_from(ConcreteValue::Int(*value))),
        ValidateExpr::Float(value) => Ok(value_from(ConcreteValue::Float(*value))),
        ValidateExpr::Duration(value) => Ok(value_from(ConcreteValue::Duration(*value))),
        ValidateExpr::String(value) => Ok(value_from(ConcreteValue::String(value.clone()))),
        ValidateExpr::Null => Ok(EvalValue::Null),
        ValidateExpr::Var(name) => arguments
            .get(name)
            .map(value_for_evaluation)
            .ok_or_else(|| format!("unknown variable '{name}' in constraint expression")),
        ValidateExpr::Compare { lhs, op, rhs } => {
            let left = eval_expr(lhs, arguments)?;
            let right = eval_expr(rhs, arguments)?;
            Ok(value_from(ConcreteValue::Bool(compare_values(
                &left, op, &right,
            )?)))
        }
        ValidateExpr::And(lhs, rhs) => {
            if !eval_expr(lhs, arguments)?.bool("left operand of &&")? {
                return Ok(value_from(ConcreteValue::Bool(false)));
            }
            let value = eval_expr(rhs, arguments)?.bool("right operand of &&")?;
            Ok(value_from(ConcreteValue::Bool(value)))
        }
        ValidateExpr::Or(lhs, rhs) => {
            if eval_expr(lhs, arguments)?.bool("left operand of ||")? {
                return Ok(value_from(ConcreteValue::Bool(true)));
            }
            let value = eval_expr(rhs, arguments)?.bool("right operand of ||")?;
            Ok(value_from(ConcreteValue::Bool(value)))
        }
        ValidateExpr::Not(inner) => {
            let value = eval_expr(inner, arguments)?.bool("operand of !")?;
            Ok(value_from(ConcreteValue::Bool(!value)))
        }
        ValidateExpr::FunctionCall { name, args } => eval_function(name, args, arguments),
    }
}

fn value_from(value: ConcreteValue) -> EvalValue {
    EvalValue::Value(Value::Concrete(value))
}

fn value_for_evaluation(value: &Value) -> EvalValue {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => value_for_evaluation(inner),
        other => EvalValue::Value(other.clone()),
    }
}

fn compare_values(left: &EvalValue, op: &CompareOp, right: &EvalValue) -> Result<bool, String> {
    match (left, right) {
        (EvalValue::Null, EvalValue::Null) => return Ok(matches!(op, CompareOp::Eq)),
        (EvalValue::Null, _) | (_, EvalValue::Null) => {
            return Ok(matches!(op, CompareOp::Ne));
        }
        _ => {}
    }

    let (EvalValue::Value(left), EvalValue::Value(right)) = (left, right) else {
        unreachable!("null values returned above");
    };
    match (left, right) {
        (Value::Concrete(ConcreteValue::Int(left)), Value::Concrete(ConcreteValue::Int(right))) => {
            compare_ordered(left, op, right)
        }
        (
            Value::Concrete(ConcreteValue::Float(left)),
            Value::Concrete(ConcreteValue::Float(right)),
        ) => compare_ordered(left, op, right),
        (
            Value::Concrete(ConcreteValue::Int(left)),
            Value::Concrete(ConcreteValue::Float(right)),
        ) => compare_ordered(&(*left as f64), op, right),
        (
            Value::Concrete(ConcreteValue::Float(left)),
            Value::Concrete(ConcreteValue::Int(right)),
        ) => compare_ordered(left, op, &(*right as f64)),
        (
            Value::Concrete(ConcreteValue::Duration(left)),
            Value::Concrete(ConcreteValue::Duration(right)),
        ) => compare_ordered(left, op, right),
        (
            Value::Concrete(ConcreteValue::String(left)),
            Value::Concrete(ConcreteValue::String(right)),
        ) => compare_equality(left, op, right, "strings"),
        (
            Value::Concrete(ConcreteValue::Bool(left)),
            Value::Concrete(ConcreteValue::Bool(right)),
        ) => compare_equality(left, op, right, "booleans"),
        (Value::Concrete(ConcreteValue::List(_)), Value::Concrete(ConcreteValue::List(_)))
        | (
            Value::Concrete(ConcreteValue::StringList(_)),
            Value::Concrete(ConcreteValue::StringList(_)),
        )
        | (Value::Concrete(ConcreteValue::Map(_)), Value::Concrete(ConcreteValue::Map(_))) => {
            compare_equality(left, op, right, "collections")
        }
        _ => Err(format!(
            "cannot compare {} with {}",
            EvalValue::Value(left.clone()).kind_name(),
            EvalValue::Value(right.clone()).kind_name()
        )),
    }
}

fn compare_ordered<T: PartialOrd + PartialEq>(
    left: &T,
    op: &CompareOp,
    right: &T,
) -> Result<bool, String> {
    Ok(match op {
        CompareOp::Gte => left >= right,
        CompareOp::Lte => left <= right,
        CompareOp::Gt => left > right,
        CompareOp::Lt => left < right,
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
    })
}

fn compare_equality<T: PartialEq>(
    left: &T,
    op: &CompareOp,
    right: &T,
    kind: &str,
) -> Result<bool, String> {
    match op {
        CompareOp::Eq => Ok(left == right),
        CompareOp::Ne => Ok(left != right),
        _ => Err(format!("{kind} only support == and != comparisons")),
    }
}

fn eval_function(
    name: &str,
    args: &[ValidateExpr],
    arguments: &HashMap<String, Value>,
) -> Result<EvalValue, String> {
    match name {
        "len" | "length" => {
            if args.len() != 1 {
                return Err(format!("{}() expects 1 argument, got {}", name, args.len()));
            }
            let value = eval_expr(&args[0], arguments)?;
            let length = match value {
                EvalValue::Value(Value::Concrete(ConcreteValue::String(value))) => value.len(),
                EvalValue::Value(Value::Concrete(ConcreteValue::List(value))) => value.len(),
                EvalValue::Value(Value::Concrete(ConcreteValue::StringList(value))) => value.len(),
                EvalValue::Value(Value::Concrete(ConcreteValue::Map(value))) => value.len(),
                other => {
                    return Err(format!(
                        "{}() argument must be a string, list, or map, got {}",
                        name,
                        other.kind_name()
                    ));
                }
            };
            Ok(value_from(ConcreteValue::Int(length as i64)))
        }
        _ => Err(format!(
            "unknown function '{name}' in constraint expression"
        )),
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;

    fn length_at_least(variable: &str, minimum: i64) -> ValidateExpr {
        ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::FunctionCall {
                name: "length".to_string(),
                args: vec![ValidateExpr::Var(variable.to_string())],
            }),
            op: CompareOp::Gte,
            rhs: Box::new(ValidateExpr::Int(minimum)),
        }
    }

    #[test]
    fn referenced_deferred_value_is_pending_without_debug_rendering() {
        let expression = ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::Var("port".to_string())),
            op: CompareOp::Gt,
            rhs: Box::new(ValidateExpr::Int(0)),
        };
        let arguments = HashMap::from([(
            "port".to_string(),
            Value::resource_ref("producer", "port", Vec::new()),
        )]);

        assert_eq!(
            evaluate_constraint(&expression, &arguments, "port must be positive").unwrap(),
            ConstraintEvaluation::Pending
        );
    }

    #[test]
    fn length_accepts_string_list_string_list_and_map_variables() {
        let cases = [
            (
                "string",
                Value::Concrete(ConcreteValue::String("ab".to_string())),
            ),
            (
                "list",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(1)),
                    Value::Concrete(ConcreteValue::Int(2)),
                ])),
            ),
            (
                "string_list",
                Value::Concrete(ConcreteValue::StringList(vec![
                    "a".to_string(),
                    "b".to_string(),
                ])),
            ),
            (
                "map",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
                    ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
                ]))),
            ),
        ];

        for (name, value) in cases {
            let expression = length_at_least(name, 2);
            let arguments = HashMap::from([(name.to_string(), value)]);
            assert_eq!(
                evaluate_constraint(&expression, &arguments, "too short").unwrap(),
                ConstraintEvaluation::Satisfied,
                "length() rejected {name}"
            );
        }
    }

    #[test]
    fn violation_contains_sorted_secret_masked_actuals() {
        let expression = ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::Var("password".to_string())),
            op: CompareOp::Eq,
            rhs: Box::new(ValidateExpr::String("expected".to_string())),
        };
        let arguments = HashMap::from([(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("plaintext".to_string()),
            )))),
        )]);

        let result = evaluate_constraint(&expression, &arguments, "password is invalid").unwrap();
        let ConstraintEvaluation::Violated(violation) = result else {
            panic!("expected a violation");
        };
        assert_eq!(violation.arguments, vec!["password"]);
        assert_eq!(violation.message, "password is invalid");
        assert_eq!(
            violation.actuals,
            vec![("password".to_string(), "(secret)".to_string())]
        );
        let rendered = format!("{violation:?}");
        assert!(!rendered.contains("plaintext"), "{rendered}");
        assert!(!rendered.contains("Deferred("), "{rendered}");
        assert!(!rendered.contains("ResourceRef"), "{rendered}");
    }
}
