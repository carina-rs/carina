//! Value descriptions shared by type-mismatch diagnostics.

use std::fmt;

use crate::parser::display_string_literal;
use crate::resource::{BindingAddress, ConcreteValue, DeferredValue, PlaceholderKind, Value};

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
    pub fn of<P: PlaceholderKind, B: BindingAddress>(value: &Value<P, B>) -> Self {
        Self(match value {
            Value::Deferred(DeferredValue::ModuleIf(_)) => "module conditional".to_string(),
            Value::Concrete(ConcreteValue::String(value)) => value
                .diagnostic_literal()
                .map(|value| format!("string {}", display_string_literal(value)))
                .unwrap_or_else(|| "string".to_string()),
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
            Value::Deferred(DeferredValue::Coalesce { .. }) => "coalesce expression".to_string(),
            Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
                "integer arithmetic expression".to_string()
            }
            Value::Deferred(DeferredValue::Interpolation(_)) => "interpolation".to_string(),
            Value::Deferred(
                DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
            ) => "function call".to_string(),
            Value::Deferred(DeferredValue::Unavailable { .. }) => {
                "unavailable function result".to_string()
            }
            Value::Deferred(DeferredValue::Access(_)) => "access expression".to_string(),
            Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => {
                "secret".to_string()
            }
            Value::Deferred(DeferredValue::Unknown(_)) => "unknown".to_string(),
            Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
                format!("placeholder ({placeholder})")
            }
        })
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
        let value: Value = Value::Concrete(ConcreteValue::String("production".into()));

        assert_eq!(
            ValueDescription::of(&value).to_string(),
            "string 'production'"
        );
    }

    #[test]
    fn typed_persisted_secret_is_masked() {
        let value: Value = Value::Deferred(crate::resource::DeferredValue::RedactedSecret {
            hash: crate::resource::RedactedSecretHash::generate(b"description-test"),
        });

        assert_eq!(ValueDescription::of(&value).to_string(), "secret");
    }
}
