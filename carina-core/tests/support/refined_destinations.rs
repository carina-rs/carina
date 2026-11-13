//! Directory-backed forwarding boundaries. Authored annotations name an
//! identity; the final schema slot owns its separate member constraints.
use super::fixture;
use carina_core::{
    binding_index::{BindingIndex, BindingValueSource, ResolvedBindings},
    provider::LoadedProviderSnapshot,
    resource::{ConcreteValue, Value, knowledge::KnowledgeContext},
    validation::{
        admission::{Candidate, admit},
        deferred_types::with_session_for_test,
    },
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Resource,
    Let,
    Export,
    ModuleArgument,
    FunctionParameter,
    FunctionReturn,
    Attributes,
}

pub fn directory(boundary: Boundary, resolved_length: Option<usize>) -> tempfile::TempDir {
    let input =
        resolved_length.map_or_else(|| "source.name".into(), |n| format!("'{}'", "A".repeat(n)));
    let slot = |value: &str| format!("let sink = awscc.MemberSink {{ name = {value} }}\n");
    let (main, child) = match boundary {
        Boundary::Resource => (slot(&input), None),
        // Typed lets are not syntax; a let forwards identity-only value evidence.
        Boundary::Let => (format!("let name = {input}\n{}", slot("name")), None),
        Boundary::FunctionParameter => (
            format!(
                "fn forward(name: aws.Name): aws.Name {{ name }}\n{}",
                slot(&format!("forward({input})"))
            ),
            None,
        ),
        Boundary::FunctionReturn => (
            format!(
                "fn forward(): aws.Name {{ {input} }}\n{}",
                slot("forward()")
            ),
            None,
        ),
        Boundary::Export => (slot(&input), None),
        Boundary::Attributes => (
            format!(
                "attributes {{ name: aws.Name = {input} }}\n{}",
                slot(&input)
            ),
            None,
        ),
        Boundary::ModuleArgument => (
            format!(
                "let forward = use {{ source = './forward' }}\nlet out = forward {{ name = {input} }}\n"
            ),
            Some(format!("arguments {{ name: aws.Name }}\n{}", slot("name"))),
        ),
    };
    let exports = if matches!(boundary, Boundary::Export) {
        format!("exports {{ name: aws.Name = {input} }}\n")
    } else {
        "exports {}\n".into()
    };
    let dir = fixture::directory(
        &format!("let source = awscc.MemberSource {{}}\n{main}"),
        &exports,
    );
    if let Some(child) = child {
        std::fs::create_dir(dir.path().join("forward")).unwrap();
        std::fs::write(dir.path().join("forward/main.crn"), child).unwrap();
    }
    dir
}

pub fn check(
    boundary: Boundary,
    snapshot: &LoadedProviderSnapshot,
    length: Option<usize>,
) -> Result<(), String> {
    let dir = directory(boundary, None);
    let root = carina_core::paths::ResolvedPath::new(dir.path()).unwrap();
    let context = snapshot.provider_context().as_provider_context();
    let schemas = snapshot.schema_registry();
    let mut loaded =
        carina_core::config_loader::load_configuration_with_config(&root, context, schemas)?;
    with_session_for_test(snapshot.provider_context(), |session| {
        loaded.parsed.resolve_type_exprs_report(session)
    })
    .map_err(|e| e.to_string())?;
    let root_bindings = BindingIndex::from_parsed(loaded.parsed.parsed(), schemas, context);
    carina_core::validation::validate_user_function_ref_types_with_bindings(
        loaded.parsed.parsed(),
        &root_bindings,
    )?;
    let parsed = with_session_for_test(snapshot.provider_context(), |session| {
        carina_core::module_resolver::resolve_modules_with_config(
            loaded.parsed.admit(schemas, session),
            &root,
        )
    })
    .map_err(|e| e.to_string())?;
    carina_core::validation::validate_resources(
        &parsed,
        schemas,
        &HashSet::from(["awscc".into()]),
        context,
        &mut Vec::new(),
    )?;
    let bindings = BindingIndex::from_parsed(&parsed, schemas, context);
    carina_core::validation::validate_attribute_param_ref_types_with_bindings(
        &parsed.attribute_params,
        &bindings,
    )?;
    carina_core::validation::validate_export_param_ref_types_with_bindings(
        &parsed.export_params,
        &bindings,
    )
    .map_err(|e| {
        e.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let expanded = parsed
        .clone()
        .into_expanded(schemas, context)
        .map_err(|e| e.to_string())?;
    let sink = expanded
        .resources
        .iter()
        .find(|r| r.id.resource_type == "MemberSink")
        .expect("the actual member-bearing slot exists");
    let destination = schemas
        .resource_attribute_destination(&sink.id, "name", context)
        .unwrap();
    with_session_for_test(snapshot.provider_context(), |session| {
        use carina_core::parser::ExportParamLike;
        let value = match boundary {
            Boundary::Export => expanded.export_params[0].value().unwrap(),
            Boundary::Attributes => expanded.attribute_params[0]
                .value
                .as_ref()
                .unwrap()
                .value()
                .unwrap(),
            _ => &sink.attributes["name"],
        };
        let (candidate, context) = Candidate::validation_classified(
            value
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
        );
        let pending = admit(&destination, candidate, &context)
            .map_err(|e| e.resolution_error().to_string())?
            .ready()
            .expect_err("the source is unknown at validate time");
        assert!(
            !pending.sites().is_empty(),
            "{boundary:?}: reference resolution must remain pending"
        );
        if let Some(length) = length {
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                "source",
                HashMap::from([(
                    "name".into(),
                    Value::Concrete(ConcreteValue::String("A".repeat(length).into())),
                )]),
                BindingValueSource::Local,
            );
            pending
                .discharge(&resolved, session)
                .map_err(|e| e.resolution_error().to_string())?;
        }
        Ok(())
    })
}
