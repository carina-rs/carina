//! Value descriptions shared by type-mismatch diagnostics.

use std::fmt;

use crate::resource::{ConcreteValue, DeferredValue, Value};

/// Value-derived description used in type-mismatch diagnostics.
///
/// Descriptions built with `of` retain the literal of concrete scalar values
/// except [`ConcreteValue::Duration`]. Duration, structural, and deferred
/// values retain only their shape, which keeps their diagnostics concise and
/// prevents deferred secrets from exposing their contents.
///
/// Descriptions built with `containing` prefix an inner description with the
/// outer value's shape and the word `containing`.
#[derive(Debug)]
pub struct ValueDescription(String);

impl ValueDescription {
    /// Describe `value` for a type-mismatch diagnostic.
    pub(crate) fn of(value: &Value) -> Self {
        Self(match value {
            Value::Concrete(ConcreteValue::String(value)) => format!("string {value:?}"),
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

    /// Describe `outer` as a container holding an offending inner value.
    pub(crate) fn containing(outer: &Value, inner: Self) -> Self {
        Self(format!("{} containing {inner}", Self::of(outer)))
    }
}

impl fmt::Display for ValueDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
