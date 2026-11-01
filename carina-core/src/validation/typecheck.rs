//! Value-versus-`TypeExpr` checking shared by expansion, validation, and diagnostics.

use std::fmt;

use crate::ValueDescription;
use crate::parser::{
    LoadedProviderContext, ProviderContext, TypeExpr, validate_builtin_custom_type,
    validate_schema_custom_type_with_context,
};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, PlaceholderKind,
    RelativeBindingName, Value,
};

/// Static authority available while checking deferred computed values.
/// Callers without a binding/schema view must say so explicitly rather than
/// passing empty registries that look authoritative.
pub enum TypecheckInference<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    Available {
        bindings: &'a super::inference::InferenceBindings<P, B>,
        schemas: &'a crate::schema::SchemaRegistry,
    },
    Unavailable,
}

impl<P: PlaceholderKind, B: BindingAddress> Copy for TypecheckInference<'_, P, B> {}

impl<P: PlaceholderKind, B: BindingAddress> Clone for TypecheckInference<'_, P, B> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Provider authority available to internal value-checking pipelines.
///
/// Construction is crate-private: loaded paths retain the opaque production
/// witness, while explicitly providerless paths use a recursive checker that
/// never invokes the provider custom-type resolver.
#[derive(Clone, Copy)]
pub(crate) enum TypecheckProviderContext<'a> {
    Loaded(&'a LoadedProviderContext),
    LoadedParserCatalog(&'a ProviderContext),
    Providerless(&'a ProviderContext),
}

impl<'a> TypecheckProviderContext<'a> {
    pub(crate) fn loaded(config: &'a LoadedProviderContext) -> Self {
        Self::Loaded(config)
    }

    pub(crate) fn providerless(config: &'a ProviderContext) -> Self {
        Self::Providerless(config)
    }

    /// The parser borrows the raw context inside its lexical scopes. Classify
    /// that authority once, before checking a parameter or return value.
    pub(crate) fn for_parser(config: &'a ProviderContext) -> Self {
        if config.custom_types_loaded() {
            Self::LoadedParserCatalog(config)
        } else {
            Self::Providerless(config)
        }
    }

    pub(crate) fn as_provider_context(self) -> &'a ProviderContext {
        match self {
            Self::Loaded(config) => config.as_provider_context(),
            Self::LoadedParserCatalog(config) | Self::Providerless(config) => config,
        }
    }

    pub(crate) fn loaded_context(self) -> Option<&'a LoadedProviderContext> {
        match self {
            Self::Loaded(config) => Some(config),
            Self::LoadedParserCatalog(_) | Self::Providerless(_) => None,
        }
    }

    pub(crate) fn check<P: PlaceholderKind, B: BindingAddress>(
        self,
        expected: &TypeExpr,
        value: &Value<P, B>,
        inference: TypecheckInference<'_, P, B>,
    ) -> Result<(), TypeExprValueError> {
        match self {
            Self::Loaded(config) => {
                check_type_expr_value(expected, value, config, inference).require_provider_schemas()
            }
            Self::Providerless(config) => {
                check_type_expr_value_providerless(expected, value, config, inference)
                    .allow_deferred()
            }
            Self::LoadedParserCatalog(_) => {
                check_type_expr_value_inner(expected, value, self, inference)
                    .require_provider_schemas()
            }
        }
    }

    pub(crate) fn requires_provider_schemas(self) -> bool {
        matches!(self, Self::Loaded(_) | Self::LoadedParserCatalog(_))
    }
}

/// The outcome of checking a value. Missing provider evidence is preserved
/// recursively until a caller chooses the policy for its validation phase.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum TypeExprValueCheck {
    Assignable,
    Mismatch(TypeExprValueError),
    Deferred(TypeExprValueDeferral),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeExprValueDeferral {
    pub path: Vec<TypeExprValuePathSegment>,
    pub expected: Box<TypeExpr>,
    pub actual: Box<ValueDescription>,
    pub reason: crate::parser::CustomTypeValidationError,
}

impl TypeExprValueCheck {
    /// Intermediate bootstrap/editor checks may retain unavailable evidence.
    /// A terminal providerless pipeline must settle its sites through
    /// [`super::deferred_types::DeferredProviderTypeChecks`].
    pub fn allow_deferred(self) -> Result<(), TypeExprValueError> {
        match self {
            Self::Assignable | Self::Deferred(_) => Ok(()),
            Self::Mismatch(error) => Err(error),
        }
    }

