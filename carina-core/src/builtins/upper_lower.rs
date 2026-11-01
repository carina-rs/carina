//! `upper(string)` and `lower(string)` built-in functions

use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value, ValueString};

use super::value_type_name;

/// `upper(string)` - Convert a string to uppercase.
///
/// - Single argument: a string value
/// - Returns: String with all characters converted to uppercase
///
/// Examples:
/// ```text
/// upper("hello")  // => "HELLO"
/// ```
pub(crate) fn builtin_upper<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 1 {
        return Err(format!(
            "upper() expects 1 argument (string), got {}",
            args.len()
        ));
    }

    match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => Ok(Value::Concrete(ConcreteValue::String(
            ValueString::derived_from(s.expose_plaintext().to_uppercase(), [s]),
        ))),
        other => Err(format!(
            "upper() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

/// `lower(string)` - Convert a string to lowercase.
///
/// - Single argument: a string value
/// - Returns: String with all characters converted to lowercase
///
/// Examples:
/// ```text
/// lower("HELLO")  // => "hello"
/// ```
pub(crate) fn builtin_lower<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 1 {
        return Err(format!(
            "lower() expects 1 argument (string), got {}",
            args.len()
        ));
    }

    match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => Ok(Value::Concrete(ConcreteValue::String(
            ValueString::derived_from(s.expose_plaintext().to_lowercase(), [s]),
        ))),
        other => Err(format!(
            "lower() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn upper_basic() {
        let args = vec![Value::Concrete(ConcreteValue::String("hello".into()))];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("HELLO".into()))
        );
    }

    #[test]
    fn upper_already_uppercase() {
        let args = vec![Value::Concrete(ConcreteValue::String("HELLO".into()))];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("HELLO".into()))
        );
    }

    #[test]
    fn upper_mixed_case() {
        let args = vec![Value::Concrete(ConcreteValue::String("Hello World".into()))];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("HELLO WORLD".into()))
        );
    }

    #[test]
    fn upper_empty_string() {
        let args = vec![Value::Concrete(ConcreteValue::String("".into()))];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
    }

    #[test]
    fn upper_with_numbers_and_symbols() {
        let args = vec![Value::Concrete(ConcreteValue::String("abc-123_def".into()))];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("ABC-123_DEF".into()))
        );
    }

    #[test]
    fn upper_wrong_arg_count() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
        ];
        let result = evaluate_builtin("upper", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn upper_non_string_arg() {
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("upper", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn lower_basic() {
        let args = vec![Value::Concrete(ConcreteValue::String("HELLO".into()))];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello".into()))
        );
    }

    #[test]
    fn lower_already_lowercase() {
        let args = vec![Value::Concrete(ConcreteValue::String("hello".into()))];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello".into()))
        );
    }

    #[test]
    fn lower_mixed_case() {
        let args = vec![Value::Concrete(ConcreteValue::String("Hello World".into()))];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello world".into()))
        );
    }

    #[test]
    fn lower_empty_string() {
        let args = vec![Value::Concrete(ConcreteValue::String("".into()))];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
    }

    #[test]
    fn lower_with_numbers_and_symbols() {
        let args = vec![Value::Concrete(ConcreteValue::String("ABC-123_DEF".into()))];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("abc-123_def".into()))
        );
    }

    #[test]
    fn lower_wrong_arg_count() {
        let result: Result<Value, String> = evaluate_builtin("lower", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn lower_non_string_arg() {
        let args = vec![Value::Concrete(ConcreteValue::Bool(true))];
        let result = evaluate_builtin("lower", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }
}
