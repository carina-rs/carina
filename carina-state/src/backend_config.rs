//! Schema-backed state backend configuration.

use carina_core::parser;
use carina_core::resource::{
    AbsoluteBindingAddress, BindingAddress, ConcreteValue, RelativeBindingName, Value, ValueString,
};
use carina_core::schema::{
    AttributeType, DslTransform, TypeError, TypeIdentity, ValidatedConfigSchema,
};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use thiserror::Error;

use crate::backend::{LOCAL_BACKEND_TYPE, S3_BACKEND_TYPE};

const AWS_PROVIDER_NAME: &str = "aws";
const AWS_REGION_TYPE_NAME: &str = "aws.Region";

/// A state backend configuration whose attributes have been checked against
/// the schema declared by its backend type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendConfig {
    S3(S3BackendSettings),
    Local(LocalBackendSettings),
}

impl BackendConfig {
    pub fn as_s3(&self) -> Option<&S3BackendSettings> {
        match self {
            Self::S3(settings) => Some(settings),
            Self::Local(_) => None,
        }
    }

    /// Convert the checked representation to the parser shape used by saved
    /// plans. Enum values use API spelling.
    pub fn to_parser_config(&self) -> parser::ExpandedBackendConfig {
        match self {
            Self::S3(settings) => {
                let mut attributes = HashMap::from([
                    (
                        "bucket".to_string(),
                        Value::Concrete(ConcreteValue::String(settings.bucket.clone())),
                    ),
                    (
                        "key".to_string(),
                        Value::Concrete(ConcreteValue::String(settings.key.clone())),
                    ),
                ]);
                if let Some(region) = &settings.region {
                    attributes.insert(
                        "region".to_string(),
                        Value::Concrete(ConcreteValue::String(region.clone().into())),
                    );
                }
                if let Some(encrypt) = settings.encrypt {
                    attributes.insert(
                        "encrypt".to_string(),
                        Value::Concrete(ConcreteValue::Bool(encrypt)),
                    );
                }
                if let Some(auto_create) = settings.auto_create {
                    attributes.insert(
                        "auto_create".to_string(),
                        Value::Concrete(ConcreteValue::Bool(auto_create)),
                    );
                }
                parser::BackendConfig {
                    backend_type: S3_BACKEND_TYPE.to_string(),
                    attributes,
                    attribute_spans: HashMap::new(),
                }
            }
            Self::Local(settings) => {
                let attributes = settings.path.iter().map(|path| {
                    (
                        "path".to_string(),
                        Value::Concrete(ConcreteValue::String(path.clone())),
                    )
                });
                parser::BackendConfig {
                    backend_type: LOCAL_BACKEND_TYPE.to_string(),
                    attributes: attributes.collect(),
                    attribute_spans: HashMap::new(),
                }
            }
        }
    }
}

/// Checked S3 backend settings.
///
/// Settings can only be constructed by resolving a parser backend block:
///
/// ```compile_fail
/// use carina_state::S3BackendSettings;
///
/// let _ = S3BackendSettings {
///     bucket: todo!(),
///     key: todo!(),
///     region: None,
///     encrypt: None,
///     auto_create: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3BackendSettings {
    bucket: ValueString,
    key: ValueString,
    region: Option<String>,
    encrypt: Option<bool>,
    auto_create: Option<bool>,
}

impl S3BackendSettings {
    pub fn bucket(&self) -> &str {
        self.bucket.expose_plaintext()
    }

    pub fn bucket_diagnostic_literal(&self) -> Option<&str> {
        self.bucket.diagnostic_literal()
    }

    pub fn key(&self) -> &str {
        self.key.expose_plaintext()
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub fn encrypt(&self) -> bool {
        self.encrypt.unwrap_or(true)
    }

    pub fn auto_create(&self) -> bool {
        self.auto_create.unwrap_or(true)
    }

    pub(crate) fn authored_encrypt(&self) -> Option<bool> {
        self.encrypt
    }

    pub(crate) fn authored_auto_create(&self) -> Option<bool> {
        self.auto_create
    }
}

/// Checked local filesystem backend settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBackendSettings {
    path: Option<ValueString>,
}

impl LocalBackendSettings {
    pub fn path(&self) -> Option<&str> {
        self.path.as_ref().map(ValueString::expose_plaintext)
    }
}

