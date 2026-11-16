//! `env(name)` built-in function

use crate::EnvironmentSnapshot;
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value, ValueString};

use super::value_type_name;

/// `env(name)` - Read an environment variable.
///
/// - First argument: name of the environment variable (string)
/// - Returns: An ordinary string that can appear in diagnostics and plan output
/// - Errors if the variable is not set or the argument is not a string
/// - Use `secret(env("NAME"))` for a value that must stay hidden
///
/// Examples:
/// ```text
/// env("HOME")                    // => "/home/user"
/// secret(env("DB_PASSWORD"))      // => (secret)
/// ```
pub(crate) fn builtin_env<B: BindingAddress>(
    args: &[crate::resource::knowledge::KnownValue<'_, NoPlaceholder, B>],
    environment: &EnvironmentSnapshot,
) -> Result<Value<NoPlaceholder, B>, String> {
    let values = args
        .iter()
        .map(|value| value.to_value())
        .collect::<Vec<_>>();
    let args = values.as_slice();
    let name = parse_env_args(args)?;
    let value = environment
        .get(name.expose_plaintext())
        .and_then(|value| value.to_str())
        .map(|s| Value::Concrete(ConcreteValue::String(s.into())))
        .ok_or_else(|| {
            format!(
                "environment variable '{}' is not set",
                name.expose_plaintext()
            )
        })?;
    Ok(value)
}

fn parse_env_args<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<&ValueString, String> {
    if args.len() != 1 {
        return Err(format!(
            "env() expects 1 argument (variable name), got {}",
            args.len()
        ));
    }

    match &args[0] {
        Value::Concrete(ConcreteValue::String(name)) => Ok(name),
        other => Err(format!(
            "env() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::ValueDescription;
    use crate::builtins::evaluate_builtin_with_config_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
    use crate::value::{format_value, value_to_json};
    use crate::{EnvironmentSnapshot, parser::ProviderContext};

    #[test]
    fn env_dispatch_uses_the_supplied_snapshot() {
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            "VARIABLE".to_string(),
            "captured".into(),
        )]));
        let args = [Value::Concrete(ConcreteValue::String("VARIABLE".into()))];
        assert_eq!(
            crate::builtins::evaluate_builtin_to_value(
                "env",
                &(args)
                    .iter()
                    .map(|value| value
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .unwrap()
                        .into_known()
                        .unwrap())
                    .collect::<Vec<_>>(),
                &config
            )
            .unwrap(),
            Value::Concrete(ConcreteValue::String("captured".into())),
        );
        let invalid = crate::builtins::evaluate_builtin_to_value::<
            crate::resource::RelativeBindingName,
        >("env", &[], &config)
        .unwrap_err();
        assert!(invalid.to_string().contains("expects 1 argument"));
    }

    #[test]
    fn env_snapshots_are_isolated_across_threads() {
        std::thread::scope(|threads| {
            for expected in ["first", "second"] {
                threads.spawn(move || {
                    let config = ProviderContext::new(EnvironmentSnapshot::new([(
                        "SHARED_NAME".to_string(),
                        expected.into(),
                    )]));
                    let args = [Value::Concrete(ConcreteValue::String("SHARED_NAME".into()))];
                    let value = evaluate_builtin("env", &args, &config).unwrap();
                    assert_eq!(
                        value,
                        Value::Concrete(ConcreteValue::String(expected.into()))
                    );
                });
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn env_non_utf8_value_uses_unset_error_and_protects_secret_names() {
        use std::os::unix::ffi::OsStringExt;
        let name = "NON_UTF8";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            name.to_string(),
            std::ffi::OsString::from_vec(vec![0xff]),
        )]));
        let args = [Value::Concrete(ConcreteValue::String(name.into()))];
        assert_eq!(
            evaluate_builtin("env", &args, &config).unwrap_err(),
            "environment variable 'NON_UTF8' is not set"
        );
        let parsed = crate::parser::parse("let name = secret('NON_UTF8')", &config).unwrap();
        let argument = parsed.variables["name"]
            .clone()
            .resolve_root(&crate::resource::RootScope::new())
            .into_expanded()
            .unwrap();
        assert_eq!(
            evaluate_builtin("env", &[argument], &config).unwrap_err(),
            crate::resource::INSIDE_SECRET_MESSAGE
        );
    }

    #[test]
    fn env_reads_set_variable() {
        let var_name = "CARINA_TEST_ENV_READS_SET";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "hello_world".into(),
        )]));
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello_world".into()))
        );
        assert_eq!(
            ValueDescription::of(&result).to_string(),
            "string 'hello_world'"
        );
        assert_eq!(format_value(&result), "'hello_world'");
        assert_eq!(
            value_to_json(&result).unwrap(),
            serde_json::json!("hello_world")
        );
        assert!(
            matches!(&result, Value::Concrete(ConcreteValue::String(value)) if !value.is_diagnostic_sensitive())
        );
    }

    #[test]
    fn env_error_on_unset_variable() {
        let var_name = "CARINA_TEST_ENV_UNSET_12345";
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("is not set"));
    }

    #[test]
    fn env_error_on_non_string_arg() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn env_preserves_origin_main_secret_argument_behavior() {
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            "SECRET_NAME".to_string(),
            "captured".into(),
        )]));
        let secret = |value: Value| {
            Value::Deferred(crate::resource::ExpandedDeferredValue::Secret(
                Box::new(value).into(),
            ))
        };
        let name = Value::Concrete(ConcreteValue::String("SECRET_NAME".into()));
        let expected = secret(Value::Concrete(ConcreteValue::String("captured".into())));
        for argument in [secret(name.clone()), secret(secret(name))] {
            assert_eq!(
                evaluate_builtin("env", &[argument], &config).unwrap(),
                expected
            );
        }
        let missing = secret(Value::Concrete(ConcreteValue::String(
            "MISSING_SECRET_NAME".into(),
        )));
        assert_eq!(
            evaluate_builtin("env", &[missing], &config).unwrap_err(),
            crate::resource::INSIDE_SECRET_MESSAGE,
        );
        for (value, kind) in [
            (Value::Concrete(ConcreteValue::Int(42)), "Int"),
            (Value::Concrete(ConcreteValue::Bool(true)), "Bool"),
            (Value::Concrete(ConcreteValue::List(Vec::new())), "List"),
        ] {
            assert_eq!(
                evaluate_builtin("env", std::slice::from_ref(&value), &config).unwrap_err(),
                format!("env() argument must be a string, got {kind}"),
            );
            assert_eq!(
                evaluate_builtin("env", &[secret(value)], &config).unwrap_err(),
                crate::resource::INSIDE_SECRET_MESSAGE,
            );
        }
    }

    #[test]
    fn env_error_on_no_args() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args: Vec<Value> = vec![];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_error_on_too_many_args() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![
            Value::Concrete(ConcreteValue::String("A".into())),
            Value::Concrete(ConcreteValue::String("B".into())),
        ];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_reads_empty_value() {
        let var_name = "CARINA_TEST_ENV_EMPTY_VAL";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "".into(),
        )]));
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
    }

    #[test]
    fn env_diagnostic_consumers_show_public_strings() {
        let config = ProviderContext {
            decryptor: Some(std::sync::Arc::new(|ciphertext, key| {
                Err(format!("decrypt failed: {ciphertext}, key={key:?}"))
            })),
            ..ProviderContext::new(EnvironmentSnapshot::new([
                ("VALUE".to_string(), "env-diagnostic-value".into()),
                ("FIELD".to_string(), ".env-diagnostic-field".into()),
            ]))
        };
        let env = |variable: &str| {
            evaluate_builtin(
                "env",
                &[Value::Concrete(ConcreteValue::String(variable.into()))],
                &config,
            )
            .unwrap()
        };
        let value = env("VALUE");
        let mut actual = vec![("description", ValueDescription::of(&value).to_string())];
        let enumeration = crate::schema::ResourceSchema::new("EnvEnum")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "mode",
                    crate::schema::AttributeType::enum_(
                        crate::schema::TypeIdentity::bare("Mode"),
                        Some(vec!["allowed".into()]),
                        Vec::new(),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap();
        actual.push((
            "enum literal",
            enumeration
                .effective_attribute("mode")
                .unwrap()
                .field_type()
                .validate(&value)
                .unwrap_err()
                .to_string(),
        ));
        for (function, args) in [
            (
                "cidr_subnet",
                vec![
                    value.clone(),
                    Value::Concrete(ConcreteValue::Int(8)),
                    Value::Concrete(ConcreteValue::Int(0)),
                ],
            ),
            ("decrypt", vec![value.clone()]),
            (
                "decrypt key",
                vec![
                    Value::Concrete(ConcreteValue::String("ciphertext".into())),
                    value.clone(),
                ],
            ),
            ("env", vec![value.clone()]),
            (
                "map",
                vec![
                    env("FIELD"),
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::Map(indexmap::IndexMap::new()),
                    )])),
                ],
            ),
        ] {
            let builtin = if function == "decrypt key" {
                "decrypt"
            } else {
                function
            };
            actual.push((
                function,
                evaluate_builtin(builtin, &args, &config).unwrap_err(),
            ));
        }
        let inventory = crate::provider_text::RedactionSet::collect([&value]);
        actual.push((
            "provider text",
            crate::provider_text::ProviderDiagnostic::message(
                crate::provider_text::ProviderText::new("provider saw env-diagnostic-value"),
            )
            .with_secrets(&inventory)
            .to_string(),
        ));
        let Value::Concrete(ConcreteValue::String(text)) = &value else {
            panic!("env must return a string");
        };
        let strings = Value::Concrete(ConcreteValue::StringList(vec![text.clone()]));
        actual.push((
            "provider string-list text",
            crate::provider_text::ProviderDiagnostic::message(
                crate::provider_text::ProviderText::new("provider saw env-diagnostic-value"),
            )
            .with_secrets(&crate::provider_text::RedactionSet::collect([&strings]))
            .to_string(),
        ));
        for (label, source) in [
            (
                "provider version",
                "fn version() { env('VALUE') }\nprovider mock { version = version() }",
            ),
            (
                "for iterable",
                "let items = for v in env('VALUE') { mock.test.resource { name = v } }",
            ),
            (
                "missing binding",
                "fn key() { env('VALUE') }\nlet v = missing[key()]",
            ),
            (
                "reference subscript",
                "fn key() { env('VALUE') }\nlet v = missing.value[key()]",
            ),
            (
                "reference binding subscript",
                "fn key() { env('VALUE') }\nlet v = missing[key()].value",
            ),
            (
                "missing map key",
                "fn key() { env('VALUE') }\nlet v = { available = 1 }[key()]",
            ),
            (
                "function parameter",
                "fn f(x: Int) { x }\nlet v = f(env('VALUE'))",
            ),
            (
                "custom type validation",
                "fn f(x: Ipv4Address) { x }\nlet v = f(env('VALUE'))",
            ),
            ("if condition", "let v = if env('VALUE') { 1 } else { 2 }"),
        ] {
            let message = match crate::parser::parse(source, &config) {
                Err(error) => error.to_string(),
                Ok(parsed) => {
                    let value = parsed.variables["v"]
                        .clone()
                        .resolve_root(&crate::resource::RootScope::new())
                        .into_expanded()
                        .unwrap();
                    value_to_json(&value).unwrap_err().to_string()
                }
            };
            actual.push((label, message));
        }
        let expected = [
            ("description", "string 'env-diagnostic-value'"),
            ("enum literal", "Mode expects an enum identifier, got a string literal 'env-diagnostic-value'. Use one of: allowed"),
            ("cidr_subnet", "cidr_subnet() invalid CIDR format: 'env-diagnostic-value' (expected 'address/prefix')"),
            ("decrypt", "decrypt failed: env-diagnostic-value, key=None"),
            ("decrypt key", "decrypt failed: ciphertext, key=Some(\"env-diagnostic-value\")"),
            ("env", "environment variable 'env-diagnostic-value' is not set"),
            ("map", "map(): field 'env-diagnostic-field' not found in map element"),
            ("provider text", "provider saw env-diagnostic-value"),
            ("provider string-list text", "provider saw env-diagnostic-value"),
            ("provider version", "Invalid expression at line 2: Provider 'mock': Invalid version constraint 'env-diagnostic-value': unexpected character 'e' while parsing major version number"),
            ("for iterable", "Invalid expression at line 1: `for v in ...` — iterable is string 'env-diagnostic-value' (expected list)"),
            ("missing binding", "cannot serialize at JSON conversion: unresolved reference missing['env-diagnostic-value']"),
            ("reference subscript", "cannot serialize at JSON conversion: unresolved reference missing.value['env-diagnostic-value']"),
            ("reference binding subscript", "cannot serialize at JSON conversion: unresolved reference missing['env-diagnostic-value'].value"),
            ("missing map key", "Invalid expression at line 2: key 'env-diagnostic-value' not found in map; available keys: 'available'"),
            ("function parameter", "Invalid expression at line 2: function 'f': parameter 'x': expected Int, got string 'env-diagnostic-value'"),
            ("custom type validation", "Invalid expression at line 2: function 'f': parameter 'x': expected Ipv4Address, got string 'env-diagnostic-value': Validation failed: Invalid IPv4 address 'env-diagnostic-value': expected 4 octets"),
            ("if condition", "Invalid expression at line 1: if condition must be a Bool value, got: string 'env-diagnostic-value'"),
        ]
        .into_iter()
        .map(|(label, message)| (label, message.to_string()))
        .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn env_cidr_diagnostic_variants_are_public() {
        let cases = [
            (
                "env-cidr",
                8,
                0,
                "cidr_subnet() invalid CIDR format: 'env-cidr' (expected 'address/prefix')",
            ),
            (
                "bad/16",
                8,
                0,
                "cidr_subnet() invalid IP address 'bad': invalid IPv4 address syntax",
            ),
            (
                "10.0.0.0/bad",
                8,
                0,
                "cidr_subnet() invalid prefix length 'bad': invalid digit found in string",
            ),
            (
                "10.0.0.0/33",
                0,
                0,
                "cidr_subnet() prefix length 33 exceeds maximum of 32",
            ),
            (
                "10.0.0.0/16",
                -1,
                0,
                "cidr_subnet() newbits must be non-negative",
            ),
            (
                "10.0.0.0/16",
                8,
                -1,
                "cidr_subnet() netnum must be non-negative",
            ),
            (
                "10.0.0.0/24",
                9,
                0,
                "cidr_subnet() resulting prefix length exceeds maximum of 32",
            ),
            (
                "10.0.0.0/16",
                8,
                256,
                "cidr_subnet() netnum is out of range for the additional bits",
            ),
        ];
        let mut observed = Vec::new();
        let mut expected_errors = Vec::new();
        for (prefix, newbits, netnum, expected) in cases {
            let case = (prefix, newbits, netnum);
            let config = ProviderContext::new(EnvironmentSnapshot::new([(
                "PREFIX".to_string(),
                prefix.into(),
            )]));
            let prefix = evaluate_builtin(
                "env",
                &[Value::Concrete(ConcreteValue::String("PREFIX".into()))],
                &config,
            )
            .unwrap();
            let error = evaluate_builtin(
                "cidr_subnet",
                &[
                    prefix,
                    Value::Concrete(ConcreteValue::Int(newbits)),
                    Value::Concrete(ConcreteValue::Int(netnum)),
                ],
                &config,
            )
            .unwrap_err();
            observed.push((case, error));
            expected_errors.push((case, expected.to_string()));
        }
        assert_eq!(observed, expected_errors);
    }

    #[test]
    fn env_strings_stay_public_through_string_builtins() {
        let var_name = "CARINA_TEST_ENV_PUBLIC_STRING_BUILTINS";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "hunter2SECRET".into(),
        )]));
        let env_value = evaluate_builtin(
            "env",
            &[Value::Concrete(ConcreteValue::String(
                var_name.to_string().into(),
            ))],
            &config,
        )
        .unwrap();

        let upper = evaluate_builtin("upper", std::slice::from_ref(&env_value), &config).unwrap();
        assert_eq!(
            ValueDescription::of(&upper).to_string(),
            "string 'HUNTER2SECRET'"
        );

        let joined = evaluate_builtin(
            "join",
            &[
                Value::Concrete(ConcreteValue::String("-".into())),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("prefix".into())),
                    env_value.clone(),
                ])),
            ],
            &config,
        )
        .unwrap();
        assert_eq!(
            ValueDescription::of(&joined).to_string(),
            "string 'prefix-hunter2SECRET'"
        );

        let concatenated = evaluate_builtin(
            "concat",
            &[
                Value::Concrete(ConcreteValue::List(vec![env_value])),
                Value::Concrete(ConcreteValue::List(vec![])),
            ],
            &config,
        )
        .unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = concatenated else {
            panic!("concat() must return a list");
        };
        assert_eq!(
            ValueDescription::of(&items[0]).to_string(),
            "string 'hunter2SECRET'"
        );
    }
}
