use super::output_kind::EnrichedAuthored;
use super::*;
use crate::paths::ResolvedPath;
use crate::resource::{
    NoReferencePayload, PlaceholderKind, SecretPayload, SnapshotLeaf, ValueKind,
};
use crate::schema::{
    AttributeSchema, AttributeType as T, ResourceSchema, SchemaRegistry, StringIdentityLayer,
    StringMemberConstraints, StructField, TypeIdentity, ValidatedResourceSchema,
};
use crate::test_utils::resolver_kind::{InvocationKind, ProjectionReport, compare_directory};
use serde_json::Value as Json;

fn expected() -> T {
    T::refined_string(
        Some(TypeIdentity::from_dotted("mock.Expected")),
        None,
        None,
        None,
    )
}
fn schemas() -> Vec<ValidatedResourceSchema> {
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
            AttributeSchema::optional("value", T::union(vec![T::list(expected()), T::string()]))
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
fn context() -> (ProviderContext, SchemaRegistry) {
    let mut schemas_registry = SchemaRegistry::new();
    for schema in schemas() {
        schemas_registry.insert("mock", schema);
    }
    let config = ProviderContext {
        resource_types: ProviderContext::resource_types_from_schema_registry(&schemas_registry),
        ..ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    (config, schemas_registry)
}
fn observe(files: &[(String, String)]) -> ProjectionReport {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let path = ResolvedPath::new(dir.path()).unwrap();
    let (config, schemas) = context();
    compare_directory(&path, &config, &schemas)
}
fn assert_projection(report: &ProjectionReport, label: &str) {
    assert_eq!(
        report.strict_succeeded, report.plain_strict_succeeded,
        "{label}: strict success differs from the independent plain run"
    );
    assert_eq!(
        report.diagnostics_succeeded, report.plain_diagnostics_succeeded,
        "{label}: diagnostics success differs from the independent plain run"
    );
    let actual_publications = report.publications();
    assert_eq!(
        actual_publications.len(),
        report.plain_publications.len(),
        "{label}: published-root count differs"
    );
    assert_eq!(
        actual_publications, report.plain_publications,
        "{label}: published-root identities/order differ"
    );
    for invocation in &report.invocations {
        assert!(
            invocation.same_inputs && invocation.same_roots && invocation.same_error,
            "{label}: invocation {} inputs, roots or error differ",
            invocation.ordinal
        );
    }

    assert!(!report.roots.is_empty(), "{label}: no published roots");
    for (index, root) in report.roots.iter().enumerate() {
        // Never format a Value: retained secret operands can contain plaintext.
        assert!(
            root.matches_plain,
            "{label}: projection differs at root {index}, invocation {}",
            root.invocation
        );
        assert_eq!(
            root.counts, root.oracle_counts,
            "{label}: operation counts, root {index}, invocation {}",
            root.invocation
        );
    }
}
#[test]
fn output_kind_first_gate() {
    let files = vec![
        (
            "sources.crn".into(),
            "let x = mock.Source {\nname = 'same'\ncount = 1\n}\nlet pending = mock.Source {}\n"
                .into(),
        ),
        (
            "main.crn".into(),
            concat!(
                "let direct = mock.Sink { value = x.name }\n",
                "let missing = mock.Sink { value = pending.name }\n",
                "let segmented = mock.Sink { value = x.name[0] }\n",
                "let computed = mock.Sink { value = upper(x.name) ?? 'fallback' }\n",
                "let arithmetic = mock.IntSink { value = x.count + 2 }\n"
            )
            .into(),
        ),
        ("providers.crn".into(), "provider mock {}\n".into()),
        (
            "backend.crn".into(),
            "backend local { path = 'state.json' }\n".into(),
        ),
    ];
    let report = observe(&files);
    assert_projection(&report, "first gate");
    for entry in ["strict", "diagnostics"] {
        let roots = report
            .roots
            .iter()
            .filter(|root| root.entry == entry)
            .collect::<Vec<_>>();
        assert!(roots.iter().any(|root| {
            root.paths
                .iter()
                .any(|path| path.path == "x.name" && path.conversion_depth == 0)
        }));
        assert!(
            roots
                .iter()
                .any(|root| root.paths.iter().any(|path| path.path == "pending.name"))
        );
        assert!(roots.iter().any(|root| {
            root.paths
                .iter()
                .any(|path| path.segmented && path.path == "x.name[0]")
        }));
        let computed = roots
            .iter()
            .filter(|root| {
                root.invocation_kind == InvocationKind::Merged && root.input.coalesce_left_deferred
            })
            .collect::<Vec<_>>();
        assert_eq!(
            computed.len(),
            1,
            "one deferred coalesce at merged invocation in {entry}"
        );
        let root = computed[0];
        let invocation = report
            .invocations
            .iter()
            .find(|invocation| invocation.ordinal == root.invocation)
            .unwrap();
        assert_eq!(invocation.counts.get("builtin"), 1);
        assert_eq!(invocation.counts.get("builtin_success"), 1);
        assert_eq!(invocation.counts.get("take_present"), 1);
        assert_eq!(invocation.counts, invocation.oracle_counts);
        assert_eq!(root.counts.get("builtin"), 1);
        assert_eq!(root.counts.get("builtin_success"), 1);
        assert_eq!(root.counts.get("take_present"), 1);
        assert!(
            root.paths
                .iter()
                .any(|path| path.path == "x.name" && path.conversion_depth == 2)
        );
    }
}

// Ambiguous inference makes these fail to compile if a forbidden impl appears.
macro_rules! assert_not_impl {
    ($ty:ty, $bound:path) => {
        const _: fn() = || {
            trait Ambiguous<A> {
                fn check() {}
            }
            impl<T: ?Sized> Ambiguous<()> for T {}
            struct IfImpl;
            impl<T: ?Sized + $bound> Ambiguous<IfImpl> for T {}
            let _ = <$ty as Ambiguous<_>>::check;
        };
    };
}
assert_not_impl!(EnrichedAuthored, ValueKind);
assert_not_impl!(EnrichedAuthored, PlaceholderKind);
assert_not_impl!(EnrichedAuthored, PartialEq);
assert_not_impl!(EnrichedAuthored, serde::Serialize);
assert_not_impl!(EnrichedAuthored, serde::Deserialize<'static>);
assert_not_impl!(V<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(crate::resource::FunctionArgument<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(crate::resource::InterpolationPart<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(SecretPayload<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(crate::integer_arithmetic::ValidatedIntegerExpr<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(crate::parser::DeferredValueIf<EnrichedAuthored,RelativeBindingName>,PartialEq);
assert_not_impl!(V<EnrichedAuthored,RelativeBindingName>,serde::Serialize);
assert_not_impl!(V<EnrichedAuthored,RelativeBindingName>,serde::Deserialize<'static>);
#[test]
fn reader_kinds_exclude_enriched() {
    fn empty<
        P: ValueKind
            + ValueShape<
                ReferencePayload<B> = NoReferencePayload,
                ConversionPayload<B> = NoReferencePayload,
            >,
        B: BindingAddress,
    >() {
    }
    fn snapshot<P: PlaceholderKind, B: BindingAddress>() {
        empty::<SnapshotLeaf<P>, B>();
    }
    fn readers<B: BindingAddress>() {
        empty::<NoPlaceholder, B>();
        empty::<ParserPlaceholder, B>();
        empty::<LoopSlot, B>();
        snapshot::<NoPlaceholder, B>();
        snapshot::<ParserPlaceholder, B>();
        snapshot::<LoopSlot, B>();
    }
    readers::<RelativeBindingName>();
    readers::<AbsoluteBindingAddress>();
    // The plain projection borrows; splitting consumes the owned plain value.
    let value: S<RelativeBindingName> =
        GenericValue::Concrete(ConcreteValue::String("allocation".into()));
    assert!(matches!(
        ParserPlaceholder::project(&value),
        Cow::Borrowed(_)
    ));
    let (value, ()) = ParserPlaceholder::split_operand(value);
    assert!(crate::resource::same_plain_representation(&value, &value));
}

const DEFECTS: [&str; 16] = [
    "20-i", "21-i", "22-i", "23l-i", "23m-i", "24l-i", "24m-i", "25l-i", "25m-i", "26-i",
    "27-xbad", "27-ybad", "28-i", "30l-i", "30m-i", "30s-i",
];
#[test]
fn defect_forms_keep_paths() {
    // Consume the Phase 0 input bytes, including its same-file twins, unchanged.
    let golden: Json = serde_json::from_str(include_str!(
        "../../../../carina-cli/tests/fixtures/authored_reference_cells.json"
    ))
    .unwrap();
    let mut rows = 0;
    for row in golden["rows"].as_array().unwrap() {
        let id = row["row"].as_str().unwrap();
        let original = id.strip_suffix("-samefile").unwrap_or(id);
        if !DEFECTS.contains(&original) {
            continue;
        }
        rows += 1;
        let same_file = id.ends_with("-samefile");
        for cell in row["cells"].as_array().unwrap() {
            let known = cell["known"].as_bool().unwrap();
            let files = cell["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| {
                    (
                        file[0].as_str().unwrap().to_owned(),
                        file[1].as_str().unwrap().to_owned(),
                    )
                })
                .collect::<Vec<_>>();
            let report = observe(&files);
            let label = format!("{id} known={known}");
            assert_projection(&report, &label);
            for entry in ["strict", "diagnostics"] {
                let expected = if same_file {
                    InvocationKind::PerFile
                } else {
                    InvocationKind::Merged
                };
                let roots = report
                    .roots
                    .iter()
                    .filter(|root| root.entry == entry && root.invocation_kind == expected)
                    .collect::<Vec<_>>();
                let paths = roots
                    .iter()
                    .flat_map(|root| root.paths.iter())
                    .map(|path| path.path.as_str())
                    .collect::<Vec<_>>();
                let expected_paths: &[&str] = match original {
                    "27-xbad" | "27-ybad" => &["x.name", "y.name"],
                    "28-i" => &["built.first.name"],
                    _ => &["x.name"],
                };
                for path in expected_paths {
                    let inputs = roots
                        .iter()
                        .map(|root| {
                            format!(
                                "{}:{}:{}",
                                root.invocation,
                                root.input.shape,
                                root.input.direct_path.as_deref().unwrap_or("-")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    assert!(
                        paths.contains(path),
                        "{label}: {entry} missing {path} in lookup invocation; paths [{}]; input shapes [{inputs}]",
                        paths.join(", ")
                    );
                }
                if original.starts_with("27-") || original == "28-i" {
                    // The parser canonicalizes the selected map resource to binding
                    // `built.first` (map_key_address), with attribute `name`.
                    // Both selections must arrive as segment-free resource refs.
                    for path in expected_paths {
                        assert!(
                            roots
                                .iter()
                                .flat_map(|root| &root.paths)
                                .any(|observed| observed.path == *path && !observed.segmented),
                            "{label}: {entry} segmented selection for {path}"
                        );
                        assert!(
                            roots.iter().any(|root| root.input.shape == "ResourceRef"
                                && root.input.direct_path.as_deref() == Some(*path)),
                            "{label}: {entry} parse-time shape for {path}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(rows, 32);
}
