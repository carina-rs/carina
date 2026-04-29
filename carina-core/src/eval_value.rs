//! Evaluator-internal values.
//!
//! `Value` is the public, user-facing type that flows through `ParsedFile`,
//! `Resource.attributes`, plan display, state serialization, and the LSP.
//! Every variant of `Value` is something a `.crn` author can directly
//! observe in their configuration.
//!
//! `EvalValue` is the strictly internal type used while the evaluator is
//! reducing expressions. It adds one variant — `Closure` — to represent a
//! partially applied builtin or user function. Closures are produced when
//! a function receives fewer arguments than its arity, and are consumed
//! by the next pipe / call that finishes the application.
//!
//! The split is enforced by the type system: a `Closure` cannot appear
//! anywhere a `Value` is expected. The boundary is `EvalValue::into_value`,
//! which returns `Err(ClosureLeak)` if a closure tried to escape the
//! evaluator. Consumers that match on `Value` therefore never need a
//! `Closure` arm — the variant does not exist for them.
//!
//! See #2230 for the design discussion.

use crate::resource::Value;

/// Evaluator-internal value. Carries everything `Value` carries plus an
/// extra `Closure` variant for partial application.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EvalValue {
    /// A user-facing value. Anything reachable from `Value` is reachable
    /// here without loss.
    User(Value),

    /// A partially applied function. Lives strictly inside the evaluator.
    /// Constructed when `evaluate_builtin` or a user-function call sees
    /// fewer arguments than the function's arity; consumed by the next
    /// pipe or call that finishes the application.
    Closure {
        /// Original function name.
        name: String,
        /// Arguments already supplied, in order.
        captured_args: Vec<EvalValue>,
        /// How many more arguments the function still needs.
        remaining_arity: usize,
    },
}

/// Returned by `EvalValue::into_value` when a closure would otherwise
/// escape the evaluator. The caller should propagate this as a
/// configuration error: a closure that doesn't get applied means the
/// user wrote a partial application without finishing it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClosureLeak {
    pub name: String,
    pub remaining_arity: usize,
}

impl EvalValue {
    /// Wrap a user-facing `Value`. This is the cheap, infallible
    /// direction — every `Value` is a valid `EvalValue`.
    pub(crate) fn from_value(value: Value) -> Self {
        Self::User(value)
    }

    /// Build a closure value from its parts.
    pub(crate) fn closure(
        name: impl Into<String>,
        captured_args: Vec<EvalValue>,
        remaining_arity: usize,
    ) -> Self {
        Self::Closure {
            name: name.into(),
            captured_args,
            remaining_arity,
        }
    }

    /// True when this value is a `Closure`.
    pub(crate) fn is_closure(&self) -> bool {
        matches!(self, Self::Closure { .. })
    }

    /// Lower an `EvalValue` to a user-facing `Value`. This is the
    /// boundary where the evaluator hands a result off to a public
    /// consumer (parser-level let-binding storage, resolver output,
    /// builtin handler input, etc.). If a closure is still here, the
    /// caller wrote a partial application that never completed.
    pub(crate) fn into_value(self) -> Result<Value, ClosureLeak> {
        match self {
            Self::User(v) => Ok(v),
            Self::Closure {
                name,
                remaining_arity,
                ..
            } => Err(ClosureLeak {
                name,
                remaining_arity,
            }),
        }
    }
}

impl From<Value> for EvalValue {
    fn from(value: Value) -> Self {
        Self::User(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Value;

    #[test]
    fn user_round_trips() {
        let original = Value::String("hi".into());
        let eval = EvalValue::from_value(original.clone());
        assert_eq!(eval.into_value().unwrap(), original);
    }

    #[test]
    fn closure_into_value_returns_leak() {
        let eval = EvalValue::closure("join", vec![], 2);
        let err = eval.into_value().unwrap_err();
        assert_eq!(err.name, "join");
        assert_eq!(err.remaining_arity, 2);
    }

}
