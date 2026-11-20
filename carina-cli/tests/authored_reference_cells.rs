//! The 216 recorded cells cover 92 reference forms and 16 same-file twins,
//! each with known and unknown sources, through production validate entries.
//! Refresh with this fixed command, then review the diff:
//! cargo nextest run -p carina-cli --test authored_reference_cells --run-ignored only -E 'test(=record_authored_reference_cells)'
//! A PR that correctly changes diagnostic wording refreshes and reviews this file's golden.
//! The golden pins the original typed error vector, including Debug field names,
//! and rendered errors. Facade/replay equality is asserted without recording a
//! duplicate. JSON is stored once and compared as a structured value.
//! Refresh and read the diff when an error field or diagnostic changes.
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
use std::path::{Path, PathBuf};

const FIRST_RECORDED_ON: &str = "dedb7bcebd10f6b9a6b2d4a7cd6c8b318f8b2ccd";
fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/authored_reference_cells.json")
}
fn normalize_prefix(value: Json, root: &Path, token: &str) -> Json {
    match value {
        Json::String(text) => Json::String(text.replace(root.to_str().unwrap(), token)),
        Json::Array(values) => Json::Array(
            values
                .into_iter()
                .map(|v| normalize_prefix(v, root, token))
                .collect(),
        ),
        Json::Object(values) => Json::Object(
            values
                .into_iter()
                .map(|(k, v)| (k, normalize_prefix(v, root, token)))
                .collect(),
        ),
        value => value,
    }
}
fn json_difference(expected: &Json, actual: &Json) -> Option<String> {
    fn difference(expected: &Json, actual: &Json, path: &str) -> Option<String> {
        match (expected, actual) {
            (Json::Array(a), Json::Array(b)) if a.len() == b.len() => a
                .iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (a, b))| difference(a, b, &format!("{path}/{i}"))),
            (Json::Object(a), Json::Object(b)) if a.keys().eq(b.keys()) => a
                .iter()
                .find_map(|(k, a)| difference(a, &b[k], &format!("{path}/{k}"))),
            _ if expected == actual => None,
            _ => Some(format!("{path}: expected {expected}, got {actual}")),
        }
    }
    difference(expected, actual, "")
}
fn assert_json_eq(expected: &Json, actual: &Json, context: &str) {
    if let Some(diff) = json_difference(expected, actual) {
        panic!("{context}: {diff}");
    }
}
// Failure evidence only, outside the worktree; never an update mode.
fn save_actual(label: &str, bytes: &[u8]) -> PathBuf {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix(&format!(
            "authored-reference-actual-{}-",
            label.replace('/', "__")
        ))
        .suffix(".json")
        .tempfile()
        .unwrap();
    file.write_all(bytes).unwrap();
    file.keep().unwrap().1
}

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
    second_source: Option<&'static str>,
    source_suffix: String,
    source_override: Option<[String; 2]>,
    extra_files: Vec<(&'static str, String)>,
}
impl Row {
    fn new(
        id: &'static str,
        construct: &'static str,
        source: &'static str,
        sink: &str,
        expression: &str,
    ) -> Self {
        Self {
            id,
            construct,
            source,
            main: format!("let sink = mock.{sink} {{ value = {expression} }}\n"),
            same_file: false,
            live_alternative: false,
            second_source: None,
            source_suffix: String::new(),
            source_override: None,
            extra_files: Vec::new(),
        }
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
    // retain the original typed vector and facade rendering in the record.
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
    let mut all = Vec::new();
    for (index, diagnostic) in errors.into_iter().enumerate() {
        let typed = aligned.then(|| &typed_errors[index]);
        let (class, reason) = classify(typed, &diagnostic);
        all.push(json!({"channel": "errors", "index": index, "class": class,
            "reason": reason,
            "resource_id": typed.and_then(error_resource_id)}));
    }
    for (index, diagnostic) in warnings.into_iter().enumerate() {
        let (class, reason) = classify(None, &diagnostic);
        all.push(
            json!({"channel": "warnings", "index": index, "class": class,
            "reason": reason}),
        );
    }
    assert_eq!(
        facade, replay,
        "production facade and diagnostics replay disagree"
    );
    json!({"status": if facade.is_empty() { "accept" } else { "reject" },
        "typed_error_vector": typed_error_vector,
        "typed_parse_warnings": parse_warnings,
        "typed_input_warnings": input_warnings, "facade_errors": facade,
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
            .prefix("authored-reference-cells-")
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
                "observation": normalize_prefix(observed, dir.path(), "<DIR>")}));
        }
        records.push(json!({"row": id, "construct": row.construct, "cells": cells}));
    }
    json!(records)
}

