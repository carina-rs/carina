//! Pure conversion-policy utilities; these should stay green before B1 syntax.

use super::*;
use crate::schema::{
    CompiledAttributeType as T, IntIdentityLayer, IntMemberConstraints, StringIdentityLayer,
    StringMemberConstraints, TypeIdentity,
};
use std::error::Error;

fn location() -> Result<DeclarationLocation, String> {
    let parsed = crate::parser::parse_file(
        "let sample = 'ok'",
        std::path::Path::new("conversion-unit.crn"),
        &super::tests::catalog(),
    )
    .map_err(|error| error.to_string())?;
    parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&crate::resource::RelativeBindingName::new("sample"))
        .map(|expression| expression.location().clone())
        .ok_or_else(|| "missing fixture declaration".into())
}

#[test]
fn conversion_error_wrappers_render_each_message_once() -> Result<(), String> {
    use crate::{
        parser::ParseError,
        schema::TypeError,
        static_type::{InferenceError, InferenceIssue},
    };
    let location = location()?;
    let mut conversion = ConversionError::new(
        ConversionErrorKind::Source,
        &location,
        "conversion-sentinel",
    );
    conversion.cause = Some(std::sync::Arc::new(ConversionCause::Destination(Box::new(
        TypeError::ListItemError {
            index: 2,
            inner: Box::new(TypeError::StructFieldError {
                field: "field-sentinel".into(),
                inner: Box::new(TypeError::MissingRequired {
                    name: "cause-sentinel".into(),
                }),
            }),
        },
    ))));
    let wrappers: Vec<Box<dyn Error>> = vec![
        Box::new(conversion.clone()),
        Box::new(ParseError::CheckedConversion(conversion.clone())),
        Box::new(InferenceError::Conversion(conversion.clone())),
        Box::new(InferenceIssue::Failed(InferenceError::Conversion(
            conversion.clone(),
        ))),
    ];
    for wrapper in wrappers {
        let rendered = crate::executor::render_error_chain(wrapper.as_ref());
        for message in ["conversion-sentinel", "field-sentinel", "cause-sentinel"] {
            assert_eq!(rendered.matches(message).count(), 1, "{rendered}");
        }
        let found = ConversionError::from_error(wrapper.as_ref())
            .ok_or_else(|| format!("lost typed conversion: {wrapper:?}"))?;
        assert_eq!(found.kind(), conversion.kind());
        assert_eq!(found.location(), &location);
    }
    Ok(())
}

#[test]
fn conversion_aggregate_wrappers_render_each_message_once() -> Result<(), String> {
    let location = location()?;
    let mut conversion = ConversionError::new(
        ConversionErrorKind::Source,
        &location,
        "aggregate-conversion-sentinel",
    );
    conversion.cause = Some(std::sync::Arc::new(ConversionCause::Destination(Box::new(
        crate::schema::TypeError::MissingRequired {
            name: "aggregate-cause-sentinel".into(),
        },
    ))));
    let mut findings = ConversionFindings::default();
    findings.extend(std::iter::once(conversion.clone()).collect());
    let wrappers: Vec<Box<dyn Error>> = vec![
        Box::new(crate::validation::ResourceValidationReport {
            ordinary: String::new(),
            conversions: findings,
        }),
        Box::new({
            let mut findings = ConversionFindings::default();
            findings.extend(std::iter::once(conversion.clone()).collect());
            crate::validation::ResourceRefTypeValidationError::Validation(
                crate::validation::ResourceValidationReport {
                    ordinary: String::new(),
                    conversions: findings,
                },
            )
        }),
    ];
    for wrapper in wrappers {
        let rendered = crate::executor::render_error_chain(wrapper.as_ref());
        assert_eq!(
            rendered.matches("aggregate-conversion-sentinel").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("aggregate-cause-sentinel").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            ConversionError::from_error(wrapper.as_ref())
                .ok_or("lost aggregate conversion")?
                .location(),
            &location
        );
    }
    Ok(())
}

#[test]
fn conversion_type_path_and_loaded_scalar_target() -> Result<(), String> {
    let path = ConversionTypePath::new("mock.Name");
    assert_eq!(path.as_str(), "mock.Name");
    let target = resolve_target(&path, &location()?, &super::tests::catalog())
        .map_err(|error| error.to_string())?;
    assert_eq!(target.identity(), &TypeIdentity::from_dotted("mock.Name"));
    assert_eq!(target.scalar_kind(), RefinedScalarKind::String);
    Ok(())
}

