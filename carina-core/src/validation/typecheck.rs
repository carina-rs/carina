//! Value-versus-`TypeExpr` checking shared by expansion, validation, and diagnostics.

use std::fmt;

use crate::ValueDescription;
use crate::parser::{ProviderContext, TypeExpr, validate_custom_type, validate_schema_custom_type};
use crate::resource::{BindingAddress, ConcreteValue, DeferredValue, Value};

/// Static authority available while checking deferred computed values.
/// Callers without a binding/schema view must say so explicitly rather than
/// passing empty registries that look authoritative.
pub enum TypecheckInference<'a, B: BindingAddress> {
    Available {
        bindings: &'a super::inference::InferenceBindings<B>,
        schemas: &'a crate::schema::SchemaRegistry,
    },
    Unavailable,
}

impl<B: BindingAddress> Copy for TypecheckInference<'_, B> {}

impl<B: BindingAddress> Clone for TypecheckInference<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

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
    InvalidAccess {
        message: String,
    },
    CustomTypeValidation {
        path: Vec<TypeExprValuePathSegment>,
        expected: Box<TypeExpr>,
        actual: Box<ValueDescription>,
        error: Box<crate::parser::CustomTypeValidationError>,
    },
}

impl TypeExprValueError {
    fn prepend(mut self, segment: TypeExprValuePathSegment) -> Self {
        match &mut self {
            Self::Mismatch { path, .. }
            | Self::MissingStructField { path, .. }
            | Self::UnknownStructField { path, .. }
            | Self::CustomTypeValidation { path, .. } => path.insert(0, segment),
            Self::InvalidAccess { .. } => {}
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
            Self::InvalidAccess { message } => f.write_str(message),
            Self::CustomTypeValidation {
                path,
                expected,
                actual,
                error,
            } => {
                write_path(f, path)?;
                write!(f, "expected {expected}, got {actual}: {error}")
            }
        }
    }
}

/// Check a value against a declared type.
///
/// Unresolved values defer until the binding/reference validation or
/// evaluation phase that has their types.
pub fn check_type_expr_value<B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<B>,
    config: &ProviderContext,
    inference: TypecheckInference<'_, B>,
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
        Value::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        )
        | Value::Deferred(DeferredValue::Unavailable { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => return Ok(()),
        Value::Deferred(DeferredValue::Access(_)) => {
            let inferred = match inference {
                TypecheckInference::Available { bindings, schemas } => {
                    super::inference::infer_type_from_value(value, bindings, schemas)
                }
                TypecheckInference::Unavailable => {
                    super::inference::infer_type_from_value_without_bindings(value)
                }
            };
            return match inferred {
                Ok(actual) if inferred_type_fits(expected, &actual) => Ok(()),
                Ok(_) => Err(mismatch(expected, value)),
                Err(error @ super::inference::InferenceError::InvalidAccess { .. })
                | Err(error @ super::inference::InferenceError::UnknownAttribute { .. }) => {
                    Err(TypeExprValueError::InvalidAccess {
                        message: error.to_string(),
                    })
                }
                Err(_) => Ok(()),
            };
        }
        Value::Deferred(DeferredValue::BindingRef { .. }) => return Ok(()),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            check_type_expr_value(expected, value, config, inference)?;
            return check_type_expr_value(expected, default, config, inference);
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            return if accepts_interpolation(expected) {
                Ok(())
            } else {
                Err(mismatch(expected, value))
            };
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            return check_type_expr_value(expected, inner, config, inference)
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
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_),
            ) => {
                let identity =
                    crate::schema::TypeIdentity::bare(crate::parser::snake_to_pascal(name));
                if identified_scalar_shape_is_supported(&identity, value, config, false) {
                    validate_custom_type(&identity, value, config)
                        .map_err(|error| validation_mismatch(expected, value, error.to_string()))
                } else {
                    Err(mismatch(expected, value))
                }
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::List(inner) => match value {
            Value::Concrete(ConcreteValue::List(items)) => {
                for (index, item) in items.iter().enumerate() {
                    check_type_expr_value(inner, item, config, inference).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::ListIndex(index))
                    })?;
                }
                Ok(())
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                for (index, item) in items.iter().enumerate() {
                    let item: Value<B> = Value::Concrete(ConcreteValue::String(item.clone()));
                    check_type_expr_value(inner, &item, config, inference).map_err(|error| {
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
                    check_type_expr_value(inner, item, config, inference).map_err(|error| {
                        error.prepend(TypeExprValuePathSegment::MapKey(key.clone()))
                    })?;
                }
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) => match value {
            // Ref annotations are also the established boundary type for
            // provider IDs. Real modules pass both literal IDs and resource
            // attributes such as `vpc.vpc_id`; the latter defers in the
            // ResourceRef arm above and a resolved ID arrives here as String.
            // Structural handles take a separate typed ModuleArgument /
            // EvalValue path and must not be conflated with this value case.
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::CanonicalEnum(_)) => Ok(()),
            Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                if is_qualified_enum_identifier(identifier.as_str()) =>
            {
                Ok(())
            }
            _ => Err(mismatch(expected, value)),
        },
        TypeExpr::SchemaType { owned } => match value {
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_),
            ) => {
                let identity = owned.identity();
                if identified_scalar_shape_is_supported(identity, value, config, true) {
                    validate_schema_custom_type(owned, value, config)
                        .map_err(|error| custom_type_validation_mismatch(expected, value, error))
                } else {
                    Err(mismatch(expected, value))
                }
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
                    check_type_expr_value(field_type, field_value, config, inference).map_err(
                        |error| error.prepend(TypeExprValuePathSegment::StructField(field.clone())),
                    )?;
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
                .any(|member| check_type_expr_value(member, value, config, inference).is_ok())
            {
                Ok(())
            } else {
                Err(mismatch(expected, value))
            }
        }
        TypeExpr::Unknown => Ok(()),
    }
}

