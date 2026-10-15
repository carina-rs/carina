//! Value-versus-`TypeExpr` checking shared by expansion, validation, and diagnostics.

use std::fmt;

use crate::ValueDescription;
use crate::parser::{ProviderContext, TypeExpr, validate_custom_type};
use crate::resource::{ConcreteValue, DeferredValue, Value};

/// One step from the checked value's root to the mismatched leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExprValuePathSegment {
    ListIndex(usize),
    MapKey(String),
    StructField(String),
}

/// A typed value mismatch against a declared [`TypeExpr`].
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExprValueError {
    Mismatch {
        path: Vec<TypeExprValuePathSegment>,
        expected: Box<TypeExpr>,
        actual: Box<ValueDescription>,
        validator_message: Option<Box<str>>,
        hint: Option<&'static str>,
    },
    MissingStructField {
        path: Vec<TypeExprValuePathSegment>,
        field: String,
    },
    UnknownStructField {
        path: Vec<TypeExprValuePathSegment>,
        expected: Box<TypeExpr>,
        actual: Box<ValueDescription>,
        field: String,
    },
}

impl TypeExprValueError {
    fn prepend(mut self, segment: TypeExprValuePathSegment) -> Self {
        match &mut self {
            Self::Mismatch { path, .. }
            | Self::MissingStructField { path, .. }
            | Self::UnknownStructField { path, .. } => path.insert(0, segment),
        }
        self
    }
}

fn write_path(f: &mut fmt::Formatter<'_>, path: &[TypeExprValuePathSegment]) -> fmt::Result {
    if path.is_empty() {
        return Ok(());
    }
    f.write_str("at ")?;
    for (index, segment) in path.iter().enumerate() {
        if index > 0 {
            f.write_str(" -> ")?;
        }
        match segment {
            TypeExprValuePathSegment::ListIndex(index) => write!(f, "list element {index}")?,
            TypeExprValuePathSegment::MapKey(key) => write!(f, "map key '{key}'")?,
            TypeExprValuePathSegment::StructField(field) => write!(f, "struct field '{field}'")?,
        }
    }
    f.write_str(": ")
}

impl fmt::Display for TypeExprValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch {
                path,
                expected,
                actual,
                validator_message,
                hint,
            } => {
                write_path(f, path)?;
                write!(f, "expected {expected}, got {actual}")?;
                if let Some(message) = validator_message {
                    write!(f, ": {message}")?;
                }
                if let Some(hint) = hint {
                    write!(f, ". {hint}")?;
                }
                Ok(())
            }
            Self::MissingStructField { path, field } => {
                write_path(f, path)?;
                write!(f, "expected struct, missing field '{field}'.")
            }
            Self::UnknownStructField { path, field, .. } => {
                write_path(f, path)?;
                write!(f, "expected struct, unknown field '{field}'.")
            }
        }
    }
}

