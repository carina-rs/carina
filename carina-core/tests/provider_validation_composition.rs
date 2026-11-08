use carina_core::provider::{CustomTypeDispatchError, ProviderError, ProviderErrorKind};
use carina_core::provider_text::{ProviderDiagnostic, Retryability};
use carina_core::schema::{TypeError, legacy_validator};
use carina_core::test_utils::provider_text::{BindProviderText, context};

#[test]
fn legacy_validator_retains_a_typed_provider_diagnostic() {
    let callback = legacy_validator(|_| {
        Err(ProviderDiagnostic::new(
            ProviderError::invalid_input_caused_by(
                "invalid reference",
                std::io::Error::other("private-reference"),
            )
            .with_retryability(Retryability::Transient),
        )
        .into())
    });
    let category = callback(&carina_core::resource::Value::Concrete(
        carina_core::resource::ConcreteValue::String("value".into()),
    ))
    .unwrap_err();
    let error = CustomTypeDispatchError::Validation(Box::new(category)).into_provider_error();
    assert_eq!(error.kind(), ProviderErrorKind::InvalidInput);
    assert_eq!(error.retryability(), Retryability::Transient);
    assert!(
        error
            .detail()
            .cause()
            .unwrap()
            .source_is::<std::io::Error>()
    );
    let error = error.with_secrets_for_test(&context(&["private-reference"]));
    assert_eq!(error.to_string(), "invalid reference: (secret)");
    assert!(error.detail().cause().unwrap().source_is::<TypeError>());
    assert!(
        error
            .detail()
            .cause()
            .unwrap()
            .source_is::<std::io::Error>()
    );
}

#[test]
fn dispatch_retains_structured_validation_category() {
    let category = TypeError::MissingRequired {
        name: "region".into(),
    };
    let expected = category.to_string();
    let error = CustomTypeDispatchError::Validation(Box::new(category)).into_provider_error();
    assert!(error.detail().cause().unwrap().source_is::<TypeError>());
    let error = error.into_host_for_test();
    assert_eq!(error.to_string(), expected);
    assert!(
        error
            .detail()
            .cause()
            .unwrap()
            .as_cause()
            .matches_for_test(|error: &TypeError| {
                matches!(error, TypeError::MissingRequired { name } if name == "region")
            })
    );
}
