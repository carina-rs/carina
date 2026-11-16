//! The 216 recorded cells cover 92 reference forms and 16 same-file twins,
//! each with known and unknown sources, through production validate entries.
//! Refresh with this fixed command, then review the diff:
//! ~/.local/bin/heavy-slot nice -n 19 env CARGO_BUILD_JOBS=4 NEXTEST_TEST_THREADS=4 cargo nextest run --offline -p carina-cli --test authored_reference_cells --run-ignored only -E 'test(=record_authored_reference_cells)'
//! A PR that correctly changes diagnostic wording refreshes and reviews this file's golden.
//! The 16 erased-reference rows and their same-file twins record main's
//! known-accept / unknown-reject behaviour where observed. The node-judge PR
//! of #4400 changes those outcomes ON PURPOSE. An unexplained accept/reject
//! change is a regression, not a refresh. Only the temporary directory prefix
//! is normalized; panics always fail. The two excluded probes remain documented.
use carina_core::provider::{BoxFuture, ProviderFactory, ProviderInitialization, ProviderResult};
use carina_core::schema::{
    AttributeSchema, AttributeType as T, ResourceSchema, StringIdentityLayer,
    StringMemberConstraints, StructField, TypeError, TypeIdentity, ValidatedResourceSchema,
};
use serde_json::{Value as Json, json};
#[path = "support/resolver_kind_main.rs"]
mod baseline;

fn expected() -> T {
    T::refined_string(
        Some(TypeIdentity::from_dotted("mock.Expected")),
        None,
        None,
        None,
    )
}
struct Factory;
impl ProviderFactory for Factory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Authored reference cells #4400"
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { panic!("Cell measurement must never initialize a provider") })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("Source")
                .attribute(AttributeSchema::optional("name", T::string()).build())
                .attribute(AttributeSchema::optional("ready", T::bool()).build())
                .attribute(AttributeSchema::optional("count", T::int()).build()),
            ResourceSchema::new("CompatibleSource")
                .attribute(AttributeSchema::optional("ready", T::bool()).build())
                .attribute(AttributeSchema::optional("name", expected()).build())
                .attribute(
                    AttributeSchema::optional(
                        "count",
                        T::refined_int(Some(TypeIdentity::from_dotted("mock.ExpectedCount")), None),
                    )
                    .build(),
                ),
            ResourceSchema::new("ReferenceSource")
                .attribute(
                    AttributeSchema::optional(
                        "name",
                        T::reference(TypeIdentity::from_dotted("mock.OtherExpected"), None, None),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("count", T::int()).build()),
            ResourceSchema::new("CompatibleReferenceSource")
                .attribute(
                    AttributeSchema::optional(
                        "name",
                        T::reference(TypeIdentity::from_dotted("mock.Expected"), None, None),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("count", T::int()).build()),
            ResourceSchema::new("CompatibleCastSource")
                .attribute(
                    AttributeSchema::optional(
                        "name",
                        T::refined_string(
                            Some(TypeIdentity::from_dotted("mock.Cast")),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("count", T::int()).build()),
            ResourceSchema::new("CastSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::refined_string(
                        Some(TypeIdentity::from_dotted("mock.Cast")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            ),
            ResourceSchema::new("UnionListSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::union(vec![T::list(expected()), T::string()]),
                )
                .build(),
            ),
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("value", expected()).build()),
            ResourceSchema::new("OtherSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::refined_string(
                        Some(TypeIdentity::from_dotted("mock.OtherExpected")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            ),
            ResourceSchema::new("IntSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::refined_int(Some(TypeIdentity::from_dotted("mock.ExpectedCount")), None),
                )
                .build(),
            ),
            ResourceSchema::new("ListSink")
                .attribute(AttributeSchema::optional("value", T::list(expected())).build()),
            ResourceSchema::new("MapSink")
                .attribute(AttributeSchema::optional("value", T::map(expected())).build()),
            ResourceSchema::new("StructSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::struct_(
                        "Fields",
                        vec![StructField::required("a", expected()).build()],
                    ),
                )
                .build(),
            ),
            // A declaration-local VALUE restriction, not a different identity.
            // 'same' remains valid at CompatibleSource, but is too long here.
            ResourceSchema::new("LengthSink").attribute(
                AttributeSchema::optional(
                    "value",
                    T::string_member(
                        StringIdentityLayer::new(
                            Some(TypeIdentity::from_dotted("mock.Expected")),
                            None,
                            None,
                            None,
                        ),
                        StringMemberConstraints::new(None, Some((None, Some(3)))),
                    ),
                )
                .build(),
            ),
        ]
        .into_iter()
        .map(|s| s.validate().unwrap())
        .collect()
    }
}
fn factories() -> Vec<carina_cli::wiring::LoadedProviderFactory> {
    carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(Factory)])
}