#[test]
fn authored_reference_cells_match_recorded() {
    let golden: Json = serde_json::from_slice(&std::fs::read(golden_path()).unwrap()).unwrap();
    assert_eq!(
        golden["header"],
        json!({"first_recorded_on": FIRST_RECORDED_ON, "original_rows": 92, "samefile_rows": 16, "rows": 108, "cells": 216})
    );
    assert_eq!(golden["skipped_probes"], json!(SKIPPED_PROBES));
    assert_eq!(golden["rows"].as_array().unwrap().len(), 108);
    let actual = cells();
    if golden["rows"] != actual {
        eprintln!(
            "actual cells: {}",
            save_actual("cells", &serde_json::to_vec_pretty(&actual).unwrap()).display()
        );
    }
    assert_json_eq(&golden["rows"], &actual, "4400 cells");
}

#[test]
#[ignore = "refresh the recorded cells, then review the diff"]
fn record_authored_reference_cells() {
    let header = json!({"first_recorded_on": FIRST_RECORDED_ON, "original_rows": 92, "samefile_rows": 16, "rows": 108, "cells": 216});
    let golden = json!({"header": header, "normalization": "temporary directory prefix only",
        "skipped_probes": SKIPPED_PROBES, "rows": cells()});

    std::fs::write(
        golden_path(),
        serde_json::to_string_pretty(&golden).unwrap() + "\n",
    )
    .unwrap();
}
macro_rules! row {
    ($test:ident, $id:literal, $construct:literal, $source:literal, $sink:literal, $expr:literal) => {
        fn $test() {
            measure(Row::new($id, $construct, $source, $sink, $expr));
        }
    };
}

row!(
    reference_4400_row1_top,
    "1",
    "attribute root: x.name",
    "Source",
    "Sink",
    "x.name"
);
row!(
    reference_4400_row2_list,
    "2",
    "list item: [x.name]",
    "Source",
    "ListSink",
    "[x.name]"
);
row!(
    reference_4400_row3_map,
    "3",
    "map value: { a = x.name }",
    "Source",
    "MapSink",
    "{ a = x.name }"
);
row!(
    reference_4400_row3_struct,
    "3b",
    "struct field: { a = x.name }",
    "Source",
    "StructSink",
    "{ a = x.name }"
);
row!(
    reference_4400_row4_secret,
    "4",
    "secret(x.name)",
    "Source",
    "Sink",
    "secret(x.name)"
);
row!(
    reference_4400_row4b_secret_literal,
    "4b",
    "secret literal: secret('same')",
    "Source",
    "Sink",
    "secret('same')"
);
row!(
    reference_4400_row5_selected,
    "5a",
    "selected operand: x.name ?? 'fallback'",
    "Source",
    "Sink",
    "x.name ?? 'fallback'"
);

fn reference_4400_row5_live_alternative() {
    let mut row = Row::new(
        "5b",
        "live alternative: x.name ?? y.name (x pending; y varies)",
        "Source",
        "Sink",
        "x.name ?? y.name",
    );
    row.live_alternative = true;
    measure(row);
}
row!(
    reference_4400_row6_input_without_reevaluation,
    "6",
    "map reference + static call: { a = x.name, b = upper('same') }",
    "Source",
    "MapSink",
    "{ a = x.name, b = upper('same') }"
);

row!(
    reference_4400_row7_literal,
    "7",
    "literal equal to known source: 'same'",
    "Source",
    "Sink",
    "'same'"
);
row!(
    reference_4400_row8_compatible,
    "8",
    "compatible reference: x.name",
    "CompatibleSource",
    "Sink",
    "x.name"
);

