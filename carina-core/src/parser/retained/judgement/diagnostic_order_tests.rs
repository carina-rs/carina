use super::*;

const NAMES: [&str; 6] = ["zulu", "alpha", "quartz", "bravo", "mango", "delta"];

fn with_report(check: impl FnOnce(&ExpressionReport<crate::resource::AbsoluteBindingAddress>)) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/diagnostic_order/retained_instances");
    let path = crate::paths::ResolvedPath::new(root).unwrap();
    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::config_loader::parse_directory(&path, &context).unwrap();
    crate::validation::deferred_types::with_session_for_test(&context, |session| {
        let file = crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            &path,
            session,
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap();
        check(file.expression_report());
    });
}

#[test]
fn retained_module_outputs_keep_call_declaration_order() {
    with_report(|report| {
        assert_eq!(
            report
                .module_attributes
                .keys()
                .map(|(binding, _)| binding.as_str())
                .collect::<Vec<_>>(),
            NAMES
        );
    });
}

#[test]
fn retained_expression_instances_keep_call_declaration_order() {
    with_report(|report| {
        let location = report
            .expressions
            .keys()
            .find(|location| {
                location
                    .authored
                    .file()
                    .path()
                    .ends_with("choice/attributes.crn")
                    && location.authored.span().start_column() == 31
            })
            .expect("module attribute expression")
            .authored
            .clone();
        let names = report
            .instances_at(&location)
            .unwrap()
            .into_iter()
            .filter_map(|expression| {
                let (location, _) = report
                    .expressions
                    .iter()
                    .find(|(_, candidate)| std::ptr::eq(*candidate, expression))
                    .unwrap();
                match &location.instance {
                    ExpressionInstance::Root => None,
                    ExpressionInstance::Module(environment) => Some(environment.scope.as_ref()),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(names, NAMES);
    });
}
