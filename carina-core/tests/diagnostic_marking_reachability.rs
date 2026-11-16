//! Real DSL inputs used to audit the secret mechanism's diagnostic marking.

use carina_core::{
    EnvironmentSnapshot,
    parser::{ProviderContext, parse_file, resolve_resource_refs_with_config},
    resource::{DeferredValue, Value},
};

#[test]
fn real_secret_expression_shapes_preserve_diagnostic_boundaries() {
    let context = ProviderContext::new(EnvironmentSnapshot::new([(
        "PUBLIC_INPUT_NAME".to_string(),
        "CANARY_4409_SECRET".into(),
    )]));
    for (label, expression) in [
        ("literal", "secret('CANARY_4409_SECRET')"),
        ("nested", "secret(secret('CANARY_4409_SECRET'))"),
        ("upper", "upper(secret('CANARY_4409_SECRET'))"),
        ("lower", "lower(secret('CANARY_4409_SECRET'))"),
        ("trim", "trim(secret(' CANARY_4409_SECRET '))"),
        (
            "replace",
            "replace('unused', 'unused', secret('CANARY_4409_SECRET'))",
        ),
        ("interpolation", "\"${secret('CANARY_4409_SECRET')}\""),
        ("join", "join('', [secret('CANARY_4409_SECRET')])"),
        (
            "concat_left",
            "concat([secret('CANARY_4409_SECRET')], [])[0]",
        ),
        (
            "concat_right",
            "concat([], [secret('CANARY_4409_SECRET')])[0]",
        ),
        ("split", "split(' ', secret('CANARY_4409_SECRET'))[0]"),
        ("flatten", "flatten([secret(['CANARY_4409_SECRET'])])[0]"),
        ("list", "[secret('CANARY_4409_SECRET')][0]"),
        ("secret_list", "secret(['CANARY_4409_SECRET'])[0]"),
        ("map", "{ k = secret('CANARY_4409_SECRET') }.k"),
        ("secret_map", "secret({ k = 'CANARY_4409_SECRET' }).k"),
        (
            "lookup",
            "lookup(secret({ k = 'CANARY_4409_SECRET' }), 'k', 'public')",
        ),
        ("values", "values({ k = secret('CANARY_4409_SECRET') })[0]"),
        (
            "secret_values",
            "values(secret({ k = 'CANARY_4409_SECRET' }))[0]",
        ),
        (
            "map_accessor",
            "map(secret('.k'), [{ k = 'CANARY_4409_SECRET' }])[0]",
        ),
        (
            "map_element",
            "map('.k', [secret({ k = 'CANARY_4409_SECRET' })])[0]",
        ),
        ("function", "identity(secret('CANARY_4409_SECRET'))"),
        ("env_argument", "env(secret('PUBLIC_INPUT_NAME'))"),
        (
            "deferred_selector",
            "secret(missing.value['CANARY_4409_SECRET'])",
        ),
        (
            "deferred_access",
            "secret(lookup(missing.value, 'CANARY_4409_SECRET', 'public'))",
        ),
    ] {
        let source = format!("fn identity(x) {{ x }}\nlet result = {expression}\n");
        let result = parse_file(&source, std::path::Path::new("main.crn"), &context).and_then(
            |mut parsed| {
                resolve_resource_refs_with_config(&mut parsed, &context)?;
                Ok(parsed)
            },
        );
        match result {
            Ok(parsed) => {
                let value = &parsed.variables["result"];
                let diagnostic = format!("{value:?} {}", carina_core::ValueDescription::of(value));
                assert!(
                    !diagnostic
                        .to_ascii_lowercase()
                        .contains("canary_4409_secret"),
                    "REAL INPUT PLAINTEXT LEAK: {label}: {diagnostic}"
                );
                assert!(
                    matches!(value, Value::Deferred(DeferredValue::Secret(_))),
                    "{label}"
                );
                assert_eq!(diagnostic, "Deferred(Secret((secret))) secret", "{label}");
            }
            Err(error) => {
                let diagnostic = format!("{error:?} {error}");
                assert!(
                    !diagnostic
                        .to_ascii_lowercase()
                        .contains("canary_4409_secret"),
                    "REAL INPUT PLAINTEXT LEAK: {label}: {diagnostic}"
                );
                assert_eq!(label, "map_element");
                assert_eq!(
                    error.to_string(),
                    "main.crn:2:5: Invalid expression at line 2: map(): map() expects list of maps, got list of Secret"
                );
            }
        }
    }
}