    /// Loaded validation must have provider evidence for custom-type checks.
    pub fn require_provider_schemas(self) -> Result<(), TypeExprValueError> {
        match self {
            Self::Assignable => Ok(()),
            Self::Mismatch(error) => Err(error),
            Self::Deferred(deferred) => Err(TypeExprValueError::CustomTypeValidation {
                path: deferred.path,
                expected: deferred.expected,
                actual: deferred.actual,
                error: Box::new(deferred.reason),
            }),
        }
    }

    fn from_result(result: Result<(), TypeExprValueError>) -> Self {
        match result {
            Ok(()) => Self::Assignable,
            Err(error) => Self::Mismatch(error),
        }
    }

    fn prepend(self, segment: TypeExprValuePathSegment) -> Self {
        match self {
            Self::Assignable => Self::Assignable,
            Self::Mismatch(error) => Self::Mismatch(error.prepend(segment)),
            Self::Deferred(mut deferred) => {
                deferred.path.insert(0, segment);
                Self::Deferred(deferred)
            }
        }
    }

    fn all(checks: impl IntoIterator<Item = Self>) -> Self {
        let mut result = Self::Assignable;
        for check in checks {
            match check {
                Self::Mismatch(_) => return check,
                Self::Deferred(_) => result = check,
                Self::Assignable => {}
            }
        }
        result
    }

    fn any(checks: impl IntoIterator<Item = Self>, mismatch: TypeExprValueError) -> Self {
        let mut result = Self::Mismatch(mismatch);
        for check in checks {
            match check {
                Self::Assignable => return Self::Assignable,
                Self::Deferred(_) => result = check,
                Self::Mismatch(_) => {}
            }
        }
        result
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
        error: crate::static_type::InferenceError,
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
            Self::InvalidAccess { error } => error.fmt(f),
            Self::CustomTypeValidation {
                path,
                expected,
                actual,
                error,
            } => {
                write_path(f, path)?;
                write!(f, "expected {expected}, got {actual}: {error}")?;
                if matches!(
                    error.as_ref(),
                    crate::parser::CustomTypeValidationError::ProvidersNotLoaded
                ) {
                    f.write_str("; value validation requires an owner-qualified schema type")?;
                }
                Ok(())
            }
        }
    }
}

/// Check a value against a declared type with loaded provider custom types.
///
/// Unresolved values defer until the binding/reference validation or
/// evaluation phase that has their types.
pub fn check_type_expr_value<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    config: &LoadedProviderContext,
    inference: TypecheckInference<'_, P, B>,
) -> TypeExprValueCheck {
    check_type_expr_value_inner_with_mode(
        expected,
        value,
        config.as_provider_context(),
        inference,
        ProviderCustomTypeValidation::Loaded,
    )
}

/// Check a value without loaded providers.
///
/// Structural checks and provider-independent leaves run normally. A
/// provider-owned custom-type leaf is accepted only after its scalar shape is
/// checked; the provider resolver is never called.
pub fn check_type_expr_value_providerless<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    config: &ProviderContext,
    inference: TypecheckInference<'_, P, B>,
) -> TypeExprValueCheck {
    check_type_expr_value_inner_with_mode(
        expected,
        value,
        config,
        inference,
        ProviderCustomTypeValidation::Providerless,
    )
}

#[derive(Clone, Copy)]
enum ProviderCustomTypeValidation {
    Loaded,
    Providerless,
}

fn check_type_expr_value_inner<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    config: TypecheckProviderContext<'_>,
    inference: TypecheckInference<'_, P, B>,
) -> TypeExprValueCheck {
    check_type_expr_value_inner_with_mode(
        expected,
        value,
        config.as_provider_context(),
        inference,
        if config.requires_provider_schemas() {
            ProviderCustomTypeValidation::Loaded
        } else {
            ProviderCustomTypeValidation::Providerless
        },
    )
}

fn deferred_provider_check<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
) -> TypeExprValueCheck {
    TypeExprValueCheck::Deferred(TypeExprValueDeferral {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        reason: crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
    })
}