/// Failure to resolve a raw backend block through its declared schema.
#[derive(Debug, Error)]
pub enum BackendConfigError {
    #[error("Unsupported backend type: {0}")]
    UnsupportedBackend(String),
    #[error(transparent)]
    Invalid(#[from] BackendConfigErrors),
}

/// All attribute-level failures from resolving one backend block.
#[derive(Debug)]
pub struct BackendConfigErrors {
    errors: Vec<BackendAttributeError>,
}

impl BackendConfigErrors {
    fn new(mut errors: Vec<BackendAttributeError>) -> Self {
        debug_assert!(!errors.is_empty());
        errors.sort_by(|left, right| {
            left.attribute()
                .cmp(right.attribute())
                .then_with(|| left.sort_rank().cmp(&right.sort_rank()))
        });
        Self { errors }
    }

    /// Attribute failures in deterministic attribute-name order.
    pub fn errors(&self) -> &[BackendAttributeError] {
        &self.errors
    }
}

impl fmt::Display for BackendConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for BackendConfigErrors {}

/// One schema failure associated with a backend attribute.
#[derive(Debug, Error)]
pub enum BackendAttributeError {
    #[error("Unknown attribute '{attribute}' for backend type '{backend_type}'")]
    UnknownAttribute {
        backend_type: String,
        attribute: String,
    },
    #[error("Missing required attribute: {attribute}")]
    MissingRequiredAttribute {
        backend_type: String,
        attribute: String,
    },
    #[error("Invalid attribute '{attribute}' for backend type '{backend_type}': {error}")]
    InvalidAttribute {
        backend_type: String,
        attribute: String,
        error: Box<TypeError>,
    },
}

impl BackendAttributeError {
    pub fn attribute(&self) -> &str {
        match self {
            Self::UnknownAttribute { attribute, .. }
            | Self::MissingRequiredAttribute { attribute, .. }
            | Self::InvalidAttribute { attribute, .. } => attribute,
        }
    }