#[test]
fn conversion_target_requires_a_loaded_named_scalar() -> Result<(), String> {
    let location = location()?;
    for path in ["String", "Bool", "mock.Missing", "mock.Source"] {
        let error = resolve_target(
            &ConversionTypePath::new(path),
            &location,
            &super::tests::catalog(),
        )
        .expect_err("non-target was resolved");
        assert_eq!(error.kind(), ConversionErrorKind::Target);
        assert_eq!(error.location(), &location);
    }
    let unloaded = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    assert_eq!(
        resolve_target(&ConversionTypePath::new("mock.Name"), &location, &unloaded)
            .expect_err("unloaded catalog supplied conversion authority")
            .kind(),
        ConversionErrorKind::Target,
    );
    Ok(())
}

#[test]
fn conversion_scalar_kind_unwraps_enums_without_numeric_coercion() {
    for (declaration, expected) in [
        (T::string(), RefinedScalarKind::String),
        (T::int(), RefinedScalarKind::Int),
        (T::float(), RefinedScalarKind::Float),
        (
            T::enum_with_base(
                TypeIdentity::from_dotted("mock.Number"),
                T::int(),
                None,
                vec![],
                None,
                None,
            ),
            RefinedScalarKind::Int,
        ),
    ] {
        assert_eq!(scalar_kind(&declaration), Some(expected));
    }
    for declaration in [
        T::bool(),
        T::duration(),
        T::list(T::string()),
        T::map(T::string()),
        T::union(vec![T::string(), T::int()]),
    ] {
        assert_eq!(scalar_kind(&declaration), None);
        assert_eq!(DeclarationBounds::from_declaration(&declaration), None);
    }
}

#[test]
fn conversion_string_bounds_intersect_identity_and_member_layers() {
    let declaration = T::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Name")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        StringMemberConstraints::new(None, Some((Some(3), Some(5)))),
    );
    assert_eq!(
        DeclarationBounds::from_declaration(&declaration),
        Some(DeclarationBounds::String {
            min: 3,
            max: Some(5)
        })
    );
    assert_eq!(
        DeclarationBounds::from_declaration(&T::string()),
        Some(DeclarationBounds::String { min: 0, max: None })
    );
}

#[test]
fn conversion_integer_bounds_intersect_identity_and_member_layers() {
    let declaration = T::int_member(
        IntIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Count")),
            Some((Some(1), Some(8))),
        ),
        IntMemberConstraints::new(Some((Some(3), Some(5)))),
    );
    assert_eq!(
        DeclarationBounds::from_declaration(&declaration),
        Some(DeclarationBounds::Int {
            min: Some(3),
            max: Some(5)
        })
    );
    assert_eq!(
        DeclarationBounds::from_declaration(&T::int()),
        Some(DeclarationBounds::Int {
            min: None,
            max: None
        })
    );
}

#[test]
fn conversion_disjoint_intervals_are_symmetric_and_endpoints_are_inclusive() {
    for (left, touching, separate, unbounded) in [
        (
            T::refined_string(None, None, Some((Some(1), Some(3))), None),
            T::refined_string(None, None, Some((Some(3), Some(5))), None),
            T::refined_string(None, None, Some((Some(4), Some(5))), None),
            T::string(),
        ),
        (
            T::refined_int(None, Some((Some(1), Some(3)))),
            T::refined_int(None, Some((Some(3), Some(5)))),
            T::refined_int(None, Some((Some(4), Some(5)))),
            T::int(),
        ),
        (
            T::refined_float(
                None,
                Some((Some(crate::finite(1.0)), Some(crate::finite(3.0)))),
            ),
            T::refined_float(
                None,
                Some((Some(crate::finite(3.0)), Some(crate::finite(5.0)))),
            ),
            T::refined_float(
                None,
                Some((Some(crate::finite(4.0)), Some(crate::finite(5.0)))),
            ),
            T::float(),
        ),
    ] {
        let left = DeclarationBounds::from_declaration(&left).unwrap();
        let touching = DeclarationBounds::from_declaration(&touching).unwrap();
        let separate = DeclarationBounds::from_declaration(&separate).unwrap();
        let unbounded = DeclarationBounds::from_declaration(&unbounded).unwrap();
        assert!(!left.disjoint(&touching));
        assert!(!touching.disjoint(&left));
        assert!(left.disjoint(&separate));
        assert!(separate.disjoint(&left));
        assert!(!left.disjoint(&unbounded));
        assert!(!unbounded.disjoint(&left));
    }
    // Scalar-kind mismatch is a separate error, not disjointness evidence.
    assert!(
        !DeclarationBounds::String { min: 0, max: None }.disjoint(&DeclarationBounds::Int {
            min: None,
            max: None
        })
    );
}

#[test]
fn conversion_source_declarations_do_not_promote_slot_member_bounds_to_read_bounds()
-> Result<(), String> {
    let declaration = T::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Name")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        StringMemberConstraints::new(None, Some((Some(2), Some(3)))),
    );
    let source = ResolvedSchemaType::from_schema_declaration("mock", &declaration)
        .ok_or("projection fixture was not a scalar")?;
    let declaration = source_declaration(&source, &location()?, &super::tests::catalog())
        .map_err(|e| e.to_string())?;
    assert_eq!(
        DeclarationBounds::from_declaration(&declaration),
        Some(DeclarationBounds::String {
            min: 1,
            max: Some(8)
        })
    );
    Ok(())
}