fn check_type_expr_value_inner_with_mode<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    config: &ProviderContext,
    inference: TypecheckInference<'_, P, B>,
    provider_custom_types: ProviderCustomTypeValidation,
) -> TypeExprValueCheck {
    use TypeExprValueCheck::{Assignable, Mismatch};
    let check = |expected: &TypeExpr, value: &Value<P, B>| {
        check_type_expr_value_inner_with_mode(
            expected,
            value,
            config,
            inference,
            provider_custom_types,
        )
    };
    if let TypeExpr::UnloadedSchemaType(_) = expected {
        return match super::resolve_type_expr(expected, config) {
            Ok(TypeExpr::UnloadedSchemaType(_)) => {
                // Provider declarations can only refine String, Int or Float.
                // Missing schemas cannot make a concrete container or another
                // primitive into one of those scalar types.
                match value {
                    Value::Concrete(
                        ConcreteValue::String(_)
                        | ConcreteValue::Int(_)
                        | ConcreteValue::Float(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_),
                    ) => deferred_provider_check(expected, value),
                    Value::Concrete(_) => Mismatch(mismatch(expected, value)),
                    Value::Deferred(DeferredValue::Secret(inner)) => check(expected, inner),
                    Value::Deferred(DeferredValue::ModuleIf(deferred)) => TypeExprValueCheck::all(
                        deferred.values()[1..]
                            .iter()
                            .map(|value| check(expected, value)),
                    ),
                    Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                        TypeExprValueCheck::all(
                            [value.as_ref(), default.as_ref()]
                                .into_iter()
                                .map(|value| check(expected, value)),
                        )
                    }
                    Value::Deferred(_) => deferred_provider_check(expected, value),
                }
            }
            Ok(resolved) => check(&resolved, value),
            Err(error) => Mismatch(validation_mismatch(expected, value, error.to_string())),
        };
    }
    if let TypeExpr::Union(members) = expected {
        return TypeExprValueCheck::any(
            members.iter().map(|member| check(member, value)),
            mismatch(expected, value),
        );
    }
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            return TypeExprValueCheck::all(
                deferred.values()[1..]
                    .iter()
                    .map(|branch| check(expected, branch)),
            );
        }
        Value::Deferred(DeferredValue::ResourceRef { .. }) => {
            return if matches!(expected, TypeExpr::StringLiteral(_)) {
                Mismatch(mismatch(expected, value))
            } else {
                Assignable
            };
        }
        Value::Deferred(
            DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => return Assignable,
        Value::Deferred(DeferredValue::Access(_)) => {
            let inferred = match inference {
                TypecheckInference::Available { bindings, schemas } => {
                    super::inference::infer_type_from_value_with_provider_context(
                        value, bindings, schemas, config,
                    )
                }
                TypecheckInference::Unavailable => {
                    super::inference::infer_type_from_value_without_bindings(value)
                }
            };
            return match inferred {
                Ok(actual) => inferred_value_check(expected, &actual, value),
                Err(crate::static_type::InferenceIssue::Failed(
                    error @ (super::inference::InferenceError::InvalidAccess { .. }
                    | super::inference::InferenceError::UnknownAttribute { .. }
                    | super::inference::InferenceError::InvalidIntegerArithmeticOperand {
                        ..
                    }),
                )) => Mismatch(TypeExprValueError::InvalidAccess { error }),
                Err(crate::static_type::InferenceIssue::Failed(
                    super::inference::InferenceError::UnknownType { .. }
                    | super::inference::InferenceError::HeterogeneousCollection { .. }
                    | super::inference::InferenceError::IncompatibleCoalesceTypes { .. }
                    | super::inference::InferenceError::UnknownBinding { .. }
                    | super::inference::InferenceError::CyclicVirtualBinding { .. },
                )) => Assignable,
                Err(crate::static_type::InferenceIssue::NotInferable(
                    crate::static_type::NotInferableReason::CustomTypeSchemaUnavailable { .. },
                )) => deferred_provider_check(expected, value),
                Err(crate::static_type::InferenceIssue::NotInferable(
                    crate::static_type::NotInferableReason::MissingValue
                    | crate::static_type::NotInferableReason::NonInferableBinding { .. }
                    | crate::static_type::NotInferableReason::SchemaUnavailable { .. },
                )) => Assignable,
            };
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
            let inferred = match inference {
                TypecheckInference::Available { bindings, schemas } => {
                    super::inference::infer_type_from_value_with_provider_context(
                        value, bindings, schemas, config,
                    )
                }
                TypecheckInference::Unavailable => {
                    super::inference::infer_type_from_value_without_bindings(value)
                }
            };
            return match inferred {
                Ok(actual) => inferred_value_check(expected, &actual, value),
                Err(crate::static_type::InferenceIssue::Failed(error)) => {
                    Mismatch(TypeExprValueError::InvalidAccess { error })
                }
                Err(crate::static_type::InferenceIssue::NotInferable(
                    crate::static_type::NotInferableReason::CustomTypeSchemaUnavailable { .. },
                )) if inferred_type_fits(expected, &TypeExpr::Int) => {
                    deferred_provider_check(expected, value)
                }
                Err(crate::static_type::InferenceIssue::NotInferable(_)) => {
                    Mismatch(mismatch(expected, value))
                }
            };
        }
        Value::Deferred(DeferredValue::BindingRef { .. }) => return Assignable,
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            return TypeExprValueCheck::all(
                [value.as_ref(), default.as_ref()]
                    .into_iter()
                    .map(|value| check(expected, value)),
            );
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            return if accepts_interpolation(expected) {
                Assignable
            } else {
                Mismatch(mismatch(expected, value))
            };
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            return match check(expected, inner) {
                Mismatch(_) => Mismatch(mismatch(expected, value)),
                result => result,
            };
        }
        Value::Concrete(_) => {}
    }
    let matches_scalar = match (expected, value) {
        (
            TypeExpr::String,
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::CanonicalEnum(_)),
        ) => true,
        (TypeExpr::String, Value::Concrete(ConcreteValue::EnumIdentifier(identifier))) => {
            is_qualified_enum_identifier(identifier.as_str())
        }
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::Bool(_))) => true,
        (TypeExpr::Int, Value::Concrete(ConcreteValue::Int(_))) => true,
        (TypeExpr::Float, Value::Concrete(ConcreteValue::Float(_) | ConcreteValue::Int(_))) => true,
        (TypeExpr::Duration, Value::Concrete(ConcreteValue::Duration(_))) => true,
        (TypeExpr::StringLiteral(literal), Value::Concrete(ConcreteValue::String(actual))) => {
            actual == literal
        }
        _ => false,
    };
    if matches_scalar {
        return Assignable;
    }
    match expected {
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::StringLiteral(_) => Mismatch(mismatch(expected, value)),
        TypeExpr::UnloadedSchemaType(_) => deferred_provider_check(expected, value),
        TypeExpr::Simple(name) => {
            let declaration = name.declaration();
            TypeExprValueCheck::from_result(
                validate_builtin_custom_type(declaration, value).map_err(|error| match error {
                    crate::schema::TypeError::TypeMismatch { .. } => mismatch(expected, value),
                    error => validation_mismatch(expected, value, error.to_string()),
                }),
            )
        }
        TypeExpr::List(inner) => match value {
            Value::Concrete(ConcreteValue::List(items)) => {
                TypeExprValueCheck::all(items.iter().enumerate().map(|(index, item)| {
                    check(inner, item).prepend(TypeExprValuePathSegment::ListIndex(index))
                }))
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                TypeExprValueCheck::all(items.iter().enumerate().map(|(index, item)| {
                    check(inner, &Value::Concrete(ConcreteValue::String(item.clone())))
                        .prepend(TypeExprValuePathSegment::ListIndex(index))
                }))
            }
            _ => Mismatch(mismatch(expected, value)),
        },
        TypeExpr::Map(inner) => match value {
            Value::Concrete(ConcreteValue::Map(entries)) => {
                TypeExprValueCheck::all(entries.iter().map(|(key, item)| {
                    check(inner, item).prepend(TypeExprValuePathSegment::MapKey(key.clone()))
                }))
            }
            _ => Mismatch(mismatch(expected, value)),
        },
        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) => match value {
            Value::Concrete(ConcreteValue::String(_) | ConcreteValue::CanonicalEnum(_)) => {
                Assignable
            }
            Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                if is_qualified_enum_identifier(identifier.as_str()) =>
            {
                Assignable
            }
            _ => Mismatch(mismatch(expected, value)),
        },
        TypeExpr::SchemaType { owned } => {
            use crate::schema::RefinedScalarKind;
            let supported = matches!(
                (owned.scalar_kind(), value),
                (
                    RefinedScalarKind::String,
                    Value::Concrete(
                        ConcreteValue::String(_)
                            | ConcreteValue::EnumIdentifier(_)
                            | ConcreteValue::CanonicalEnum(_)
                    )
                ) | (
                    RefinedScalarKind::Int,
                    Value::Concrete(ConcreteValue::Int(_))
                ) | (
                    RefinedScalarKind::Float,
                    Value::Concrete(ConcreteValue::Int(_) | ConcreteValue::Float(_))
                )
            );
            if !supported {
                return Mismatch(mismatch(expected, value));
            }
            match provider_custom_types {
                ProviderCustomTypeValidation::Loaded => {
                    match validate_schema_custom_type_with_context(owned, value, config) {
                        Ok(()) => Assignable,
                        Err(crate::parser::CustomTypeValidationError::ProvidersNotLoaded) => {
                            deferred_provider_check(expected, value)
                        }
                        Err(error) => {
                            Mismatch(custom_type_validation_mismatch(expected, value, error))
                        }
                    }
                }
                ProviderCustomTypeValidation::Providerless => {
                    deferred_provider_check(expected, value)
                }
            }
        }
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
                    return Mismatch(TypeExprValueError::UnknownStructField {
                        path: Vec::new(),
                        expected: Box::new(expected.clone()),
                        actual: Box::new(ValueDescription::of(actual)),
                        field: (*field).clone(),
                    });
                }
                TypeExprValueCheck::all(fields.iter().map(|(field, field_type)| {
                    match entries.get(field) {
                        Some(value) => check(field_type, value)
                            .prepend(TypeExprValuePathSegment::StructField(field.clone())),
                        None => Mismatch(TypeExprValueError::MissingStructField {
                            path: Vec::new(),
                            field: field.clone(),
                        }),
                    }
                }))
            }
            _ => Mismatch(mismatch(expected, value)),
        },
        TypeExpr::Union(members) => TypeExprValueCheck::any(
            members.iter().map(|member| check(member, value)),
            mismatch(expected, value),
        ),
    }
}

