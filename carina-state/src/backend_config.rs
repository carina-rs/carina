//! Schema-backed state backend configuration.

use carina_core::parser;
use carina_core::paths::ResolvedPath;
use carina_core::resource::{
    AbsoluteBindingAddress, BindingAddress, ConcreteValue, NoPlaceholder, PlaceholderKind, Value,
    ValueString,
};
use carina_core::validation::admission::{
    configuration::{
        AdmittedConfiguration,
        backend::{self, BackendKind},
    },
    kind::BackendAttribute,
};
use std::collections::HashMap;

pub use backend::{BackendAttributeError, BackendConfigError, BackendConfigErrors};

use crate::backend::{LOCAL_BACKEND_TYPE, S3_BACKEND_TYPE};

/// A state backend configuration whose attributes have been checked against
/// the schema declared by its backend type.
///
/// Concrete extraction accepts only the owning admitted aggregate:
///
/// ```
/// use carina_core::{parser::BackendConfig as Raw, resource::NoPlaceholder,
///     validation::admission::configuration::backend};
/// use carina_state::BackendConfig;
/// let raw = Raw::<NoPlaceholder> { backend_type: "local".into(), attributes: Default::default(), attribute_spans: Default::default() };
/// let checked = BackendConfig::from(backend::admit(&raw).unwrap());
/// assert!(matches!(checked, BackendConfig::Local(_)));
/// ```
/// ```compile_fail,E0277
/// use carina_core::{parser::BackendConfig as Raw, resource::NoPlaceholder};
/// use carina_state::BackendConfig;
/// fn bypass(raw: Raw<NoPlaceholder>) { let _ = BackendConfig::from(raw); }
/// ```
/// ```compile_fail,E0277
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::BackendAttribute};
/// let _: AdmittedConfiguration<BackendAttribute> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0277
/// use carina_core::validation::admission::{PendingAdmission, kind::BackendAttribute};
/// use carina_state::BackendConfig;
/// fn bypass(pending: PendingAdmission<BackendAttribute>) { let _ = BackendConfig::from(pending); }
/// ```
/// ```compile_fail,E0277
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::ProviderConfigAttribute};
/// use carina_state::BackendConfig;
/// fn bypass(provider: AdmittedConfiguration<ProviderConfigAttribute>) { let _ = BackendConfig::from(provider); }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendConfig {
    S3(S3BackendSettings),
    Local(LocalBackendSettings),
}

impl BackendConfig {
    pub(crate) fn implicit_local() -> LocalBackendSettings {
        let raw = parser::BackendConfig::<NoPlaceholder> {
            backend_type: LOCAL_BACKEND_TYPE.into(),
            attributes: Default::default(),
            attribute_spans: Default::default(),
        };
        let Self::Local(settings) =
            Self::from(backend::admit(&raw).expect("built-in local default"))
        else {
            unreachable!()
        };
        settings
    }

    pub fn as_s3(&self) -> Option<&S3BackendSettings> {
        match self {
            Self::S3(settings) => Some(settings),
            Self::Local(_) => None,
        }
    }

    /// Convert the checked representation to the parser shape used by saved
    /// plans. Enum values use API spelling.
    pub fn to_parser_config(&self) -> parser::BackendConfig<NoPlaceholder, AbsoluteBindingAddress> {
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
                    attributes: attributes.into(),
                    attribute_spans: Default::default(),
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
                    attribute_spans: Default::default(),
                }
            }
        }
    }
}

/// Checked S3 backend settings.
///
/// Settings can only be extracted from admitted backend configuration:
///
/// ```compile_fail,E0451
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
    pub(crate) fn from_url_parts(bucket: String, key: String) -> Result<Self, BackendConfigError> {
        let raw = parser::BackendConfig::<NoPlaceholder> {
            backend_type: S3_BACKEND_TYPE.into(),
            attributes: [
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String(bucket.into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String(key.into())),
                ),
                (
                    "encrypt".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                ),
                (
                    "auto_create".to_string(),
                    Value::Concrete(ConcreteValue::Bool(false)),
                ),
            ]
            .into_iter()
            .collect(),
            attribute_spans: Default::default(),
        };
        let BackendConfig::S3(settings) = BackendConfig::try_from(&raw)? else {
            unreachable!()
        };
        Ok(settings)
    }

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
///
/// ```compile_fail,E0451
/// use carina_state::LocalBackendSettings;
/// let _ = LocalBackendSettings { path: None };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBackendSettings {
    path: Option<ValueString>,
}