fn mismatch<B: BindingAddress>(expected: &TypeExpr, value: &Value<B>) -> TypeExprValueError {
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

fn validation_mismatch<B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<B>,
    message: String,
) -> TypeExprValueError {
    TypeExprValueError::Mismatch {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        validator_message: (!value.contains_diagnostic_sensitive_string())
            .then(|| message.into_boxed_str()),
        hint: None,
    }
}

fn custom_type_validation_mismatch<B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<B>,
    error: crate::parser::CustomTypeValidationError,
) -> TypeExprValueError {
    TypeExprValueError::CustomTypeValidation {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        error: Box::new(error),
    }
}

fn identified_scalar_shape_is_supported<B: BindingAddress>(
    identity: &crate::schema::TypeIdentity,
    value: &Value<B>,
    config: &ProviderContext,
    owner_routed: bool,
) -> bool {
    match value {
        Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_)) => true,
        Value::Concrete(ConcreteValue::CanonicalEnum(_)) => {
            owner_routed
                || ![
                    crate::schema::RefinedScalarKind::String,
                    crate::schema::RefinedScalarKind::Int,
                    crate::schema::RefinedScalarKind::Float,
                ]
                .into_iter()
                .any(|kind| crate::schema::builtin_validator(kind, identity).is_some())
        }
        Value::Concrete(ConcreteValue::Int(_)) => {
            (!owner_routed
                && (crate::schema::builtin_validator(
                    crate::schema::RefinedScalarKind::Int,
                    identity,
                )
                .is_some()
                    || crate::schema::builtin_validator(
                        crate::schema::RefinedScalarKind::Float,
                        identity,
                    )
                    .is_some()))
                || (owner_routed && config.custom_types_loaded())
        }
        Value::Concrete(ConcreteValue::Float(_)) => {
            (!owner_routed
                && crate::schema::builtin_validator(
                    crate::schema::RefinedScalarKind::Float,
                    identity,
                )
                .is_some())
                || (owner_routed && config.custom_types_loaded())
        }
        Value::Concrete(
            ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::List(_)
            | ConcreteValue::StringList(_)
            | ConcreteValue::Map(_),
        )
        | Value::Deferred(_) => false,
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

fn inferred_type_fits(expected: &TypeExpr, actual: &TypeExpr) -> bool {
    if expected == actual || matches!(expected, TypeExpr::Unknown) {
        return true;
    }
    match (expected, actual) {
        (TypeExpr::Float, TypeExpr::Int) => true,
        (
            TypeExpr::SchemaType {
                owned: expected_owned,
            },
            TypeExpr::SchemaType {
                owned: actual_owned,
            },
        ) => expected_owned.identity() == actual_owned.identity(),
        (TypeExpr::Union(members), actual) => members
            .iter()
            .any(|member| inferred_type_fits(member, actual)),
        (TypeExpr::List(expected), TypeExpr::List(actual))
        | (TypeExpr::Map(expected), TypeExpr::Map(actual)) => inferred_type_fits(expected, actual),
        _ => false,
    }
}

