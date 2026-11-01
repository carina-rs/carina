//! Shared inspection for argument-dependent `if` templates.

use std::collections::HashSet;

use crate::resource::{
    ApplyTimeUnknown, BindingAddress, ConcreteValue, DeferredValue, LoopSlot, NoPlaceholder,
    ParserPlaceholder, PlaceholderKind, Value,
};

pub(super) trait ModuleIfAuditPlaceholder: PlaceholderKind {
    fn contains_module_if(&self) -> bool;
}
impl ModuleIfAuditPlaceholder for ParserPlaceholder {
    fn contains_module_if(&self) -> bool {
        match self {
            Self::FnParam { name: _ }
            | Self::ModuleArgument {
                name: _,
                diagnostic: _,
            }
            | Self::FnLocal { name: _ }
            | Self::Loop(LoopSlot::Key)
            | Self::Loop(LoopSlot::Index)
            | Self::Loop(LoopSlot::Value)
            | Self::EmptyInterpolation { source: _ } => false,
        }
    }
}

impl ModuleIfAuditPlaceholder for NoPlaceholder {
    fn contains_module_if(&self) -> bool {
        match *self {}
    }
}

impl ModuleIfAuditPlaceholder for LoopSlot {
    fn contains_module_if(&self) -> bool {
        match self {
            Self::Key | Self::Index | Self::Value => false,
        }
    }
}

/// Return whether a value still contains a parser-only module `if`.
///
/// The publication gate uses this after expansion. The traversal is
/// exhaustive so adding a value shape requires an explicit classification.
pub(super) fn value_contains_module_if<P: ModuleIfAuditPlaceholder, B: BindingAddress>(
    value: &Value<P, B>,
) -> bool {
    value_contains_module_if_inner(value, &mut HashSet::new(), &mut HashSet::new())
}

fn value_contains_module_if_inner<P: ModuleIfAuditPlaceholder, B: BindingAddress>(
    value: &Value<P, B>,
    functions: &mut HashSet<usize>,
    function_scopes: &mut HashSet<usize>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => true,
        Value::Concrete(ConcreteValue::List(values)) => values
            .iter()
            .any(|value| value_contains_module_if_inner(value, functions, function_scopes)),
        Value::Concrete(ConcreteValue::Map(values)) => values
            .values()
            .any(|value| value_contains_module_if_inner(value, functions, function_scopes)),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            parts.iter().any(|part| match part {
                crate::resource::InterpolationPart::Literal(_literal) => false,
                crate::resource::InterpolationPart::Expr(value) => {
                    value_contains_module_if_inner(value, functions, function_scopes)
                }
            })
        }
        Value::Deferred(
            DeferredValue::FunctionCall {
                name: _,
                args,
                source_line: _,
            }
            | DeferredValue::Unavailable {
                name: _,
                args,
                reason: _,
                source_line: _,
            },
        ) => args.iter().any(|argument| {
            argument
                .try_visit_values(&mut |value| {
                    if value_contains_module_if_inner(value, functions, function_scopes) {
                        Err(())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: scope,
            args,
            source_line: _,
        }) => {
            if args.iter().any(|argument| {
                argument
                    .try_visit_values(&mut |value| {
                        if value_contains_module_if_inner(value, functions, function_scopes) {
                            Err(())
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
            }) {
                return true;
            }
            let function_id = std::sync::Arc::as_ptr(function) as usize;
            if functions.insert(function_id)
                && user_function_contains_module_if(function, functions, function_scopes)
            {
                return true;
            }
            let scope_id = std::sync::Arc::as_ptr(scope) as usize;
            function_scopes.insert(scope_id)
                && scope.values().any(|function| {
                    user_function_contains_module_if(function, functions, function_scopes)
                })
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            value_contains_module_if_inner(access.base(), functions, function_scopes)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            value_contains_module_if_inner(inner, functions, function_scopes)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_contains_module_if_inner(value, functions, function_scopes)
                || value_contains_module_if_inner(default, functions, function_scopes)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            value_contains_module_if_inner(expression.lhs(), functions, function_scopes)
                || value_contains_module_if_inner(expression.rhs(), functions, function_scopes)
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            ,
        ) => false,
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            unknown_reason_contains_module_if(reason)
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            placeholder.contains_module_if()
        }
    }
}

fn unknown_reason_contains_module_if<B: BindingAddress>(reason: &ApplyTimeUnknown<B>) -> bool {
    match reason {
        ApplyTimeUnknown::UpstreamExport { path: _ }
        | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
        | ApplyTimeUnknown::PendingReRead(_) => false,
    }
}

fn user_function_contains_module_if<B: BindingAddress>(
    function: &crate::parser::UserFunction<B>,
    functions: &mut HashSet<usize>,
    function_scopes: &mut HashSet<usize>,
) -> bool {
    function.params.iter().any(|parameter| {
        parameter
            .default
            .as_ref()
            .is_some_and(|value| value_contains_module_if_inner(value, functions, function_scopes))
    }) || function
        .local_lets
        .iter()
        .any(|(_, value, _)| value_contains_module_if_inner(value, functions, function_scopes))
        || value_contains_module_if_inner(&function.body.value, functions, function_scopes)
}
