//! `replace(search, replacement, string)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `replace(search, replacement, string)` - Replace all occurrences of a substring.
///
/// Follows F#/Haskell convention: data argument (string) is last,
/// so pipe form works naturally: `string |> replace(search, replacement)`.
///
/// - First argument: search substring (String)
/// - Second argument: replacement substring (String)
/// - Third argument: input string (String)
/// - Returns: String with all occurrences replaced
///
/// Examples:
/// ```text
/// replace("-", "_", "hello-world")  // => "hello_world"
/// "hello-world" |> replace("-", "_") // => "hello_world" (pipe form)
/// ```
pub(crate) fn builtin_replace(args: &[Value]) -> Result<Value, String> {
    if args.len() != 3 {
        return Err(format!(
            "replace() expects 3 arguments (search, replacement, string), got {}",
            args.len()
        ));
    }

    let search = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "replace() first argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let replacement = match &args[1] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "replace() second argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let input = match &args[2] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "replace() third argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    Ok(Value::String(input.replace(&search, &replacement)))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn replace_basic() {
        // replace(search, replacement, string)
        let args = vec![
            Value::String("-".to_string()),
            Value::String("_".to_string()),
            Value::String("hello-world".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("hello_world".to_string()));
    }

    #[test]
    fn replace_multiple_occurrences() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("_".to_string()),
            Value::String("a-b-c-d".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("a_b_c_d".to_string()));
    }

    #[test]
    fn replace_no_match() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("_".to_string()),
            Value::String("hello".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn replace_empty_search() {
        let args = vec![
            Value::String("".to_string()),
            Value::String("-".to_string()),
            Value::String("abc".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        // Rust's replace("", x) inserts x between every char and at boundaries
        assert_eq!(result, Value::String("-a-b-c-".to_string()));
    }

    #[test]
    fn replace_with_empty() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("".to_string()),
            Value::String("hello-world".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("helloworld".to_string()));
    }

    #[test]
    fn replace_multi_char() {
        let args = vec![
            Value::String("::".to_string()),
            Value::String(".".to_string()),
            Value::String("foo::bar::baz".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("foo.bar.baz".to_string()));
    }

    #[test]
    fn replace_empty_input() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("_".to_string()),
            Value::String("".to_string()),
        ];
        let result = evaluate_builtin("replace", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn replace_partial_application() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![
            Value::String("hello".to_string()),
            Value::String("-".to_string()),
        ];
        let result = evaluate_builtin_for_tests("replace", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn replace_non_string_first_arg() {
        let args = vec![
            Value::Int(1),
            Value::String("_".to_string()),
            Value::String("hello".to_string()),
        ];
        let result = evaluate_builtin("replace", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn replace_non_string_second_arg() {
        let args = vec![
            Value::String("-".to_string()),
            Value::Int(1),
            Value::String("hello".to_string()),
        ];
        let result = evaluate_builtin("replace", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }

    #[test]
    fn replace_non_string_third_arg() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("_".to_string()),
            Value::Int(1),
        ];
        let result = evaluate_builtin("replace", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("third argument must be a string")
        );
    }
}
