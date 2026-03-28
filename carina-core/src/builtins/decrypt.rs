//! `decrypt(ciphertext)` / `decrypt(ciphertext, key)` built-in function
//!
//! Decrypts ciphertext using a provider-supplied decryptor registered at startup.
//! The decryptor is provider-agnostic: the CLI wires in the concrete implementation
//! (e.g., AWS KMS) before parsing begins.

use std::sync::Mutex;

use crate::resource::Value;

use super::value_type_name;

/// Signature for a decryptor function.
///
/// Takes `(ciphertext, optional_key)` and returns the decrypted plaintext or an error.
pub type DecryptFn = Box<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>;

static DECRYPTOR: Mutex<Option<DecryptFn>> = Mutex::new(None);

/// Register a provider-supplied decryptor function.
///
/// Must be called before any .crn file that uses `decrypt()` is parsed.
/// Replaces any previously registered decryptor.
pub fn register_decryptor(f: DecryptFn) {
    let mut guard = DECRYPTOR.lock().expect("DECRYPTOR mutex poisoned");
    *guard = Some(f);
}

/// Call the registered decryptor, or return an error if none is configured.
fn call_decryptor(ciphertext: &str, key: Option<&str>) -> Result<String, String> {
    let guard = DECRYPTOR.lock().expect("DECRYPTOR mutex poisoned");
    let decryptor = guard.as_ref().ok_or_else(|| {
        "decrypt() requires a configured provider with encryption support. \
         Ensure a provider (e.g., awscc) is configured and credentials are available."
            .to_string()
    })?;
    decryptor(ciphertext, key)
}

/// `decrypt(ciphertext)` or `decrypt(ciphertext, key)` - Decrypt an encrypted value.
///
/// - First argument: base64-encoded ciphertext (string)
/// - Second argument (optional): key identifier such as ARN or alias (string)
/// - Returns: decrypted plaintext as a string
///
/// Examples:
/// ```text
/// decrypt("AQICAHh...")                          // AWS KMS (key embedded in ciphertext)
/// decrypt("AQICAHh...", "alias/my-key")          // explicit key
/// secret(decrypt("AQICAHh..."))                  // combine with secret()
/// ```
pub(crate) fn builtin_decrypt(args: &[Value]) -> Result<Value, String> {
    if args.is_empty() || args.len() > 2 {
        return Err(format!(
            "decrypt() expects 1 or 2 arguments (ciphertext[, key]), got {}",
            args.len()
        ));
    }

    let ciphertext = match &args[0] {
        Value::String(s) => s,
        other => {
            return Err(format!(
                "decrypt() first argument must be a string (ciphertext), got {}",
                value_type_name(other)
            ));
        }
    };

    let key = if args.len() == 2 {
        match &args[1] {
            Value::String(s) => Some(s.as_str()),
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

    let plaintext = call_decryptor(ciphertext, key)?;
    Ok(Value::String(plaintext))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    use super::{DECRYPTOR, register_decryptor};

    /// Serialize all decrypt tests that touch the global DECRYPTOR state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_decryptor() {
        let mut guard = DECRYPTOR.lock().expect("DECRYPTOR mutex poisoned");
        *guard = None;
    }

    #[test]
    fn decrypt_with_mock_decryptor() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_decryptor();
        register_decryptor(Box::new(|ciphertext, _key| {
            Ok(format!("decrypted:{ciphertext}"))
        }));

        let args = vec![Value::String("AQICAHh".to_string())];
        let result = evaluate_builtin("decrypt", &args).unwrap();
        assert_eq!(result, Value::String("decrypted:AQICAHh".to_string()));

        reset_decryptor();
    }

    #[test]
    fn decrypt_with_key_argument() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_decryptor();
        register_decryptor(Box::new(|ciphertext, key| {
            let key_str = key.unwrap_or("none");
            Ok(format!("decrypted:{ciphertext}:key={key_str}"))
        }));

        let args = vec![
            Value::String("AQICAHh".to_string()),
            Value::String("alias/my-key".to_string()),
        ];
        let result = evaluate_builtin("decrypt", &args).unwrap();
        assert_eq!(
            result,
            Value::String("decrypted:AQICAHh:key=alias/my-key".to_string())
        );

        reset_decryptor();
    }

    #[test]
    fn decrypt_without_decryptor_returns_error() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_decryptor();

        let args = vec![Value::String("AQICAHh".to_string())];
        let result = evaluate_builtin("decrypt", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("requires a configured provider")
        );
    }

    #[test]
    fn decrypt_error_on_no_args() {
        let args = vec![];
        let result = evaluate_builtin("decrypt", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 or 2 arguments"));
    }

    #[test]
    fn decrypt_error_on_too_many_args() {
        let args = vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
            Value::String("c".to_string()),
        ];
        let result = evaluate_builtin("decrypt", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 or 2 arguments"));
    }

    #[test]
    fn decrypt_error_on_non_string_ciphertext() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("decrypt", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn decrypt_error_on_non_string_key() {
        let args = vec![Value::String("cipher".to_string()), Value::Int(42)];
        let result = evaluate_builtin("decrypt", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }

    #[test]
    fn secret_decrypt_composition() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_decryptor();
        register_decryptor(Box::new(|_ciphertext, _key| {
            Ok("my-secret-password".to_string())
        }));

        // decrypt() then wrap with secret()
        let decrypted =
            evaluate_builtin("decrypt", &[Value::String("cipher".to_string())]).unwrap();
        let secret = evaluate_builtin("secret", &[decrypted]).unwrap();
        assert_eq!(
            secret,
            Value::Secret(Box::new(Value::String("my-secret-password".to_string())))
        );

        reset_decryptor();
    }
}
