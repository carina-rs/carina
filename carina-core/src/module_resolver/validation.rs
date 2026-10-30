//! Expression evaluator for module `validation` and `require` blocks.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use crate::binding_index::ResolvedBindings;
use crate::parser::{
    ArgumentParameter, CompareOp, ModuleCallId, RequireBlock, ValidateExpr, display_string_literal,
};
use crate::resource::{
    AccessMissPolicy, ConcreteValue, DeferredValue, LiveComposition, ModuleConstraintId,
    PendingModuleConstraint, ResourceId, Value, apply_value_path_segment,
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
    pub diagnostic: ModuleConstraintDiagnostic,
}

/// Lifecycle of the composition call that produced a constraint diagnostic.
/// Live diagnostics retain their parser identity for editor routing; saved
/// diagnostics deliberately name the absence of live parser state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum ConstraintOrigin {
    LiveCall(ModuleCallId),
    #[default]
    Saved,
}

impl fmt::Display for ModuleConstraintFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diagnostic.fmt(f)
    }
}

/// User-facing module-constraint diagnostic shared by validation, planning,
/// apply, and editor diagnostics.
///
/// Actual values are rendered eagerly with the secret-aware value formatter;
/// this type never retains a [`Value`] and is therefore safe to display or
/// debug-log.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ModuleConstraintDiagnostic {
    pub module: String,
    /// Internal call identity used to correlate diagnostics with expansion.
    /// Display must use `call` so anonymous content hashes never leak.
    pub instance: String,
    /// Parser-minted identity for live resolver diagnostics, or an explicit
    /// saved-plan lifecycle marker.
    #[serde(skip)]
    pub origin: ConstraintOrigin,
    pub call: ModuleConstraintCall,
    pub kind: ModuleConstraintKind,
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
    pub detail: Option<String>,
}

