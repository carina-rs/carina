//! Expression evaluator for module `validation` and `require` blocks.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use crate::binding_index::ResolvedBindings;
use crate::parser::{ArgumentParameter, CompareOp, RequireBlock, ValidateExpr};
use crate::resource::{
    Composition, ConcreteValue, DeferredValue, ModuleConstraintId, PendingModuleConstraint,
    ResourceId, Value,
};

use super::ModuleError;

/// A failed module value constraint with deterministic, display-safe actuals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConstraintViolation {
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
}

/// A pending module constraint that failed at a later resolution boundary.
///
/// Actual values are rendered eagerly with the secret-aware value formatter;
/// this type never retains a [`Value`] and is therefore safe to display or
/// debug-log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleConstraintFailure {
    pub composition_id: ResourceId,
    constraint_id: ModuleConstraintId,
    pub module: String,
    pub instance: String,
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
    pub detail: Option<String>,
}

impl ModuleConstraintFailure {
    /// The authored message plus any evaluator or resolution detail.
    pub fn message_with_detail(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} ({detail})", self.message),
            None => self.message.clone(),
        }
    }
}

impl fmt::Display for ModuleConstraintFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arguments = self
            .arguments
            .iter()
            .map(|argument| format!("`{argument}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let actuals = self
            .actuals
            .iter()
            .map(|(argument, value)| format!("{argument} = {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "module `{}` instance `{}` constraint failed for argument(s) {}: {}; values: {}",
            self.module, self.instance, arguments, self.message, actuals
        )?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
    }
}

/// Result of evaluating a module value constraint at a resolution boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintEvaluation {
    Satisfied,
    Pending,
    Violated(ModuleConstraintViolation),
    EvalError(String),
}

/// Where a module constraint is declared, and therefore which arguments are
/// visible while it is evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleConstraintKind {
    /// A `validation` block nested in one argument declaration. Only that
    /// argument is in scope, even when the module declares other arguments.
    ArgumentValidation { argument: String },
    /// A module-level `require`, which may reference any module argument.
    Require,
}

/// The constraint definitions to evaluate at one value-resolution boundary.
///
/// Expansion and editor diagnostics start from authored declarations. Plan
/// and apply re-evaluate the subset that was pending during expansion and was
/// retained on the composition. Both paths enter the same evaluator.
#[derive(Clone, Copy)]
pub enum ModuleConstraints<'a> {
    Declarations {
        arguments: &'a [ArgumentParameter],
        requires: &'a [RequireBlock],
    },
    Pending(&'a [PendingModuleConstraint]),
}

impl<'a> ModuleConstraints<'a> {
    pub fn declarations(arguments: &'a [ArgumentParameter], requires: &'a [RequireBlock]) -> Self {
        Self::Declarations {
            arguments,
            requires,
        }
    }

    pub fn pending(constraints: &'a [PendingModuleConstraint]) -> Self {
        Self::Pending(constraints)
    }
}

/// One authored module constraint together with its evaluation at the current
/// resolution boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatedModuleConstraint {
    constraint: PendingModuleConstraint,
    kind: ModuleConstraintKind,
    arguments: Vec<String>,
    actuals: Vec<(String, String)>,
    evaluation: ConstraintEvaluation,
}

impl EvaluatedModuleConstraint {
    pub fn constraint(&self) -> &PendingModuleConstraint {
        &self.constraint
    }

