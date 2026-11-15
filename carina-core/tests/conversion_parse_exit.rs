use carina_core::{
    EnvironmentSnapshot,
    parser::{ProviderContext, parse_file},
    schema::SchemaRegistry,
    validation::{self, PresentedFinding},
};
use std::{collections::HashSet, path::Path};

#[test]
fn parser_conversion_rejections_wait_for_the_presented_exit() {
    let catalog = ProviderContext::new(EnvironmentSnapshot::empty());
    for source in [
        "let value = { \"${42 as String}\" = 'ok' }",
        "let value = if 42 as String { 'ok' } else { 'ok' }",
        "let value = does_not_exist(42 as String)",
        "let value = ('ok' as String) + 1",
    ] {
        let parsed =
            parse_file(source, Path::new("parse-exit.crn"), &catalog).unwrap_or_else(|error| {
                panic!("parsing must retain the rejected Convert: {source}: {error}")
            });
        let schemas = SchemaRegistry::new();
        let configuration = carina_core::config_loader::load_buffer_configuration(
            carina_core::config_loader::issue_buffer_configuration(&parsed),
            &schemas,
            &catalog,
        );
        let judged = configuration.judged();
        let bindings = carina_core::binding_index::BindingIndex::from_parsed(
            judged.file(),
            &schemas,
            &catalog,
        );
        let findings = validation::resource_admission_findings(
            judged,
            &bindings,
            &HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(findings.len(), 1, "{source}: {findings:?}");
        for finding in findings {
            match finding {
                PresentedFinding::Ordinary(error) => {
                    panic!("conversion rejection escaped its channel: {error}")
                }
                PresentedFinding::Conversion(error) => {
                    assert_eq!(error.code(), "conversion_unsupported_position");
                    assert!(error.location().span().start_column() > 1);
                }
            }
        }
    }
}