    fn sort_rank(&self) -> u8 {
        match self {
            Self::UnknownAttribute { .. } => 0,
            Self::MissingRequiredAttribute { .. } => 1,
            Self::InvalidAttribute { .. } => 2,
        }
    }
}

fn s3_schema() -> ValidatedConfigSchema {
    ValidatedConfigSchema::validate(BTreeMap::from([
        ("bucket".to_string(), AttributeType::string()),
        ("key".to_string(), AttributeType::string()),
        (
            "region".to_string(),
            AttributeType::enum_(
                TypeIdentity::new(Some(AWS_PROVIDER_NAME), Vec::<String>::new(), "Region"),
                None,
                Vec::new(),
                None,
                Some(DslTransform::HyphenToUnderscore),
            ),
        ),
        ("encrypt".to_string(), AttributeType::bool()),
        ("auto_create".to_string(), AttributeType::bool()),
    ]))
    .expect("the built-in S3 backend config schema must be valid")
}

fn local_schema() -> ValidatedConfigSchema {
    ValidatedConfigSchema::validate(BTreeMap::from([(
        "path".to_string(),
        AttributeType::string(),
    )]))
    .expect("the built-in local backend config schema must be valid")
}

fn canonicalize_attributes<B: BindingAddress>(
    config: &parser::BackendConfig<B>,
    schema: &ValidatedConfigSchema,
    required: &[&'static str],
) -> (HashMap<String, Value<B>>, Vec<BackendAttributeError>) {
    let mut errors = Vec::new();
    for attribute in required {
        if !config.attributes.contains_key(*attribute) {
            errors.push(BackendAttributeError::MissingRequiredAttribute {
                backend_type: config.backend_type.clone(),
                attribute: (*attribute).to_string(),
            });
        }
    }

    let mut canonicalized = HashMap::new();
    for (attribute, value) in &config.attributes {
        let Some(rootless_type) = schema.rootless_type(attribute) else {
            errors.push(BackendAttributeError::UnknownAttribute {
                backend_type: config.backend_type.clone(),
                attribute: attribute.clone(),
            });
            continue;
        };
        // Passing the schema identity's own provider name deliberately
        // disables the provider-block alias rule: backend regions accept
        // `aws.Region.<member>`, never `awscc.Region.<member>`.
        let canonical =
            match rootless_type.canonicalize_provider_config(AWS_PROVIDER_NAME, value.clone()) {
                Ok(canonical) => canonical,
                Err(error) => {
                    errors.push(BackendAttributeError::InvalidAttribute {
                        backend_type: config.backend_type.clone(),
                        attribute: attribute.clone(),
                        error: Box::new(error),
                    });
                    continue;
                }
            };
        if let Err(error) = rootless_type.validate_in_phase(&canonical) {
            errors.push(BackendAttributeError::InvalidAttribute {
                backend_type: config.backend_type.clone(),
                attribute: attribute.clone(),
                error: Box::new(error),
            });
            continue;
        }
        canonicalized.insert(attribute.clone(), canonical);
    }

    (canonicalized, errors)
}

fn invalid_resolved_shape<B: BindingAddress>(
    backend_type: &str,
    attribute: &str,
    expected: &str,
    value: &Value<B>,
) -> BackendAttributeError {
    BackendAttributeError::InvalidAttribute {
        backend_type: backend_type.to_string(),
        attribute: attribute.to_string(),
        error: Box::new(TypeError::TypeMismatch {
            expected: expected.to_string(),
            got: value.type_name(),
        }),
    }
}

fn take_string<B: BindingAddress>(
    attributes: &mut HashMap<String, Value<B>>,
    backend_type: &str,
    attribute: &str,
) -> Result<Option<ValueString>, BackendAttributeError> {
    let Some(value) = attributes.remove(attribute) else {
        return Ok(None);
    };
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Ok(Some(value)),
        other => Err(invalid_resolved_shape(
            backend_type,
            attribute,
            "String",
            &other,
        )),
    }
}

fn take_bool<B: BindingAddress>(
    attributes: &mut HashMap<String, Value<B>>,
    backend_type: &str,
    attribute: &str,
) -> Result<Option<bool>, BackendAttributeError> {
    let Some(value) = attributes.remove(attribute) else {
        return Ok(None);
    };
    match value {
        Value::Concrete(ConcreteValue::Bool(value)) => Ok(Some(value)),
        other => Err(invalid_resolved_shape(
            backend_type,
            attribute,
            "Bool",
            &other,
        )),
    }
}

fn collect_attribute<T>(
    result: Result<Option<T>, BackendAttributeError>,
    errors: &mut Vec<BackendAttributeError>,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

fn resolve_s3_backend<B: BindingAddress>(
    config: &parser::BackendConfig<B>,
) -> Result<BackendConfig, BackendConfigError> {
    let (mut attributes, mut errors) =
        canonicalize_attributes(config, &s3_schema(), &["bucket", "key"]);
    let bucket = collect_attribute(
        take_string(&mut attributes, S3_BACKEND_TYPE, "bucket"),
        &mut errors,
    );
    let key = collect_attribute(
        take_string(&mut attributes, S3_BACKEND_TYPE, "key"),
        &mut errors,
    );
    let region = match attributes.remove("region") {
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(value))) => {
            Some(value.api_value().to_string())
        }
        Some(other) => {
            errors.push(invalid_resolved_shape(
                S3_BACKEND_TYPE,
                "region",
                AWS_REGION_TYPE_NAME,
                &other,
            ));
            None
        }
        None => None,
    };
    let encrypt = collect_attribute(
        take_bool(&mut attributes, S3_BACKEND_TYPE, "encrypt"),
        &mut errors,
    );
    let auto_create = collect_attribute(
        take_bool(&mut attributes, S3_BACKEND_TYPE, "auto_create"),
        &mut errors,
    );
    debug_assert!(attributes.is_empty());

    if !errors.is_empty() {
        return Err(BackendConfigErrors::new(errors).into());
    }

    Ok(BackendConfig::S3(S3BackendSettings {
        bucket: bucket.expect("a valid S3 backend has a bucket"),
        key: key.expect("a valid S3 backend has a key"),
        region,
        encrypt,
        auto_create,
    }))
}