/// A local address pinned from admitted settings or native operation context.
pub(crate) struct LocalBackendLocation {
    path: ResolvedPath,
}
impl LocalBackendLocation {
    pub(crate) fn path(&self) -> &ResolvedPath {
        &self.path
    }
    pub(crate) fn into_path(self) -> ResolvedPath {
        self.path
    }
}

impl LocalBackendSettings {
    pub(crate) fn pin(&self, base: &ResolvedPath) -> crate::BackendResult<LocalBackendLocation> {
        let path = crate::backends::anchored_local_path(self, base)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        Ok(LocalBackendLocation { path })
    }

    pub(crate) fn for_file(path: &std::path::Path) -> crate::BackendResult<LocalBackendLocation> {
        let path = if let Some(text) = path.to_str() {
            let raw = parser::BackendConfig::<NoPlaceholder> {
                backend_type: LOCAL_BACKEND_TYPE.into(),
                attributes: [(
                    "path".to_owned(),
                    Value::Concrete(ConcreteValue::String(text.into())),
                )]
                .into_iter()
                .collect(),
                attribute_spans: Default::default(),
            };
            let BackendConfig::Local(settings) = BackendConfig::try_from(&raw)? else {
                unreachable!()
            };
            ResolvedPath::new(settings.path().expect("admitted file address"))
        } else {
            // Native OS addresses cannot inhabit a declared String field.
            // Admit the local kind's empty settings and pin this address as
            // operation context; no path-field receipt or lossy text is made.
            let settings = BackendConfig::implicit_local();
            settings.pin_native_file(path)
        }
        .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        Ok(LocalBackendLocation { path })
    }

    fn pin_native_file(self, path: &std::path::Path) -> std::io::Result<ResolvedPath> {
        ResolvedPath::new(path)
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_ref().map(ValueString::expose_plaintext)
    }
}

impl From<AdmittedConfiguration<BackendAttribute>> for BackendConfig {
    fn from(configuration: AdmittedConfiguration<BackendAttribute>) -> Self {
        match configuration.backend_kind() {
            BackendKind::S3 => Self::S3(S3BackendSettings {
                bucket: configuration
                    .string("bucket")
                    .expect("required backend bucket")
                    .clone(),
                key: configuration
                    .string("key")
                    .expect("required backend key")
                    .clone(),
                region: configuration.enum_api_value("region").map(str::to_owned),
                encrypt: configuration.boolean("encrypt"),
                auto_create: configuration.boolean("auto_create"),
            }),
            BackendKind::Local => Self::Local(LocalBackendSettings {
                path: configuration.string("path").cloned(),
            }),
        }
    }
}

impl<P: PlaceholderKind, B: BindingAddress> TryFrom<&parser::BackendConfig<P, B>>
    for BackendConfig
{
    type Error = BackendConfigError;
    fn try_from(config: &parser::BackendConfig<P, B>) -> Result<Self, Self::Error> {
        backend::admit(config).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(source: &str) -> Result<BackendConfig, BackendConfigError> {
        let parsed = carina_core::parser::parse_file(
            source,
            std::path::Path::new("backend.crn"),
            &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
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
        assert_eq!(
            s3_region(
                r#"backend s3 {
                    bucket = "state"
                    key = "state.json"
                    region = "us_west_2"
                }"#,
            ),
            "us-west-2"
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
        let source = format!(
            r#"backend s3 {{
                bucket = env('{variable}')
                key = "state.json"
            }}"#
        );

        let context = carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::new([(variable.to_string(), plaintext.into())]),
        );
        let mut parsed =
            carina_core::parser::parse_file(&source, std::path::Path::new("backend.crn"), &context)
                .unwrap();
        carina_core::parser::resolve_resource_refs_with_config(&mut parsed, &context).unwrap();
        let resolved = BackendConfig::try_from(parsed.backend.as_ref().unwrap()).unwrap();
        let settings = resolved.as_s3().unwrap();
        assert_eq!(settings.bucket(), plaintext);
        assert_eq!(settings.bucket_diagnostic_literal(), None);
        assert!(!format!("{settings:?}").contains(plaintext));
    }
}

#[cfg(test)]
#[path = "backend_config/admission_tests.rs"]
mod admission_tests;