/// Check a value against a declared type.
///
/// Unresolved values defer until the binding/reference validation or
/// evaluation phase that has their types.
pub fn check_type_expr_value(
    expected: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), TypeExprValueError> {
    if matches!(expected, TypeExpr::Unknown) {
        return Ok(());
    }

    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. }) => match expected {
            TypeExpr::StringLiteral(_) => return Err(mismatch(expected, value)),
            TypeExpr::Union(_) => {}
            _ => return Ok(()),
        },
        Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => return Ok(()),
        Value::Deferred(DeferredValue::BindingRef { .. }) => return Ok(()),
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            return if accepts_interpolation(expected) {
                Ok(())
            } else {
                Err(mismatch(expected, value))
            };
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            return check_type_expr_value(expected, inner, config)
                .map_err(|_| mismatch(expected, value));
        }
        Value::Concrete(_) => {}
    }

    match expected {
        TypeExpr::String => match value {
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::CanonicalEnum(_)) => Ok(()),
            Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                if is_qualified_enum_identifier(identifier.as_str()) =>
            {
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Bool => match value {
            Value::Concrete(ConcreteValue::Bool(_)) => Ok(()),
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Int => match value {
            Value::Concrete(ConcreteValue::Int(_)) => Ok(()),
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Float => match value {
            Value::Concrete(ConcreteValue::Float(_) | ConcreteValue::Int(_)) => Ok(()),
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Duration => match value {
            Value::Concrete(ConcreteValue::Duration(_)) => Ok(()),
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Simple(name) => match value {
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_)) => {
                let identity =
                    crate::schema::TypeIdentity::bare(crate::parser::snake_to_pascal(name));
                validate_custom_type(&identity, value, config)
                    .map_err(|message| validation_mismatch(expected, value, message))
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::List(inner) => match value {
            Value::Concrete(ConcreteValue::List(items)) => {
                for (index, item) in items.iter().enumerate() {
                    check_type_expr_value(inner, item, config).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::ListIndex(index))
                    })?;
                }
                Ok(())
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                for (index, item) in items.iter().enumerate() {
                    let item = Value::Concrete(ConcreteValue::String(item.clone()));
                    check_type_expr_value(inner, &item, config).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::ListIndex(index))
                    })?;
                }
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Map(inner) => match value {
            Value::Concrete(ConcreteValue::Map(entries)) => {
                for (key, item) in entries {
                    check_type_expr_value(inner, item, config).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::MapKey(key.clone()))
                    })?;
                }
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) => match value {
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::CanonicalEnum(_)) => Ok(()),
            Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                if is_qualified_enum_identifier(identifier.as_str()) =>
            {
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::SchemaType {
            provider,
            path,
            type_name,
        } => match value {
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_)) => {
                let identity =
                    crate::schema::TypeIdentity::from_schema_type(provider, path, type_name);
                validate_custom_type(&identity, value, config)
                    .map_err(|message| validation_mismatch(expected, value, message))
            }
            Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
                let identity =
                    crate::schema::TypeIdentity::from_schema_type(provider, path, type_name);
                let validator_value = Value::Concrete(ConcreteValue::String(
                    canonical.api_value().to_string().into(),
                ));
                validate_custom_type(&identity, &validator_value, config)
                    .map_err(|message| validation_mismatch(expected, value, message))
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Struct { fields } => match value {
            Value::Concrete(ConcreteValue::Map(entries)) => {
                let mut unknown = entries
                    .keys()
                    .filter(|key| !fields.iter().any(|(name, _)| name == *key))
                    .collect::<Vec<_>>();
                unknown.sort();
                if let Some(field) = unknown.first() {
                    let actual = entries
                        .get(*field)
                        .expect("unknown field came from the value map");
                    return Err(TypeExprValueError::UnknownStructField {
                        path: Vec::new(),
                        expected: Box::new(expected.clone()),
                        actual: Box::new(ValueDescription::of(actual)),
                        field: (*field).clone(),
                    });
                }
                for (field, field_type) in fields {
                    let Some(field_value) = entries.get(field) else {
                        return Err(TypeExprValueError::MissingStructField {
                            path: Vec::new(),
                            field: field.clone(),
                        });
                    };
                    check_type_expr_value(field_type, field_value, config).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::StructField(field.clone()))
                    })?;
                }
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::StringLiteral(literal) => match value {
            Value::Concrete(ConcreteValue::String(actual)) if actual == literal => Ok(()),
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Union(members) => {
            if members
                .iter()
                .any(|member| check_type_expr_value(member, value, config).is_ok())
            {
                Ok(())
            } else {
                Err(mismatch(expected, value))
            }
        }
        TypeExpr::Unknown => Ok(()),
    }
}

fn mismatch(expected: &TypeExpr, value: &Value) -> TypeExprValueError {
    let hint = matches!(
        (expected, value),
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::String(_)))
    )
    .then_some("Use true or false.");
    TypeExprValueError::Mismatch {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        validator_message: None,
        hint,
    }
}

fn validation_mismatch(expected: &TypeExpr, value: &Value, message: String) -> TypeExprValueError {
    TypeExprValueError::Mismatch {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        validator_message: (!value.contains_diagnostic_sensitive_string())
            .then(|| message.into_boxed_str()),
        hint: None,
    }
}

fn accepts_interpolation(expected: &TypeExpr) -> bool {
    is_string_shaped(expected)
        || matches!(expected, TypeExpr::Union(members) if members.iter().any(accepts_interpolation))
}

fn is_string_shaped(type_expr: &TypeExpr) -> bool {
    matches!(
        type_expr,
        TypeExpr::String
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
    )
}

fn is_qualified_enum_identifier(identifier: &str) -> bool {
    identifier.contains('.')
}