fn resolve_local_backend<B: BindingAddress>(
    config: &parser::BackendConfig<B>,
) -> Result<BackendConfig, BackendConfigError> {
    let (mut attributes, mut errors) = canonicalize_attributes(config, &local_schema(), &[]);
    let path = collect_attribute(
        take_string(&mut attributes, LOCAL_BACKEND_TYPE, "path"),
        &mut errors,
    );
    debug_assert!(attributes.is_empty());
    if !errors.is_empty() {
        return Err(BackendConfigErrors::new(errors).into());
    }
    Ok(BackendConfig::Local(LocalBackendSettings { path }))
}

impl TryFrom<&parser::BackendConfig<RelativeBindingName>> for BackendConfig {
    type Error = BackendConfigError;

    fn try_from(config: &parser::BackendConfig<RelativeBindingName>) -> Result<Self, Self::Error> {
        match config.backend_type.as_str() {
            S3_BACKEND_TYPE => resolve_s3_backend(config),
            LOCAL_BACKEND_TYPE => resolve_local_backend(config),
            other => Err(BackendConfigError::UnsupportedBackend(other.to_string())),
        }
    }
}

impl TryFrom<&parser::BackendConfig<AbsoluteBindingAddress>> for BackendConfig {
    type Error = BackendConfigError;

    fn try_from(
        config: &parser::BackendConfig<AbsoluteBindingAddress>,
    ) -> Result<Self, Self::Error> {
        match config.backend_type.as_str() {
            S3_BACKEND_TYPE => resolve_s3_backend(config),
            LOCAL_BACKEND_TYPE => resolve_local_backend(config),
            other => Err(BackendConfigError::UnsupportedBackend(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(source: &str) -> Result<BackendConfig, BackendConfigError> {
        let parsed = carina_core::parser::parse_file(
            source,
            std::path::Path::new("backend.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("backend fixture must parse");
        BackendConfig::try_from(parsed.backend.as_ref().expect("backend fixture"))
    }

    fn s3_region(source: &str) -> String {
        match resolve(source).expect("backend must resolve") {
            BackendConfig::S3(settings) => settings.region().unwrap().to_string(),
            BackendConfig::Local(_) => panic!("expected s3 backend"),
        }
    }

    #[test]
    fn enum_region_resolves_to_api_spelling() {
        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = aws.Region.us_west_2
                }"#,
            ),
            "us-west-2"
        );
    }

    #[test]
    fn literal_region_remains_a_literal_api_value() {
        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = 'us-west-2'
                }"#,
            ),
            "us-west-2"
        );
    }