impl fmt::Display for ModuleConstraintDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.call {
            ModuleConstraintCall::Named(name) => {
                write!(f, "module '{}' (call '{name}')", self.module)?;
            }
            ModuleConstraintCall::Anonymous => {
                write!(f, "anonymous call to module '{}'", self.module)?;
            }
        }
        f.write_str(": ")?;
        match &self.kind {
            ModuleConstraintKind::ArgumentValidation { argument } => {
                write!(f, "argument '{argument}'")?;
            }
            ModuleConstraintKind::Require => {
                let arguments = self
                    .arguments
                    .iter()
                    .map(|argument| format!("'{argument}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "arguments {arguments}")?;
            }
        }
        write!(f, ": {}", self.message)?;
        if !self.actuals.is_empty() {
            let actuals = match &self.kind {
                ModuleConstraintKind::ArgumentValidation { argument } => self
                    .actuals
                    .iter()
                    .find(|(name, _)| name == argument)
                    .or_else(|| self.actuals.first())
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default(),
                ModuleConstraintKind::Require => self
                    .actuals
                    .iter()
                    .map(|(argument, value)| format!("{argument} = {value}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            write!(f, " (got {actuals})")?;
        }
        if let Some(detail) = &self.detail {
            write!(f, "; {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ModuleConstraintDiagnostic {}

/// Authored identity of a module call for user-facing diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ModuleConstraintCall {
    Named(String),
    Anonymous,
}

impl ModuleConstraintCall {
    fn from_binding(binding: Option<&str>) -> Self {
        binding
            .map(|name| Self::Named(name.to_string()))
            .unwrap_or(Self::Anonymous)
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
    pub fn module_error(
        &self,
        module: &str,
        instance: &str,
        binding: Option<&str>,
        call_id: Option<&ModuleCallId>,
    ) -> Option<ModuleError> {
        self.module_diagnostic(module, instance, binding, call_id)
            .map(|diagnostic| ModuleError::Constraint(Box::new(diagnostic)))
    }

    /// Convert a concrete outcome to the shared typed diagnostic without
    /// re-evaluating its expression or reconstructing its argument values.
    pub fn module_diagnostic(
        &self,
        module: &str,
        instance: &str,
        binding: Option<&str>,
        call_id: Option<&ModuleCallId>,
    ) -> Option<ModuleConstraintDiagnostic> {
        let (message, actuals, detail) = match &self.evaluation {
            ConstraintEvaluation::Violated(violation) => {
                (violation.message.clone(), violation.actuals.clone(), None)
            }
            ConstraintEvaluation::EvalError(error) => (
                self.constraint.message().to_string(),
                self.actuals.clone(),
                Some(format!("error evaluating constraint: {error}")),
            ),
            ConstraintEvaluation::Satisfied | ConstraintEvaluation::Pending => return None,
        };
        Some(ModuleConstraintDiagnostic {
            module: module.to_string(),
            instance: instance.to_string(),
            origin: call_id
                .cloned()
                .map(ConstraintOrigin::LiveCall)
                .unwrap_or(ConstraintOrigin::Saved),
            call: ModuleConstraintCall::from_binding(binding),
            kind: self.kind.clone(),
            arguments: self.arguments.clone(),
            message,
            actuals,
            detail,
        })
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
            let declared_arguments = arguments
                .iter()
                .map(|argument| argument.name.clone())
                .collect::<BTreeSet<_>>();
            let mut definitions = Vec::new();
            for argument in arguments {
                for (ordinal, validation) in argument.validations.iter().enumerate() {
                    definitions.push((
                        PendingModuleConstraint {
                            id: ModuleConstraintId::argument_validation(&argument.name, ordinal),
                            expression: validation.condition.clone(),
                            message: validation.error_message.clone().unwrap_or_else(|| {
                                format!("validation failed for argument '{}'", argument.name)
                            }),
                        },
                        ModuleConstraintKind::ArgumentValidation {
                            argument: argument.name.clone(),
                        },
                        BTreeSet::from([argument.name.clone()]),
                    ));
                }
            }
            for (ordinal, require) in requires.iter().enumerate() {
                definitions.push((
                    PendingModuleConstraint {
                        id: ModuleConstraintId::require(ordinal),
                        expression: require.condition.clone(),
                        message: require.error_message.clone(),
                    },
                    ModuleConstraintKind::Require,
                    declared_arguments.clone(),
                ));
            }
            definitions
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
                let declared_arguments = match &kind {
                    ModuleConstraintKind::ArgumentValidation { argument } => {
                        BTreeSet::from([argument.clone()])
                    }
                    ModuleConstraintKind::Require => argument_values.keys().cloned().collect(),
                };
                (constraint, kind, declared_arguments)
            })
            .collect(),
    };

    definitions
        .into_iter()
        .map(|(constraint, kind, declared_arguments)| {
            let referenced_arguments = referenced_constraint_arguments(constraint.expression());
            let arguments = match &kind {
                ModuleConstraintKind::ArgumentValidation { argument } => vec![argument.clone()],
                ModuleConstraintKind::Require => referenced_arguments.clone(),
            };
            let actuals = constraint_actuals_for_expression(
                constraint.expression(),
                &arguments,
                argument_values,
            );
            let unknown_argument = referenced_arguments
                .iter()
                .find(|argument| !declared_arguments.contains(*argument));
            let evaluation = if let Some(argument) = unknown_argument {
                ConstraintEvaluation::EvalError(format!(
                    "unknown variable '{argument}' in constraint expression"
                ))
            } else {
                match &kind {
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
                }
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
    if !constraint_inputs_are_concrete(expr, arguments)? {
        return Ok(ConstraintEvaluation::Pending);
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

    let actuals = constraint_actuals_for_expression(expr, &referenced, arguments);
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
            ValidateExpr::Access { argument, .. }
            | ValidateExpr::BoundArgument { argument, .. } => {
                names.insert(argument.clone());
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
/// stored in [`LiveComposition::signature`] remain untouched so apply can resolve
/// them again against newer bindings. Evaluation never removes constraints,
/// so apply can re-evaluate constraints that planning found satisfied against
/// values published by upstream effects. A replacement or update may publish
/// a value different from the pre-apply state. Violations are returned with
/// display-safe actual values.
/// When `terminal` is false, constraints with unresolved inputs remain pending.
/// When it is true, unresolved inputs become failures because no later effect
/// can make them decidable.
pub fn evaluate_live_pending_constraints(
    compositions: &[LiveComposition],
    bindings: &ResolvedBindings,
    terminal: bool,
) -> Vec<ModuleConstraintFailure> {
    evaluate_pending_constraint_views(
        compositions
            .iter()
            .map(crate::resource::CompositionRuntimeRef::Live),
        bindings,
        terminal,
    )
}

pub fn evaluate_saved_pending_constraints(
    compositions: &[crate::resource::SavedComposition],
    bindings: &ResolvedBindings,
    terminal: bool,
) -> Vec<ModuleConstraintFailure> {
    evaluate_pending_constraint_views(
        compositions
            .iter()
            .map(crate::resource::CompositionRuntimeRef::Saved),
        bindings,
        terminal,
    )
}

fn evaluate_pending_constraint_views<'a>(
    compositions: impl IntoIterator<Item = crate::resource::CompositionRuntimeRef<'a>>,
    bindings: &ResolvedBindings,
    terminal: bool,
) -> Vec<ModuleConstraintFailure> {
    let mut failures = Vec::new();

    for composition in compositions {
        let mut resolution_errors = HashMap::new();
        let resolved_constraints = composition
            .pending_constraints()
            .iter()
            .map(|constraint| PendingModuleConstraint {
                id: constraint.id.clone(),
                expression: resolve_bound_constraint_values(
                    constraint.expression(),
                    bindings,
                    &mut resolution_errors,
                ),
                message: constraint.message.clone(),
            })
            .collect::<Vec<_>>();
        let raw_arguments = resolved_constraints
            .iter()
            .flat_map(|constraint| raw_constraint_arguments(constraint.expression()))
            .collect::<BTreeSet<_>>();
        let mut resolved_arguments = HashMap::new();
        for (name, argument) in composition.arguments() {
            if raw_arguments.contains(name) {
                match crate::resolver::resolve_ref_value(argument, bindings) {
                    Ok(value) => {
                        resolved_arguments.insert(name.clone(), value);
                    }
                    Err(error) => {
                        resolution_errors.insert(name.clone(), error);
                        resolved_arguments.insert(name.clone(), argument.clone());
                    }
                }
            } else {
                resolved_arguments.insert(name.clone(), argument.clone());
            }
        }

        for evaluated in evaluate_module_constraints(
            ModuleConstraints::pending(&resolved_constraints),
            &resolved_arguments,
        ) {
            let constraint = evaluated.constraint();
            let arguments = evaluated.arguments().to_vec();
            let failure =
                |message: String, actuals: Vec<(String, String)>, detail: Option<String>| {
                    ModuleConstraintFailure {
                        composition_id: composition.id().as_inner().clone(),
                        constraint_id: constraint.id().clone(),
                        diagnostic: ModuleConstraintDiagnostic {
                            module: composition.module_name().to_string(),
                            instance: composition.instance().to_string(),
                            origin: composition
                                .call_id()
                                .cloned()
                                .map(ConstraintOrigin::LiveCall)
                                .unwrap_or(ConstraintOrigin::Saved),
                            call: ModuleConstraintCall::from_binding(composition.binding()),
                            kind: evaluated.kind().clone(),
                            arguments: arguments.clone(),
                            message,
                            actuals,
                            detail,
                        },
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

fn resolve_bound_constraint_values(
    expression: &ValidateExpr,
    bindings: &ResolvedBindings,
    errors: &mut HashMap<String, String>,
) -> ValidateExpr {
    match expression {
        ValidateExpr::BoundArgument {
            argument,
            operand,
            value,
        } => match crate::resolver::resolve_ref_value(value, bindings) {
            Ok(value) => ValidateExpr::BoundArgument {
                argument: argument.clone(),
                operand: operand.clone(),
                value,
            },
            Err(error) => {
                errors.insert(argument.clone(), error);
                expression.clone()
            }
        },
        ValidateExpr::Compare { lhs, op, rhs } => ValidateExpr::Compare {
            lhs: Box::new(resolve_bound_constraint_values(lhs, bindings, errors)),
            op: op.clone(),
            rhs: Box::new(resolve_bound_constraint_values(rhs, bindings, errors)),
        },
        ValidateExpr::And(lhs, rhs) => ValidateExpr::And(
            Box::new(resolve_bound_constraint_values(lhs, bindings, errors)),
            Box::new(resolve_bound_constraint_values(rhs, bindings, errors)),
        ),
        ValidateExpr::Or(lhs, rhs) => ValidateExpr::Or(
            Box::new(resolve_bound_constraint_values(lhs, bindings, errors)),
            Box::new(resolve_bound_constraint_values(rhs, bindings, errors)),
        ),
        ValidateExpr::Not(inner) => ValidateExpr::Not(Box::new(resolve_bound_constraint_values(
            inner, bindings, errors,
        ))),
        ValidateExpr::FunctionCall { name, args } => ValidateExpr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|argument| resolve_bound_constraint_values(argument, bindings, errors))
                .collect(),
        },
        ValidateExpr::Bool(_)
        | ValidateExpr::Int(_)
        | ValidateExpr::Float(_)
        | ValidateExpr::Duration(_)
        | ValidateExpr::String(_)
        | ValidateExpr::Var(_)
        | ValidateExpr::Access { .. }
        | ValidateExpr::Null => expression.clone(),
    }
}

fn raw_constraint_arguments(expression: &ValidateExpr) -> BTreeSet<String> {
    fn collect(expression: &ValidateExpr, out: &mut BTreeSet<String>) {
        match expression {
            ValidateExpr::Var(argument) | ValidateExpr::Access { argument, .. } => {
                out.insert(argument.clone());
            }
            ValidateExpr::Compare { lhs, rhs, .. }
            | ValidateExpr::And(lhs, rhs)
            | ValidateExpr::Or(lhs, rhs) => {
                collect(lhs, out);
                collect(rhs, out);
            }
            ValidateExpr::Not(inner) => collect(inner, out),
            ValidateExpr::FunctionCall { args, .. } => {
                for argument in args {
                    collect(argument, out);
                }
            }
            ValidateExpr::Bool(_)
            | ValidateExpr::Int(_)
            | ValidateExpr::Float(_)
            | ValidateExpr::Duration(_)
            | ValidateExpr::String(_)
            | ValidateExpr::BoundArgument { .. }
            | ValidateExpr::Null => {}
        }
    }

    let mut out = BTreeSet::new();
    collect(expression, &mut out);
    out
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
                .map(format_constraint_actual)
                .unwrap_or_else(|| "<missing>".to_string());
            (name.clone(), rendered)
        })
        .collect()
}

fn constraint_actuals_for_expression(
    expression: &ValidateExpr,
    arguments: &[String],
    values: &HashMap<String, Value>,
) -> Vec<(String, String)> {
    fn collect(
        expression: &ValidateExpr,
        actuals: &mut std::collections::BTreeMap<String, String>,
    ) {
        match expression {
            ValidateExpr::BoundArgument {
                argument,
                operand,
                value,
            } => {
                let operand = if operand.is_empty() {
                    argument
                } else {
                    operand
                };
                actuals.insert(operand.clone(), format_constraint_actual(value));
            }
            ValidateExpr::Compare { lhs, rhs, .. }
            | ValidateExpr::And(lhs, rhs)
            | ValidateExpr::Or(lhs, rhs) => {
                collect(lhs, actuals);
                collect(rhs, actuals);
            }
            ValidateExpr::Not(inner) => collect(inner, actuals),
            ValidateExpr::FunctionCall { args, .. } => {
                for argument in args {
                    collect(argument, actuals);
                }
            }
            ValidateExpr::Bool(_)
            | ValidateExpr::Int(_)
            | ValidateExpr::Float(_)
            | ValidateExpr::Duration(_)
            | ValidateExpr::String(_)
            | ValidateExpr::Var(_)
            | ValidateExpr::Access { .. }
            | ValidateExpr::Null => {}
        }
    }

    let mut actuals = std::collections::BTreeMap::new();
    collect(expression, &mut actuals);
    if actuals.is_empty() {
        constraint_actuals(arguments, values)
    } else {
        actuals.into_iter().collect()
    }
}

fn format_constraint_actual(value: &Value) -> String {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => value
            .diagnostic_literal()
            .map(display_string_literal)
            .unwrap_or_else(|| "string".to_string()),
        Value::Concrete(ConcreteValue::List(values)) => format!(
            "[{}]",
            values
                .iter()
                .map(format_constraint_actual)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Concrete(ConcreteValue::StringList(values)) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| {
                    value
                        .diagnostic_literal()
                        .map(display_string_literal)
                        .unwrap_or_else(|| "string".to_string())
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Concrete(ConcreteValue::Map(values)) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{key}: {}", format_constraint_actual(&values[key])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => crate::value::format_value(value),
    }
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
            Self::Value(Value::Deferred(DeferredValue::RedactedSecret { .. })) => "secret",
            Self::Value(Value::Deferred(DeferredValue::ResourceRef { .. })) => "resource reference",
            Self::Value(Value::Deferred(DeferredValue::BindingRef { .. })) => "binding reference",
            Self::Value(Value::Deferred(DeferredValue::Coalesce { .. })) => "coalesce expression",
            Self::Value(Value::Deferred(DeferredValue::Interpolation(_))) => "interpolation",
            Self::Value(Value::Deferred(DeferredValue::FunctionCall { .. })) => "function call",
            Self::Value(Value::Deferred(DeferredValue::Unavailable { .. })) => {
                "unavailable function result"
            }
            Self::Value(Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })) => {
                "function call"
            }
            Self::Value(Value::Deferred(DeferredValue::Access(_))) => "access expression",
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
        ValidateExpr::String(value) => Ok(value_from(ConcreteValue::String(value.clone().into()))),
        ValidateExpr::Null => Ok(EvalValue::Null),
        ValidateExpr::Var(name) => arguments
            .get(name)
            .map(value_for_evaluation)
            .ok_or_else(|| format!("unknown variable '{name}' in constraint expression")),
        ValidateExpr::Access { argument, path } => {
            let base = arguments
                .get(argument)
                .cloned()
                .ok_or_else(|| format!("unknown variable '{argument}' in constraint expression"))?;
            apply_constraint_path(base, path).map(|value| value_for_evaluation(&value))
        }
        ValidateExpr::BoundArgument { value, .. } => Ok(value_for_evaluation(value)),
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

fn apply_constraint_path(
    mut value: Value,
    path: &[crate::resource::PathSegment],
) -> Result<Value, String> {
    for step in path {
        value = apply_value_path_segment(value, step.clone(), 0, AccessMissPolicy::Error)
            .and_then(crate::resource::ValuePathSegmentResult::into_value)
            .map_err(|error| error.to_string())?;
    }
    Ok(value)
}

fn constraint_inputs_are_concrete(
    expr: &ValidateExpr,
    arguments: &HashMap<String, Value>,
) -> Result<bool, String> {
    match expr {
        ValidateExpr::Var(name) => arguments
            .get(name)
            .map(is_recursively_concrete)
            .ok_or_else(|| format!("unknown variable '{name}' in constraint expression")),
        ValidateExpr::Access { argument, path } => {
            let base = arguments
                .get(argument)
                .cloned()
                .ok_or_else(|| format!("unknown variable '{argument}' in constraint expression"))?;
            Ok(is_recursively_concrete(&apply_constraint_path(base, path)?))
        }
        ValidateExpr::BoundArgument { value, .. } => Ok(is_recursively_concrete(value)),
        ValidateExpr::Compare { lhs, rhs, .. }
        | ValidateExpr::And(lhs, rhs)
        | ValidateExpr::Or(lhs, rhs) => Ok(constraint_inputs_are_concrete(lhs, arguments)?
            && constraint_inputs_are_concrete(rhs, arguments)?),
        ValidateExpr::Not(inner) => constraint_inputs_are_concrete(inner, arguments),
        ValidateExpr::FunctionCall { args, .. } => {
            for argument in args {
                if !constraint_inputs_are_concrete(argument, arguments)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ValidateExpr::Bool(_)
        | ValidateExpr::Int(_)
        | ValidateExpr::Float(_)
        | ValidateExpr::Duration(_)
        | ValidateExpr::String(_)
        | ValidateExpr::Null => Ok(true),
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
                EvalValue::Value(Value::Concrete(ConcreteValue::String(value))) => {
                    value.expose_plaintext().len()
                }
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

    fn redacted_secret() -> Value {
        Value::Deferred(DeferredValue::RedactedSecret {
            hash: crate::resource::RedactedSecretHash::generate(b"validation-test"),
        })
    }

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
                Value::Concrete(ConcreteValue::String("ab".into())),
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
                Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()])),
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
    fn list_string_constraint_actual_uses_carina_literals() {
        let expression = length_at_least("items", 3);
        let arguments = HashMap::from([(
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
        )]);

        let result = evaluate_constraint(&expression, &arguments, "need three items");
        let ConstraintEvaluation::Violated(violation) = result else {
            panic!("expected a violation");
        };
        assert_eq!(
            violation.actuals,
            vec![("items".to_string(), "['a', 'b']".to_string())]
        );

        let nested = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "list".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()])),
            ),
            ("secret".to_string(), redacted_secret()),
        ])));
        assert_eq!(
            format_constraint_actual(&nested),
            "{list: ['a', 'b'], secret: (secret)}"
        );
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
                ConcreteValue::String("plaintext".into()),
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
    fn constraint_actuals_render_strings_and_mask_hashed_secrets() {
        let names = vec![
            "plain".to_string(),
            "dotted_text".to_string(),
            "secret_hash".to_string(),
        ];
        let values = HashMap::from([
            (
                "plain".to_string(),
                Value::Concrete(ConcreteValue::String("abc".into())),
            ),
            (
                "dotted_text".to_string(),
                Value::Concrete(ConcreteValue::String("aws.Region.us_east_1".into())),
            ),
            ("secret_hash".to_string(), redacted_secret()),
        ]);

        assert_eq!(
            constraint_actuals(&names, &values),
            vec![
                ("plain".to_string(), "'abc'".to_string()),
                (
                    "dotted_text".to_string(),
                    "'aws.Region.us_east_1'".to_string()
                ),
                ("secret_hash".to_string(), "(secret)".to_string()),
            ]
        );
    }

    #[test]
    fn argument_validation_cannot_reference_a_sibling_argument() {
        use crate::parser::{TypeExpr, ValidationBlock};

        let declarations = vec![
            ArgumentParameter {
                name: "x".to_string(),
                static_type: TypeExpr::Int,
                default: None,
                description: None,
                validations: vec![ValidationBlock::dummy(
                    ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("y".to_string())),
                        op: CompareOp::Gt,
                        rhs: Box::new(ValidateExpr::Int(0)),
                    },
                    Some("x must be valid".to_string()),
                )],
            },
            ArgumentParameter {
                name: "y".to_string(),
                static_type: TypeExpr::Int,
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

    #[test]
    fn module_constraint_evaluation_error_has_one_renderer_at_every_boundary() {
        use crate::parser::{TypeExpr, ValidationBlock};
        use crate::plan::PlanErrorKind;

        let declarations = vec![ArgumentParameter {
            name: "name".to_string(),
            static_type: TypeExpr::String,
            default: None,
            description: None,
            validations: vec![ValidationBlock::dummy(
                ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("name".to_string())),
                    op: CompareOp::Gt,
                    rhs: Box::new(ValidateExpr::Int(5)),
                },
                Some("bad name".to_string()),
            )],
        }];
        let values = HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("abc".into())),
        )]);
        let evaluated = evaluate_module_constraints(
            ModuleConstraints::declarations(&declarations, &[]),
            &values,
        )
        .into_iter()
        .next()
        .expect("one validation");
        let ConstraintEvaluation::EvalError(detail) = evaluated.evaluation() else {
            panic!("expected evaluation error");
        };
        let detail = format!("error evaluating constraint: {detail}");
        let actuals = evaluated.actuals().to_vec();
        assert_eq!(actuals, [("name".to_string(), "'abc'".to_string())]);
        let parsed = crate::parser::parse_file(
            "let mod = use { source = './mod' }\nmod {}\n",
            std::path::Path::new("constraint-test.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse module call identity");
        let call_id = parsed.module_calls[0].id();
        let resolver_error = evaluated
            .module_error("mod", "c", Some("c"), Some(call_id))
            .expect("evaluation error must render");
        let ModuleError::Constraint(diagnostic) = &resolver_error else {
            panic!("constraint evaluation must produce a constraint error");
        };
        assert_eq!(
            diagnostic.origin,
            ConstraintOrigin::LiveCall(call_id.clone())
        );
        let apply_failure = ModuleConstraintFailure {
            composition_id: ResourceId::with_identity("_virtual", "c"),
            constraint_id: ModuleConstraintId::argument_validation("name", 0),
            diagnostic: ModuleConstraintDiagnostic {
                module: "mod".to_string(),
                instance: "c".to_string(),
                origin: ConstraintOrigin::Saved,
                call: ModuleConstraintCall::Named("c".to_string()),
                kind: ModuleConstraintKind::ArgumentValidation {
                    argument: "name".to_string(),
                },
                arguments: vec!["name".to_string()],
                message: "bad name".to_string(),
                actuals: actuals.clone(),
                detail: Some(detail.clone()),
            },
        };
        let plan_error = PlanErrorKind::ModuleConstraint(ModuleConstraintDiagnostic {
            module: "mod".to_string(),
            instance: "c".to_string(),
            origin: ConstraintOrigin::Saved,
            call: ModuleConstraintCall::Named("c".to_string()),
            kind: ModuleConstraintKind::ArgumentValidation {
                argument: "name".to_string(),
            },
            arguments: vec!["name".to_string()],
            message: "bad name".to_string(),
            actuals,
            detail: Some(detail.clone()),
        });
        let expected =
            format!("module 'mod' (call 'c'): argument 'name': bad name (got 'abc'); {detail}");
        let plan_error = crate::plan::PlanError::new(
            crate::resource::ResourceId::with_identity("_virtual", "c"),
            plan_error,
        );

        assert_eq!(resolver_error.to_string(), expected);
        assert_eq!(apply_failure.to_string(), expected);
        assert_eq!(
            plan_error
                .display_with_project_dir(std::path::Path::new("."))
                .to_string(),
            expected
        );
    }
}