/// Directional compatibility for a declared source flowing into a declared sink.
#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResourceTypePath;
    use crate::resource::{
        AuthoredConcreteValue as ConcreteValue, AuthoredDeferredValue as DeferredValue,
        AuthoredInterpolationPart as InterpolationPart, AuthoredUnknownReason as UnknownReason,
        AuthoredValue as Value, CanonicalEnumValue,
    };
    use crate::schema::TypeIdentity;

    fn check(expected: &TypeExpr, value: &Value) -> Result<(), TypeExprValueError> {
        check_type_expr_value(
            expected,
            value,
            &ProviderContext::default(),
            TypecheckInference::Unavailable,
        )
    }

    #[test]
    fn ref_parameter_accepts_literal_and_attribute_string_values() {
        let expected = TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.Vpc"));
        check(&expected, &string("vpc-123")).expect("literal provider ID");
        check(&expected, &Value::resource_ref("vpc", "vpc_id", Vec::new()))
            .expect("resource attribute provider ID");
    }

    fn check_with_config(
        expected: &TypeExpr,
        value: &Value,
        config: &ProviderContext,
    ) -> Result<(), TypeExprValueError> {
        check_type_expr_value(expected, value, config, TypecheckInference::Unavailable)
    }

    #[test]
    fn inferred_schema_type_compatibility_uses_identity_not_routing_owner_set() {
        use crate::parser::{NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner};

        let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let expected = TypeExpr::SchemaType {
            owned: OwnedTypeIdentity::with_owners(
                identity.clone(),
                NonEmptySchemaTypeOwners::try_from_iter([
                    SchemaTypeOwner::Provider("aws".to_string()),
                    SchemaTypeOwner::Provider("awscc".to_string()),
                ])
                .expect("two annotation owners are non-empty"),
            ),
        };
        let actual = TypeExpr::SchemaType {
            owned: OwnedTypeIdentity::provider("aws", identity),
        };

        assert!(
            inferred_type_fits(&expected, &actual),
            "routing provenance must not make one semantic type incompatible with itself"
        );
    }

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn split_result_element_access() -> Value {
        use crate::resource::{PathSegment, Subscript};

        crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    string("/").into(),
                    Value::resource_ref("role", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap()
    }

    #[test]
    fn postfix_access_uses_inferred_element_type_for_typecheck() {
        let value = split_result_element_access();

        check(&TypeExpr::String, &value).expect("an element of split() is assignable to String");
        check(&TypeExpr::Int, &value).expect_err("an element of split() is not assignable to Int");
    }

    #[test]
    fn postfix_access_on_known_type_rejects_incompatible_step() {
        use crate::resource::{PathSegment, Subscript};

        let list_with_string_key = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    string("/").into(),
                    Value::resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Str { key: "key".into() },
            },
            1,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        let field_on_string = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: vec![Value::resource_ref("q", "identifier", vec![]).into()],
                source_line: (0).into(),
            }),
            PathSegment::Field {
                name: "field".to_string(),
            },
            1,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();

        check(&TypeExpr::String, &list_with_string_key)
            .expect_err("a string key cannot select from list(String)");
        check(&TypeExpr::String, &field_on_string).expect_err("a String does not expose fields");
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
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                ),
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
            name: crate::builtins::BuiltinFunction::upper,
            args: Vec::new(),
            source_line: (0).into(),
        });
        let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        let untyped_binding = Value::Deferred(DeferredValue::BindingRef {
            binding: "outside".into(),
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
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                ),
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
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                ),
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
        let identity = TypeIdentity::from_schema_type("test", "network", "Id");
        let schema_type = TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider("test", identity.clone()),
        };
        let checked_identity = identity.clone();
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(move |target, value| {
                    if target.identity() != &checked_identity {
                        return Ok(());
                    }
                    let text = match value {
                        crate::resource::ExpandedValue::Concrete(
                            crate::resource::ExpandedConcreteValue::String(value),
                        ) => value.expose_plaintext(),
                        crate::resource::ExpandedValue::Concrete(
                            crate::resource::ExpandedConcreteValue::EnumIdentifier(value),
                        ) => value.as_str(),
                        crate::resource::ExpandedValue::Concrete(
                            crate::resource::ExpandedConcreteValue::CanonicalEnum(value),
                        ) => value.api_value(),
                        _ => {
                            return Err(crate::parser::fixture_custom_type_rejection(
                                target,
                                "Expected string",
                            ));
                        }
                    };
                    text.starts_with("net-").then_some(()).ok_or_else(|| {
                        crate::parser::fixture_custom_type_rejection(
                            target,
                            format!("invalid network id: {text}"),
                        )
                    })
                }),
            ),
            ..ProviderContext::default()
        };

        let invalid = string("wrong");
        let error = check_with_config(&schema_type, &invalid, &config)
            .expect_err("SchemaType must run its provider validator");
        assert!(matches!(
            error,
            TypeExprValueError::CustomTypeValidation {
                error,
                ..
            } if error.to_string().contains("invalid network id")
        ));
        let invalid_shorthand = Value::Concrete(ConcreteValue::enum_identifier("wrong"));
        assert!(matches!(
            check_with_config(&schema_type, &invalid_shorthand, &config),
            Err(TypeExprValueError::CustomTypeValidation {
                error,
                ..
            }) if error.to_string().contains("invalid network id")
        ));

        let raw = Value::Concrete(ConcreteValue::enum_identifier("net-raw"));
        let qualified = Value::Concrete(ConcreteValue::enum_identifier("test.network.Id.net_raw"));
        let canonical = Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(identity, "net-canonical"),
        ));
        assert!(
            check_with_config(&schema_type, &raw, &config).is_ok(),
            "raw enum shorthand should be validated by {schema_type}"
        );
        assert!(
            check_with_config(&schema_type, &canonical, &config).is_ok(),
            "canonical enum should satisfy {schema_type}"
        );
        for expected in [
            TypeExpr::Ref(ResourceTypePath::new("test", "network")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("test", "network.Id")),
        ] {
            assert!(
                check_with_config(&expected, &raw, &config).is_err(),
                "bare enum identifier must not satisfy {expected}"
            );
            assert!(
                check_with_config(&expected, &qualified, &config).is_ok(),
                "qualified enum identifier should satisfy {expected}"
            );
            assert!(
                check_with_config(&expected, &canonical, &config).is_ok(),
                "canonical enum should satisfy {expected}"
            );
        }
    }

    #[test]
    fn schema_enum_identifier_reaches_provider_validator_as_api_spelling() {
        use std::sync::{Arc, Mutex};

        let identity = TypeIdentity::new(Some("test"), ["widget"], "Mode");
        let declared_type = crate::schema::AttributeType::enum_(
            identity.clone(),
            Some(vec!["API_VALUE".to_string()]),
            vec![("API_VALUE".to_string(), "dsl_value".to_string())],
            None,
            None,
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_validator = Arc::clone(&seen);
        let declaration = declared_type.clone();
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(move |target, value| {
                    let canonical =
                        crate::provider::canonicalize_custom_type_value(&declaration, value)
                            .map_err(|error| {
                                crate::parser::fixture_invalid_custom_type_value(
                                    target,
                                    error.into_type_error(),
                                )
                            })?
                            .expect("concrete enum value must canonicalize");
                    seen_by_validator
                        .lock()
                        .unwrap()
                        .push(canonical.to_string());
                    Ok(())
                }),
            ),
            ..ProviderContext::default()
        };
        let schema_type = TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider("test", identity),
        };
        let value = Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.dsl_value"));

        check_with_config(&schema_type, &value, &config)
            .expect("the declared enum member must validate");

        assert_eq!(*seen.lock().unwrap(), ["API_VALUE"]);
    }

    #[test]
    fn positive_int_type_expression_validates_numeric_values() {
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(|target, value| {
                    if target.identity().kind == "PositiveInt"
                        && matches!(
                            value,
                            crate::resource::ExpandedValue::Concrete(
                                crate::resource::ExpandedConcreteValue::Int(value)
                            ) if *value > 0
                        )
                    {
                        Ok(())
                    } else {
                        Err(crate::parser::fixture_custom_type_rejection(
                            target,
                            "expected a positive integer",
                        ))
                    }
                }),
            ),
            ..ProviderContext::default()
        };

        for expected in [
            TypeExpr::Simple("positive_int".to_string()),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "test",
                    TypeIdentity::from_schema_type("test", "widget", "PositiveInt"),
                ),
            },
        ] {
            check_with_config(&expected, &Value::Concrete(ConcreteValue::Int(1)), &config)
                .expect("PositiveInt must accept 1 as an integer value");
            assert!(
                check_with_config(&expected, &Value::Concrete(ConcreteValue::Int(0)), &config,)
                    .is_err(),
                "PositiveInt must reject zero through {expected}"
            );
        }
    }

    #[test]
    fn native_refined_int_typecheck_preserves_the_integer_value() {
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

        fn schema(identity: TypeIdentity, accepted: i64) -> crate::schema::ValidatedResourceSchema {
            ResourceSchema::new(format!("test.number_{accepted}"))
                .attribute(
                    AttributeSchema::optional(
                        "value",
                        AttributeType::refined_int_with_validator(
                            identity,
                            None,
                            legacy_validator(move |value| match value {
                                crate::resource::ExpandedValue::Concrete(
                                    crate::resource::ExpandedConcreteValue::Int(value),
                                ) if *value == accepted => Ok(()),
                                crate::resource::ExpandedValue::Concrete(
                                    crate::resource::ExpandedConcreteValue::Int(value),
                                ) => Err(format!("expected {accepted}, got {value}")),
                                _ => Err("Expected integer".to_string()),
                            }),
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("test schema must validate")
        }

        let identity = TypeIdentity::from_schema_type("one", "number", "X");
        let schema = schema(identity.clone(), 5);
        let declaration = schema
            .parts()
            .attributes
            .get("value")
            .expect("value declaration")
            .attr_type()
            .clone();
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                std::collections::HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                    "one",
                    identity.clone(),
                )]),
                Box::new(move |target, value| {
                    declaration
                        .validate_identified_scalar_value(value)
                        .map_err(|error| {
                            crate::parser::fixture_invalid_custom_type_value(target, error)
                        })
                }),
            ),
            ..ProviderContext::default()
        };
        let ty = |provider: &str| TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider(provider, identity.clone()),
        };

        check_with_config(&ty("one"), &Value::Concrete(ConcreteValue::Int(5)), &config)
            .expect("owner one must run its typed validator");
        assert!(
            check_with_config(&ty("one"), &Value::Concrete(ConcreteValue::Int(7)), &config,)
                .is_err()
        );
    }

    #[test]
    fn enum_typed_module_and_function_string_reaches_validator_as_api_spelling() {
        use std::sync::{Arc, Mutex};

        let bare_identity = TypeIdentity::bare("Mode");
        let schema_identity = TypeIdentity::new(Some("test"), ["widget"], "Mode");
        let enum_type = |identity| {
            crate::schema::AttributeType::enum_(
                identity,
                Some(vec!["Enabled".to_string(), "Disabled".to_string()]),
                vec![("Enabled".to_string(), "enabled".to_string())],
                None,
                None,
            )
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_validator = Arc::clone(&seen);
        let bare_declaration = enum_type(bare_identity.clone());
        let schema_declaration = enum_type(schema_identity.clone());
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                std::collections::HashSet::from([
                    crate::parser::OwnedTypeIdentity::provider(
                        "bare-provider",
                        bare_identity.clone(),
                    ),
                    crate::parser::OwnedTypeIdentity::provider("test", schema_identity.clone()),
                ]),
                Box::new(move |target, value| {
                    let owned = match target {
                        crate::parser::CustomTypeValidationTarget::Annotation(owned) => owned,
                        crate::parser::CustomTypeValidationTarget::Declaration { .. } => {
                            return Err(crate::parser::fixture_custom_type_rejection(
                                target,
                                "expected an annotation target",
                            ));
                        }
                    };
                    let owner = owned
                        .provider_names()
                        .next()
                        .expect("fixture annotation has one owner");
                    let declaration = match owner {
                        "bare-provider" => &bare_declaration,
                        "test" => &schema_declaration,
                        owner => {
                            return Err(crate::parser::fixture_custom_type_rejection(
                                target,
                                format!("unexpected owner {owner:?}"),
                            ));
                        }
                    };
                    let canonical =
                        crate::provider::canonicalize_custom_type_value(declaration, value)
                            .map_err(|error| {
                                crate::parser::fixture_invalid_custom_type_value(
                                    target,
                                    error.into_type_error(),
                                )
                            })?
                            .expect("concrete enum value must canonicalize");
                    seen_by_validator.lock().unwrap().push((
                        owner.to_string(),
                        owned.identity().to_string(),
                        canonical.to_string(),
                    ));
                    Ok(())
                }),
            ),
            ..ProviderContext::default()
        };
        let value = Value::Concrete(ConcreteValue::String("enabled".into()));

        for expected in [
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider("bare-provider", bare_identity),
            },
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider("test", schema_identity.clone()),
            },
        ] {
            check_with_config(&expected, &value, &config)
                .expect("enum-typed string must resolve through state-text spelling");
        }

        assert_eq!(
            *seen.lock().unwrap(),
            [
                (
                    "bare-provider".to_string(),
                    "Mode".to_string(),
                    "Enabled".to_string(),
                ),
                (
                    "test".to_string(),
                    "test.widget.Mode".to_string(),
                    "Enabled".to_string(),
                ),
            ]
        );
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
