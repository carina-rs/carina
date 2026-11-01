//! `decrypt(ciphertext)` / `decrypt(ciphertext, key)` built-in function
//!
//! Decrypts ciphertext using a decryptor function injected via [`ProviderContext`].
//! The CLI wires in the concrete implementation (e.g., AWS KMS) via `ProviderContext::decryptor`
//! before parsing begins.

use crate::parser::ProviderContext;
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, Value, ValueString,
};

use super::value_type_name;

/// `decrypt(ciphertext)` or `decrypt(ciphertext, key)` - Decrypt an encrypted value.
///
/// This is the fallback entry point used by `evaluate_builtin` (no config).
/// It always returns an error since the decryptor is only available via `ProviderContext`.
/// Use `builtin_decrypt_with_config` for the config-aware version.
pub(crate) fn builtin_decrypt<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    // Validate arguments for better error messages
    let (_ciphertext, _key) = parse_decrypt_args(args)?;

    Err(
        "decrypt() requires a configured provider with encryption support. \
         Ensure a provider (e.g., awscc) is configured and credentials are available."
            .to_string(),
    )
}

/// `decrypt()` implementation that uses the decryptor from [`ProviderContext`].
pub(crate) fn builtin_decrypt_with_config<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
    config: &ProviderContext,
) -> Result<Value<NoPlaceholder, B>, String> {
    let (ciphertext, key) = parse_decrypt_args(args)?;
    let decryptor = config.decryptor.as_ref().ok_or_else(|| {
        "decrypt() requires a configured provider with encryption support. \
         Ensure a provider (e.g., awscc) is configured and credentials are available."
            .to_string()
    })?;
    let plaintext = decryptor(
        ciphertext.expose_plaintext(),
        key.map(ValueString::expose_plaintext),
    )
    .map_err(|error| {
        if ciphertext.is_diagnostic_sensitive()
            || key.is_some_and(ValueString::is_diagnostic_sensitive)
        {
            "decrypt() failed for a sensitive value".to_string()
        } else {
            error
        }
    })?;
    Ok(Value::Deferred(DeferredValue::Secret(Box::new(
        Value::Concrete(ConcreteValue::String(plaintext.into())),
    ))))
}

/// Parse and validate arguments for `decrypt()`.
///
/// Returns `(ciphertext, optional_key)` or an error.
fn parse_decrypt_args<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<(&ValueString, Option<&ValueString>), String> {
    if args.is_empty() || args.len() > 2 {
        return Err(format!(
            "decrypt() expects 1 or 2 arguments (ciphertext[, key]), got {}",
            args.len()
        ));
    }

    let ciphertext = match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "decrypt() first argument must be a string (ciphertext), got {}",
                value_type_name(other)
            ));
        }
    };

    let key = if args.len() == 2 {
        match &args[1] {
            Value::Concrete(ConcreteValue::String(s)) => Some(s),
            other => {
                return Err(format!(
                    "decrypt() second argument must be a string (key), got {}",
                    value_type_name(other)
                ));
            }
        }
    } else {
        None
    };

    Ok((ciphertext, key))
}

#[cfg(test)]
mod tests {

    use crate::builtins::evaluate_builtin_with_config_to_value as evaluate_builtin_with_config;
    use crate::parser::ProviderContext;
    use crate::resource::{
        ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
        ExpandedValue as Value,
    };

    use super::builtin_decrypt;

    fn mock_config(decrypt_fn: crate::parser::DecryptorFn) -> ProviderContext {
        ProviderContext {
            decryptor: Some(decrypt_fn),
            resource_types: Default::default(),
            ..ProviderContext::default()
        }
    }

    #[test]
    fn decrypt_with_mock_decryptor() {
        let config = mock_config(Box::new(|ciphertext, _key| {
            Ok(format!("decrypted:{ciphertext}"))
        }));

        let args = vec![Value::Concrete(ConcreteValue::String("AQICAHh".into()))];
        let result = evaluate_builtin_with_config("decrypt", &args, &config).unwrap();
        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("decrypted:AQICAHh".into())
            ))))
        );
    }

    #[test]
    fn decrypt_with_key_argument() {
        let config = mock_config(Box::new(|ciphertext, key| {
            let key_str = key.unwrap_or("none");
            Ok(format!("decrypted:{ciphertext}:key={key_str}"))
        }));

        let args = vec![
            Value::Concrete(ConcreteValue::String("AQICAHh".into())),
            Value::Concrete(ConcreteValue::String("alias/my-key".into())),
        ];
        let result = evaluate_builtin_with_config("decrypt", &args, &config).unwrap();
        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("decrypted:AQICAHh:key=alias/my-key".into())
            ))))
        );
    }

    #[test]
    fn decrypt_without_decryptor_returns_error() {
        let config = ProviderContext::default();

        let args = vec![Value::Concrete(ConcreteValue::String("AQICAHh".into()))];
        let result = evaluate_builtin_with_config("decrypt", &args, &config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("requires a configured provider")
        );
    }

    #[test]
    fn decrypt_fallback_without_config_returns_error() {
        let args = vec![Value::Concrete(ConcreteValue::String("AQICAHh".into()))];
        let result = builtin_decrypt(&args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("requires a configured provider")
        );
    }

    #[test]
    fn decrypt_error_on_no_args() {
        let config = ProviderContext::default();
        let args: Vec<Value> = vec![];
        let result = evaluate_builtin_with_config("decrypt", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 or 2 arguments"));
    }

    #[test]
    fn decrypt_error_on_too_many_args() {
        let config = ProviderContext::default();
        let args = vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
            Value::Concrete(ConcreteValue::String("c".into())),
        ];
        let result = evaluate_builtin_with_config("decrypt", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 or 2 arguments"));
    }

    #[test]
    fn decrypt_error_on_non_string_ciphertext() {
        let config = ProviderContext::default();
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin_with_config("decrypt", &args, &config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn decrypt_error_on_non_string_key() {
        let config = ProviderContext::default();
        let args = vec![
            Value::Concrete(ConcreteValue::String("cipher".into())),
            Value::Concrete(ConcreteValue::Int(42)),
        ];
        let result = evaluate_builtin_with_config("decrypt", &args, &config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }

    #[test]
    fn secret_decrypt_composition() {
        let config = mock_config(Box::new(|_ciphertext, _key| {
            Ok("my-secret-password".to_string())
        }));

        // decrypt() is already secret; wrapping it again is idempotent.
        let decrypted = evaluate_builtin_with_config(
            "decrypt",
            &[Value::Concrete(ConcreteValue::String("cipher".into()))],
            &config,
        )
        .unwrap();
        let secret =
            evaluate_builtin_with_config("secret", std::slice::from_ref(&decrypted), &config)
                .unwrap();
        assert_eq!(secret, decrypted);
        assert_eq!(
            secret,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-secret-password".into())
            ))))
        );
    }
}