fn reference_4400_row9_known_value_constraint() {
    let row = Row::new(
        "9",
        "compatible type; destination member length <= 3",
        "CompatibleSource",
        "LengthSink",
        "x.name",
    );

    measure(row);
}
row!(
    reference_4400_row10a_interpolation_incompatible,
    "10a-i",
    "interpolation, directly incompatible source: p-${x.name}",
    "Source",
    "Sink",
    "\"p-${x.name}\""
);
row!(
    reference_4400_row10a_interpolation_compatible,
    "10a-c",
    "interpolation, directly compatible source: p-${x.name}",
    "CompatibleSource",
    "Sink",
    "\"p-${x.name}\""
);
row!(
    reference_4400_row10b_arithmetic_incompatible,
    "10b-i",
    "addition, directly incompatible source: x.count + 1",
    "Source",
    "IntSink",
    "x.count + 1"
);
row!(
    reference_4400_row10b_arithmetic_compatible,
    "10b-c",
    "addition, directly compatible source: x.count + 1",
    "CompatibleSource",
    "IntSink",
    "x.count + 1"
);
row!(
    reference_4400_row10c_upper_incompatible,
    "10c-i",
    "upper(x.name), directly incompatible source",
    "Source",
    "Sink",
    "upper(x.name)"
);
row!(
    reference_4400_row10c_upper_compatible,
    "10c-c",
    "upper(x.name), directly compatible source",
    "CompatibleSource",
    "Sink",
    "upper(x.name)"
);
row!(
    reference_4400_row11_if_incompatible,
    "11-i",
    "if true { x.name } else { 'fallback' }, incompatible",
    "Source",
    "Sink",
    "if true { x.name } else { 'fallback' }"
);
row!(
    reference_4400_row11_if_compatible,
    "11-c",
    "if true { x.name } else { 'fallback' }, compatible",
    "CompatibleSource",
    "Sink",
    "if true { x.name } else { 'fallback' }"
);

fn reference_4400_row12_same_file_incompatible() {
    let mut row = Row::new(
        "12-i",
        "same-file reference, incompatible (control for 1)",
        "Source",
        "Sink",
        "x.name",
    );
    row.same_file = true;
    measure(row);
}

fn reference_4400_row12_same_file_compatible() {
    let mut row = Row::new(
        "12-c",
        "same-file reference, compatible (control for 8)",
        "CompatibleSource",
        "Sink",
        "x.name",
    );
    row.same_file = true;
    measure(row);
}

fn reference_4400_row13_alias_incompatible() {
    let mut row = Row::new(
        "13-i",
        "let n = x.name; value = n, incompatible",
        "Source",
        "Sink",
        "n",
    );
    row.main = format!("let n = x.name\n{}", row.main);
    measure(row);
}

fn reference_4400_row13_alias_compatible() {
    let mut row = Row::new(
        "13-c",
        "let n = x.name; value = n, compatible",
        "CompatibleSource",
        "Sink",
        "n",
    );
    row.main = format!("let n = x.name\n{}", row.main);
    measure(row);
}

fn reference_4400_row14_two_sinks() {
    let mut row = Row::new(
        "14",
        "x.name at sinks mock.Expected and mock.OtherExpected",
        "CompatibleSource",
        "Sink",
        "x.name",
    );
    row.main = "let accepted = mock.Sink { value = x.name }\nlet rejected = mock.OtherSink { value = x.name }\n".into();
    measure(row);
}