    #[test]
    fn quoted_region_with_underscores_matches_provider_config_canonicalization() {
        let provider_value = s3_schema()
            .rootless_type("region")
            .expect("S3 schema must contain region")
            .canonicalize_provider_config(
                AWS_PROVIDER_NAME,
                carina_core::resource::ExpandedValue::Concrete(
                    carina_core::resource::ExpandedConcreteValue::String("us_west_2".into()),
                ),
            )
            .unwrap();
        let Value::Concrete(ConcreteValue::CanonicalEnum(expected)) = provider_value else {
            panic!("provider config must canonicalize the quoted region")
        };

        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = "us_west_2"
                }"#,
            ),
            expected.api_value()
        );
    }

    #[test]
    fn quoted_region_with_hyphens_keeps_api_spelling() {
        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = "us-west-2"
                }"#,
            ),
            "us-west-2"
        );
    }

    #[test]
    fn open_region_enum_accepts_unknown_members() {
        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = aws.Region.not_a_region
                }"#,
            ),
            "not-a-region"
        );
    }

    #[test]
    fn region_rejects_an_unrelated_provider_namespace() {
        let error = resolve(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
                region = awscc.Region.us_west_2
            }"#,
        )
        .unwrap_err();
        let BackendConfigError::Invalid(errors) = &error else {
            panic!("expected attribute errors, got {error:?}");
        };
        assert_eq!(errors.errors()[0].attribute(), "region");
        assert!(error.to_string().contains("awscc.Region.us_west_2"));
    }

    #[test]
    fn region_rejects_bool() {
        let error = resolve(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
                region = true
            }"#,
        )
        .unwrap_err();
        let BackendConfigError::Invalid(errors) = &error else {
            panic!("expected attribute errors, got {error:?}");
        };
        assert_eq!(errors.errors()[0].attribute(), "region");
        assert!(error.to_string().contains("type"));
    }

    #[test]
    fn unknown_attribute_is_rejected() {
        let error = resolve(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
                typo = true
            }"#,
        )
        .unwrap_err();
        let BackendConfigError::Invalid(errors) = error else {
            panic!("expected attribute errors");
        };
        assert!(matches!(
            errors.errors(),
            [BackendAttributeError::UnknownAttribute {
                backend_type,
                attribute,
            }] if backend_type == S3_BACKEND_TYPE && attribute == "typo"
        ));
    }

    #[test]
    fn attribute_errors_are_all_reported_in_sorted_order() {
        let source = r#"backend s3 {
                auto_create = "yes"
                encrypt = 1
                region = true
                typo = false
            }"#;

        // Each parse creates fresh randomized HashMaps. Repeating the exact
        // assertion proves resolution order does not leak map iteration order.
        for _ in 0..32 {
            let error = resolve(source).unwrap_err();
            assert_eq!(
                error.to_string().lines().collect::<Vec<_>>(),
                vec![
                    "Invalid attribute 'auto_create' for backend type 's3': Type mismatch: expected Bool, got String",
                    "Missing required attribute: bucket",
                    "Invalid attribute 'encrypt' for backend type 's3': Type mismatch: expected Bool, got Int",
                    "Missing required attribute: key",
                    "Invalid attribute 'region' for backend type 's3': Type mismatch: expected Region, got Bool",
                    "Unknown attribute 'typo' for backend type 's3'",
                ]
            );
        }
    }

    #[test]
    fn missing_bucket_is_rejected() {
        let error = resolve(
            r#"backend s3 {
                key = "state.json"
            }"#,
        )
        .unwrap_err();
        let BackendConfigError::Invalid(errors) = error else {
            panic!("expected attribute errors");
        };
        assert!(matches!(
            errors.errors(),
            [BackendAttributeError::MissingRequiredAttribute { attribute, .. }]
                if attribute == "bucket"
        ));
    }

    #[test]
    fn unknown_backend_type_is_rejected() {
        let error = resolve("backend gcs {}").unwrap_err();
        assert_eq!(error.to_string(), "Unsupported backend type: gcs");
    }

    #[test]
    fn defaults_are_applied_without_being_projected_as_authored() {
        let resolved = resolve(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
            }"#,
        )
        .unwrap();
        let settings = resolved.as_s3().unwrap();
        assert!(settings.encrypt());
        assert!(settings.auto_create());

        let projected = resolved.to_parser_config();
        assert!(!projected.attributes.contains_key("encrypt"));
        assert!(!projected.attributes.contains_key("auto_create"));
    }

    #[test]
    fn authored_bool_values_are_projected() {
        let resolved = resolve(
            r#"backend s3 {
                bucket = "state"
                key = "state.json"
                encrypt = false
                auto_create = false
            }"#,
        )
        .unwrap();
        let settings = resolved.as_s3().unwrap();
        assert!(!settings.encrypt());
        assert!(!settings.auto_create());

        let projected = resolved.to_parser_config();
        assert_eq!(
            projected.attributes.get("encrypt"),
            Some(&Value::Concrete(ConcreteValue::Bool(false)))
        );
        assert_eq!(
            projected.attributes.get("auto_create"),
            Some(&Value::Concrete(ConcreteValue::Bool(false)))
        );
    }

    #[test]
    fn environment_strings_remain_usable_and_redacted() {
        let variable = "CARINA_3880_BACKEND_BUCKET";
        let plaintext = "bucket-from-env";
        unsafe {
            std::env::set_var(variable, plaintext);
        }
        let source = format!(
            r#"backend s3 {{
                bucket = env('{variable}')
                key = "state.json"
            }}"#
        );

        let context = carina_core::parser::ProviderContext::default();
        let mut parsed =
            carina_core::parser::parse_file(&source, std::path::Path::new("backend.crn"), &context)
                .unwrap();
        carina_core::parser::resolve_resource_refs_with_config(&mut parsed, &context).unwrap();
        let resolved = BackendConfig::try_from(parsed.backend.as_ref().unwrap()).unwrap();
        unsafe {
            std::env::remove_var(variable);
        }

        let settings = resolved.as_s3().unwrap();
        assert_eq!(settings.bucket(), plaintext);
        assert_eq!(settings.bucket_diagnostic_literal(), None);
        assert!(!format!("{settings:?}").contains(plaintext));
    }
}