fn inferred_value_check<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    actual: &TypeExpr,
    value: &Value<P, B>,
) -> TypeExprValueCheck {
    match inferred_type_assignment(expected, actual) {
        crate::schema::TypeAssignability::Assignable => TypeExprValueCheck::Assignable,
        crate::schema::TypeAssignability::Mismatch => {
            TypeExprValueCheck::Mismatch(mismatch(expected, value))
        }
        crate::schema::TypeAssignability::Unchecked(_) => deferred_provider_check(expected, value),
    }
}

/// Internal parser seam preserving deferred provider evidence. The caller
/// consumes the outcome using the authority carried by `config`; public
/// validation and resolver APIs use the witness-carrying entry points above.
pub(crate) fn check_type_expr_value_with_context<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    config: TypecheckProviderContext<'_>,
    inference: TypecheckInference<'_, P, B>,
) -> TypeExprValueCheck {
    check_type_expr_value_inner(expected, value, config, inference)
}

fn mismatch<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
) -> TypeExprValueError {
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

fn validation_mismatch<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
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

fn custom_type_validation_mismatch<P: PlaceholderKind, B: BindingAddress>(
    expected: &TypeExpr,
    value: &Value<P, B>,
    error: crate::parser::CustomTypeValidationError,
) -> TypeExprValueError {
    TypeExprValueError::CustomTypeValidation {
        path: Vec::new(),
        expected: Box::new(expected.clone()),
        actual: Box::new(ValueDescription::of(value)),
        error: Box::new(error),
    }
}

fn accepts_interpolation(expected: &TypeExpr) -> bool {
    expected.is_string_shaped()
        || matches!(expected, TypeExpr::Ref(_))
        || matches!(expected, TypeExpr::Union(members) if members.iter().any(accepts_interpolation))
}

fn is_qualified_enum_identifier(identifier: &str) -> bool {
    identifier.contains('.')
}

fn inferred_type_assignment(
    expected: &TypeExpr,
    actual: &TypeExpr,
) -> crate::schema::TypeAssignability {
    match (
        super::lift_type_expr(actual),
        super::lift_type_expr(expected),
    ) {
        (Some(actual), Some(expected)) => crate::schema::TypeInSchema::schemaless(&actual)
            .assignment_to(crate::schema::TypeInSchema::schemaless(&expected)),
        (None, _) | (_, None) => crate::schema::TypeAssignability::Unchecked(
            crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
        ),
    }
}

fn inferred_type_fits(expected: &TypeExpr, actual: &TypeExpr) -> bool {
    inferred_type_assignment(expected, actual).allow_unchecked()
}

/// Directional compatibility for a declared source flowing into a declared sink.
#[cfg(test)]
mod tests {
    use crate::schema::{PatternAttributeSchema, PatternAttributeType, PatternResourceSchema};

    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResourceTypePath;
    use crate::resource::{
        ApplyTimeUnknown, AuthoredConcreteValue as ConcreteValue,
        AuthoredDeferredValue as DeferredValue, AuthoredInterpolationPart as InterpolationPart,
        AuthoredValue as Value, CanonicalEnumValue, RelativeBindingName,
    };
    use crate::schema::TypeIdentity;

    fn check(expected: &TypeExpr, value: &Value) -> Result<(), TypeExprValueError> {
        check_type_expr_value_inner(
            expected,
            value,
            TypecheckProviderContext::for_parser(&ProviderContext::default()),
            TypecheckInference::Unavailable,
        )
        .require_provider_schemas()
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
        check_type_expr_value_inner(
            expected,
            value,
            TypecheckProviderContext::for_parser(config),
            TypecheckInference::Unavailable,
        )
        .require_provider_schemas()
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
                crate::schema::RefinedScalarKind::String,
            ),
        };
        let actual = TypeExpr::SchemaType {
            owned: OwnedTypeIdentity::provider(
                "aws",
                identity,
                crate::schema::RefinedScalarKind::String,
            ),
        };

        assert!(
            inferred_type_fits(&expected, &actual),
            "routing provenance must not make one semantic type incompatible with itself"
        );
    }

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    #[test]
    fn nested_checks_preserve_deferral_without_hiding_sibling_mismatches() {
        let config = ProviderContext::default();
        let unknown = crate::parser::parse_type_expr_str("Count", &config).unwrap();
        let check = |expected: &TypeExpr, value: &Value| {
            check_type_expr_value_providerless(
                expected,
                value,
                &config,
                TypecheckInference::Unavailable,
            )
        };
        let value = Value::Concrete(ConcreteValue::Int(3));
        for members in [
            vec![unknown.clone(), TypeExpr::Bool],
            vec![TypeExpr::Bool, unknown.clone()],
        ] {
            assert!(matches!(
                check(&TypeExpr::Union(members), &value),
                TypeExprValueCheck::Deferred(TypeExprValueDeferral {
                    reason: crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
                    ..
                })
            ));
        }
        let expected = TypeExpr::Struct {
            fields: vec![("count".into(), unknown), ("flag".into(), TypeExpr::Bool)],
        };
        let mut entries = IndexMap::from([
            ("count".into(), value),
            ("flag".into(), Value::Concrete(ConcreteValue::Bool(true))),
        ]);
        assert!(matches!(
            check(
                &expected,
                &Value::Concrete(ConcreteValue::Map(entries.clone()))
            ),
            TypeExprValueCheck::Deferred(_)
        ));
        entries.insert("flag".into(), string("wrong"));
        assert!(matches!(
            check(&expected, &Value::Concrete(ConcreteValue::Map(entries))),
            TypeExprValueCheck::Mismatch(_)
        ));
    }

    #[test]
    fn unloaded_declaration_scalar_kind_is_a_typed_outcome() {
        let annotation =
            crate::parser::parse_type_expr_str("Count", &ProviderContext::default()).unwrap();
        let declaration = super::super::lift_type_expr(&annotation).unwrap();
        assert!(
            matches!(crate::provider::identified_custom_type_scalar_kind(&declaration), Err(unloaded) if unloaded.identity() == &TypeIdentity::bare("Count"))
        );
    }

    #[test]
    fn providerless_bare_annotation_defers_literal_validation() {
        let expected =
            crate::parser::parse_type_expr_str("Count", &ProviderContext::default()).unwrap();
        providerless_check(&expected, &Value::Concrete(ConcreteValue::Int(3)))
            .expect("an unloaded bare declaration must defer in providerless mode");
    }

    #[test]
    fn providerless_unloaded_annotation_rejects_non_scalar_values() {
        let config = ProviderContext::default();
        let expected = crate::parser::parse_type_expr_str("AwsAccountId", &config).unwrap();
        for value in [
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(1))),
            Value::Concrete(ConcreteValue::List(vec![string("account")])),
            Value::Concrete(ConcreteValue::StringList(vec!["account".into()])),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "id".into(),
                string("account"),
            )]))),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::Bool(true),
            )))),
        ] {
            assert!(
                matches!(
                    check_type_expr_value_providerless(
                        &expected,
                        &value,
                        &config,
                        TypecheckInference::Unavailable,
                    ),
                    TypeExprValueCheck::Mismatch(_)
                ),
                "a provider scalar cannot accept {value:?}"
            );
        }
    }

    #[test]
    fn check_type_expr_value_providerless_with_loaded_catalog_rejects_unloaded_typo() {
        let expected =
            crate::parser::parse_type_expr_str("UndeclaredAccountId", ProviderContext::unloaded())
                .unwrap();
        let config = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(|_, _| panic!("providerless checking must not validate")),
            ),
            ..ProviderContext::default()
        };
        let result = check_type_expr_value_providerless(
            &expected,
            &string("account"),
            &config,
            TypecheckInference::Unavailable,
        );
        assert!(
            matches!(&result, TypeExprValueCheck::Mismatch(_)),
            "{result:?}"
        );
        let TypeExprValueCheck::Mismatch(error) = result else {
            unreachable!()
        };
        assert!(
            error
                .to_string()
                .contains("unknown custom type 'UndeclaredAccountId'"),
            "{error}"
        );
    }

    #[test]
    fn providerless_union_preserves_a_deferred_member() {
        let unknown =
            crate::parser::parse_type_expr_str("Count", &ProviderContext::default()).unwrap();
        let expected = TypeExpr::Union(vec![TypeExpr::Bool, unknown]);
        providerless_check(&expected, &Value::Concrete(ConcreteValue::Int(3)))
            .expect("an unresolved union member cannot prove a mismatch");
    }

    #[test]
    fn providerless_schema_literals_use_the_declared_scalar_kind() {
        use crate::schema::RefinedScalarKind;

        for (kind, value, assignable) in [
            (
                RefinedScalarKind::Int,
                Value::Concrete(ConcreteValue::Int(3)),
                true,
            ),
            (RefinedScalarKind::Int, string("3"), false),
            (
                RefinedScalarKind::Float,
                Value::Concrete(ConcreteValue::Int(3)),
                true,
            ),
            (
                RefinedScalarKind::Float,
                Value::Concrete(ConcreteValue::Float(3.5)),
                true,
            ),
            (RefinedScalarKind::Float, string("3.5"), false),
            (RefinedScalarKind::String, string("value"), true),
            (
                RefinedScalarKind::String,
                Value::Concrete(ConcreteValue::Int(3)),
                false,
            ),
        ] {
            let expected = TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "mock",
                    TypeIdentity::bare("Count"),
                    kind,
                ),
            };
            assert_eq!(
                providerless_check(&expected, &value).is_ok(),
                assignable,
                "{expected:?}: {value:?}"
            );
        }
    }

    #[test]
    fn inferred_schema_identity_widening_is_independent_of_catalog_state() {
        let config = ProviderContext::default();
        let variants = |name| {
            let identity = TypeIdentity::from_dotted(name);
            [
                TypeExpr::SchemaType {
                    owned: crate::parser::OwnedTypeIdentity::provider(
                        "awscc",
                        identity.clone(),
                        crate::schema::RefinedScalarKind::String,
                    ),
                },
                crate::parser::resolve_custom_type_identity(&identity, &config)
                    .unwrap()
                    .unwrap(),
            ]
        };
        for broad in variants("aws.Arn") {
            for narrow in variants("aws.iam.Role.Arn") {
                assert!(inferred_type_fits(&broad, &narrow));
                assert!(!inferred_type_fits(&narrow, &broad));
            }
        }
    }

    #[test]
    fn inferred_schema_identity_widening_is_directional() {
        let schema_type = |name| TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider(
                "awscc",
                TypeIdentity::from_dotted(name),
                crate::schema::RefinedScalarKind::String,
            ),
        };
        let broad = schema_type("aws.Arn");
        let narrow = schema_type("aws.iam.Role.Arn");
        assert!(inferred_type_fits(&broad, &narrow));
        assert!(!inferred_type_fits(&narrow, &broad));
    }

    #[test]
    fn unloaded_nominal_type_cannot_be_a_validation_declaration() {
        let annotation = crate::parser::resolve_custom_type_identity(
            &TypeIdentity::bare("Count"),
            &ProviderContext::default(),
        )
        .unwrap()
        .unwrap();
        let declaration = super::super::lift_type_expr(&annotation).unwrap();
        assert!(crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration).is_none());
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
            crate::parser::bare_type_for_test("ipv4_address".to_string()),
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                    crate::schema::RefinedScalarKind::String,
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
        ]
    }

    #[test]
    fn providerless_unloaded_annotation_defers_while_strict_check_requires_providers() {
        let expected = crate::parser::bare_type_for_test("account_id".to_string());
        let value = string("account-123");
        let config = ProviderContext::default();

        let unloaded = check_type_expr_value_inner(
            &expected,
            &value,
            TypecheckProviderContext::for_parser(&config),
            TypecheckInference::Unavailable,
        )
        .require_provider_schemas();
        let providerless = check_type_expr_value_providerless(
            &expected,
            &value,
            &config,
            TypecheckInference::Unavailable,
        );

        assert!(matches!(unloaded,
            Err(TypeExprValueError::CustomTypeValidation { error, .. })
                if matches!(*error, crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
        ));
        assert!(matches!(
            providerless,
            TypeExprValueCheck::Deferred(TypeExprValueDeferral {
                reason: crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
                ..
            })
        ));
    }

    fn account_id_schema_type() -> TypeExpr {
        TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider(
                "mock",
                TypeIdentity::bare("AccountId"),
                crate::schema::RefinedScalarKind::String,
            ),
        }
    }

    fn providerless_check(expected: &TypeExpr, value: &Value) -> Result<(), TypeExprValueError> {
        TypecheckProviderContext::providerless(&ProviderContext::default()).check(
            expected,
            value,
            TypecheckInference::Unavailable,
        )
    }

    #[test]
    fn providerless_list_with_provider_leaf_still_rejects_non_list_shape() {
        let expected = TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
            "account_id".to_string(),
        )));

        let result = providerless_check(&expected, &string("account-123"));

        assert!(
            matches!(result, Err(TypeExprValueError::Mismatch { .. })),
            "the list shape must be checked before the unavailable provider leaf: {result:?}"
        );
    }

    #[test]
    fn providerless_struct_with_provider_leaf_still_rejects_builtin_sibling() {
        let expected = TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("id".to_string(), account_id_schema_type()),
            ],
        };
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("name".to_string(), Value::Concrete(ConcreteValue::Int(7))),
            ("id".to_string(), string("account-123")),
        ])));

        let result = providerless_check(&expected, &value);

        assert!(
            matches!(result, Err(TypeExprValueError::Mismatch { .. })),
            "the built-in sibling must be checked before the unavailable provider leaf: {result:?}"
        );
    }

    #[test]
    fn providerless_correct_structure_accepts_uncheckable_provider_leaf() {
        let expected = TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("id".to_string(), account_id_schema_type()),
            ],
        };
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("name".to_string(), string("production")),
            ("id".to_string(), string("account-123")),
        ])));

        let result = providerless_check(&expected, &value);

        assert!(
            matches!(result, Ok(())),
            "the provider leaf must be skipped after structural checks pass: {result:?}"
        );
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
        let unknown = Value::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::UpstreamAllExports {
                binding: RelativeBindingName::new("upstream"),
            },
        ));
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
            crate::parser::bare_type_for_test("ipv4_address".to_string()),
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                    crate::schema::RefinedScalarKind::String,
                ),
            },
            TypeExpr::Struct { fields: Vec::new() },
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
            crate::parser::bare_type_for_test("ipv4_address".to_string()),
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
            TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "VpcId")),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::from_schema_type("aws", "ec2", "VpcId"),
                    crate::schema::RefinedScalarKind::String,
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
            owned: crate::parser::OwnedTypeIdentity::provider(
                "test",
                identity.clone(),
                crate::schema::RefinedScalarKind::String,
            ),
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
        let declared_type = crate::schema::CompiledAttributeType::enum_(
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
            owned: crate::parser::OwnedTypeIdentity::provider(
                "test",
                identity,
                crate::schema::RefinedScalarKind::String,
            ),
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
            crate::parser::bare_type_for_test("positive_int".to_string()),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "test",
                    TypeIdentity::from_schema_type("test", "widget", "PositiveInt"),
                    crate::schema::RefinedScalarKind::Int,
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
        use crate::schema::legacy_validator;

        fn schema(identity: TypeIdentity, accepted: i64) -> crate::schema::ValidatedResourceSchema {
            PatternResourceSchema::new(format!("test.number_{accepted}"))
                .attribute(
                    PatternAttributeSchema::optional(
                        "value",
                        PatternAttributeType::refined_int_with_validator(
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
                    crate::schema::RefinedScalarKind::Int,
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
            owned: crate::parser::OwnedTypeIdentity::provider(
                provider,
                identity.clone(),
                crate::schema::RefinedScalarKind::Int,
            ),
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
            crate::schema::CompiledAttributeType::enum_(
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
                        crate::schema::RefinedScalarKind::String,
                    ),
                    crate::parser::OwnedTypeIdentity::provider(
                        "test",
                        schema_identity.clone(),
                        crate::schema::RefinedScalarKind::String,
                    ),
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
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "bare-provider",
                    bare_identity,
                    crate::schema::RefinedScalarKind::String,
                ),
            },
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "test",
                    schema_identity.clone(),
                    crate::schema::RefinedScalarKind::String,
                ),
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
        assert!(
            check(
                &crate::parser::bare_type_for_test("ipv4_address".to_string()),
                &raw
            )
            .is_ok()
        );
        assert!(
            check(
                &crate::parser::bare_type_for_test("ipv4_address".to_string()),
                &canonical,
            )
            .is_err()
        );
    }

    #[test]
    fn nested_custom_validator_errors_carry_root_to_leaf_paths() {
        let invalid = string("not-an-address");
        let list = Value::Concrete(ConcreteValue::List(vec![
            string("192.0.2.1"),
            invalid.clone(),
        ]));
        let error = check(
            &TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                "ipv4_address".to_string(),
            ))),
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
            &TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                "ipv4_address".to_string(),
            ))),
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
                    crate::parser::bare_type_for_test("ipv4_address".to_string()),
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
        let error = check(
            &crate::parser::bare_type_for_test("ipv4_address".to_string()),
            &secret,
        )
        .unwrap_err();

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