macro_rules! issue_row {
    ($test:ident, $id:literal, $construct:literal, $source:literal,
     $definitions:literal, $expr:literal, $pair:literal) => {
        fn $test() {
            let mut row = Row::new($id, $construct, $source, "Sink", $expr);
            row.source_suffix = $definitions.into();
            if $pair {
                row.second_source = Some($source);
            }
            measure(row);
        }
    };
}
issue_row!(
    reference_4400_row20_sole_incompatible,
    "20-i",
    "sole interpolation, incompatible",
    "Source",
    "",
    "\"${x.name}\"",
    false
);
issue_row!(
    reference_4400_row20_sole_compatible,
    "20-c",
    "sole interpolation, compatible",
    "CompatibleSource",
    "",
    "\"${x.name}\"",
    false
);
issue_row!(
    reference_4400_row21_list_incompatible,
    "21-i",
    "[x.name][0], incompatible",
    "Source",
    "",
    "[x.name][0]",
    false
);
issue_row!(
    reference_4400_row21_list_compatible,
    "21-c",
    "[x.name][0], compatible",
    "CompatibleSource",
    "",
    "[x.name][0]",
    false
);
issue_row!(
    reference_4400_row22_map_incompatible,
    "22-i",
    "{ f = x.name }.f, incompatible",
    "Source",
    "",
    "{ f = x.name }.f",
    false
);
issue_row!(
    reference_4400_row22_map_compatible,
    "22-c",
    "{ f = x.name }.f, compatible",
    "CompatibleSource",
    "",
    "{ f = x.name }.f",
    false
);
issue_row!(
    reference_4400_row23_map_incompatible,
    "23m-i",
    "let m = { f = x.name }; m.f, incompatible",
    "Source",
    "let m = { f = x.name }\n",
    "m.f",
    false
);
issue_row!(
    reference_4400_row23_map_compatible,
    "23m-c",
    "let m = { f = x.name }; m.f, compatible",
    "CompatibleSource",
    "let m = { f = x.name }\n",
    "m.f",
    false
);
issue_row!(
    reference_4400_row23_list_incompatible,
    "23l-i",
    "let l = [x.name]; l[0], incompatible",
    "Source",
    "let l = [x.name]\n",
    "l[0]",
    false
);
issue_row!(
    reference_4400_row23_list_compatible,
    "23l-c",
    "let l = [x.name]; l[0], compatible",
    "CompatibleSource",
    "let l = [x.name]\n",
    "l[0]",
    false
);
issue_row!(
    reference_4400_row24_list_incompatible,
    "24l-i",
    "let s = if true { [x.name] } else { [y.name] }; s[0], incompatible",
    "Source",
    "let s = if true { [x.name] } else { [y.name] }\n",
    "s[0]",
    true
);
issue_row!(
    reference_4400_row24_list_compatible,
    "24l-c",
    "let s = if true { [x.name] } else { [y.name] }; s[0], compatible",
    "CompatibleSource",
    "let s = if true { [x.name] } else { [y.name] }\n",
    "s[0]",
    true
);
issue_row!(
    reference_4400_row24_map_incompatible,
    "24m-i",
    "let s = if true { { f = x.name } } else { { f = y.name } }; s.f, incompatible",
    "Source",
    "let s = if true { { f = x.name } } else { { f = y.name } }\n",
    "s.f",
    true
);
issue_row!(
    reference_4400_row24_map_compatible,
    "24m-c",
    "let s = if true { { f = x.name } } else { { f = y.name } }; s.f, compatible",
    "CompatibleSource",
    "let s = if true { { f = x.name } } else { { f = y.name } }\n",
    "s.f",
    true
);
issue_row!(
    reference_4400_row25_list_incompatible,
    "25l-i",
    "let s = [x.name] ?? [y.name]; s[0], incompatible",
    "Source",
    "let s = [x.name] ?? [y.name]\n",
    "s[0]",
    true
);
issue_row!(
    reference_4400_row25_list_compatible,
    "25l-c",
    "let s = [x.name] ?? [y.name]; s[0], compatible",
    "CompatibleSource",
    "let s = [x.name] ?? [y.name]\n",
    "s[0]",
    true
);
issue_row!(
    reference_4400_row25_map_incompatible,
    "25m-i",
    "let s = { f = x.name } ?? { f = y.name }; s.f, incompatible",
    "Source",
    "let s = { f = x.name } ?? { f = y.name }\n",
    "s.f",
    true
);
issue_row!(
    reference_4400_row25_map_compatible,
    "25m-c",
    "let s = { f = x.name } ?? { f = y.name }; s.f, compatible",
    "CompatibleSource",
    "let s = { f = x.name } ?? { f = y.name }\n",
    "s.f",
    true
);
issue_row!(
    reference_4400_row26_inner_if_incompatible,
    "26-i",
    "[if true { x.name } else { y.name }][0], incompatible",
    "Source",
    "",
    "[if true { x.name } else { y.name }][0]",
    true
);
issue_row!(
    reference_4400_row26_inner_if_compatible,
    "26-c",
    "[if true { x.name } else { y.name }][0], compatible",
    "CompatibleSource",
    "",
    "[if true { x.name } else { y.name }][0]",
    true
);

