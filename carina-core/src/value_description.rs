//! Value descriptions shared by type-mismatch diagnostics.

use std::fmt;

use crate::parser::display_string_literal;
use crate::resource::{ConcreteValue, DeferredValue, Value};
use crate::value::SECRET_PREFIX;

/// Value-derived description used in type-mismatch diagnostics.
///
/// Descriptions built with `of` retain the literal of concrete scalar values
/// except [`ConcreteValue::Duration`]. Duration, structural, and deferred
/// values retain only their shape, which keeps their diagnostics concise and
/// prevents deferred secrets from exposing their contents.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueDescription(String);

impl ValueDescription {
    /// Describe `value` for a type-mismatch diagnostic.
    pub fn of(value: &Value) -> Self {
        Self(match value {
            Value::Concrete(ConcreteValue::String(value)) if value.starts_with(SECRET_PREFIX) => {
                "secret".to_string()
            }
            Value::Concrete(ConcreteValue::String(value)) => {
                format!("string {}", display_string_literal(value))
            }
            Value::Concrete(ConcreteValue::Int(value)) => format!("int ({value})"),
            Value::Concrete(ConcreteValue::Float(value)) => format!("float ({value:?})"),
            Value::Concrete(ConcreteValue::Bool(value)) => format!("bool ({value})"),
            Value::Concrete(ConcreteValue::Duration(_)) => "duration".to_string(),
            Value::Concrete(ConcreteValue::List(_))
            | Value::Concrete(ConcreteValue::StringList(_)) => "list".to_string(),
            Value::Concrete(ConcreteValue::Map(_)) => "map".to_string(),
            Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
                format!("enum identifier {value}")
            }
            Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                format!("canonical enum {value}")
            }
            Value::Deferred(DeferredValue::ResourceRef { .. }) => "resource reference".to_string(),
            Value::Deferred(DeferredValue::BindingRef { .. }) => "binding reference".to_string(),
            Value::Deferred(DeferredValue::Interpolation(_)) => "interpolation".to_string(),
            Value::Deferred(DeferredValue::FunctionCall { .. }) => "function call".to_string(),
            Value::Deferred(DeferredValue::Secret(_)) => "secret".to_string(),
            Value::Deferred(DeferredValue::Unknown(_)) => "unknown".to_string(),
        })
    }

    /// Describe only the runtime shape of a value.
    ///
    /// Function evaluation can turn `env()` / `decrypt()` results into an
    /// ordinary concrete string. Diagnostics at the next typed boundary must
    /// not mistake that generated value for an authored literal and print its
    /// contents.
    pub(crate) fn type_only(value: &Value) -> Self {
        Self(
            match value {
                Value::Concrete(ConcreteValue::String(_)) => "string",
                Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "enum identifier",
                Value::Concrete(ConcreteValue::CanonicalEnum(_)) => "canonical enum",
                Value::Concrete(ConcreteValue::Int(_)) => "int",
                Value::Concrete(ConcreteValue::Float(_)) => "float",
                Value::Concrete(ConcreteValue::Bool(_)) => "bool",
                Value::Concrete(ConcreteValue::Duration(_)) => "duration",
                Value::Concrete(ConcreteValue::List(_))
                | Value::Concrete(ConcreteValue::StringList(_)) => "list",
                Value::Concrete(ConcreteValue::Map(_)) => "map",
                Value::Deferred(DeferredValue::ResourceRef { .. }) => "resource reference",
                Value::Deferred(DeferredValue::BindingRef { .. }) => "binding reference",
                Value::Deferred(DeferredValue::Interpolation(_)) => "interpolation",
                Value::Deferred(DeferredValue::FunctionCall { .. }) => "function call",
                Value::Deferred(DeferredValue::Secret(_)) => "secret",
                Value::Deferred(DeferredValue::Unknown(_)) => "unknown",
            }
            .to_string(),
        )
    }
}

impl fmt::Display for ValueDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_value_uses_canonical_carina_literal() {
        let value = Value::Concrete(ConcreteValue::String("production".to_string()));

        assert_eq!(
            ValueDescription::of(&value).to_string(),
            "string 'production'"
        );
    }

    #[test]
    fn hashed_secret_string_is_masked() {
        let value = Value::Concrete(ConcreteValue::String(format!(
            "{}deadbeef",
            crate::value::SECRET_PREFIX
        )));

        assert_eq!(ValueDescription::of(&value).to_string(), "secret");
    }
}