    pub fn kind(&self) -> &ModuleConstraintKind {
        &self.kind
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn actuals(&self) -> &[(String, String)] {
        &self.actuals
    }

    pub fn evaluation(&self) -> &ConstraintEvaluation {
        &self.evaluation
    }

    pub fn into_constraint(self) -> PendingModuleConstraint {
        self.constraint
    }

    /// Convert a concrete failure to the resolver's public error type.
    /// Expansion and LSP diagnostics both use this renderer, so their text
    /// cannot drift independently.
    pub fn module_error(&self, module: &str, instance: &str) -> Option<ModuleError> {
        let message = match &self.evaluation {
            ConstraintEvaluation::Violated(violation) => violation.message.clone(),
            ConstraintEvaluation::EvalError(error) => format!(
                "{} (error evaluating constraint: {error})",
                self.constraint.message()
            ),
            ConstraintEvaluation::Satisfied | ConstraintEvaluation::Pending => return None,
        };
        match &self.kind {
            ModuleConstraintKind::ArgumentValidation { argument } => {
                let actual = self
                    .actuals
                    .first()
                    .map(|(_, value)| value.clone())
                    .unwrap_or_else(|| "<missing>".to_string());
                Some(ModuleError::ArgumentValidationFailed {
                    module: module.to_string(),
                    instance: instance.to_string(),
                    argument: argument.clone(),
                    message,
                    actual,
                })
            }
            ModuleConstraintKind::Require => Some(ModuleError::RequireConstraintFailed {
                module: module.to_string(),
                instance: instance.to_string(),
                arguments: self.arguments.join(", "),
                message,
                actuals: self
                    .actuals
                    .iter()
                    .map(|(name, value)| format!("{name} = {value}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }
}

/// Collect and evaluate every module constraint using the language's single
/// scoping rule: an argument-local `validation` sees only its own argument;
/// a module-level `require` sees every argument.
pub fn evaluate_module_constraints(
    constraints: ModuleConstraints<'_>,
    argument_values: &HashMap<String, Value>,
) -> Vec<EvaluatedModuleConstraint> {
    let definitions = match constraints {
        ModuleConstraints::Declarations {
            arguments,
            requires,
        } => {
            let validations = arguments.iter().flat_map(|argument| {
                argument
                    .validations
                    .iter()
                    .enumerate()
                    .map(move |(ordinal, validation)| {
                        (
                            PendingModuleConstraint {
                                id: ModuleConstraintId::argument_validation(
                                    &argument.name,
                                    ordinal,
                                ),
                                expression: validation.condition.clone(),
                                message: validation.error_message.clone().unwrap_or_else(|| {
                                    format!("validation failed for argument '{}'", argument.name)
                                }),
                            },
                            ModuleConstraintKind::ArgumentValidation {
                                argument: argument.name.clone(),
                            },
                        )
                    })
            });
            let requirements = requires.iter().enumerate().map(|(ordinal, require)| {
                (
                    PendingModuleConstraint {
                        id: ModuleConstraintId::require(ordinal),
                        expression: require.condition.clone(),
                        message: require.error_message.clone(),
                    },
                    ModuleConstraintKind::Require,
                )
            });
            validations.chain(requirements).collect::<Vec<_>>()
        }
        ModuleConstraints::Pending(constraints) => constraints
            .iter()
            .cloned()
            .map(|constraint| {
                let kind = constraint
                    .id()
                    .validation_argument()
                    .map(|argument| ModuleConstraintKind::ArgumentValidation {
                        argument: argument.to_string(),
                    })
                    .unwrap_or(ModuleConstraintKind::Require);
                (constraint, kind)
            })
            .collect(),
    };

    definitions
        .into_iter()
        .map(|(constraint, kind)| {
            let arguments = match &kind {
                ModuleConstraintKind::ArgumentValidation { argument } => vec![argument.clone()],
                ModuleConstraintKind::Require => {
                    referenced_constraint_arguments(constraint.expression())
                }
            };
            let actuals = constraint_actuals(&arguments, argument_values);
            let evaluation = match &kind {
                ModuleConstraintKind::ArgumentValidation { argument } => {
                    let Some(value) = argument_values.get(argument) else {
                        return EvaluatedModuleConstraint {
                            constraint,
                            kind,
                            arguments,
                            actuals,
                            evaluation: ConstraintEvaluation::Pending,
                        };
                    };
                    evaluate_constraint(
                        constraint.expression(),
                        &HashMap::from([(argument.clone(), value.clone())]),
                        constraint.message(),
                    )
                }
                ModuleConstraintKind::Require
                    if arguments
                        .iter()
                        .any(|argument| !argument_values.contains_key(argument)) =>
                {
                    ConstraintEvaluation::Pending
                }
                ModuleConstraintKind::Require => evaluate_constraint(
                    constraint.expression(),
                    argument_values,
                    constraint.message(),
                ),
            };
            EvaluatedModuleConstraint {
                constraint,
                kind,
                arguments,
                actuals,
                evaluation,
            }
        })
        .collect()
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
) -> ConstraintEvaluation {
    match evaluate_constraint_inner(expr, arguments, message.into()) {
        Ok(evaluation) => evaluation,
        Err(error) => ConstraintEvaluation::EvalError(error),
    }
}

fn evaluate_constraint_inner(
    expr: &ValidateExpr,
    arguments: &HashMap<String, Value>,
    message: String,
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
        message,
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

/// Resolve and evaluate every constraint pending on the supplied compositions.
///
/// Argument resolution is deliberately temporary: the authored source values
/// stored in [`Composition::signature`] remain untouched so apply can resolve
/// them again against newer bindings. Evaluation never removes constraints,
/// so apply can re-evaluate constraints that planning found satisfied against
/// values published by upstream effects. A replacement or update may publish
/// a value different from the pre-apply state. Violations are returned with
/// display-safe actual values.
/// When `terminal` is false, constraints with unresolved inputs remain pending.
/// When it is true, unresolved inputs become failures because no later effect
/// can make them decidable.
pub fn evaluate_pending_constraints(
    compositions: &[Composition],
    bindings: &ResolvedBindings,
    terminal: bool,
) -> Vec<ModuleConstraintFailure> {
    let mut failures = Vec::new();

    for composition in compositions {
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

        for evaluated in evaluate_module_constraints(
            ModuleConstraints::pending(&composition.signature.pending_constraints),
            &resolved_arguments,
        ) {
            let constraint = evaluated.constraint();
            let arguments = evaluated.arguments().to_vec();
            let failure =
                |message: String, actuals: Vec<(String, String)>, detail: Option<String>| {
                    ModuleConstraintFailure {
                        composition_id: composition.id.clone(),
                        constraint_id: constraint.id().clone(),
                        module: composition.module_name.clone(),
                        instance: composition.instance.clone(),
                        arguments: arguments.clone(),
                        message,
                        actuals,
                        detail,
                    }
                };
            let actuals = || evaluated.actuals().to_vec();
            let resolution_error = arguments
                .iter()
                .find_map(|name| resolution_errors.get(name).map(|error| (name, error)));
            if let Some((name, error)) = resolution_error {
                failures.push(failure(
                    constraint.message().to_string(),
                    actuals(),
                    Some(format!("could not resolve argument `{name}`: {error}")),
                ));
                continue;
            }

            match evaluated.evaluation() {
                ConstraintEvaluation::Satisfied => {}
                ConstraintEvaluation::Pending if !terminal => {}
                ConstraintEvaluation::Pending => failures.push(failure(
                    constraint.message().to_string(),
                    actuals(),
                    Some("constraint inputs are still unresolved at end of apply".to_string()),
                )),
                ConstraintEvaluation::Violated(violation) => {
                    failures.push(failure(
                        violation.message.clone(),
                        violation.actuals.clone(),
                        None,
                    ));
                }
                ConstraintEvaluation::EvalError(error) => {
                    failures.push(failure(
                        constraint.message().to_string(),
                        actuals(),
                        Some(format!("error evaluating constraint: {error}")),
                    ));
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
            evaluate_constraint(&expression, &arguments, "port must be positive"),
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
                evaluate_constraint(&expression, &arguments, "too short"),
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

        let result = evaluate_constraint(&expression, &arguments, "password is invalid");
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

    #[test]
    fn argument_validation_cannot_reference_a_sibling_argument() {
        use crate::parser::{TypeExpr, ValidationBlock};

        let declarations = vec![
            ArgumentParameter {
                name: "x".to_string(),
                type_expr: TypeExpr::Int,
                default: None,
                description: None,
                validations: vec![ValidationBlock {
                    condition: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("y".to_string())),
                        op: CompareOp::Gt,
                        rhs: Box::new(ValidateExpr::Int(0)),
                    },
                    error_message: Some("x must be valid".to_string()),
                }],
            },
            ArgumentParameter {
                name: "y".to_string(),
                type_expr: TypeExpr::Int,
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ];
        let values = HashMap::from([
            ("x".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("y".to_string(), Value::Concrete(ConcreteValue::Int(2))),
        ]);

        let evaluated = evaluate_module_constraints(
            ModuleConstraints::declarations(&declarations, &[]),
            &values,
        );

        assert_eq!(evaluated.len(), 1);
        assert_eq!(
            evaluated[0].evaluation(),
            &ConstraintEvaluation::EvalError(
                "unknown variable 'y' in constraint expression".to_string()
            )
        );
    }
}