fn loop_selection(id: &'static str, construct: &'static str, x: &'static str, y: &'static str) {
    let mut row = Row::new(id, construct, x, "Sink", "");
    row.second_source = Some(y);
    row.main =
        "let probes = for i in [0, 1] {\n  mock.Sink {\n    value = [x.name, y.name][i]\n  }\n}\n"
            .into();

    measure(row);
}

fn reference_4400_row27_bad_first() {
    loop_selection(
        "27-xbad",
        "for i in [0,1]: [x.name,y.name][i]; only x incompatible",
        "Source",
        "CompatibleSource",
    );
}

fn reference_4400_row27_bad_second() {
    loop_selection(
        "27-ybad",
        "for i in [0,1]: [x.name,y.name][i]; only y incompatible",
        "CompatibleSource",
        "Source",
    );
}

fn reference_4400_row27_both_compatible() {
    loop_selection(
        "27-c",
        "for i in [0,1]: [x.name,y.name][i]; both compatible",
        "CompatibleSource",
        "CompatibleSource",
    );
}
fn for_map_source(schema: &str, known: bool) -> String {
    let block = source_block("unused", schema, known).replacen(
        &format!("let unused = mock.{schema}"),
        &format!("  mock.{schema}"),
        1,
    );
    format!(
        "let input = {{ first = 0, second = 1 }}\nlet built = for key, _ in input {{\n{block}}}\n"
    )
}
fn resource_map_read(id: &'static str, construct: &'static str, schema: &'static str) {
    let mut row = Row::new(id, construct, schema, "Sink", "built['first'].name");
    row.source_override = Some([for_map_source(schema, true), for_map_source(schema, false)]);
    measure(row);
}

fn reference_4400_row28_resource_map_incompatible() {
    resource_map_read(
        "28-i",
        "for-built map of Source resources; built['first'].name, incompatible",
        "Source",
    );
}

fn reference_4400_row28_resource_map_compatible() {
    resource_map_read(
        "28-c",
        "for-built map of CompatibleSource resources; built['first'].name, compatible",
        "CompatibleSource",
    );
}
fn alias_public_secret(
    id: &'static str,
    construct: &'static str,
    schema: &'static str,
    public_first: bool,
) {
    let mut row = Row::new(id, construct, schema, "Sink", "n");
    row.source_suffix = "let n = x.name\n".into();
    let public = "let public = mock.Sink { value = n }\n";
    let protected = "let protected = mock.Sink { value = secret(n) }\n";
    row.main = if public_first {
        format!("{public}{protected}")
    } else {
        format!("{protected}{public}")
    };

    measure(row);
}

fn reference_4400_row29_public_first_incompatible() {
    alias_public_secret(
        "29-p-i",
        "let n = x.name; public n then secret(n), incompatible",
        "Source",
        true,
    );
}

fn reference_4400_row29_secret_first_incompatible() {
    alias_public_secret(
        "29-s-i",
        "let n = x.name; secret(n) then public n, incompatible",
        "Source",
        false,
    );
}

fn reference_4400_row29_public_first_compatible() {
    alias_public_secret(
        "29-p-c",
        "let n = x.name; public n then secret(n), compatible",
        "CompatibleSource",
        true,
    );
}

fn reference_4400_row29_secret_first_compatible() {
    alias_public_secret(
        "29-s-c",
        "let n = x.name; secret(n) then public n, compatible",
        "CompatibleSource",
        false,
    );
}
issue_row!(
    reference_4400_row30_sole_incompatible,
    "30s-i",
    "sole interpolation, incompatible Reference source",
    "ReferenceSource",
    "",
    "\"${x.name}\"",
    false
);
issue_row!(
    reference_4400_row30_sole_compatible,
    "30s-c",
    "sole interpolation, compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "\"${x.name}\"",
    false
);
issue_row!(
    reference_4400_row30_list_incompatible,
    "30l-i",
    "[x.name][0], incompatible Reference source",
    "ReferenceSource",
    "",
    "[x.name][0]",
    false
);
issue_row!(
    reference_4400_row30_list_compatible,
    "30l-c",
    "[x.name][0], compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "[x.name][0]",
    false
);
issue_row!(
    reference_4400_row30_map_incompatible,
    "30m-i",
    "{ f = x.name }.f, incompatible Reference source",
    "ReferenceSource",
    "",
    "{ f = x.name }.f",
    false
);
issue_row!(
    reference_4400_row30_map_compatible,
    "30m-c",
    "{ f = x.name }.f, compatible Reference source",
    "CompatibleReferenceSource",
    "",
    "{ f = x.name }.f",
    false
);