#[derive(Clone)]
struct Row {
    id: &'static str,
    construct: &'static str,
    source: &'static str,
    main: String,
    same_file: bool,
    live_alternative: bool,
    reference_count: usize,
    known_value_count: usize,
    require_acceptance: bool,
    second_source: Option<&'static str>,
    source_suffix: String,
    source_override: Option<[String; 2]>,
    expected_instances: Option<Vec<&'static str>>,
    check_public_origin: bool,
    extra_files: Vec<(&'static str, String)>,
    expected_error_count: Option<usize>,
}
impl Row {
    fn new(
        id: &'static str,
        construct: &'static str,
        source: &'static str,
        sink: &str,
        expression: &str,
        reference_count: usize,
    ) -> Self {
        Self {
            id,
            construct,
            source,
            main: format!("let sink = mock.{sink} {{ value = {expression} }}\n"),
            same_file: false,
            live_alternative: false,
            reference_count,
            known_value_count: 0,
            require_acceptance: false,
            second_source: None,
            source_suffix: String::new(),
            source_override: None,
            expected_instances: None,
            check_public_origin: false,
            extra_files: Vec::new(),
            expected_error_count: None,
        }
    }
    fn accepting(mut self) -> Self {
        self.require_acceptance = true;
        self
    }
    fn files(&self, known: bool) -> Vec<(&'static str, String)> {
        let mut source = if let Some(overrides) = &self.source_override {
            overrides[usize::from(!known)].clone()
        } else if self.live_alternative {
            // x stays pending in BOTH runs; y is the varying source.
            format!(
                "{}{}",
                source_block("x", "Source", false),
                source_block("y", self.source, known)
            )
        } else {
            source_block("x", self.source, known)
        };
        if let Some(schema) = self.second_source {
            source.push_str(&source_block("y", schema, known));
        }
        source.push_str(&self.source_suffix);
        let (sources, main) = if self.same_file {
            (String::new(), format!("{source}{}", self.main))
        } else {
            (source, self.main.clone())
        };
        let mut files = vec![
            ("sources.crn", sources),
            ("main.crn", main),
            ("providers.crn", "provider mock {}\n".into()),
            (
                "backend.crn",
                "backend local { path = 'state.json' }\n".into(),
            ),
        ];
        files.extend(self.extra_files.clone());
        files
    }
}
fn source_block(name: &str, schema: &str, known: bool) -> String {
    let mut body = format!("let {name} = mock.{schema} {{\n");
    for line in ["    name = 'same'", "    count = 1"] {
        body.push_str(&if known {
            line.to_string()
        } else {
            " ".repeat(line.len())
        });
        body.push('\n');
    }
    body.push_str("}\n");
    body
}

// Meaning/typed error determines the class; the row's expectation does not.
// A is deliberately NOT coerced into R or V. O is an unrelated lint.
fn classify(
    error: Option<&carina_cli::error::AppError>,
    diagnostic: &Json,
) -> (&'static str, &'static str) {
    fn is_value(error: &TypeError) -> bool {
        match error {
            TypeError::PatternMismatch { .. }
            | TypeError::LengthOutOfRange { .. }
            | TypeError::ValueOutOfRange { .. }
            | TypeError::CustomTypeValidation { .. }
            | TypeError::ValidationFailed { .. } => true,
            TypeError::ListItemError { inner, .. }
            | TypeError::MapKeyError { inner, .. }
            | TypeError::MapValueError { inner, .. }
            | TypeError::StructFieldError { inner, .. } => is_value(inner),
            _ => false,
        }
    }
    if diagnostic["severity"] == "warning" && diagnostic["type"] == "unused_binding" {
        return ("O", "unused-binding lint; neither R nor V");
    }
    if diagnostic["severity"] == "warning"
        && diagnostic["message"]
            .as_str()
            .unwrap()
            .starts_with("for-loop binding ")
        && diagnostic["message"]
            .as_str()
            .unwrap()
            .contains(" is unused.")
    {
        return ("O", "unused for-loop binding lint; neither R nor V");
    }
    if let Some(carina_cli::error::AppError::ResourceAdmission(finding)) = error
        && let Some(ty) = finding.type_error()
        && is_value(ty)
    {
        return ("V", "typed concrete-value constraint error");
    }
    if let Some(carina_cli::error::AppError::BindingUse(finding)) = error
        && matches!(
            &finding.error,
            carina_core::validation::BindingResolutionError::ReferenceType(_)
        )
    {
        return ("R", "typed binding-reference source/destination mismatch");
    }
    if let Some(carina_cli::error::AppError::ResourceAdmission(finding)) = error
        && let Some(TypeError::Reference { error, .. }) = finding.type_error()
        && matches!(
            error.as_ref(),
            carina_core::resource::SecretDiagnosticError::Public(
                carina_core::validation::admission::AdmissionReferenceError::Computed(
                    carina_core::validation::DeferredAccessTypeError::TypeMismatch { .. }
                )
            )
        )
    {
        return (
            "R",
            "typed reference/computed-access assignment mismatch; no concrete value constraint",
        );
    }
    let message = diagnostic["message"].as_str().unwrap();
    let human = error
        .map(|error| carina_cli::error::format_error_with_source_chain(error))
        .unwrap_or_default();
    // Public JSON and terminal assignment text use distinct renderers. A
    // direct source origin in either identifies the source type comparison.
    // Whole-expression origins are left ambiguous rather than guessed.
    if (message.contains("expected ") || human.contains("expected "))
        && (message.contains("(from ") || human.contains("(from "))
    {
        return (
            "R",
            "declared source type versus destination; direct/protected reference origin",
        );
    }
    (
        "A",
        "not unambiguously declared-reference type R or concrete-value V; retain verbatim",
    )
}
fn error_resource_id(error: &carina_cli::error::AppError) -> Option<String> {
    match error {
        carina_cli::error::AppError::ResourceAdmission(finding) => {
            finding.resource_id().map(ToString::to_string)
        }
        carina_cli::error::AppError::BindingUse(finding) => match &finding.use_site.owner {
            carina_core::parser::BindingUseOwner::Resource(address) => {
                Some(address.resource_id().to_string())
            }
            _ => None,
        },
        _ => None,
    }
}
fn observe(path: &carina_core::paths::ResolvedPath) -> Json {
    let facade = carina_cli::commands::validate::validate_with_factories(path, factories());
    let detailed = carina_cli::commands::validate::validate_diagnostics_with_factories(
        path,
        factories(),
        Default::default(),
    );
    let replay = detailed
        .errors
        .iter()
        .map(|error| carina_cli::error::format_error_with_source_chain(error))
        .collect::<Vec<_>>();
    let typed_error_vector = detailed
        .errors
        .iter()
        .map(|error| format!("{error:#?}"))
        .collect::<Vec<_>>();
    // A BindingUseReport can present several diagnostics from one AppError.
    // Expand only this typed grouping for classification/instance ownership;
    // keep the original facade and replay vectors verbatim in the record.
    let json_bytes = serde_json::to_string(&detailed.json).unwrap();
    let parse_warnings = detailed
        .warnings
        .iter()
        .map(|w| format!("{w:#?}"))
        .collect::<Vec<_>>();
    let input_warnings = detailed
        .input_warnings
        .iter()
        .map(|w| format!("{w:#?}"))
        .collect::<Vec<_>>();
    let typed_errors = detailed
        .errors
        .into_iter()
        .flat_map(|error| match error {
            carina_cli::error::AppError::BindingUseReport(report) => report
                .findings()
                .iter()
                .cloned()
                .map(|finding| carina_cli::error::AppError::BindingUse(Box::new(finding)))
                .collect::<Vec<_>>(),
            error => vec![error],
        })
        .collect::<Vec<_>>();
    let errors = detailed.json["errors"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let warnings = detailed.json["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let aligned = errors.len() == typed_errors.len();
    let facade_aligned = errors.len() == facade.len();
    let mut all = Vec::new();
    for (index, diagnostic) in errors.into_iter().enumerate() {
        let typed = aligned.then(|| &typed_errors[index]);
        let (class, reason) = classify(typed, &diagnostic);
        all.push(json!({"channel": "errors", "index": index, "class": class,
            "reason": reason, "diagnostic": diagnostic,
            "facade_error": if facade_aligned { facade.get(index) } else { None },
            "typed_rendered_error": typed.map(|error| carina_cli::error::format_error_with_source_chain(error)),
            "resource_id": typed.and_then(error_resource_id),
            "typed_error_debug": typed.map(|e| format!("{e:#?}"))}));
    }
    for (index, diagnostic) in warnings.into_iter().enumerate() {
        let (class, reason) = classify(None, &diagnostic);
        all.push(
            json!({"channel": "warnings", "index": index, "class": class,
            "reason": reason, "diagnostic": diagnostic}),
        );
    }
    assert_eq!(
        facade, replay,
        "production facade and diagnostics replay disagree"
    );
    json!({"status": if facade.is_empty() { "accept" } else { "reject" },
        "typed_error_vector": typed_error_vector,
        "json_bytes": json_bytes, "typed_parse_warnings": parse_warnings,
        "typed_input_warnings": input_warnings, "facade_errors": facade, "replayed_errors": replay, "facade_matches": facade == replay,
        "json": detailed.json, "diagnostics": all})
}

thread_local! {
    static ROWS: std::cell::RefCell<Vec<Row>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn measure(row: Row) {
    ROWS.with(|rows| rows.borrow_mut().push(row));
}
const DEFECT_ROWS: [&str; 16] = [
    "20-i", "21-i", "22-i", "23l-i", "23m-i", "24l-i", "24m-i", "25l-i", "25m-i", "26-i",
    "27-xbad", "27-ybad", "28-i", "30l-i", "30m-i", "30s-i",
];
fn all_rows() -> Vec<Row> {
    ROWS.with(|rows| assert!(rows.borrow().is_empty()));
    reference_4400_row1_top();
    reference_4400_row2_list();
    reference_4400_row3_map();
    reference_4400_row3_struct();
    reference_4400_row4_secret();
    reference_4400_row4b_secret_literal();
    reference_4400_row5_selected();
    reference_4400_row5_live_alternative();
    reference_4400_row6_input_without_reevaluation();
    reference_4400_row7_literal();
    reference_4400_row8_compatible();
    reference_4400_row9_known_value_constraint();
    reference_4400_row10a_interpolation_incompatible();
    reference_4400_row10a_interpolation_compatible();
    reference_4400_row10b_arithmetic_incompatible();
    reference_4400_row10b_arithmetic_compatible();
    reference_4400_row10c_upper_incompatible();
    reference_4400_row10c_upper_compatible();
    reference_4400_row11_if_incompatible();
    reference_4400_row11_if_compatible();
    reference_4400_row12_same_file_incompatible();
    reference_4400_row12_same_file_compatible();
    reference_4400_row13_alias_incompatible();
    reference_4400_row13_alias_compatible();
    reference_4400_row14_two_sinks();
    reference_4400_row20_sole_incompatible();
    reference_4400_row20_sole_compatible();
    reference_4400_row21_list_incompatible();
    reference_4400_row21_list_compatible();
    reference_4400_row22_map_incompatible();
    reference_4400_row22_map_compatible();
    reference_4400_row23_map_incompatible();
    reference_4400_row23_map_compatible();
    reference_4400_row23_list_incompatible();
    reference_4400_row23_list_compatible();
    reference_4400_row24_list_incompatible();
    reference_4400_row24_list_compatible();
    reference_4400_row24_map_incompatible();
    reference_4400_row24_map_compatible();
    reference_4400_row25_list_incompatible();
    reference_4400_row25_list_compatible();
    reference_4400_row25_map_incompatible();
    reference_4400_row25_map_compatible();
    reference_4400_row26_inner_if_incompatible();
    reference_4400_row26_inner_if_compatible();
    reference_4400_row27_bad_first();
    reference_4400_row27_bad_second();
    reference_4400_row27_both_compatible();
    reference_4400_row28_resource_map_incompatible();
    reference_4400_row28_resource_map_compatible();
    reference_4400_row29_public_first_incompatible();
    reference_4400_row29_secret_first_incompatible();
    reference_4400_row29_public_first_compatible();
    reference_4400_row29_secret_first_compatible();
    reference_4400_row30_sole_incompatible();
    reference_4400_row30_sole_compatible();
    reference_4400_row30_list_incompatible();
    reference_4400_row30_list_compatible();
    reference_4400_row30_map_incompatible();
    reference_4400_row30_map_compatible();
    reference_4400_row31lookup_i();
    reference_4400_row31lookup_c();
    reference_4400_row31values_i();
    reference_4400_row31values_c();
    reference_4400_row31concat_i();
    reference_4400_row31concat_c();
    reference_4400_row31flatten_i();
    reference_4400_row31flatten_c();
    reference_4400_row32id_i();
    reference_4400_row32id_c();
    reference_4400_row36union_i();
    reference_4400_row36union_c();
    reference_4400_row38unknownif_i();
    reference_4400_row38unknownif_c();
    reference_4400_row33param_si_di();
    reference_4400_row33param_si_dc();
    reference_4400_row33param_sc_di();
    reference_4400_row33param_sc_dc();
    reference_4400_row34declalias_si_di();
    reference_4400_row34declalias_si_dc();
    reference_4400_row34declalias_sc_di();
    reference_4400_row34declalias_sc_dc();
    reference_4400_row35fnparam_si_di();
    reference_4400_row35fnparam_si_dc();
    reference_4400_row35fnparam_sc_di();
    reference_4400_row35fnparam_sc_dc();
    reference_4400_row37ref_i();
    reference_4400_row37ref_c();
    reference_4400_row37coalesce_i();
    reference_4400_row37coalesce_c();
    reference_4400_row39typedlet_syntax_i();
    reference_4400_row39typedlet_syntax_c();
    let mut rows = ROWS.with(|rows| rows.take());
    assert_eq!(rows.len(), 92);
    for id in DEFECT_ROWS {
        let mut twin = rows.iter().find(|row| row.id == id).unwrap().clone();
        twin.same_file = true;
        rows.push(twin);
    }
    rows
}

// Excluded probes, intentionally outside the 92 originals:
// 6-reevaluation: CLI admission uses ValidationEvaluation::None. upper('same')
// folds before admission; whole-map reevaluation is only on LSP expanded /
// relative_static paths, outside this collector.
// 28-dot: sibling-file built.first.name fails name resolution (unknown binding
// 'built') before source-type admission. Row 28 uses built['first'].name.
const SKIPPED_PROBES: [(&str, &str); 2] = [
    (
        "6-reevaluation",
        "CLI admission uses ValidationEvaluation::None; upper('same') folds before admission. Whole-map reevaluation is only on LSP expanded/relative_static paths.",
    ),
    (
        "28-dot",
        "Sibling-file built.first.name fails name resolution (unknown binding 'built') before source-type admission. Row 28 uses built['first'].name.",
    ),
];

fn cells() -> Json {
    let mut records = Vec::new();
    for (index, row) in all_rows().into_iter().enumerate() {
        let id = if index < 92 {
            row.id.to_owned()
        } else {
            format!("{}-samefile", row.id)
        };
        let dir = tempfile::Builder::new()
            .prefix("pr1-4400-")
            .tempdir()
            .unwrap();
        let path = carina_core::paths::ResolvedPath::new(dir.path()).unwrap();
        let mut cells = Vec::new();
        for known in [true, false] {
            let files = row.files(known);
            for (name, body) in &files {
                let file = dir.path().join(name);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, body).unwrap();
            }
            // Deliberately no catch_unwind: a panic is never a recorded rejection.
            let observed = observe(&path);
            cells.push(json!({"known": known, "files": files,
                "observation": baseline::normalize_prefix(observed, dir.path(), "<DIR>")}));
        }
        records.push(json!({"row": id, "construct": row.construct, "cells": cells,
            "original_expectations": {
                "reference_count": row.reference_count, "known_value_count": row.known_value_count,
                "require_acceptance": row.require_acceptance, "expected_instances": row.expected_instances,
                "check_public_origin": row.check_public_origin, "expected_error_count": row.expected_error_count
            }}));
    }
    json!(records)
}

#[test]
fn authored_reference_cells_match_recorded() {
    let golden: Json =
        serde_json::from_slice(&std::fs::read(baseline::golden_path()).unwrap()).unwrap();
    assert_eq!(
        golden["header"],
        json!({"first_recorded_on": baseline::FIRST_RECORDED_ON, "original_rows": 92, "samefile_rows": 16, "rows": 108, "cells": 216})
    );
    assert_eq!(golden["skipped_probes"], json!(SKIPPED_PROBES));
    assert_eq!(golden["rows"].as_array().unwrap().len(), 108);
    let actual = cells();
    if golden["rows"] != actual {
        eprintln!(
            "actual cells: {}",
            baseline::save_actual("cells", &serde_json::to_vec_pretty(&actual).unwrap()).display()
        );
    }
    baseline::assert_json_eq(&golden["rows"], &actual, "4400 cells");
}

#[test]
#[ignore = "refresh the recorded cells, then review the diff"]
fn record_authored_reference_cells() {
    let header = json!({"first_recorded_on": baseline::FIRST_RECORDED_ON, "original_rows": 92, "samefile_rows": 16, "rows": 108, "cells": 216});
    let golden = json!({"header": header, "normalization": "temporary directory prefix only",
        "json_bytes": "compact serialization of validate_diagnostics_with_factories.json",
        "skipped_probes": SKIPPED_PROBES, "rows": cells()});

    std::fs::write(
        baseline::golden_path(),
        serde_json::to_string_pretty(&golden).unwrap() + "\n",
    )
    .unwrap();
}
// Original row constructors. R/V metadata below is historical, not asserted.
macro_rules! row {
    ($test:ident, $id:literal, $construct:literal, $source:literal, $sink:literal, $expr:literal, $r:literal) => {
        fn $test() {
            measure(Row::new($id, $construct, $source, $sink, $expr, $r));
        }
    };
}
macro_rules! accepting_row {
    ($test:ident, $id:literal, $construct:literal, $source:literal, $sink:literal, $expr:literal, $r:literal) => {
        fn $test() {
            measure(Row::new($id, $construct, $source, $sink, $expr, $r).accepting());
        }
    };
}
// 1 R: one identical String -> mock.Expected rejection. V: none.
row!(
    reference_4400_row1_top,
    "1",
    "attribute root: x.name",
    "Source",
    "Sink",
    "x.name",
    1
);
// 2 R: one identical rejection at the list item. V: none.
row!(
    reference_4400_row2_list,
    "2",
    "list item: [x.name]",
    "Source",
    "ListSink",
    "[x.name]",
    1
);
// 3 R: one identical rejection at map value a. V: none.
row!(
    reference_4400_row3_map,
    "3",
    "map value: { a = x.name }",
    "Source",
    "MapSink",
    "{ a = x.name }",
    1
);
// 3b R: one identical rejection at struct field a. V: none.
row!(
    reference_4400_row3_struct,
    "3b",
    "struct field: { a = x.name }",
    "Source",
    "StructSink",
    "{ a = x.name }",
    1
);
// 4 R: one identical protected source-type rejection. V: none.
row!(
    reference_4400_row4_secret,
    "4",
    "secret(x.name)",
    "Source",
    "Sink",
    "secret(x.name)",
    1
);
// Keep the original secret-literal control. R: none. V: none. Accept both.
accepting_row!(
    reference_4400_row4b_secret_literal,
    "4b",
    "secret literal: secret('same')",
    "Source",
    "Sink",
    "secret('same')",
    0
);
// 5a R: x rejected once per side, equal FULL counts/text. V: none;
// fallback is a valid literal, and x's concrete value has no constraint.
row!(
    reference_4400_row5_selected,
    "5a",
    "selected operand: x.name ?? 'fallback'",
    "Source",
    "Sink",
    "x.name ?? 'fallback'",
    1
);

fn reference_4400_row5_live_alternative() {
    // R: x and y each rejected once, identical stream when y becomes pending.
    // V: none. x is pending in BOTH runs; only y switches known/unknown.
    let mut row = Row::new(
        "5b",
        "live alternative: x.name ?? y.name (x pending; y varies)",
        "Source",
        "Sink",
        "x.name ?? y.name",
        2,
    );
    row.live_alternative = true;
    measure(row);
}
// 6 R: only a's reference rejected, identically once per side. V: none.
// b's statically evaluable call creates a valid literal-like expression value.
row!(
    reference_4400_row6_input_without_reevaluation,
    "6",
    "map reference + static call: { a = x.name, b = upper('same') }",
    "Source",
    "MapSink",
    "{ a = x.name, b = upper('same') }",
    1
);

// 7 R: none; literal equals known source but has no reference. V: none. Accept.
accepting_row!(
    reference_4400_row7_literal,
    "7",
    "literal equal to known source: 'same'",
    "Source",
    "Sink",
    "'same'",
    0
);
// 8 R: none, mock.Expected -> mock.Expected. V: none. Accept both.
accepting_row!(
    reference_4400_row8_compatible,
    "8",
    "compatible reference: x.name",
    "CompatibleSource",
    "Sink",
    "x.name",
    0
);

fn reference_4400_row9_known_value_constraint() {
    // R: none; same nominal identity. V: one length error for 'same' (4 > 3),
    // none when no value is available. The source itself has no length limit.
    let mut row = Row::new(
        "9",
        "compatible type; destination member length <= 3",
        "CompatibleSource",
        "LengthSink",
        "x.name",
        0,
    );
    row.known_value_count = 1;
    measure(row);
}
// 10a-i R: none. Source String is incompatible with Sink DIRECTLY, but valid
// for interpolation, which computes a new String. V: none. Accept both.
accepting_row!(
    reference_4400_row10a_interpolation_incompatible,
    "10a-i",
    "interpolation, directly incompatible source: p-${x.name}",
    "Source",
    "Sink",
    "\"p-${x.name}\"",
    0
);
// 10a-c R: none; nominal String also valid as interpolation input. V: none.
accepting_row!(
    reference_4400_row10a_interpolation_compatible,
    "10a-c",
    "interpolation, directly compatible source: p-${x.name}",
    "CompatibleSource",
    "Sink",
    "\"p-${x.name}\"",
    0
);
// 10b-i R: none; plain Int is valid as an addition operand, despite lacking
// mock.ExpectedCount identity. The sum is a new Int expression. V: none.
// This is R/V and full-stream parity, not an assertion that a computed Int is
// implicitly admitted to a nominal Int destination. Any result-type findings
// remain A (unforced) and must have identical content/count on both sides.
row!(
    reference_4400_row10b_arithmetic_incompatible,
    "10b-i",
    "addition, directly incompatible source: x.count + 1",
    "Source",
    "IntSink",
    "x.count + 1",
    0
);
// 10b-c R: none; nominal Int also valid as addition input. V: none.
row!(
    reference_4400_row10b_arithmetic_compatible,
    "10b-c",
    "addition, directly compatible source: x.count + 1",
    "CompatibleSource",
    "IntSink",
    "x.count + 1",
    0
);
// 10c-i R: none; upper accepts String and returns a new String. V: none.
accepting_row!(
    reference_4400_row10c_upper_incompatible,
    "10c-i",
    "upper(x.name), directly incompatible source",
    "Source",
    "Sink",
    "upper(x.name)",
    0
);
// 10c-c R: none; upper also accepts the compatible nominal String. V: none.
accepting_row!(
    reference_4400_row10c_upper_compatible,
    "10c-c",
    "upper(x.name), directly compatible source",
    "CompatibleSource",
    "Sink",
    "upper(x.name)",
    0
);
// 11 DSL supports if/else VALUES in resource attributes, not only modules.
// Condition must be static. The selected x.name independently changes knownness.
// 11-i R: one identical rejection of selected source. V: none.
row!(
    reference_4400_row11_if_incompatible,
    "11-i",
    "if true { x.name } else { 'fallback' }, incompatible",
    "Source",
    "Sink",
    "if true { x.name } else { 'fallback' }",
    1
);
// 11-c R: none for compatible selected branch. V: none. Accept both.
accepting_row!(
    reference_4400_row11_if_compatible,
    "11-c",
    "if true { x.name } else { 'fallback' }, compatible",
    "CompatibleSource",
    "Sink",
    "if true { x.name } else { 'fallback' }",
    0
);

fn reference_4400_row12_same_file_incompatible() {
    // R: one identical rejection (same-file control for 1). V: none.
    let mut row = Row::new(
        "12-i",
        "same-file reference, incompatible (control for 1)",
        "Source",
        "Sink",
        "x.name",
        1,
    );
    row.same_file = true;
    measure(row);
}

fn reference_4400_row12_same_file_compatible() {
    // R: none (same-file control for 8). V: none. Accept both.
    let mut row = Row::new(
        "12-c",
        "same-file reference, compatible (control for 8)",
        "CompatibleSource",
        "Sink",
        "x.name",
        0,
    );
    row.same_file = true;
    measure(row.accepting());
}

fn reference_4400_row13_alias_incompatible() {
    // R: one identical rejection through let n = x.name. V: none.
    let mut row = Row::new(
        "13-i",
        "let n = x.name; value = n, incompatible",
        "Source",
        "Sink",
        "n",
        1,
    );
    row.main = format!("let n = x.name\n{}", row.main);
    measure(row);
}

fn reference_4400_row13_alias_compatible() {
    // R: none through compatible alias. V: none. Accept both.
    let mut row = Row::new(
        "13-c",
        "let n = x.name; value = n, compatible",
        "CompatibleSource",
        "Sink",
        "n",
        0,
    );
    row.main = format!("let n = x.name\n{}", row.main);
    measure(row.accepting());
}

fn reference_4400_row14_two_sinks() {
    // R: only OtherSink rejects mock.Expected; Sink accepts it. One identical
    // finding per side. Rejecting every reference would yield TWO and fail.
    // V: none. These are two distinct authored reference uses.
    let mut row = Row::new(
        "14",
        "x.name at sinks mock.Expected and mock.OtherExpected",
        "CompatibleSource",
        "Sink",
        "x.name",
        1,
    );
    row.main = "let accepted = mock.Sink { value = x.name }\nlet rejected = mock.OtherSink { value = x.name }\n".into();
    measure(row);
}

// SECOND MEASUREMENT: the issue's expression forms. Every issue_row has:
// R: exactly the stated count on BOTH sides, full content/order/location equal;
//    zero means this compatible reference must be accepted, not just R-free.
// V: NONE on either side ('same' satisfies every source/destination value rule).
// Aliases/containers are authored in sources.crn; only their use is in main.crn.
// Both candidates have the SAME known literal: values cannot identify origins.
macro_rules! issue_row {
    ($test:ident, $id:literal, $construct:literal, $source:literal,
     $definitions:literal, $expr:literal, $pair:literal, $r:literal) => {
        fn $test() {
            let mut row = Row::new($id, $construct, $source, "Sink", $expr, $r);
            row.source_suffix = $definitions.into();
            if $pair {
                row.second_source = Some($source);
            }
            if $r == 0 {
                row = row.accepting();
            }
            measure(row);
        }
    };
}
// 20: Sole interpolation denotes the source reference, not a new String.
issue_row!(
    reference_4400_row20_sole_incompatible,
    "20-i",
    "sole interpolation, incompatible",
    "Source",
    "",
    "\"${x.name}\"",
    false,
    1
);
issue_row!(
    reference_4400_row20_sole_compatible,
    "20-c",
    "sole interpolation, compatible",
    "CompatibleSource",
    "",
    "\"${x.name}\"",
    false,
    0
);
// 21: The chosen element retains x.name's declared type.
issue_row!(
    reference_4400_row21_list_incompatible,
    "21-i",
    "[x.name][0], incompatible",
    "Source",
    "",
    "[x.name][0]",
    false,
    1
);
issue_row!(
    reference_4400_row21_list_compatible,
    "21-c",
    "[x.name][0], compatible",
    "CompatibleSource",
    "",
    "[x.name][0]",
    false,
    0
);
// 22: The chosen field retains x.name's declared type.
issue_row!(
    reference_4400_row22_map_incompatible,
    "22-i",
    "{ f = x.name }.f, incompatible",
    "Source",
    "",
    "{ f = x.name }.f",
    false,
    1
);
issue_row!(
    reference_4400_row22_map_compatible,
    "22-c",
    "{ f = x.name }.f, compatible",
    "CompatibleSource",
    "",
    "{ f = x.name }.f",
    false,
    0
);
// 23: Cross-file let container projection, for both container shapes.
issue_row!(
    reference_4400_row23_map_incompatible,
    "23m-i",
    "let m = { f = x.name }; m.f, incompatible",
    "Source",
    "let m = { f = x.name }\n",
    "m.f",
    false,
    1
);
issue_row!(
    reference_4400_row23_map_compatible,
    "23m-c",
    "let m = { f = x.name }; m.f, compatible",
    "CompatibleSource",
    "let m = { f = x.name }\n",
    "m.f",
    false,
    0
);
issue_row!(
    reference_4400_row23_list_incompatible,
    "23l-i",
    "let l = [x.name]; l[0], incompatible",
    "Source",
    "let l = [x.name]\n",
    "l[0]",
    false,
    1
);
issue_row!(
    reference_4400_row23_list_compatible,
    "23l-c",
    "let l = [x.name]; l[0], compatible",
    "CompatibleSource",
    "let l = [x.name]\n",
    "l[0]",
    false,
    0
);
// 24: Only the statically selected branch is live; expect one x finding.
issue_row!(
    reference_4400_row24_list_incompatible,
    "24l-i",
    "let s = if true { [x.name] } else { [y.name] }; s[0], incompatible",
    "Source",
    "let s = if true { [x.name] } else { [y.name] }\n",
    "s[0]",
    true,
    1
);
issue_row!(
    reference_4400_row24_list_compatible,
    "24l-c",
    "let s = if true { [x.name] } else { [y.name] }; s[0], compatible",
    "CompatibleSource",
    "let s = if true { [x.name] } else { [y.name] }\n",
    "s[0]",
    true,
    0
);
issue_row!(
    reference_4400_row24_map_incompatible,
    "24m-i",
    "let s = if true { { f = x.name } } else { { f = y.name } }; s.f, incompatible",
    "Source",
    "let s = if true { { f = x.name } } else { { f = y.name } }\n",
    "s.f",
    true,
    1
);
issue_row!(
    reference_4400_row24_map_compatible,
    "24m-c",
    "let s = if true { { f = x.name } } else { { f = y.name } }; s.f, compatible",
    "CompatibleSource",
    "let s = if true { { f = x.name } } else { { f = y.name } }\n",
    "s.f",
    true,
    0
);
// 25: The non-null container is present even when its leaf is unknown.
// R is the selected x reference's finding once, not both candidate sources.
issue_row!(
    reference_4400_row25_list_incompatible,
    "25l-i",
    "let s = [x.name] ?? [y.name]; s[0], incompatible",
    "Source",
    "let s = [x.name] ?? [y.name]\n",
    "s[0]",
    true,
    1
);
issue_row!(
    reference_4400_row25_list_compatible,
    "25l-c",
    "let s = [x.name] ?? [y.name]; s[0], compatible",
    "CompatibleSource",
    "let s = [x.name] ?? [y.name]\n",
    "s[0]",
    true,
    0
);
issue_row!(
    reference_4400_row25_map_incompatible,
    "25m-i",
    "let s = { f = x.name } ?? { f = y.name }; s.f, incompatible",
    "Source",
    "let s = { f = x.name } ?? { f = y.name }\n",
    "s.f",
    true,
    1
);
issue_row!(
    reference_4400_row25_map_compatible,
    "25m-c",
    "let s = { f = x.name } ?? { f = y.name }; s.f, compatible",
    "CompatibleSource",
    "let s = { f = x.name } ?? { f = y.name }\n",
    "s.f",
    true,
    0
);
// 26: Conditional INSIDE the projected list item; selected source remains x.
issue_row!(
    reference_4400_row26_inner_if_incompatible,
    "26-i",
    "[if true { x.name } else { y.name }][0], incompatible",
    "Source",
    "",
    "[if true { x.name } else { y.name }][0]",
    true,
    1
);
issue_row!(
    reference_4400_row26_inner_if_compatible,
    "26-c",
    "[if true { x.name } else { y.name }][0], compatible",
    "CompatibleSource",
    "",
    "[if true { x.name } else { y.name }][0]",
    true,
    0
);

fn loop_selection(
    id: &'static str,
    construct: &'static str,
    x: &'static str,
    y: &'static str,
    rejected: Vec<&'static str>,
) {
    // R: ONLY the resource instance selecting a non-fitting source rejects,
    // once, with the same full findings for known and unknown. V: none.
    let mut row = Row::new(id, construct, x, "Sink", "", rejected.len());
    row.second_source = Some(y);
    row.main =
        "let probes = for i in [0, 1] {\n  mock.Sink {\n    value = [x.name, y.name][i]\n  }\n}\n"
            .into();
    if rejected.is_empty() {
        row = row.accepting();
    }
    row.expected_instances = Some(rejected);
    measure(row);
}

fn reference_4400_row27_bad_first() {
    loop_selection(
        "27-xbad",
        "for i in [0,1]: [x.name,y.name][i]; only x incompatible",
        "Source",
        "CompatibleSource",
        vec!["mock.Sink.probes[0]"],
    );
}

fn reference_4400_row27_bad_second() {
    loop_selection(
        "27-ybad",
        "for i in [0,1]: [x.name,y.name][i]; only y incompatible",
        "CompatibleSource",
        "Source",
        vec!["mock.Sink.probes[1]"],
    );
}

fn reference_4400_row27_both_compatible() {
    loop_selection(
        "27-c",
        "for i in [0,1]: [x.name,y.name][i]; both compatible",
        "CompatibleSource",
        "CompatibleSource",
        vec![],
    );
}
fn for_map_source(schema: &str, known: bool) -> String {
    // Both resources are themselves the sources. This is not a projection
    // through an authored x.name operand that could supply source recovery.
    let block = source_block("unused", schema, known).replacen(
        &format!("let unused = mock.{schema}"),
        &format!("  mock.{schema}"),
        1,
    );
    format!(
        "let input = {{ first = 0, second = 1 }}\nlet built = for key, _ in input {{\n{block}}}\n"
    )
}
fn resource_map_read(
    id: &'static str,
    construct: &'static str,
    schema: &'static str,
    count: usize,
) {
    // R: the selected built.first.name type is rejected once, or accepted,
    // according to its declaration, equally known/unknown. V: none.
    let mut row = Row::new(id, construct, schema, "Sink", "built['first'].name", count);
    row.source_override = Some([for_map_source(schema, true), for_map_source(schema, false)]);
    if count == 0 {
        row = row.accepting();
    }
    measure(row);
}

fn reference_4400_row28_resource_map_incompatible() {
    resource_map_read(
        "28-i",
        "for-built map of Source resources; built['first'].name, incompatible",
        "Source",
        1,
    );
}

fn reference_4400_row28_resource_map_compatible() {
    resource_map_read(
        "28-c",
        "for-built map of CompatibleSource resources; built['first'].name, compatible",
        "CompatibleSource",
        0,
    );
}
fn alias_public_secret(
    id: &'static str,
    construct: &'static str,
    schema: &'static str,
    public_first: bool,
    count: usize,
) {
    // R: two rejections (public and protected) for the incompatible source;
    // none for the compatible source. Full known/unknown parity. V: none.
    // Additionally, the PUBLIC finding must not acquire a protected origin
    // just because the same let alias has another use inside secret(...).
    let mut row = Row::new(id, construct, schema, "Sink", "n", count);
    row.source_suffix = "let n = x.name\n".into();
    let public = "let public = mock.Sink { value = n }\n";
    let protected = "let protected = mock.Sink { value = secret(n) }\n";
    row.main = if public_first {
        format!("{public}{protected}")
    } else {
        format!("{protected}{public}")
    };
    row.check_public_origin = true;
    if count == 0 {
        row = row.accepting();
    }
    measure(row);
}
// Main emits one finding: lexical binding-use validation rejects public n
// and stops the directory before resource admission can visit secret(n).

fn reference_4400_row29_public_first_incompatible() {
    alias_public_secret(
        "29-p-i",
        "let n = x.name; public n then secret(n), incompatible",
        "Source",
        true,
        1,
    );
}

fn reference_4400_row29_secret_first_incompatible() {
    alias_public_secret(
        "29-s-i",
        "let n = x.name; secret(n) then public n, incompatible",
        "Source",
        false,
        1,
    );
}

fn reference_4400_row29_public_first_compatible() {
    alias_public_secret(
        "29-p-c",
        "let n = x.name; public n then secret(n), compatible",
        "CompatibleSource",
        true,
        0,
    );
}

fn reference_4400_row29_secret_first_compatible() {
    alias_public_secret(
        "29-s-c",
        "let n = x.name; secret(n) then public n, compatible",
        "CompatibleSource",
        false,
        0,
    );
}
// 30: Reference-typed attribute in place of the String-typed source in 20-22.
// Incompatible identity mock.OtherExpected -> mock.Expected rejects once;
// compatible Reference identity mock.Expected -> identified String accepts.
// Literal known values and the omitted unknown attribute are built identically.
issue_row!(
    reference_4400_row30_sole_incompatible,
    "30s-i",
    "sole interpolation, incompatible Reference source",
    "ReferenceSource",
    "",
    "\"${x.name}\"",
    false,
    1
);
issue_row!(
    reference_4400_row30_sole_compatible,
    "30s-c",
    "sole interpolation, compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "\"${x.name}\"",
    false,
    0
);
issue_row!(
    reference_4400_row30_list_incompatible,
    "30l-i",
    "[x.name][0], incompatible Reference source",
    "ReferenceSource",
    "",
    "[x.name][0]",
    false,
    1
);
issue_row!(
    reference_4400_row30_list_compatible,
    "30l-c",
    "[x.name][0], compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "[x.name][0]",
    false,
    0
);
issue_row!(
    reference_4400_row30_map_incompatible,
    "30m-i",
    "{ f = x.name }.f, incompatible Reference source",
    "ReferenceSource",
    "",
    "{ f = x.name }.f",
    false,
    1
);
issue_row!(
    reference_4400_row30_map_compatible,
    "30m-c",
    "{ f = x.name }.f, compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "{ f = x.name }.f",
    false,
    0
);

// Round 3: critique discriminators. Every row varies x's value known/unknown;
// omitted attributes leave the raw unresolved reference on the unknown side.
fn round3(
    id: &'static str,
    construct: &'static str,
    compatible: bool,
    sink: &str,
    expression: &str,
    count: usize,
    prefix: &str,
) {
    let mut row = Row::new(
        id,
        construct,
        if compatible {
            "CompatibleSource"
        } else {
            "Source"
        },
        sink,
        expression,
        count,
    );
    row.main = format!("{prefix}{}", row.main);
    if id.starts_with("35fnparam-") {
        row.reference_count = 0;
        row.require_acceptance = false;
        row.expected_error_count = Some(if id.ends_with("-dc") { 0 } else { 2 });
    }
    if id.starts_with("38unknownif-") || id.starts_with("39typedlet-") {
        row.reference_count = 0;
        row.expected_error_count = Some(1);
        row.require_acceptance = false;
    }
    if compatible && count == 0 && row.expected_error_count.is_none() {
        row = row.accepting();
    }
    measure(row);
}
fn round3_module(
    id: &'static str,
    construct: &'static str,
    compatible: bool,
    declaration: &str,
    exported: bool,
    count: usize,
) {
    let mut row = Row::new(
        id,
        construct,
        if compatible {
            "CompatibleSource"
        } else {
            "Source"
        },
        "Sink",
        "child.output",
        count,
    );
    row.main =
        "let template = use { source = './child' }\nlet child = template { input = x.name }\n"
            .into();
    let child = if exported {
        row.main
            .push_str("let n = child.output\nlet sink = mock.Sink { value = n }\n");
        format!(
            "arguments {{ input: String }}\nlet n = input\nattributes {{ output: {declaration} = n }}\n"
        )
    } else {
        format!("arguments {{ input: {declaration} }}\nlet n = input\nmock.Sink {{ value = n }}\n")
    };
    row.extra_files.push(("child/main.crn", child));
    // These counts are main's measured declaration-boundary behaviour.
    if exported {
        row.reference_count = 1;
        row.expected_error_count = Some(if declaration == "mock.Expected" { 2 } else { 1 });
    } else if declaration == "String" {
        row.reference_count = 3;
        row.expected_error_count = Some(3);
    }
    if compatible && count == 0 && row.expected_error_count.is_none() {
        row = row.accepting();
    }
    measure(row);
}

fn reference_4400_row31lookup_i() {
    round3(
        "31lookup-i",
        "lookup({ a = x.name }, 'a', 'fallback')",
        false,
        "Sink",
        "lookup({ a = x.name }, 'a', 'fallback')",
        1,
        "",
    );
}

fn reference_4400_row31lookup_c() {
    round3(
        "31lookup-c",
        "lookup({ a = x.name }, 'a', 'fallback')",
        true,
        "Sink",
        "lookup({ a = x.name }, 'a', 'fallback')",
        0,
        "",
    );
}

fn reference_4400_row31values_i() {
    round3(
        "31values-i",
        "values({ a = x.name })[0]",
        false,
        "Sink",
        "values({ a = x.name })[0]",
        1,
        "",
    );
}

fn reference_4400_row31values_c() {
    round3(
        "31values-c",
        "values({ a = x.name })[0]",
        true,
        "Sink",
        "values({ a = x.name })[0]",
        0,
        "",
    );
}

fn reference_4400_row31concat_i() {
    round3(
        "31concat-i",
        "concat([x.name], [])[0]",
        false,
        "Sink",
        "concat([x.name], [])[0]",
        1,
        "",
    );
}

fn reference_4400_row31concat_c() {
    round3(
        "31concat-c",
        "concat([x.name], [])[0]",
        true,
        "Sink",
        "concat([x.name], [])[0]",
        0,
        "",
    );
}

fn reference_4400_row31flatten_i() {
    round3(
        "31flatten-i",
        "flatten([[x.name]])[0]",
        false,
        "Sink",
        "flatten([[x.name]])[0]",
        1,
        "",
    );
}

fn reference_4400_row31flatten_c() {
    round3(
        "31flatten-c",
        "flatten([[x.name]])[0]",
        true,
        "Sink",
        "flatten([[x.name]])[0]",
        0,
        "",
    );
}

fn reference_4400_row32id_i() {
    round3(
        "32id-i",
        "id(x.name)",
        false,
        "Sink",
        "id(x.name)",
        1,
        "fn id(v) { v }\n",
    );
}

fn reference_4400_row32id_c() {
    round3(
        "32id-c",
        "id(x.name)",
        true,
        "Sink",
        "id(x.name)",
        0,
        "fn id(v) { v }\n",
    );
}

fn reference_4400_row36union_i() {
    round3(
        "36union-i",
        "[x.name]",
        false,
        "UnionListSink",
        "[x.name]",
        1,
        "",
    );
}

fn reference_4400_row36union_c() {
    round3(
        "36union-c",
        "[x.name]",
        true,
        "UnionListSink",
        "[x.name]",
        0,
        "",
    );
}

fn reference_4400_row38unknownif_i() {
    round3(
        "38unknownif-i",
        "if x.ready { x.name } else { 'fallback' }",
        false,
        "Sink",
        "if x.ready { x.name } else { 'fallback' }",
        1,
        "",
    );
}

fn reference_4400_row38unknownif_c() {
    round3(
        "38unknownif-c",
        "if x.ready { x.name } else { 'fallback' }",
        true,
        "Sink",
        "if x.ready { x.name } else { 'fallback' }",
        0,
        "",
    );
}

fn reference_4400_row33param_si_di() {
    round3_module(
        "33param-si-di",
        "module typed input read inside module; source=si, declaration=String",
        false,
        "String",
        false,
        1,
    );
}

fn reference_4400_row33param_si_dc() {
    round3_module(
        "33param-si-dc",
        "module typed input read inside module; source=si, declaration=mock.Expected",
        false,
        "mock.Expected",
        false,
        1,
    );
}

fn reference_4400_row33param_sc_di() {
    round3_module(
        "33param-sc-di",
        "module typed input read inside module; source=sc, declaration=String",
        true,
        "String",
        false,
        1,
    );
}

fn reference_4400_row33param_sc_dc() {
    round3_module(
        "33param-sc-dc",
        "module typed input read inside module; source=sc, declaration=mock.Expected",
        true,
        "mock.Expected",
        false,
        0,
    );
}

fn reference_4400_row34declalias_si_di() {
    round3_module(
        "34declalias-si-di",
        "typed output with let alias; source=si, declaration=String",
        false,
        "String",
        true,
        1,
    );
}

fn reference_4400_row34declalias_si_dc() {
    round3_module(
        "34declalias-si-dc",
        "typed output with let alias; source=si, declaration=mock.Expected",
        false,
        "mock.Expected",
        true,
        1,
    );
}

fn reference_4400_row34declalias_sc_di() {
    round3_module(
        "34declalias-sc-di",
        "typed output with let alias; source=sc, declaration=String",
        true,
        "String",
        true,
        1,
    );
}

fn reference_4400_row34declalias_sc_dc() {
    round3_module(
        "34declalias-sc-dc",
        "typed output with let alias; source=sc, declaration=mock.Expected",
        true,
        "mock.Expected",
        true,
        0,
    );
}

fn reference_4400_row35fnparam_si_di() {
    round3(
        "35fnparam-si-di",
        "typed function parameter and return",
        false,
        "Sink",
        "typed(x.name)",
        1,
        "fn typed(v: String): String { v }\n",
    );
}

fn reference_4400_row35fnparam_si_dc() {
    round3(
        "35fnparam-si-dc",
        "typed function parameter and return",
        false,
        "Sink",
        "typed(x.name)",
        1,
        "fn typed(v: mock.Expected): mock.Expected { v }\n",
    );
}

fn reference_4400_row35fnparam_sc_di() {
    round3(
        "35fnparam-sc-di",
        "typed function parameter and return",
        true,
        "Sink",
        "typed(x.name)",
        1,
        "fn typed(v: String): String { v }\n",
    );
}

fn reference_4400_row35fnparam_sc_dc() {
    round3(
        "35fnparam-sc-dc",
        "typed function parameter and return",
        true,
        "Sink",
        "typed(x.name)",
        0,
        "fn typed(v: mock.Expected): mock.Expected { v }\n",
    );
}

fn reference_4400_row37ref_i() {
    let mut row = Row::new(
        "37ref-i",
        "x.name as mock.Cast",
        "Source",
        "CastSink",
        "x.name as mock.Cast",
        0,
    );
    row.second_source = Some("Source");
    measure(row.accepting());
}

fn reference_4400_row37ref_c() {
    let mut row = Row::new(
        "37ref-c",
        "x.name as mock.Cast",
        "CompatibleCastSource",
        "CastSink",
        "x.name as mock.Cast",
        0,
    );
    row.second_source = Some("CompatibleCastSource");
    measure(row.accepting());
}

fn reference_4400_row37coalesce_i() {
    let mut row = Row::new(
        "37coalesce-i",
        "(x.name ?? y.name) as mock.Cast",
        "Source",
        "CastSink",
        "(x.name ?? y.name) as mock.Cast",
        0,
    );
    row.second_source = Some("Source");
    measure(row.accepting());
}

fn reference_4400_row37coalesce_c() {
    let mut row = Row::new(
        "37coalesce-c",
        "(x.name ?? y.name) as mock.Cast",
        "CompatibleCastSource",
        "CastSink",
        "(x.name ?? y.name) as mock.Cast",
        0,
    );
    row.second_source = Some("CompatibleCastSource");
    measure(row.accepting());
}

fn reference_4400_row39typedlet_syntax_i() {
    round3(
        "39typedlet-i",
        "literal typed-let spelling (grammar probe)",
        false,
        "Sink",
        "n",
        0,
        "let n: mock.Expected = x.name\n",
    );
}

fn reference_4400_row39typedlet_syntax_c() {
    round3(
        "39typedlet-c",
        "literal typed-let spelling (grammar probe)",
        true,
        "Sink",
        "n",
        0,
        "let n: mock.Expected = x.name\n",
    );
}
