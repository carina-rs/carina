//! Built-in backend admission uses the resolved directory, without providers.

use carina_core::parser::ProviderContext;
use carina_core::validation::admission::configuration::backend::{self, BackendKind};

fn directory(
    source: &str,
    values: &str,
    context: &ProviderContext,
) -> Result<
    carina_core::validation::admission::configuration::AdmittedConfiguration<
        carina_core::validation::admission::kind::BackendAttribute,
    >,
    backend::BackendConfigError,
> {
    let root = carina_core::test_utils::configuration::ConfigurationDirectory::new(&[
        ("backend.crn", source),
        ("values.crn", values),
    ]);
    let loaded = root.load(context);
    backend::admit(loaded.parsed.backend.as_ref().unwrap())
}

#[test]
fn backend_config_directory_admits_before_concrete_extraction() {
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let admitted = directory(
        "backend s3 { bucket = state_bucket key = 'state.json' region = aws.Region.not_a_region }",
        "let state_bucket = 'state'",
        &context,
    )
    .unwrap();
    assert_eq!(admitted.backend_kind(), BackendKind::S3);
    assert_eq!(admitted.attributes().len(), 3);
    assert!(
        directory("backend local {}", "", &context)
            .unwrap()
            .attributes()
            .is_empty()
    );

    let error = directory(
        "backend s3 { auto_create = 'yes' encrypt = 1 region = true typo = false }",
        "",
        &context,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string().lines().collect::<Vec<_>>(),
        [
            "Invalid attribute 'auto_create' for backend type 's3': Type mismatch: expected Bool, got String",
            "Missing required attribute: bucket",
            "Invalid attribute 'encrypt' for backend type 's3': Type mismatch: expected Bool, got Int",
            "Missing required attribute: key",
            "Invalid attribute 'region' for backend type 's3': Type mismatch: expected Region, got Bool",
            "Unknown attribute 'typo' for backend type 's3'",
        ]
    );
    for (source, expected) in [
        ("backend local { path = true }", "expected String, got Bool"),
        (
            "backend local { typo = 'state.json' }",
            "Unknown attribute 'typo'",
        ),
        (
            "backend s3 { bucket = 'b' key = 'k' region = awscc.Region.us_east_1 }",
            "awscc.Region.us_east_1",
        ),
        ("backend gcs {}", "Unsupported backend type: gcs"),
        ("backend local { path = missing.path }", "Type mismatch"),
        (
            "backend local { path = secret('do-not-show') }",
            "Type mismatch",
        ),
        (
            "backend s3 { bucket = 'b' key = 'k' encrypt = missing.flag region = missing.region }",
            "Type mismatch",
        ),
    ] {
        let root = carina_core::test_utils::configuration::ConfigurationDirectory::new(&[
            ("backend.crn", source),
            ("values.crn", "let ordinary_value = 'value'\n"),
        ]);
        // Exercise raw-candidate admission too: the directory resolver already
        // rejects secret-derived fields before this boundary can see them.
        let path = root.path().join("backend.crn");
        let parsed = carina_core::parser::parse_file(
            &std::fs::read_to_string(&path).unwrap(),
            &path,
            &context,
        )
        .unwrap();
        let error = backend::admit(parsed.backend.as_ref().unwrap()).unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(error.to_string().contains(expected), "{source}: {rendered}");
        assert!(!rendered.contains("do-not-show"));
    }
}

#[test]
fn backend_config_directory_preserves_region_defaults_and_env_strings() {
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::new([(
        "CARINA_BACKEND_BUCKET".into(),
        "protected-bucket".into(),
    )]));
    let admitted = directory(
        "backend s3 { bucket = env('CARINA_BACKEND_BUCKET') key = 'state.json' region = 'us-west-2' encrypt = false }",
        "",
        &context,
    )
    .unwrap();
    assert!(!format!("{admitted:?}").contains("protected-bucket"));
    assert!(!admitted.attributes().contains_key("auto_create"));
    assert_eq!(
        admitted.attributes()["encrypt"],
        carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::Bool(false))
    );
    let region = &admitted.attributes()["region"];
    let carina_core::resource::Value::Concrete(
        carina_core::resource::ConcreteValue::CanonicalEnum(region),
    ) = region
    else {
        panic!("canonical region")
    };
    assert_eq!(region.api_value(), "us-west-2");
}