fn expression_row(
    id: &'static str,
    construct: &'static str,
    compatible: bool,
    sink: &str,
    expression: &str,
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
    );
    row.main = format!("{prefix}{}", row.main);
    measure(row);
}
fn module_row(
    id: &'static str,
    construct: &'static str,
    compatible: bool,
    declaration: &str,
    exported: bool,
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
    measure(row);
}

fn reference_4400_row31lookup_i() {
    expression_row(
        "31lookup-i",
        "lookup({ a = x.name }, 'a', 'fallback')",
        false,
        "Sink",
        "lookup({ a = x.name }, 'a', 'fallback')",
        "",
    );
}

fn reference_4400_row31lookup_c() {
    expression_row(
        "31lookup-c",
        "lookup({ a = x.name }, 'a', 'fallback')",
        true,
        "Sink",
        "lookup({ a = x.name }, 'a', 'fallback')",
        "",
    );
}

fn reference_4400_row31values_i() {
    expression_row(
        "31values-i",
        "values({ a = x.name })[0]",
        false,
        "Sink",
        "values({ a = x.name })[0]",
        "",
    );
}

fn reference_4400_row31values_c() {
    expression_row(
        "31values-c",
        "values({ a = x.name })[0]",
        true,
        "Sink",
        "values({ a = x.name })[0]",
        "",
    );
}

fn reference_4400_row31concat_i() {
    expression_row(
        "31concat-i",
        "concat([x.name], [])[0]",
        false,
        "Sink",
        "concat([x.name], [])[0]",
        "",
    );
}

fn reference_4400_row31concat_c() {
    expression_row(
        "31concat-c",
        "concat([x.name], [])[0]",
        true,
        "Sink",
        "concat([x.name], [])[0]",
        "",
    );
}

fn reference_4400_row31flatten_i() {
    expression_row(
        "31flatten-i",
        "flatten([[x.name]])[0]",
        false,
        "Sink",
        "flatten([[x.name]])[0]",
        "",
    );
}

fn reference_4400_row31flatten_c() {
    expression_row(
        "31flatten-c",
        "flatten([[x.name]])[0]",
        true,
        "Sink",
        "flatten([[x.name]])[0]",
        "",
    );
}

fn reference_4400_row32id_i() {
    expression_row(
        "32id-i",
        "id(x.name)",
        false,
        "Sink",
        "id(x.name)",
        "fn id(v) { v }\n",
    );
}

fn reference_4400_row32id_c() {
    expression_row(
        "32id-c",
        "id(x.name)",
        true,
        "Sink",
        "id(x.name)",
        "fn id(v) { v }\n",
    );
}

fn reference_4400_row36union_i() {
    expression_row(
        "36union-i",
        "[x.name]",
        false,
        "UnionListSink",
        "[x.name]",
        "",
    );
}

fn reference_4400_row36union_c() {
    expression_row(
        "36union-c",
        "[x.name]",
        true,
        "UnionListSink",
        "[x.name]",
        "",
    );
}

fn reference_4400_row38unknownif_i() {
    expression_row(
        "38unknownif-i",
        "if x.ready { x.name } else { 'fallback' }",
        false,
        "Sink",
        "if x.ready { x.name } else { 'fallback' }",
        "",
    );
}

fn reference_4400_row38unknownif_c() {
    expression_row(
        "38unknownif-c",
        "if x.ready { x.name } else { 'fallback' }",
        true,
        "Sink",
        "if x.ready { x.name } else { 'fallback' }",
        "",
    );
}