#[test]
fn conversion_source_declarations_load_annotation_evidence() -> Result<(), String> {
    let catalog = super::tests::catalog();
    let location = location()?;
    let source = resolve_target(&ConversionTypePath::new("mock.Name"), &location, &catalog)
        .map_err(|e| e.to_string())?;
    let declaration =
        source_declaration(&source, &location, &catalog).map_err(|e| e.to_string())?;
    assert_eq!(
        DeclarationBounds::from_declaration(&declaration),
        Some(DeclarationBounds::String {
            min: 1,
            max: Some(8)
        })
    );
    Ok(())
}

#[test]
fn conversion_source_declarations_keep_all_annotation_owners() -> Result<(), String> {
    use crate::parser::{IdentifiedCustomTypeDeclaration, OwnedTypeIdentity, ProviderCustomTypes};
    let identity = TypeIdentity::from_dotted("shared.Name");
    let declarations = std::collections::BTreeMap::from([
        (
            "first".to_owned(),
            vec![T::refined_string(
                Some(identity.clone()),
                None,
                Some((Some(1), Some(12))),
                None,
            )],
        ),
        (
            "second".to_owned(),
            vec![T::refined_string(
                Some(identity.clone()),
                None,
                Some((Some(4), Some(10))),
                None,
            )],
        ),
    ]);
    let known = declarations
        .iter()
        .flat_map(|(owner, declarations)| {
            declarations.iter().map(move |declaration| {
                OwnedTypeIdentity::from_declaration(
                    owner,
                    IdentifiedCustomTypeDeclaration::new(declaration).unwrap(),
                )
            })
        })
        .collect();
    let catalog = ProviderContext {
        custom_types: ProviderCustomTypes::loaded(known, Box::new(|_, _| Ok(())))
            .with_declarations_for_test(declarations),
        ..ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let owned = resolve_target(
        &ConversionTypePath::new("shared.Name"),
        &location()?,
        &catalog,
    )
    .map_err(|error| error.to_string())?;
    let declaration =
        source_declaration(&owned, &location()?, &catalog).map_err(|error| error.to_string())?;
    assert_eq!(
        DeclarationBounds::from_declaration(&declaration),
        Some(DeclarationBounds::String {
            min: 4,
            max: Some(10)
        })
    );
    Ok(())
}

#[test]
fn conversion_source_declaration_resolution_errors_keep_the_source_kind() -> Result<(), String> {
    let location = location()?;
    let target = resolve_target(
        &ConversionTypePath::new("mock.Name"),
        &location,
        &super::tests::catalog(),
    )
    .map_err(|e| e.to_string())?;
    let unloaded = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let error = source_declaration(&target, &location, &unloaded)
        .expect_err("unloaded source evidence was accepted");
    assert_eq!(error.kind(), ConversionErrorKind::Source);
    assert_eq!(error.location(), &location);
    assert!(error.source().is_some());
    Ok(())
}

#[test]
fn conversion_error_redacts_display_debug_and_validation_cause() -> Result<(), String> {
    let location = location()?;
    let error = ConversionError::validation(
        &location,
        crate::parser::CustomTypeValidationError::Rejected {
            owner: "mock".into(),
            identity: TypeIdentity::from_dotted("mock.Name"),
            message: "sensitive-payload".into(),
        },
    )
    .inside_secret();
    assert_eq!(error.kind(), ConversionErrorKind::Target);
    assert_eq!(error.location(), &location);
    assert!(error.to_string().contains("(secret)"));
    let cause = error.source().ok_or("validation cause was lost")?;
    let rendered = format!("{error}\n{error:?}\n{cause}\n{cause:?}");
    assert!(!rendered.contains("sensitive-payload"), "{rendered}");
    Ok(())
}

#[test]
fn conversion_error_redaction_preserves_structured_resolution_cause() -> Result<(), String> {
    let mut error =
        ConversionError::new(ConversionErrorKind::Target, &location()?, "private-value");
    error.cause = Some(std::sync::Arc::new(ConversionCause::TypeResolution(
        crate::static_type::StaticTypeResolutionError::UnknownCustomType {
            name: "mock.Unknown".into(),
        },
    )));
    let error = error.inside_secret();
    let cause = error.source().ok_or("resolution cause was lost")?;
    assert!(
        cause
            .downcast_ref::<crate::static_type::StaticTypeResolutionError>()
            .is_some()
    );
    assert!(!format!("{error}\n{error:?}").contains("private-value"));
    Ok(())
}