/// Directional compatibility for a declared source flowing into a declared sink.
#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResourceTypePath;
    use crate::resource::{CanonicalEnumValue, InterpolationPart, UnknownReason};
    use crate::schema::TypeIdentity;

    fn check(expected: &TypeExpr, value: &Value) -> Result<(), TypeExprValueError> {
        check_type_expr_value(expected, value, &ProviderContext::default())
    }

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn all_type_shapes() -> Vec<TypeExpr> {
        vec![
            TypeExpr::String,
            TypeExpr::Bool,
            TypeExpr::Int,
            TypeExpr::Float,
            TypeExpr::Duration,
            TypeExpr::Simple("ipv4_address".to_string()),
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                provider: "aws".to_string(),
                path: "ec2".to_string(),
                type_name: "VpcId".to_string(),
            },
            TypeExpr::Struct {
                fields: vec![("name".to_string(), TypeExpr::String)],
            },
            TypeExpr::StringLiteral("dev".to_string()),
            TypeExpr::Union(vec![
                TypeExpr::StringLiteral("dev".to_string()),
                TypeExpr::StringLiteral("prod".to_string()),
            ]),
            TypeExpr::Unknown,
        ]
    }

    #[test]
    fn concrete_kind_matrix_is_strict_and_widens_int_to_float() {
        let cases = [
            (TypeExpr::String, string("value"), true),
            (
                TypeExpr::String,
                Value::Concrete(ConcreteValue::Int(1)),
                false,
            ),
            (
                TypeExpr::Bool,
                Value::Concrete(ConcreteValue::Bool(true)),
                true,
            ),
            (TypeExpr::Bool, string("true"), false),
            (TypeExpr::Int, Value::Concrete(ConcreteValue::Int(1)), true),
            (
                TypeExpr::Int,
                Value::Concrete(ConcreteValue::Float(1.0)),
                false,
            ),
            (
                TypeExpr::Float,
                Value::Concrete(ConcreteValue::Float(1.0)),
                true,
            ),
            (
                TypeExpr::Float,
                Value::Concrete(ConcreteValue::Int(1)),
                true,
            ),
            (
                TypeExpr::Duration,
                Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(1))),
                true,
            ),
            (TypeExpr::Duration, string("1s"), false),
        ];

        for (expected, value, accepted) in cases {
            assert_eq!(
                check(&expected, &value).is_ok(),
                accepted,
                "expected {expected}, value {value:?}"
            );
        }
    }

    #[test]
    fn literal_union_map_and_string_list_are_checked_recursively() {
        let environment = TypeExpr::Union(vec![
            TypeExpr::StringLiteral("dev".to_string()),
            TypeExpr::StringLiteral("stg".to_string()),
            TypeExpr::StringLiteral("prod".to_string()),
        ]);
        assert!(check(&environment, &string("dev")).is_ok());
        assert_eq!(
            check(&environment, &string("production"))
                .unwrap_err()
                .to_string(),
            "expected 'dev' | 'stg' | 'prod', got string 'production'"
        );

        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("primary".to_string(), string("dev")),
            ("secondary".to_string(), string("production")),
        ])));
        let error = check(&TypeExpr::Map(Box::new(environment.clone())), &map).unwrap_err();
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch { path, expected, .. }
                if path == [TypeExprValuePathSegment::MapKey("secondary".to_string())]
                    && *expected == environment
        ));

        let list = Value::Concrete(ConcreteValue::StringList(vec![
            "dev".into(),
            "production".into(),
        ]));
        let error = check(&TypeExpr::List(Box::new(environment.clone())), &list).unwrap_err();
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch { path, expected, .. }
                if path == [TypeExprValuePathSegment::ListIndex(1)]
                    && *expected == environment
        ));
    }

    #[test]
    fn unresolved_values_follow_the_shared_deferred_policy() {
        let resource_ref = Value::resource_ref("source", "value", Vec::new());
        let function = Value::Deferred(DeferredValue::FunctionCall {
            name: "identity".to_string(),
            args: Vec::new(),
        });
        let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        let untyped_binding = Value::Deferred(DeferredValue::BindingRef {
            binding: "outside".to_string(),
        });

        for expected in all_type_shapes() {
            for value in [&function, &unknown, &untyped_binding] {
                assert!(
                    check(&expected, value).is_ok(),
                    "{value:?} should defer for {expected}"
                );
            }
        }

        for expected in [
            TypeExpr::String,
            TypeExpr::Bool,
            TypeExpr::Int,
            TypeExpr::Float,
            TypeExpr::Duration,
            TypeExpr::Simple("ipv4_address".to_string()),
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                provider: "aws".to_string(),
                path: "ec2".to_string(),
                type_name: "VpcId".to_string(),
            },
            TypeExpr::Struct { fields: Vec::new() },
            TypeExpr::Unknown,
        ] {
            assert!(
                check(&expected, &resource_ref).is_ok(),
                "resource reference should defer for {expected}"
            );
        }
        assert!(check(&TypeExpr::StringLiteral("dev".to_string()), &resource_ref).is_err());
        assert!(
            check(
                &TypeExpr::Union(vec![
                    TypeExpr::StringLiteral("dev".to_string()),
                    TypeExpr::StringLiteral("prod".to_string()),
                ]),
                &resource_ref,
            )
            .is_err()
        );
        assert!(
            check(
                &TypeExpr::Union(vec![
                    TypeExpr::StringLiteral("dev".to_string()),
                    TypeExpr::String,
                ]),
                &resource_ref,
            )
            .is_ok()
        );

        let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("value".into()),
        ]));
        for expected in [
            TypeExpr::String,
            TypeExpr::Simple("ipv4_address".to_string()),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                provider: "aws".to_string(),
                path: "ec2".to_string(),
                type_name: "VpcId".to_string(),
            },
            TypeExpr::Union(vec![
                TypeExpr::StringLiteral("dev".to_string()),
                TypeExpr::String,
            ]),
        ] {
            assert!(check(&expected, &interpolation).is_ok(), "{expected}");
        }
        for expected in [
            TypeExpr::Bool,
            TypeExpr::Int,
            TypeExpr::Float,
            TypeExpr::Duration,
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::Struct { fields: Vec::new() },
            TypeExpr::StringLiteral("dev".to_string()),
            TypeExpr::Union(vec![
                TypeExpr::StringLiteral("dev".to_string()),
                TypeExpr::StringLiteral("prod".to_string()),
            ]),
        ] {
            assert!(check(&expected, &interpolation).is_err(), "{expected}");
        }
    }

    #[test]
    fn schema_and_reference_types_accept_string_values_and_schema_validates() {
        use crate::parser::ValidatorFn;

        let schema_type = TypeExpr::SchemaType {
            provider: "test".to_string(),
            path: "network".to_string(),
            type_name: "Id".to_string(),
        };
        let identity = TypeIdentity::from_schema_type("test", "network", "Id");
        let mut validators = std::collections::HashMap::<TypeIdentity, ValidatorFn>::new();
        validators.insert(
            identity.clone(),
            Box::new(|value| {
                value
                    .starts_with("net-")
                    .then_some(())
                    .ok_or_else(|| format!("invalid network id: {value}"))
            }),
        );
        let config = ProviderContext {
            validators,
            ..ProviderContext::default()
        };

        let invalid = string("wrong");
        let error = check_type_expr_value(&schema_type, &invalid, &config)
            .expect_err("SchemaType must run its provider validator");
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch {
                validator_message: Some(message),
                ..
            } if message.contains("invalid network id")
        ));
        let invalid_shorthand = Value::Concrete(ConcreteValue::enum_identifier("wrong"));
        assert!(matches!(
            check_type_expr_value(&schema_type, &invalid_shorthand, &config),
            Err(TypeExprValueError::Mismatch {
                validator_message: Some(message),
                ..
            }) if message.contains("invalid network id")
        ));

        let raw = Value::Concrete(ConcreteValue::enum_identifier("net-raw"));
        let qualified = Value::Concrete(ConcreteValue::enum_identifier("test.network.Id.net_raw"));
        let canonical = Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(identity, "net-canonical"),
        ));
        assert!(
            check_type_expr_value(&schema_type, &raw, &config).is_ok(),
            "raw enum shorthand should be validated by {schema_type}"
        );
        assert!(
            check_type_expr_value(&schema_type, &canonical, &config).is_ok(),
            "canonical enum should satisfy {schema_type}"
        );
        for expected in [
            TypeExpr::Ref(ResourceTypePath::new("test", "network")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("test", "network.Id")),
        ] {
            assert!(
                check_type_expr_value(&expected, &raw, &config).is_err(),
                "bare enum identifier must not satisfy {expected}"
            );
            assert!(
                check_type_expr_value(&expected, &qualified, &config).is_ok(),
                "qualified enum identifier should satisfy {expected}"
            );
            assert!(
                check_type_expr_value(&expected, &canonical, &config).is_ok(),
                "canonical enum should satisfy {expected}"
            );
        }
    }

    #[test]
    fn enum_kinds_and_custom_validators_keep_their_distinct_shapes() {
        let raw = Value::Concrete(ConcreteValue::enum_identifier("192.168.1.1"));
        let bare = Value::Concrete(ConcreteValue::enum_identifier("production"));
        let qualified =
            Value::Concrete(ConcreteValue::enum_identifier("aws.Environment.production"));
        let canonical = Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(TypeIdentity::bare("Address"), "192.168.1.1"),
        ));
        assert!(check(&TypeExpr::String, &bare).is_err());
        assert!(check(&TypeExpr::StringLiteral("production".to_string()), &bare,).is_err());
        assert!(check(&TypeExpr::String, &qualified).is_ok());
        assert!(check(&TypeExpr::String, &canonical).is_ok());
        assert!(check(&TypeExpr::Simple("ipv4_address".to_string()), &raw).is_ok());
        assert!(check(&TypeExpr::Simple("ipv4_address".to_string()), &canonical,).is_err());
    }

    #[test]
    fn nested_custom_validator_errors_carry_root_to_leaf_paths() {
        let invalid = string("not-an-address");
        let list = Value::Concrete(ConcreteValue::List(vec![
            string("192.0.2.1"),
            invalid.clone(),
        ]));
        let error = check(
            &TypeExpr::List(Box::new(TypeExpr::Simple("ipv4_address".to_string()))),
            &list,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch {
                path,
                validator_message: Some(_),
                ..
            } if path == [TypeExprValuePathSegment::ListIndex(1)]
        ));

        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "secondary".to_string(),
            invalid.clone(),
        )])));
        let error = check(
            &TypeExpr::Map(Box::new(TypeExpr::Simple("ipv4_address".to_string()))),
            &map,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch { path, .. }
                if path == [TypeExprValuePathSegment::MapKey("secondary".to_string())]
        ));

        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "address".to_string(),
            invalid,
        )])));
        let error = check(
            &TypeExpr::Struct {
                fields: vec![(
                    "address".to_string(),
                    TypeExpr::Simple("ipv4_address".to_string()),
                )],
            },
            &value,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TypeExprValueError::Mismatch { path, .. }
                if path == [TypeExprValuePathSegment::StructField("address".to_string())]
        ));
    }

    #[test]
    fn struct_shape_errors_are_typed_and_unknown_choice_is_stable() {
        let expected = TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("count".to_string(), TypeExpr::Int),
            ],
        };
        let missing = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            string("example"),
        )])));
        assert!(matches!(
            check(&expected, &missing),
            Err(TypeExprValueError::MissingStructField {
                field,
                ..
            }) if field == "count"
        ));

        let unknown = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("name".to_string(), string("example")),
            ("count".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("z_extra".to_string(), string("z")),
            ("a_extra".to_string(), string("a")),
        ])));
        assert!(matches!(
            check(&expected, &unknown),
            Err(TypeExprValueError::UnknownStructField { field, .. }) if field == "a_extra"
        ));

        let nested_expected = TypeExpr::Struct {
            fields: vec![("config".to_string(), expected)],
        };
        let nested_missing = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        )])));
        let error = check(&nested_expected, &nested_missing).unwrap_err();
        assert!(matches!(
            &error,
            TypeExprValueError::MissingStructField { path, field }
                if path == &[TypeExprValuePathSegment::StructField("config".to_string())]
                    && field == "name"
        ));
        assert_eq!(
            error.to_string(),
            "at struct field 'config': expected struct, missing field 'name'."
        );
    }

    #[test]
    fn secret_checks_inner_value_without_leaking_validator_text() {
        let plaintext = "secret-invalid-ip-address";
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(string(plaintext))));
        let error = check(&TypeExpr::Simple("ipv4_address".to_string()), &secret).unwrap_err();

        assert!(matches!(
            &error,
            TypeExprValueError::Mismatch {
                path,
                actual,
                validator_message: None,
                hint: None,
                ..
            } if path.is_empty() && actual.to_string() == "secret"
        ));
        assert!(!error.to_string().contains(plaintext));

        let bool_error = check(&TypeExpr::Bool, &secret).unwrap_err();
        assert!(matches!(
            bool_error,
            TypeExprValueError::Mismatch {
                path,
                actual,
                validator_message: None,
                hint: None,
                ..
            } if path.is_empty() && actual.to_string() == "secret"
        ));
    }

    #[test]
    fn secret_errors_stop_at_the_secret_boundary() {
        let secret_map = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "private_key".to_string(),
                string("not-a-bool"),
            )])),
        ))));
        let error = check(&TypeExpr::Map(Box::new(TypeExpr::Bool)), &secret_map).unwrap_err();
        assert!(matches!(
            &error,
            TypeExprValueError::Mismatch {
                path,
                actual,
                validator_message: None,
                hint: None,
                ..
            } if path.is_empty() && actual.to_string() == "secret"
        ));
        assert!(!error.to_string().contains("private_key"));

        let secret_struct = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::Map(IndexMap::new()),
        ))));
        assert!(matches!(
            check(
                &TypeExpr::Struct {
                    fields: vec![("password".to_string(), TypeExpr::String)],
                },
                &secret_struct,
            ),
            Err(TypeExprValueError::Mismatch {
                path,
                validator_message: None,
                hint: None,
                ..
            }) if path.is_empty()
        ));
    }
}