fn reference_4400_row33param_si_di() {
    module_row(
        "33param-si-di",
        "module typed input read inside module; source=si, declaration=String",
        false,
        "String",
        false,
    );
}

fn reference_4400_row33param_si_dc() {
    module_row(
        "33param-si-dc",
        "module typed input read inside module; source=si, declaration=mock.Expected",
        false,
        "mock.Expected",
        false,
    );
}

fn reference_4400_row33param_sc_di() {
    module_row(
        "33param-sc-di",
        "module typed input read inside module; source=sc, declaration=String",
        true,
        "String",
        false,
    );
}

fn reference_4400_row33param_sc_dc() {
    module_row(
        "33param-sc-dc",
        "module typed input read inside module; source=sc, declaration=mock.Expected",
        true,
        "mock.Expected",
        false,
    );
}

fn reference_4400_row34declalias_si_di() {
    module_row(
        "34declalias-si-di",
        "typed output with let alias; source=si, declaration=String",
        false,
        "String",
        true,
    );
}

fn reference_4400_row34declalias_si_dc() {
    module_row(
        "34declalias-si-dc",
        "typed output with let alias; source=si, declaration=mock.Expected",
        false,
        "mock.Expected",
        true,
    );
}

fn reference_4400_row34declalias_sc_di() {
    module_row(
        "34declalias-sc-di",
        "typed output with let alias; source=sc, declaration=String",
        true,
        "String",
        true,
    );
}

fn reference_4400_row34declalias_sc_dc() {
    module_row(
        "34declalias-sc-dc",
        "typed output with let alias; source=sc, declaration=mock.Expected",
        true,
        "mock.Expected",
        true,
    );
}

fn reference_4400_row35fnparam_si_di() {
    expression_row(
        "35fnparam-si-di",
        "typed function parameter and return",
        false,
        "Sink",
        "typed(x.name)",
        "fn typed(v: String): String { v }\n",
    );
}

fn reference_4400_row35fnparam_si_dc() {
    expression_row(
        "35fnparam-si-dc",
        "typed function parameter and return",
        false,
        "Sink",
        "typed(x.name)",
        "fn typed(v: mock.Expected): mock.Expected { v }\n",
    );
}

fn reference_4400_row35fnparam_sc_di() {
    expression_row(
        "35fnparam-sc-di",
        "typed function parameter and return",
        true,
        "Sink",
        "typed(x.name)",
        "fn typed(v: String): String { v }\n",
    );
}

fn reference_4400_row35fnparam_sc_dc() {
    expression_row(
        "35fnparam-sc-dc",
        "typed function parameter and return",
        true,
        "Sink",
        "typed(x.name)",
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
    );
    row.second_source = Some("Source");
    measure(row);
}

fn reference_4400_row37ref_c() {
    let mut row = Row::new(
        "37ref-c",
        "x.name as mock.Cast",
        "CompatibleCastSource",
        "CastSink",
        "x.name as mock.Cast",
    );
    row.second_source = Some("CompatibleCastSource");
    measure(row);
}

fn reference_4400_row37coalesce_i() {
    let mut row = Row::new(
        "37coalesce-i",
        "(x.name ?? y.name) as mock.Cast",
        "Source",
        "CastSink",
        "(x.name ?? y.name) as mock.Cast",
    );
    row.second_source = Some("Source");
    measure(row);
}

fn reference_4400_row37coalesce_c() {
    let mut row = Row::new(
        "37coalesce-c",
        "(x.name ?? y.name) as mock.Cast",
        "CompatibleCastSource",
        "CastSink",
        "(x.name ?? y.name) as mock.Cast",
    );
    row.second_source = Some("CompatibleCastSource");
    measure(row);
}

fn reference_4400_row39typedlet_syntax_i() {
    expression_row(
        "39typedlet-i",
        "literal typed-let spelling (grammar probe)",
        false,
        "Sink",
        "n",
        "let n: mock.Expected = x.name\n",
    );
}

fn reference_4400_row39typedlet_syntax_c() {
    expression_row(
        "39typedlet-c",
        "literal typed-let spelling (grammar probe)",
        true,
        "Sink",
        "n",
        "let n: mock.Expected = x.name\n",
    );
}
