//! Tests for the `module_resolver` module.
//!
//! This file is referenced from `mod.rs` as `#[cfg(test)] mod tests;` and
//! depends on the `#[cfg(test)] use expander::{parse_synthetic_instance_prefix,
//! substitute_arguments};` re-export there to bring those `pub(super)`
//! helpers into the `super::*` glob below.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use super::resolver::ModuleResolver;
use super::*;
use crate::parser::{
    ArgumentOrigin, ArgumentParameter, AttributePathSegment, BindingUseOwner, DeferredForBody,
    DeferredForExpression, DeferredIfCondition, DeferredValueIf, LoadedProviderContext,
    ModuleArgument, ModuleCall, ModuleResolvedParsedFile, ParsedDeferredForExpression,
    ParsedDeferredForPhase, ParsedFile, ProviderContext, ResourceTypePath, SourceSpan, TypeExpr,
};
use crate::resource::{
    CanonicalEnumValue, ConcreteValue, DeferredValue, Directives, RawEnumIdentifier, Resource,
    ResourceId, ResourceIdentity, UnknownReason, Value,
};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};

fn known(type_expr: TypeExpr) -> TypeExpr {
    type_expr
}

fn consumer_type(type_expr: TypeExpr) -> crate::static_type::ConsumerStaticType {
    crate::validation::resolve_consumer_static_type_for_signature(
        &type_expr,
        &ProviderContext::default(),
    )
}

fn resolve_modules(
    parsed: ParsedFile,
    base_dir: &std::path::Path,
) -> Result<ModuleResolvedParsedFile, ModuleError> {
    super::resolve_modules_providerless(parsed, base_dir)
}

fn resolve_modules_with_config(
    parsed: ParsedFile,
    base_dir: &std::path::Path,
    config: &ProviderContext,
    schemas: &crate::schema::SchemaRegistry,
) -> Result<ModuleResolvedParsedFile, ModuleError> {
    super::resolve_modules_with_providerless_config(parsed, base_dir, config, schemas)
}

fn reconcile_anonymous_module_instances(
    resources: &mut [Resource],
    compositions: &mut [crate::resource::LiveComposition],
    find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
) {
    crate::module_resolver::reconcile_anonymous_module_instances(
        resources,
        compositions,
        find_state_names_by_type,
        &crate::identifier::StateBlockClaims::empty(),
    );
}

fn module_arguments(
    values: impl IntoIterator<Item = (String, Value)>,
) -> HashMap<String, ModuleArgument> {
    values
        .into_iter()
        .map(|(name, value)| (name, ModuleArgument::new(value)))
        .collect()
}

/// Small filesystem fixture shared by module-resolution regressions. Keeping
/// directory creation and writes here makes each test describe only the
/// module graph it is exercising.
struct ModuleResolutionFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl ModuleResolutionFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary module-resolution fixture");
        let root = temp.path().to_path_buf();
        Self { _temp: temp, root }
    }

    fn write(&self, relative: impl AsRef<Path>, source: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file has a parent"))
            .expect("create fixture directory");
        fs::write(&path, source).expect("write fixture file");
        path
    }

    fn resolve_caller(
        &self,
        caller_relative: impl AsRef<Path>,
    ) -> Result<ModuleResolvedParsedFile, ModuleError> {
        let caller_file = self.root.join(caller_relative);
        let caller = fs::read_to_string(&caller_file).expect("read fixture caller");
        let parsed = crate::parser::parse_file(&caller, &caller_file, &ProviderContext::default())
            .expect("fixture caller parses");
        resolve_modules(
            parsed,
            caller_file.parent().expect("caller file has a directory"),
        )
    }
}

#[test]
fn module_argument_if_resolves_true_and_false_value_branches() {
    let root = tempfile::tempdir().expect("temporary module fixture");
    let module_dir = root.path().join("choice");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&module_dir).expect("create module directory");
    fs::create_dir_all(&caller_dir).expect("create caller directory");
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
let selected = if flag { "yes" } else { "no" }
attributes {
  value: String = selected
}
"#,
    )
    .expect("write module");
    let caller = r#"
let choice = use { source = "../choice" }
let enabled = choice { flag = true }
let disabled = choice { flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).expect("write caller");

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("module conditionals expand");

    let value = |binding: &str| {
        parsed
            .compositions
            .iter()
            .find(|composition| composition.binding.as_deref() == Some(binding))
            .and_then(|composition| composition.signature.attributes.get("value"))
            .map(crate::resource::LiveCompositionAttribute::to_value)
            .unwrap_or_else(|| panic!("missing `{binding}.value`"))
    };
    assert_eq!(
        value("enabled"),
        Value::Concrete(ConcreteValue::String("yes".into()))
    );
    assert_eq!(
        value("disabled"),
        Value::Concrete(ConcreteValue::String("no".into()))
    );
}

#[test]
fn module_argument_if_selects_integer_arithmetic_branches() {
    let fixture = ModuleResolutionFixture::new();
    fixture.write(
        "choice/main.crn",
        r#"arguments {
  flag: Bool
  n: Int
}
attributes {
  value: Int = if flag { n + 1 } else { n - 1 }
  operand_value: Int = (if flag { n } else { n - 2 }) + 1
}
"#,
    );
    fixture.write(
        "caller/main.crn",
        r#"let choice = use { source = "../choice" }
let incremented = choice { flag = true  n = 10 }
let decremented = choice { flag = false  n = 10 }
"#,
    );

    let parsed = fixture
        .resolve_caller("caller/main.crn")
        .expect("module conditionals with integer arithmetic expand");
    let value = |binding: &str, attribute: &str| {
        parsed
            .compositions
            .iter()
            .find(|composition| composition.binding.as_deref() == Some(binding))
            .and_then(|composition| composition.signature.attributes.get(attribute))
            .map(crate::resource::LiveCompositionAttribute::to_value)
            .unwrap_or_else(|| panic!("missing `{binding}.{attribute}`"))
    };

    assert_eq!(
        value("incremented", "value"),
        Value::Concrete(ConcreteValue::Int(11))
    );
    assert_eq!(
        value("decremented", "value"),
        Value::Concrete(ConcreteValue::Int(9))
    );
    assert_eq!(
        value("incremented", "operand_value"),
        Value::Concrete(ConcreteValue::Int(11)),
        "a ModuleIf nested in an arithmetic operand must be selected",
    );
    assert_eq!(
        value("decremented", "operand_value"),
        Value::Concrete(ConcreteValue::Int(9)),
        "the false ModuleIf operand must be selected before arithmetic folds",
    );
}

#[test]
fn deferred_for_body_selects_module_if_before_deriving_dependencies() {
    let fixture = ModuleResolutionFixture::new();
    fixture.write(
        "choice/main.crn",
        r#"arguments { flag: Bool }
let seed = mock.test.resource {
  name = "seed"
  tags = { item = "value" }
}
let generated = for _, item in seed.tags {
  mock.test.resource {
    name = if flag { seed.name } else { item }
  }
}
"#,
    );
    fixture.write(
        "caller/main.crn",
        r#"let choice = use { source = "../choice" }
let enabled = choice { flag = true }
let disabled = choice { flag = false }
"#,
    );

    let parsed = fixture
        .resolve_caller("caller/main.crn")
        .expect("module conditionals in deferred-for bodies expand");
    let deferred = |iterable_binding: &str| {
        parsed
            .deferred_for_expressions()
            .iter()
            .find(|deferred| deferred.root_bindings().contains(iterable_binding))
            .unwrap_or_else(|| panic!("missing deferred loop for {iterable_binding}"))
    };

    let enabled = deferred("enabled.seed");
    assert_eq!(
        enabled.template_resource.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("seed".into()))),
    );
    assert_eq!(
        enabled.template_resource.dependency_bindings,
        BTreeSet::from(["enabled.seed".to_string()]),
        "dependency metadata must be derived from the selected value",
    );

    let disabled = deferred("disabled.seed");
    assert!(
        matches!(
            disabled.template_resource.get_attr("name"),
            Some(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::ForValue
            )))
        ),
        "the selected false branch must retain only the loop placeholder: {:?}",
        disabled.template_resource.get_attr("name"),
    );
    assert!(
        disabled.template_resource.dependency_bindings.is_empty(),
        "the unselected seed reference must not leave a dependency edge: {:?}",
        disabled.template_resource.dependency_bindings,
    );
}

#[test]
fn module_argument_if_deferred_value_is_not_serializable_or_deserializable() {
    let deferred = DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
        DeferredIfCondition::new(
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
                name: "flag".to_string(),
                source_line: 1.into(),
                diagnostic_name: None,
            })),
            PathBuf::from("module/main.crn"),
            1,
        ),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    )));

    let error = serde_json::to_string(&deferred)
        .expect_err("module-template conditionals must not cross a serde boundary");
    assert!(
        error
            .to_string()
            .contains("module-template conditional cannot be serialized before expansion"),
        "unexpected serialization error: {error}",
    );

    let arithmetic = crate::integer_arithmetic::ValidatedIntegerExpr::new(
        Value::Deferred(deferred.clone()),
        crate::integer_arithmetic::IntegerOp::Add,
        Value::Concrete(ConcreteValue::Int(1)),
        crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
    )
    .expect("an Int-valued module conditional is a valid arithmetic operand");
    let error = serde_json::to_string(&DeferredValue::IntegerArithmetic(arithmetic))
        .expect_err("an arithmetic operand must not hide a module-template conditional");
    assert!(
        error
            .to_string()
            .contains("module-template conditional cannot be serialized before expansion"),
        "unexpected nested serialization error: {error}",
    );

    let error = serde_json::from_str::<DeferredValue>(r#"{"ModuleIf":{}}"#)
        .expect_err("ModuleIf must not be a deserializable deferred-value variant");
    assert!(
        error.to_string().contains("unknown variant `ModuleIf`"),
        "unexpected deserialization error: {error}",
    );
}

#[test]
fn module_argument_if_materializes_structural_branches() {
    let root = tempfile::tempdir().expect("temporary structural-if fixture");
    let module_dir = root.path().join("choice");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&module_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
let selected = if flag {
  mock.test.Thing { value = "then" }
} else {
  mock.test.Thing { value = "else" }
}
let optional = if flag {
  mock.test.Optional { value = "present" }
}
"#,
    )
    .unwrap();
    let caller = r#"
let choice = use { source = "../choice" }
let enabled = choice { flag = true }
let disabled = choice { flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("structural branches expand");

    let attribute = |binding: &str| {
        parsed
            .resources
            .iter()
            .find(|resource| resource.binding.as_deref() == Some(binding))
            .and_then(|resource| resource.get_attr("value"))
            .cloned()
    };
    assert_eq!(
        attribute("enabled.selected"),
        Some(Value::Concrete(ConcreteValue::String("then".into())))
    );
    assert_eq!(
        attribute("disabled.selected"),
        Some(Value::Concrete(ConcreteValue::String("else".into())))
    );
    assert_eq!(
        attribute("enabled.optional"),
        Some(Value::Concrete(ConcreteValue::String("present".into())))
    );
    assert_eq!(attribute("disabled.optional"), None);
    assert!(parsed.deferred_module_ifs.is_empty());
}

#[test]
fn module_argument_if_resolves_nested_values_and_pure_function() {
    let root = tempfile::tempdir().expect("temporary nested-value fixture");
    let module_dir = root.path().join("choice");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&module_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
fn identity(value: Bool): Bool { value }
let carrier = mock.test.Thing {
  selected = if flag { "resource-yes" } else { "resource-no" }
  nested = [{ selected = if flag { "nested-yes" } else { "nested-no" } }]
}
attributes {
  nested = [{ selected = if flag { "yes" } else { "no" } }]
  by_function: String = if identity(flag) { "yes" } else { "no" }
}
"#,
    )
    .unwrap();
    let caller = r#"
let choice = use { source = "../choice" }
let enabled = choice { flag = true }
let disabled = choice { flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("nested conditionals expand");

    let output = |binding: &str, name: &str| {
        parsed
            .compositions
            .iter()
            .find(|composition| composition.binding.as_deref() == Some(binding))
            .and_then(|composition| composition.signature.attributes.get(name))
            .map(crate::resource::LiveCompositionAttribute::to_value)
            .unwrap_or_else(|| panic!("missing `{binding}.{name}`"))
    };
    assert_eq!(
        output("enabled", "by_function"),
        Value::Concrete(ConcreteValue::String("yes".into()))
    );
    assert_eq!(
        output("disabled", "by_function"),
        Value::Concrete(ConcreteValue::String("no".into()))
    );
    let Value::Concrete(ConcreteValue::List(items)) = output("enabled", "nested") else {
        panic!("nested output was not a list")
    };
    let Value::Concrete(ConcreteValue::Map(entry)) = &items[0] else {
        panic!("nested output item was not a map")
    };
    assert_eq!(
        entry.get("selected"),
        Some(&Value::Concrete(ConcreteValue::String("yes".into())))
    );
    let resource_value = |binding: &str, name: &str| {
        parsed
            .resources
            .iter()
            .find(|resource| resource.binding.as_deref() == Some(binding))
            .and_then(|resource| resource.get_attr(name))
            .cloned()
            .unwrap_or_else(|| panic!("missing `{binding}.{name}`"))
    };
    assert_eq!(
        resource_value("enabled.carrier", "selected"),
        Value::Concrete(ConcreteValue::String("resource-yes".into()))
    );
    assert_eq!(
        resource_value("disabled.carrier", "selected"),
        Value::Concrete(ConcreteValue::String("resource-no".into()))
    );
    let Value::Concrete(ConcreteValue::List(items)) = resource_value("disabled.carrier", "nested")
    else {
        panic!("nested resource attribute was not a list")
    };
    let Value::Concrete(ConcreteValue::Map(entry)) = &items[0] else {
        panic!("nested resource attribute item was not a map")
    };
    assert_eq!(
        entry.get("selected"),
        Some(&Value::Concrete(ConcreteValue::String("nested-no".into())))
    );
}

#[test]
fn module_argument_if_forwarding_resolves_in_outer_caller() {
    let root = tempfile::tempdir().expect("temporary forwarded-if fixture");
    let inner_dir = root.path().join("inner");
    let outer_dir = root.path().join("outer");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::create_dir_all(&outer_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
attributes { value: String = if flag { "yes" } else { "no" } }
"#,
    )
    .unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
let inner = use { source = "../inner" }
let forwarded = inner { flag = flag }
"#,
    )
    .unwrap();
    let caller = r#"
let outer = use { source = "../outer" }
let enabled = outer { flag = true }
let disabled = outer { flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("forwarded conditions expand");

    let nested_value = |binding: &str| {
        parsed
            .compositions
            .iter()
            .find(|composition| composition.binding.as_deref() == Some(binding))
            .and_then(|composition| composition.signature.attributes.get("value"))
            .map(crate::resource::LiveCompositionAttribute::to_value)
            .unwrap_or_else(|| panic!("missing `{binding}.value`"))
    };
    assert_eq!(
        nested_value("enabled.forwarded"),
        Value::Concrete(ConcreteValue::String("yes".into()))
    );
    assert_eq!(
        nested_value("disabled.forwarded"),
        Value::Concrete(ConcreteValue::String("no".into()))
    );
}

#[test]
fn module_argument_if_forwards_structural_selection_across_modules() {
    let root = tempfile::tempdir().expect("temporary forwarded structural-if fixture");
    let inner_dir = root.path().join("inner");
    let outer_dir = root.path().join("outer");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::create_dir_all(&outer_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments { inner_flag: Bool }
let optional = if inner_flag {
  mock.test.Thing { value = "present" }
}
"#,
    )
    .unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments { outer_flag: Bool }
let inner = use { source = "../inner" }
let forwarded = inner { inner_flag = outer_flag }
"#,
    )
    .unwrap();
    let caller = r#"
let outer = use { source = "../outer" }
let enabled = outer { outer_flag = true }
let disabled = outer { outer_flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed =
        resolve_modules(parsed, &caller_dir).expect("forwarded structural condition expands");

    assert!(
        parsed
            .resources
            .iter()
            .any(|resource| resource.binding.as_deref() == Some("enabled.forwarded.optional")),
        "true forwarding must materialize the inner resource: {:?}",
        parsed
            .resources
            .iter()
            .filter_map(|resource| resource.binding.as_deref())
            .collect::<Vec<_>>(),
    );
    assert!(
        parsed
            .resources
            .iter()
            .all(|resource| resource.binding.as_deref() != Some("disabled.forwarded.optional")),
        "false forwarding must not materialize the inner resource",
    );
}

#[test]
fn forwarded_structural_condition_is_not_rebound_by_a_colliding_callee_argument() {
    let root = tempfile::tempdir().expect("temporary forwarded collision fixture");
    let inner_dir = root.path().join("dbl");
    let middle_dir = root.path().join("m");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::create_dir_all(&middle_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  flag: Bool
  other: Bool
}
let a = if flag {
  mock.test.resource { name = "then" }
} else {
  mock.test.resource { name = "else" }
}
"#,
    )
    .unwrap();
    fs::write(
        middle_dir.join("main.crn"),
        r#"
arguments { other: Bool }
let inner = use { source = "../dbl" }
let mm = inner {
  flag = other
  other = true
}
"#,
    )
    .unwrap();
    let caller = r#"
let middle = use { source = "../m" }
let off = middle { other = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("forwarded condition expands");

    let selected = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("off.mm.a"))
        .expect("selected inner resource");
    assert_eq!(
        selected.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("else".into()))),
        "the already-substituted outer `other` argument was rebound by the inner `other` argument",
    );
}

#[test]
fn diagnostic_expansion_keeps_forwarded_structural_if_in_standalone_module() {
    let root = tempfile::tempdir().expect("temporary diagnostic forwarding fixture");
    let inner_dir = root.path().join("inner");
    let outer_dir = root.path().join("outer");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
let optional = if flag {
  mock.test.Thing { value = "present" }
}
"#,
    )
    .unwrap();
    let outer_source = r#"
arguments { flag: Bool }
let inner = use { source = "../inner" }
let forwarded = inner { flag = flag }
"#;
    let outer_file = outer_dir.join("main.crn");
    fs::write(&outer_file, outer_source).unwrap();

    let parsed = crate::parser::parse_file(outer_source, &outer_file, &ProviderContext::default())
        .expect("standalone outer module parses");
    let result = super::resolve_modules_with_providerless_diagnostics(
        &parsed,
        &outer_dir,
        &ProviderContext::default(),
        &crate::schema::SchemaRegistry::new(),
        &[],
    );

    let ModuleResolutionDiagnosticResult::Expanded { graph } = result else {
        panic!("standalone diagnostic expansion rejected a forwarded template")
    };
    assert_eq!(
        graph.deferred_module_ifs.len(),
        1,
        "editor expansion must retain the forwarded structural template",
    );
}

#[test]
fn module_argument_if_conditional_module_call_materializes_only_selected_branch() {
    let root = tempfile::tempdir().expect("temporary conditional-call fixture");
    let leaf_dir = root.path().join("modules/leaf");
    let chooser_dir = root.path().join("modules/chooser");
    let caller_dir = root.path().join("apps/caller");
    fs::create_dir_all(&leaf_dir).unwrap();
    fs::create_dir_all(&chooser_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        leaf_dir.join("main.crn"),
        r#"
arguments { label: String }
let item = mock.test.Thing { value = label }
"#,
    )
    .unwrap();
    fs::write(
        chooser_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
let leaf = use { source = "../leaf" }
let selected = if flag {
  leaf { label = "then" }
} else {
  leaf { label = "else" }
}
"#,
    )
    .unwrap();
    let caller = r#"
let chooser = use { source = "../../modules/chooser" }
let enabled = chooser { flag = true }
let disabled = chooser { flag = false }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir).expect("conditional module calls expand");

    let value = |binding: &str| {
        parsed
            .resources
            .iter()
            .find(|resource| resource.binding.as_deref() == Some(binding))
            .and_then(|resource| resource.get_attr("value"))
            .cloned()
    };
    assert_eq!(
        value("enabled.selected.item"),
        Some(Value::Concrete(ConcreteValue::String("then".into())))
    );
    assert_eq!(
        value("disabled.selected.item"),
        Some(Value::Concrete(ConcreteValue::String("else".into())))
    );
    let selected_call = parsed
        .compositions
        .iter()
        .find(|composition| composition.binding.as_deref() == Some("enabled.selected"))
        .expect("selected nested module composition");
    let expected_leaf_dir = leaf_dir.canonicalize().unwrap();
    assert_eq!(
        selected_call
            .call_path()
            .immediate()
            .module_directory
            .as_deref(),
        Some(expected_leaf_dir.as_path()),
        "a retained conditional call must keep the directory where its import was prepared",
    );
}

#[test]
fn conditional_module_call_keeps_caller_refs_distinct_from_colliding_local_bindings() {
    let root = tempfile::tempdir().expect("temporary conditional-call collision fixture");
    let leaf_dir = root.path().join("leaf");
    let chooser_dir = root.path().join("chooser");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&leaf_dir).unwrap();
    fs::create_dir_all(&chooser_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        leaf_dir.join("main.crn"),
        r#"
arguments {
  caller: mock.test.resource
  local: mock.test.resource
}
let nested = mock.test.resource {
  name = "nested"
  parent = caller.name
  parent_name = local.name
}
"#,
    )
    .unwrap();
    fs::write(
        chooser_dir.join("main.crn"),
        r#"
arguments {
  flag: Bool
  caller: mock.test.resource
}
let shared = mock.test.resource { name = "module-local" }
let leaf = use { source = "../leaf" }
let selected = if flag {
  leaf {
    caller = caller
    local = shared
  }
}
"#,
    )
    .unwrap();
    let caller = r#"
let shared = mock.test.resource { name = "caller" }
let chooser = use { source = "../chooser" }
let enabled = chooser { flag = true caller = shared }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let mut config = ProviderContext::default();
    config
        .resource_types
        .insert(("mock".to_string(), "test.resource".to_string()));
    let parsed = crate::parser::parse_file(caller, &caller_file, &config).expect("caller parse");
    let parsed = resolve_modules_with_config(
        parsed,
        &caller_dir,
        &config,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("conditional nested call expands in the chooser's module scope");

    let nested = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("enabled.selected.nested"))
        .expect("selected leaf resource");
    assert!(
        matches!(
            nested.get_attr("parent"),
            Some(Value::Deferred(DeferredValue::ResourceRef { path }))
                if path.binding() == "shared" && path.attribute() == "name"
        ),
        "caller reference was captured by the chooser-local `shared`: {:?}",
        nested.get_attr("parent"),
    );
    assert!(
        matches!(
            nested.get_attr("parent_name"),
            Some(Value::Deferred(DeferredValue::ResourceRef { path }))
                if path.binding() == "enabled.shared" && path.attribute() == "name"
        ),
        "chooser-local reference was not prefixed: {:?}",
        nested.get_attr("parent_name"),
    );
}

#[test]
fn dropped_structural_if_binding_never_falls_back_to_caller_scope() {
    let root = tempfile::tempdir().expect("temporary dropped-binding fixture");
    let module_dir = root.path().join("choice");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"arguments { flag: Bool }
let optional = if flag {
  mock.test.resource { name = "module-optional" }
}
let consumer = mock.test.resource {
  name = "consumer"
  parent_name = optional.name
}
"#,
    )
    .unwrap();

    let resolve = |flag: bool| {
        let caller_dir = root.path().join(if flag { "enabled" } else { "disabled" });
        fs::create_dir_all(&caller_dir).unwrap();
        let caller = format!(
            r#"let optional = mock.test.resource {{ name = "caller-optional" }}
let choice = use {{ source = "../choice" }}
let instance = choice {{ flag = {flag} }}
"#,
        );
        let caller_file = caller_dir.join("main.crn");
        fs::write(&caller_file, &caller).unwrap();
        let parsed = crate::parser::parse_file(&caller, &caller_file, &ProviderContext::default())
            .expect("caller parse");
        resolve_modules(parsed, &caller_dir)
    };

    let enabled = resolve(true).expect("selected optional binding remains available");
    let consumer = enabled
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("instance.consumer"))
        .expect("expanded consumer");
    assert!(
        matches!(
            consumer.get_attr("parent_name"),
            Some(Value::Deferred(DeferredValue::ResourceRef { path }))
                if path.binding() == "instance.optional"
        ),
        "selected branch reference did not stay module-local: {:?}",
        consumer.get_attr("parent_name"),
    );

    let error = resolve(false)
        .expect_err("a dropped module binding must not resolve against the caller collision")
        .to_string();
    assert!(
        error.contains("optional"),
        "binding missing from error: {error}"
    );
    assert!(
        error.contains("condition `flag`") && !error.contains("(unresolved module argument: flag)"),
        "condition source expression missing from error: {error}"
    );
    assert!(
        error.contains("choice/main.crn") && error.contains("line 2"),
        "module source missing from error: {error}",
    );
}

#[test]
fn dropped_structural_if_binding_is_reported_from_wait_target() {
    let fixture = ModuleResolutionFixture::new();
    fixture.write(
        "choice/main.crn",
        r#"arguments { flag: Bool }
let conditional = if flag {
  mock.test.resource { name = "conditional" }
}
let ready = wait conditional {
  until = conditional.name == "conditional"
}
"#,
    );
    fixture.write(
        "caller/main.crn",
        r#"let choice = use { source = "../choice" }
let instance = choice { flag = false }
"#,
    );

    let error = fixture
        .resolve_caller("caller/main.crn")
        .expect_err("a wait target cannot refer to a dropped conditional binding")
        .to_string();
    assert!(
        error.contains("binding 'conditional' is defined only in a dropped branch")
            && error.contains("condition `flag`")
            && error.contains("choice/main.crn"),
        "wait target did not use the dedicated dropped-binding diagnostic: {error}",
    );
}

#[test]
fn dropped_structural_if_binding_is_reported_from_wait_depends_on() {
    let fixture = ModuleResolutionFixture::new();
    fixture.write(
        "choice/main.crn",
        r#"arguments { flag: Bool }
let conditional = if flag {
  mock.test.resource { name = "conditional" }
}
let stable = mock.test.resource { name = "stable" }
let ready = wait stable {
  until = stable.name == "stable"
  depends_on = [conditional]
}
"#,
    );
    fixture.write(
        "caller/main.crn",
        r#"let choice = use { source = "../choice" }
let instance = choice { flag = false }
"#,
    );

    let error = fixture
        .resolve_caller("caller/main.crn")
        .expect_err("wait depends_on cannot refer to a dropped conditional binding")
        .to_string();
    assert!(
        error.contains("binding 'conditional' is defined only in a dropped branch")
            && error.contains("condition `flag`")
            && error.contains("choice/main.crn"),
        "wait depends_on did not use the dedicated dropped-binding diagnostic: {error}",
    );
}

#[test]
fn same_condition_guarded_references_are_checked_after_value_branch_selection() {
    let root = tempfile::tempdir().expect("temporary guarded-reference fixture");
    let module_dir = root.path().join("choice");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&module_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"arguments { flag: Bool }
let a = if flag {
  mock.test.resource { name = "a" }
}
let b = mock.test.resource {
  name = "b"
  comment = if flag { a.name } else { "none" }
}
attributes {
  n: String = if flag { a.name } else { "none" }
}
"#,
    )
    .unwrap();
    let caller = r#"let choice = use { source = "../choice" }
let off = choice { flag = false }
let on = choice { flag = true }
"#;
    let caller_file = caller_dir.join("main.crn");
    fs::write(&caller_file, caller).unwrap();

    let parsed = crate::parser::parse_file(caller, &caller_file, &ProviderContext::default())
        .expect("caller parses");
    let parsed = resolve_modules(parsed, &caller_dir)
        .expect("guarded references must be checked after selecting their value branch");

    let off = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("off.b"))
        .expect("false instance consumer");
    assert_eq!(
        off.get_attr("comment"),
        Some(&Value::Concrete(ConcreteValue::String("none".into())))
    );
    assert!(
        parsed
            .resources
            .iter()
            .any(|resource| resource.binding.as_deref() == Some("on.a")),
        "true instance must retain the selected conditional binding"
    );
    assert!(
        !parsed
            .resources
            .iter()
            .any(|resource| resource.binding.as_deref() == Some("off.a")),
        "false instance must drop the conditional binding"
    );

    let off_output = parsed
        .compositions
        .iter()
        .find(|composition| composition.binding.as_deref() == Some("off"))
        .and_then(|composition| composition.signature.attributes.get("n"))
        .map(crate::resource::LiveCompositionAttribute::to_value)
        .expect("false instance output");
    assert_eq!(
        off_output,
        Value::Concrete(ConcreteValue::String("none".into()))
    );
}

#[test]
fn module_argument_if_post_substitution_errors_use_module_source_and_line() {
    let root = tempfile::tempdir().expect("temporary deferred-if error fixture");
    let runtime_dir = root.path().join("runtime");
    let non_bool_dir = root.path().join("non_bool");
    let caller_dir = root.path().join("caller");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&non_bool_dir).unwrap();
    fs::create_dir_all(&caller_dir).unwrap();
    fs::write(
        runtime_dir.join("main.crn"),
        "arguments { flag: Bool }\nattributes {\n  value = if flag { 'yes' } else { 'no' }\n}\n",
    )
    .unwrap();
    fs::write(
        non_bool_dir.join("main.crn"),
        "arguments { flag: String }\nattributes {\n  value = if flag { 'yes' } else { 'no' }\n}\n",
    )
    .unwrap();

    let runtime_caller = r#"
let runtime = use { source = "../runtime" }
let source = mock.test.Thing { ready = true }
let selected = runtime { flag = source.ready }
"#;
    let runtime_file = caller_dir.join("runtime.crn");
    let parsed =
        crate::parser::parse_file(runtime_caller, &runtime_file, &ProviderContext::default())
            .expect("runtime caller parses");
    let error = resolve_modules(parsed, &caller_dir)
        .expect_err("runtime argument must fail after substitution")
        .to_string();
    assert!(error.contains("runtime/main.crn"), "{error}");
    assert!(error.contains("line 3"), "{error}");
    assert!(
        error.contains("if condition depends on a runtime value; condition must be statically known at parse time"),
        "{error}"
    );

    let non_bool_caller = r#"
let non_bool = use { source = "../non_bool" }
let selected = non_bool { flag = "not-bool" }
"#;
    let non_bool_file = caller_dir.join("non_bool.crn");
    let parsed =
        crate::parser::parse_file(non_bool_caller, &non_bool_file, &ProviderContext::default())
            .expect("non-bool caller parses");
    let error = resolve_modules(parsed, &caller_dir)
        .expect_err("non-Bool argument must fail after substitution")
        .to_string();
    assert!(error.contains("non_bool/main.crn"), "{error}");
    assert!(error.contains("line 3"), "{error}");
    assert!(
        error.contains("if condition must be a Bool value, got: string"),
        "{error}"
    );
}

#[test]
fn resolve_modules_rejects_a_root_module_template_before_publication() {
    let root = tempfile::tempdir().expect("temporary root module template");
    let source_file = root.path().join("main.crn");
    let source = r#"
arguments { flag: Bool }
attributes { value = if flag { "yes" } else { "no" } }
"#;
    fs::write(&source_file, source).unwrap();
    let parsed = crate::parser::parse_file(source, &source_file, &ProviderContext::default())
        .expect("standalone module template parses");

    let error = resolve_modules(parsed, root.path())
        .expect_err("the ordinary resolver must not publish an unresolved root template")
        .to_string();
    assert!(
        error.contains("cannot publish a reusable module template")
            && error.contains("argument-dependent `if`")
            && !error.contains("Internal parser error"),
        "unexpected root publication error: {error}",
    );
}

#[test]
fn resolve_modules_rejects_a_deferred_contribution_before_merging_it() {
    let root = tempfile::tempdir().expect("temporary leaking contribution fixture");
    let choice_dir = root.path().join("choice");
    let outer_dir = root.path().join("outer");
    fs::create_dir_all(&choice_dir).unwrap();
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        choice_dir.join("main.crn"),
        r#"
arguments { flag: Bool }
attributes { value = if flag { "yes" } else { "no" } }
"#,
    )
    .unwrap();
    let source = r#"
arguments { flag: Bool }
let choice = use { source = "../choice" }
let forwarded = choice { flag = flag }
"#;
    let source_file = outer_dir.join("main.crn");
    fs::write(&source_file, source).unwrap();
    let parsed = crate::parser::parse_file(source, &source_file, &ProviderContext::default())
        .expect("standalone forwarding module parses");

    let error = resolve_modules(parsed, &outer_dir)
        .expect_err("a deferred module contribution must fail its publication conversion")
        .to_string();
    assert!(error.contains("deferred module conditionals"), "{error}");
}

fn parsed_deferred_resource(expression: DeferredForExpression) -> ParsedDeferredForExpression {
    let DeferredForExpression {
        file,
        line,
        header,
        resource_type: _,
        attributes: _,
        target,
        iterable,
        binding,
        template_resource,
    } = expression;
    ParsedDeferredForExpression {
        file,
        line,
        span: SourceSpan::dummy(),
        header,
        target,
        iterable,
        iterable_state: crate::parser::ParsedDeferredForIterableState::Deferred,
        binding,
        body: DeferredForBody::Resource(Box::new(template_resource)),
    }
}

fn resolve_anonymous_loop_module_fixture(
    module_body: &str,
    root_body: &str,
) -> ModuleResolvedParsedFile {
    let root = tempfile::tempdir().expect("temporary module fixture");
    let module_dir = root.path().join("modules/network");
    fs::create_dir_all(&module_dir).expect("create module directory");
    fs::write(module_dir.join("main.crn"), module_body).expect("write module body");
    let root_file = root.path().join("main.crn");
    fs::write(&root_file, root_body).expect("write root body");

    let parsed = crate::parser::parse_file(root_body, &root_file, &ProviderContext::default())
        .expect("root fixture parses");
    resolve_modules(parsed, root.path()).expect("module fixture expands")
}

fn loop_module_identity_registry() -> crate::schema::SchemaRegistry {
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Leaf")
            .attribute(
                AttributeSchema::optional("identity_input", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("loop module schema is valid"),
    );
    registry
}

#[test]
fn identical_loop_generated_module_calls_emit_one_complete_loop_rewrite() {
    let module =
        "arguments { label: String }\nlet leaf = mock.test.Leaf { identity_input = label }\n";
    let root = "let network = use { source = './modules/network' }\n\
                for label in ['same', 'same', 'other'] {\n  network { label = label }\n}\n";
    let mut parsed = resolve_anonymous_loop_module_fixture(module, root);

    let error = crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &crate::schema::SchemaRegistry::new(),
        &|_| Vec::new(),
    )
    .expect_err("identical anonymous module prefixes must not silently share rows");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        1,
        "one authored loop must produce one error: {rendered}",
    );
    assert!(
        rendered.contains("let network_2 = for label in ['same', 'same', 'other']"),
        "the module use binding must remain available inside the rewritten loop: {rendered}",
    );
    for index in 0..3 {
        assert!(
            rendered.contains(&format!("from = mock.test.Leaf '_for0[{index}].leaf'")),
            "missing legacy inner-row move for element {index}: {rendered}",
        );
        assert!(
            rendered.contains(&format!("to = mock.test.Leaf 'network_2[{index}].leaf'")),
            "missing bound inner-row move for element {index}: {rendered}",
        );
    }
}

#[test]
fn loop_generated_module_calls_do_not_hide_module_internal_collisions() {
    let module = "arguments { label: String }\n\
                  mock.test.Leaf { identity_input = 'inner' }\n\
                  mock.test.Leaf { identity_input = 'inner' }\n";
    let root = "let network = use { source = './modules/network' }\n\
                for label in ['same', 'same'] { network { label = label } }\n";
    let mut parsed = resolve_anonymous_loop_module_fixture(module, root);

    let error = crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &loop_module_identity_registry(),
        &|_| Vec::new(),
    )
    .expect_err("binding the outer loop cannot repair a collision inside the module");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        1,
        "the outer loop still needs one complete rewrite: {rendered}",
    );
    assert!(
        rendered.contains("Anonymous resource identifier collision"),
        "the independent module-internal collision was hidden: {rendered}",
    );
}

#[test]
fn identical_module_prefixes_from_distinct_loop_sources_are_typed_errors() {
    let module =
        "arguments { label: String }\nlet leaf = mock.test.Leaf { identity_input = label }\n";
    let root = "let network = use { source = './modules/network' }\n\
                for label in ['same'] { network { label = label } }\n\
                for label in ['same'] { network { label = label } }\n";
    let mut parsed = resolve_anonymous_loop_module_fixture(module, root);

    let error = crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &crate::schema::SchemaRegistry::new(),
        &|_| Vec::new(),
    )
    .expect_err("distinct loop sources cannot share an anonymous module prefix");

    assert!(
        matches!(
            error,
            crate::identifier::AnonymousIdentityError::Collision(_)
        ),
        "cross-source sharing must use a typed plain collision: {error:?}",
    );
}

#[test]
fn identical_plain_anonymous_module_calls_are_typed_errors() {
    let module =
        "arguments { label: String }\nlet leaf = mock.test.Leaf { identity_input = label }\n";
    let root = "let network = use { source = './modules/network' }\n\
                network { label = 'same' }\n\
                network { label = 'same' }\n";
    let mut parsed = resolve_anonymous_loop_module_fixture(module, root);

    let error = crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &crate::schema::SchemaRegistry::new(),
        &|_| Vec::new(),
    )
    .expect_err("plain anonymous calls cannot silently share an instance prefix");

    assert!(
        matches!(
            error,
            crate::identifier::AnonymousIdentityError::Collision(_)
        ),
        "plain-call sharing must use a typed collision: {error:?}",
    );
}

#[test]
fn loop_inside_module_is_reported_once_across_instances_with_local_name_scope() {
    let module = "arguments { label: String }\n\
                  for value in ['same', 'same'] {\n  mock.test.Leaf { identity_input = value }\n}\n";
    let root = "let leaf = mock.test.Root { value = 'reserved only at root' }\n\
                let network = use { source = './modules/network' }\n\
                let first = network { label = 'first' }\n\
                let second = network { label = 'second' }\n";
    let mut parsed = resolve_anonymous_loop_module_fixture(module, root);

    let error = crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &loop_module_identity_registry(),
        &|_| Vec::new(),
    )
    .expect_err("each expanded instance contains the same unsafe authored loop");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        1,
        "the authored module loop must be reported once: {rendered}",
    );
    assert!(
        rendered.contains("let leaf = for value in ['same', 'same']"),
        "a root binding must not reserve a module-local suggestion: {rendered}",
    );
    assert!(!rendered.contains("let leaf_2 = for"));
    for instance in ["first", "second"] {
        for index in 0..2 {
            assert!(
                rendered.contains(&format!(
                    "from = mock.test.Leaf '{instance}._for0[{index}]'"
                )),
                "missing legacy move for {instance}[{index}]: {rendered}",
            );
            assert!(
                rendered.contains(&format!("to = mock.test.Leaf '{instance}.leaf[{index}]'")),
                "missing current-prefix move for {instance}[{index}]: {rendered}",
            );
        }
    }
}

#[test]
fn nested_unbound_module_loop_migrates_composed_legacy_address() {
    let root = tempfile::tempdir().expect("temporary configuration root");
    let module_dir = root.path().join("modules/network");
    fs::create_dir_all(&module_dir).expect("create module directory");
    fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  label: String\n}\n\n\
         for inner in ['inner'] {\n  mock.test.resource { name = inner }\n}\n",
    )
    .expect("write module fixture");
    let root_source = "let network = use { source = './modules/network' }\n\n\
                       for outer in ['outer'] {\n  network { label = outer }\n}\n";
    let root_file = root.path().join("main.crn");
    fs::write(&root_file, root_source).expect("write root fixture");

    let parsed = crate::parser::parse_file(root_source, &root_file, &ProviderContext::default())
        .expect("root fixture parses");
    let mut parsed = resolve_modules(parsed, root.path()).expect("nested loop module expands");
    assert_eq!(
        parsed.resources.len(), // allow: direct — fixture test inspection
        1,
        "fixture emits one inner resource"
    );
    assert_eq!(
        parsed.resources[0]
            .loop_provenance
            .display_origin()
            .expect("inner loop origin")
            .file(),
        "modules/network/main.crn",
        "loop origin labels must be relative to the configuration root",
    );

    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("resource schema is valid"),
    );
    crate::identifier::compute_anonymous_identifiers_for_test(
        &mut parsed.resources,
        &[],
        &registry,
        &|_| Vec::new(),
    )
    .expect("compute anonymous identity");

    let identity = parsed.resources[0]
        .id
        .identity()
        .expect("computed resource identity");
    let candidates = parsed.resources[0]
        .loop_provenance
        .legacy_candidates(identity)
        .into_iter()
        .map(|candidate| candidate.exact_state_name().to_string())
        .collect::<Vec<_>>();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate == "_for0[0]._for0[0]"),
        "nested loop expansion must retain the recursively composed legacy address: {candidates:?}",
    );

    let claims = crate::identifier::StateBlockClaims::empty();
    let state_entry = crate::identifier::AnonymousIdStateInfo::try_from_state_row(
        crate::resource::ResolvedResourceId::with_provider_identity(
            "mock",
            "test.resource",
            ResourceIdentity::new("_for0[0]._for0[0]"),
            None,
        ),
        None,
        registry
            .get("mock", "test.resource", crate::schema::SchemaKind::Resource)
            .expect("state fixture schema")
            .schema(),
        &HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("inner".into())),
        )]),
        &HashMap::new(),
    )
    .expect("state fixture has a valid resolved address");
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &registry,
        parsed.iter_all_resources().map(|resource| resource.id()),
    );
    let moves = crate::identifier::detect_legacy_loop_address_moves(
        &parsed.resources,
        &registry,
        &comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                [state_entry.clone()],
                crate::identifier::AnonymousIdStateInfo::name,
            )
        },
        &|_binding| Vec::new(),
        &claims,
        &[],
    )
    .expect("legacy-loop comparison fixture must resolve");
    assert_eq!(
        moves.len(),
        1,
        "the nested legacy row must migrate exactly once"
    );
    assert_eq!(moves[0].from().identity_str(), "_for0[0]._for0[0]");
    assert_eq!(
        moves[0].to().identity_str(),
        parsed.resources[0]
            .id
            .identity_str()
            .expect("resolved destination"),
    );
}

#[test]
fn resolver_error_from_parsed_call_retains_call_identity() {
    let source = "let missing = use { source = './missing' }\nmissing {}\n";
    let base = tempfile::tempdir().expect("temporary module root");
    let source_file = base.path().join("main.crn");
    let parsed = crate::parser::parse_file(source, &source_file, &ProviderContext::default())
        .expect("fixture must parse");
    let expected_span = parsed.module_calls[0].source_span();
    assert_eq!(
        (
            expected_span.start_line(),
            expected_span.start_column(),
            expected_span.end_line(),
            expected_span.end_column(),
        ),
        (2, 1, 2, 11),
        "fixture must pin the authored call range",
    );

    let error = super::resolve_modules_with_providerless_config(
        parsed,
        base.path(),
        &ProviderContext::default(),
        &crate::schema::SchemaRegistry::new(),
    )
    .expect_err("missing imported module must fail");

    let ModuleErrorCallSource::Owners(owners) = error.call_source() else {
        panic!("parsed call must own the resolver error: {error:?}");
    };
    assert_eq!(owners.len(), 1, "parsed call identity must not be dropped");
    assert_eq!(owners[0].source_file(), source_file);
    assert_eq!(owners[0].call_span(), expected_span);
}

fn create_test_module() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![Resource {
            id: ResourceId::with_provider_identity("aws", "security_group", "sg", None),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("sg".into())),
                );
                attrs.insert(
                    "vpc_id".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "vpc_id".to_string(),
                    }),
                );
                attrs.into_iter().collect()
            },
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "vpc_id".to_string(),
                static_type: known(TypeExpr::String),
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "enable_flag".to_string(),
                static_type: known(TypeExpr::Bool),
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_substitute_arguments() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".into())),
    );

    // Argument params are lexically scoped: binding_name is the param name itself
    let value = Value::Deferred(DeferredValue::BindingRef {
        binding: "vpc_id".to_string(),
    });
    let result = substitute_arguments(&value, &inputs).expect("substitution");

    assert_eq!(
        result,
        Value::Concrete(ConcreteValue::String("vpc-123".into()))
    );
}

#[test]
fn test_substitute_arguments_nested() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "port".to_string(),
        Value::Concrete(ConcreteValue::Int(8080)),
    );

    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Deferred(DeferredValue::BindingRef {
            binding: "port".to_string(),
        }),
        Value::Concrete(ConcreteValue::Int(443)),
    ]));
    let result = substitute_arguments(&value, &inputs).expect("substitution");

    match result {
        Value::Concrete(ConcreteValue::List(items)) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], Value::Concrete(ConcreteValue::Int(8080)));
            assert_eq!(items[1], Value::Concrete(ConcreteValue::Int(443)));
        }
        _ => panic!("Expected list"),
    }
}

#[test]
fn substitute_arguments_enters_secret_only_along_module_if_paths() {
    let module_argument = |name: &str| {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
            name: name.to_string(),
            source_line: 1.into(),
            diagnostic_name: None,
        }))
    };
    let conditional = Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
        DeferredIfCondition::new(module_argument("flag"), PathBuf::from("module/main.crn"), 1),
        Value::Concrete(ConcreteValue::String("yes".into())),
        Value::Concrete(ConcreteValue::String("no".into())),
    ))));
    let value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::List(vec![module_argument("plain"), conditional]),
    ))));
    let inputs = HashMap::from([
        (
            "flag".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "plain".to_string(),
            Value::Concrete(ConcreteValue::String("must-stay-deferred".into())),
        ),
    ]);

    let substituted = substitute_arguments(&value, &inputs).expect("secret substitution");
    let Value::Deferred(DeferredValue::Secret(inner)) = substituted else {
        panic!("secret wrapper was not preserved")
    };
    let Value::Concrete(ConcreteValue::List(items)) = inner.as_ref() else {
        panic!("secret list was not preserved")
    };
    assert_eq!(
        items[0],
        module_argument("plain"),
        "ordinary argument substitution inside Secret is outside #4149's behavior change",
    );
    assert_eq!(
        items[1],
        Value::Concrete(ConcreteValue::String("yes".into())),
        "a ModuleIf nested inside Secret still has to be selected",
    );
}

#[test]
fn module_argument_substitution_folds_and_checks_integer_arithmetic() {
    let divisor = Value::Deferred(DeferredValue::Unknown(
        crate::resource::UnknownReason::ModuleArgument {
            name: "divisor".to_string(),
            source_line: 1.into(),
            diagnostic_name: None,
        },
    ));
    let value = crate::integer_arithmetic::evaluate_integer_values(
        Value::Concrete(ConcreteValue::Int(10)),
        crate::integer_arithmetic::IntegerOp::Divide,
        divisor,
        crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
    )
    .expect("typed module argument is a structurally valid parser-stage operand");

    assert_eq!(
        substitute_arguments(
            &value,
            &HashMap::from([(
                "divisor".to_string(),
                Value::Concrete(ConcreteValue::Int(2)),
            )]),
        )
        .expect("known operands must fold during substitution"),
        Value::Concrete(ConcreteValue::Int(5)),
    );

    let error = substitute_arguments(
        &value,
        &HashMap::from([(
            "divisor".to_string(),
            Value::Concrete(ConcreteValue::Int(0)),
        )]),
    )
    .expect_err("a known zero module argument must fail during substitution");
    let error = format!("{error:?}");
    assert!(error.contains("division by zero"), "{error}");
}

#[test]
fn test_substitute_arguments_recurses_into_residual_coalesce() {
    let inputs = HashMap::from([(
        "fallback".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("default".into()),
        )])),
    )]);
    let value = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::resource_ref("up", "accounts", vec![])),
        default: Box::new(Value::Deferred(DeferredValue::BindingRef {
            binding: "fallback".to_string(),
        })),
    });

    let result = substitute_arguments(&value, &inputs).expect("substitution");
    let Value::Deferred(DeferredValue::Coalesce { value, default }) = result else {
        panic!("unresolved left operand must retain coalesce");
    };
    assert!(matches!(
        value.as_ref(),
        Value::Deferred(DeferredValue::ResourceRef { path })
            if path.binding() == "up" && path.attribute() == "accounts"
    ));
    assert_eq!(
        default.as_ref(),
        &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("default".into()),
        )])),
    );
}

/// Module containing one anonymous resource (`id.name` is `Pending`,
/// no `let` binding). Used to verify that expansion preserves
/// `Pending` rather than collapsing it to `Bound("<instance>.")` —
/// a `Bound` value with a trailing dot would slip past
/// `compute_anonymous_identifiers`'s absent-identity filter and never
/// receive its hash-derived suffix (#2516).
fn create_test_module_with_anonymous_resource() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![Resource {
            id: ResourceId::pending_with_provider("awscc", "iam.RolePolicy", None),
            attributes: {
                let mut attrs = IndexMap::new();
                attrs.insert(
                    "policy_name".to_string(),
                    Value::Concrete(ConcreteValue::String("inline".into())),
                );
                attrs
            },
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_expand_anonymous_resource_in_named_module_keeps_identity_absent() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "policy_module".to_string(),
            create_test_module_with_anonymous_resource(),
        );
        r
    };
    let call = ModuleCall::test(
        "policy_module".to_string(),
        Some("bootstrap".to_string()),
        HashMap::new(),
    );

    let expanded = resolver
        .expand_module_call(&call, "bootstrap")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);
    let policy = &expanded[0];
    assert!(
        matches!(
            policy.id.identity_state(),
            crate::resource::ResourceIdentityState::Pending(_)
        ),
        "anonymous resource inside a module instance must keep identity absent after expansion \
         (compute_anonymous_identifiers filters on absent identity and would skip an assigned value); \
         got {:?}",
        policy.id.identity_state(),
    );
    let Some(crate::resource::ModuleSource::Module(source)) = &policy.module_source else {
        panic!("expanded resource must retain its live module source")
    };
    assert_eq!(source.name(), "policy_module");
    assert_eq!(source.instance(), "bootstrap");
    assert_eq!(&source.module_call_path().immediate().id, call.id());
}

#[test]
fn anonymous_module_resource_instantiated_twice_has_distinct_pending_ids() {
    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver.imported_modules.insert(
            "policy_module".to_string(),
            create_test_module_with_anonymous_resource(),
        );
        resolver
    };
    let call = ModuleCall::test("policy_module".to_string(), None, HashMap::new());

    let first = resolver
        .expand_module_call(&call, "first")
        .unwrap()
        .resources
        .into_iter()
        .next()
        .unwrap();
    let second = resolver
        .expand_module_call(&call, "second")
        .unwrap()
        .resources
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(first.id.identity_str(), None);
    assert_eq!(second.id.identity_str(), None);
    assert_ne!(first.id, second.id);

    let resources = vec![first.clone(), second.clone()];
    let sorted = crate::deps::sort_resources_by_dependencies(&resources).unwrap();
    assert_eq!(sorted.len(), 2);
    assert_eq!(sorted[0].id, first.id);
    assert_eq!(sorted[1].id, second.id);
}

#[test]
fn test_expand_module_call() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".into())),
            );
            module_arguments(args)
        },
    );

    let expanded = resolver
        .expand_module_call(&call, "my_instance")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let sg = &expanded[0];
    assert_eq!(
        sg.id.identity_str().expect("resolved identity"),
        "my_instance.sg"
    );
    assert_eq!(
        sg.get_attr("vpc_id"),
        Some(&Value::Concrete(ConcreteValue::String("vpc-456".into())))
    );
    let Some(crate::resource::ModuleSource::Module(source)) = &sg.module_source else {
        panic!("expanded resource must retain its live module source")
    };
    assert_eq!(source.name(), "test_module");
    assert_eq!(source.instance(), "my_instance");
    assert_eq!(&source.module_call_path().immediate().id, call.id());
    // Module info should NOT be in attributes
    assert!(!sg.attributes.contains_key("_module"));
    assert!(!sg.attributes.contains_key("_module_instance"));
}

/// Module whose resource targets a named provider instance via
/// `directives.provider_instance`. Reproduces carina#3038: expansion
/// rebuilt `id` with `with_provider(...)`, silently dropping the
/// `provider_instance` field. `ProviderRouter` keys on
/// `(id.provider, id.provider_instance)`, so the lost binding made
/// `create` dispatch to the kind's default instance even though
/// state-writeback (which reads `directives.provider_instance` from
/// the `Resource`, not the `id`) still recorded the routing
/// correctly. Net effect for users: an ACM cert that should live in
/// `us-east-1` (where CloudFront viewer certs *must* live) lands in
/// the default region instead, and subsequent `read` against the
/// recorded `us` instance fails with `ResourceNotFoundException`.
fn create_module_with_named_provider_instance() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![Resource {
            id: ResourceId::with_provider_identity(
                "aws",
                "acm.Certificate",
                "cert",
                Some("us".to_string()),
            ),
            attributes: {
                let mut attrs = IndexMap::new();
                attrs.insert(
                    "domain_name".to_string(),
                    Value::Concrete(ConcreteValue::String("example.com".into())),
                );
                attrs
            },
            directives: Directives::default().with_provider_instance("us"),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("cert".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_expand_module_call_preserves_provider_instance_on_id() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "registry".to_string(),
            create_module_with_named_provider_instance(),
        );
        r
    };

    let call = ModuleCall::test(
        "registry".to_string(),
        Some("acme".to_string()),
        HashMap::new(),
    );

    let expanded = resolver
        .expand_module_call(&call, "acme")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);
    let cert = &expanded[0];
    assert_eq!(
        cert.id.provider_instance.as_deref(),
        Some("us"),
        "module expansion must preserve `id.provider_instance` so \
         ProviderRouter dispatches `create` through the named \
         instance (carina#3038)"
    );
    // The directives copy stays correct (and is what state writeback
    // already reads from) — assert it too so a regression that flips
    // *both* sides at once is still caught.
    assert_eq!(
        cert.directives.provider_instance.as_deref(),
        Some("us"),
        "directives.provider_instance must survive module expansion"
    );
}

#[test]
fn test_reconcile_anonymous_module_instances_preserves_provider_instance() {
    // The SimHash-remap path in `reconcile_anonymous_module_instances`
    // was the second `with_provider(...)` site that dropped
    // `provider_instance` before carina#3038. A close-but-different
    // SimHash in state triggers a name rewrite, so this test must
    // assert that the named-instance routing survives the rewrite.
    use crate::resource::{ConcreteValue, Resource, ResourceId, Value};

    let current_hash = crate::identifier::SimHash::parse_16_hex("000000000000abcd").unwrap();
    let current_prefix = format!("thing_{:016x}", current_hash);
    let state_hash = current_hash.with_flipped_mask_for_test(1);
    let state_name = format!("thing_{:016x}.role", state_hash);

    let mut resources = vec![Resource {
        id: ResourceId::with_provider_identity(
            "aws",
            "iam.Role",
            ResourceIdentity::try_from(format!("{}.role", current_prefix)).unwrap(),
            Some("us".to_string()),
        ),
        attributes: {
            let mut attrs = IndexMap::new();
            attrs.insert(
                "role_name".to_string(),
                Value::Concrete(ConcreteValue::String("r".into())),
            );
            attrs
        },
        directives: Directives::default().with_provider_instance("us"),
        resolved_ignore_changes: Default::default(),
        prefixes: HashMap::new(),
        binding: Some(format!("{}.role", current_prefix)),
        dependency_bindings: BTreeSet::new(),
        module_source: Some(crate::resource::ModuleSource::expanded_module(
            "thing",
            current_prefix.clone(),
            crate::resource::ModuleInstanceScope::for_call("thing", None, &current_prefix),
            crate::resource::CompositionCallPath::direct(crate::resource::CompositionCall {
                id: crate::parser::ModuleCallId::test_dummy_in("reconcile.crn"),
                module_name: "thing".to_string(),
                binding: None,
                instance: current_prefix.clone(),
                module_source: None,
                module_directory: None,
                loop_origin: None,
            }),
        )),
        default_tag_keys: Default::default(),
        loop_provenance: Default::default(),
        quoted_string_attrs: std::collections::HashSet::new(),
    }];

    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];
    reconcile_anonymous_module_instances(&mut resources, &mut [], &state_lookup);

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        state_name,
        "precondition: remap must actually have rewritten the name",
    );
    assert_eq!(
        resources[0].id.provider_instance.as_deref(),
        Some("us"),
        "reconcile_anonymous_module_instances must preserve \
         id.provider_instance through the SimHash prefix rewrite \
         (carina#3038)"
    );
}

/// Module with two resources where one references the other via _binding / ResourceRef.
fn create_module_with_intra_refs() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![
            Resource {
                id: ResourceId::with_identity("ec2.Vpc", "main_vpc"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert(
                        "cidr_block".to_string(),
                        Value::Deferred(DeferredValue::BindingRef {
                            binding: "cidr".to_string(),
                        }),
                    );
                    attrs.into_iter().collect()
                },
                directives: Directives::default(),
                resolved_ignore_changes: Default::default(),
                prefixes: HashMap::new(),
                binding: Some("vpc".to_string()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                default_tag_keys: Default::default(),
                loop_provenance: Default::default(),
                quoted_string_attrs: std::collections::HashSet::new(),
            },
            Resource {
                id: ResourceId::with_identity("ec2.Subnet", "sub"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert(
                        "vpc_id".to_string(),
                        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                    );
                    attrs.into_iter().collect()
                },
                directives: Directives::default(),
                resolved_ignore_changes: Default::default(),
                prefixes: HashMap::new(),
                binding: Some("subnet".to_string()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                default_tag_keys: Default::default(),
                loop_provenance: Default::default(),
                quoted_string_attrs: std::collections::HashSet::new(),
            },
        ],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "cidr".to_string(),
            static_type: known(TypeExpr::String),
            default: None,
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_multiple_module_instances_no_collision() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("net".to_string(), create_module_with_intra_refs());
        r
    };

    let call_a = ModuleCall::test("net".to_string(), Some("prod".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        module_arguments(args)
    });
    let call_b = ModuleCall::test("net".to_string(), Some("staging".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        );
        module_arguments(args)
    });

    let expanded_a = resolver
        .expand_module_call(&call_a, "prod")
        .unwrap()
        .resources;
    let expanded_b = resolver
        .expand_module_call(&call_b, "staging")
        .unwrap()
        .resources;

    // binding must be prefixed so they don't collide (using dot notation)
    assert_eq!(
        expanded_a[0].binding,
        Some("prod.vpc".to_string()),
        "Instance A vpc binding should use dot path"
    );
    assert_eq!(
        expanded_a[1].binding,
        Some("prod.subnet".to_string()),
        "Instance A subnet binding should use dot path"
    );
    assert_eq!(
        expanded_b[0].binding,
        Some("staging.vpc".to_string()),
        "Instance B vpc binding should use dot path"
    );
    assert_eq!(
        expanded_b[1].binding,
        Some("staging.subnet".to_string()),
        "Instance B subnet binding should use dot path"
    );

    // Intra-module ResourceRef must point to the dot-path binding
    assert_eq!(
        expanded_a[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "prod.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
        "Instance A subnet should reference prod.vpc, not bare vpc"
    );
    assert_eq!(
        expanded_b[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "staging.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
        "Instance B subnet should reference staging.vpc, not bare vpc"
    );

    // Resource names should also be distinct (dot notation)
    assert_eq!(
        expanded_a[0].id.identity_str().expect("resolved identity"),
        "prod.main_vpc"
    );
    assert_eq!(
        expanded_b[0].id.identity_str().expect("resolved identity"),
        "staging.main_vpc"
    );
}

/// Module with an attributes block that exposes a security_group binding.
fn create_module_with_attributes() -> ParsedFile {
    use crate::parser::AttributeParameter;

    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![Resource {
            id: ResourceId::with_provider_identity("aws", "security_group", "sg", None),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("sg".into())),
                );
                attrs.into_iter().collect()
            },
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("sg".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![AttributeParameter::dummy(
            "security_group".to_string(),
            None,
            Some(Value::resource_ref(
                "sg".to_string(),
                "id".to_string(),
                vec![],
            )),
        )],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_expand_module_call_creates_composition() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();
    // carina#3181 PR C: `resources` is managed-only — 1 real resource.
    assert_eq!(expanded.resources.len(), 1);
    // The composition resource lives in the typed `compositions` slice.
    assert_eq!(expanded.compositions.len(), 1);
    let composition_res = &expanded.compositions[0];

    assert_eq!(composition_res.binding, Some("web".to_string()));
    // Module info lives in the flattened module_name / instance fields.
    assert_eq!(composition_res.module_name, "web_tier");
    assert_eq!(composition_res.instance, "web");
    assert_eq!(composition_res.call_path().calls().count(), 1);
    assert_eq!(&composition_res.call_path().root().id, call.id());
    assert_eq!(&composition_res.call_path().immediate().id, call.id());
    assert!(!composition_res.signature.attributes.contains_key("_module"));
    assert!(
        !composition_res
            .signature
            .attributes
            .contains_key("_module_instance")
    );
    // The security_group attribute should be a rewritten ResourceRef
    // pointing to the dot-path binding (web.sg)
    assert_eq!(
        composition_res
            .signature
            .attributes
            .get("security_group")
            .map(|a| a.to_value()),
        Some(Value::resource_ref(
            "web.sg".to_string(),
            "id".to_string(),
            vec![]
        ))
    );
}

/// carina#3181 PR C: module-call expansion partitions the expanded
/// resources into the managed-only `resources` Vec and the typed
/// `compositions` / `data_sources` slices — each resource lands in
/// exactly one slice.
#[test]
fn test_expand_module_call_populates_compositions_slice() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();

    // `resources` is managed-only — the synthetic composition is NOT here.
    assert_eq!(expanded.resources.len(), 1);

    // The composition resource lives only in the typed slice.
    assert_eq!(expanded.compositions.len(), 1);
    assert_eq!(expanded.compositions[0].binding, Some("web".to_string()));
    assert_eq!(expanded.compositions[0].module_name, "web_tier");
    assert_eq!(expanded.compositions[0].instance, "web");

    // This module declares no data sources.
    assert!(expanded.data_sources.is_empty());
}

/// #3306 acceptance: a single-level module call records each leaf
/// resource it produces in the `ExpansionTrace`, with the call-site
/// `EphemeralId` as the only chain element.
#[test]
fn test_expand_module_call_populates_expansion_trace_single_level() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();

    // One leaf produced (the security_group resource).
    assert_eq!(expanded.resources.len(), 1);
    let leaf = &expanded.resources[0];

    // The trace records the leaf against a one-element chain.
    let chain = expanded
        .expansion_trace
        .call_sites_of(&leaf.persistent_id());
    assert_eq!(
        chain.len(),
        1,
        "single-level expansion must yield a one-element chain, got {chain:?}",
    );

    // The chain element points at this call site's instance prefix
    // (`_virtual.<instance_prefix>`).
    let expected_call_site =
        crate::resource::EphemeralId::new(crate::resource::ResolvedResourceId::new(
            crate::resource::ResourceId::with_identity("_virtual", "web"),
        ));
    assert_eq!(chain[0].id, expected_call_site);
}

/// carina#3322 acceptance: when `process_imports` recorded a
/// `use { source = "..." }` path for the module alias, the call site
/// stamped onto each leaf in the `ExpansionTrace` must carry that
/// path verbatim. The plan renderer reads it to label a composition
/// group with `module "<binding>" (<source_path>)` — without this,
/// the group label silently falls back to the path-less form.
#[test]
fn test_expand_module_call_records_use_source_path_on_call_site() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        // Simulates what `process_imports` records for
        // `use { source = "./modules/web_tier", alias = "web_tier" }`.
        r.module_paths
            .insert("web_tier".to_string(), "./modules/web_tier".to_string());
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();
    let leaf = &expanded.resources[0];
    let chain = expanded
        .expansion_trace
        .call_sites_of(&leaf.persistent_id());

    assert_eq!(
        chain[0].source_path.as_deref(),
        Some("./modules/web_tier"),
        "the call site must carry the DSL `use` source path so the \
         renderer can emit `module \"<binding>\" (./modules/web_tier)`",
    );
}

/// Absence-of-path fallback: when no `module_paths` entry exists for
/// the alias (e.g. hand-built test resolvers, or a module synthesized
/// without going through `process_imports`), the call site falls back
/// to `source_path = None`. The renderer treats that as
/// "drop the parenthesized suffix" — never a panic, never the literal
/// string "None".
#[test]
fn test_expand_module_call_call_site_source_path_none_when_unmapped() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        // Deliberately no `module_paths` entry.
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();
    let leaf = &expanded.resources[0];
    let chain = expanded
        .expansion_trace
        .call_sites_of(&leaf.persistent_id());
    assert_eq!(chain[0].source_path, None);
}

/// #3306 acceptance: when an outer expansion absorbs leaves from a
/// previously-expanded inner module, the trace prepends the outer
/// call site to the inner chain — outermost-first overall.
#[test]
fn test_build_expansion_trace_prepends_outer_call_site_to_inner_chain() {
    use crate::resource::{CallSite, EphemeralId, ExpansionTrace, PersistentId, ResourceId};

    // Simulate an inner module that already finished its own
    // expansion: one leaf nested one level deep (inner_call_site).
    let mut inner_trace = ExpansionTrace::new();
    let inner_leaf = PersistentId::new(ResourceId::with_identity(
        "aws.s3.Bucket",
        "outer.inner.logs",
    ));
    let inner_call_site = CallSite::new(
        EphemeralId::new(crate::resource::ResolvedResourceId::new(
            ResourceId::with_identity("_virtual", "outer.inner"),
        )),
        "./modules/inner",
    );
    inner_trace.record(inner_leaf.clone(), vec![inner_call_site.clone()]);

    // Build the outer trace: no direct leaves at this level, only
    // inherited ones. The outer call-site is `outer`.
    let outer_trace = crate::module_resolver::expander::build_expansion_trace(
        "outer",
        Some("./modules/outer"),
        &inner_trace,
        &[],
        &[],
    );

    // The inner leaf is still recorded, but its chain now leads with
    // the outer call site.
    let chain = outer_trace.call_sites_of(&inner_leaf);
    assert_eq!(
        chain.len(),
        2,
        "two-level expansion must produce a two-element chain, got {chain:?}",
    );
    let expected_outer = EphemeralId::new(crate::resource::ResolvedResourceId::new(
        ResourceId::with_identity("_virtual", "outer"),
    ));
    assert_eq!(
        chain[0].id, expected_outer,
        "outermost element must be the outer call site",
    );
    assert_eq!(
        chain[0].source_path.as_deref(),
        Some("./modules/outer"),
        "outer call site must carry the outer module's source path",
    );
    assert_eq!(
        chain[1], inner_call_site,
        "inner element must follow the outer one",
    );
}

/// PR E (#3292) acceptance: a composition's `signature.arguments`
/// preserves the resolved call-site arguments. The pre-#3292 expander
/// dropped this information with the `ModuleCall`; with `LiveCompositionSignature`
/// on `LiveComposition`, the call boundary is now inspectable on the
/// expanded node itself.
#[test]
fn test_expand_module_call_preserves_arguments_on_composition_signature() {
    use crate::parser::{ArgumentParameter, AttributeParameter, TypeExpr};

    // A small module that declares two `argument` parameters and one
    // `attribute` output, used to verify both halves of the signature.
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        data_sources: vec![],
        compositions: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "region".to_string(),
                static_type: known(TypeExpr::String),
                default: None,
                description: None,
                validations: vec![],
            },
            ArgumentParameter {
                name: "instance_count".to_string(),
                static_type: known(TypeExpr::Int),
                default: None,
                description: None,
                validations: vec![],
            },
        ],
        attribute_params: vec![AttributeParameter::dummy(
            "endpoint".to_string(),
            None,
            Some(Value::Concrete(ConcreteValue::String(
                "fixed-endpoint".into(),
            ))),
        )],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("svc".to_string(), module);
        r
    };

    let mut call_arguments: HashMap<String, Value> = HashMap::new();
    call_arguments.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    call_arguments.insert(
        "instance_count".to_string(),
        Value::Concrete(ConcreteValue::Int(3)),
    );

    let call = ModuleCall::test(
        "svc".to_string(),
        Some("api".to_string()),
        module_arguments(call_arguments),
    );

    let expanded = resolver.expand_module_call(&call, "api").unwrap();
    assert_eq!(expanded.compositions.len(), 1);
    let composition = &expanded.compositions[0];

    // The composition's signature.arguments must contain BOTH values
    // passed at the call site, keyed by argument name.
    assert_eq!(
        composition
            .signature
            .arguments
            .get("region")
            .map(|argument| argument.value()),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".into()
        ))),
        "region argument must be recorded on the composition signature",
    );
    assert_eq!(
        composition
            .signature
            .arguments
            .get("instance_count")
            .map(|argument| argument.value()),
        Some(&Value::Concrete(ConcreteValue::Int(3))),
        "instance_count argument must be recorded on the composition signature",
    );
    assert_eq!(
        composition
            .signature
            .arguments
            .get("region")
            .and_then(|argument| argument.declared_type().known()),
        Some(&TypeExpr::String),
        "the declared sink type must travel with the argument value",
    );

    // Arguments are recorded in module.arguments declaration order so
    // the trace is stable across runs.
    let keys: Vec<&String> = composition.signature.arguments.keys().collect();
    assert_eq!(keys, vec!["region", "instance_count"]);

    // The attribute half remains populated as before.
    assert_eq!(
        composition
            .signature
            .attributes
            .get("endpoint")
            .map(|a| a.to_value()),
        Some(Value::Concrete(ConcreteValue::String(
            "fixed-endpoint".into()
        ))),
    );
}

#[test]
fn test_expand_module_call_without_binding_still_records_boundary_composition() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    // Module call without binding_name
    let call = ModuleCall::test("web_tier".to_string(), None, HashMap::new());

    let expanded = resolver.expand_module_call(&call, "web_tier").unwrap();
    assert_eq!(expanded.compositions.len(), 1);
    assert_eq!(expanded.compositions[0].binding, None);
    assert_eq!(expanded.compositions[0].module_name, "web_tier");
}

/// Regression fixtures for #2197. Writes a minimal `modules/thing` module
/// (one `awscc.iam.Role` whose `role_name` comes from a `name` argument)
/// and a `root/main.crn` with the caller-supplied body; returns the parsed
/// root with modules already resolved.
fn resolve_thing_fixture(root_body: &str) -> ModuleResolvedParsedFile {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/thing");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir).expect("resolve_modules should succeed")
}

fn role_names(parsed: &ModuleResolvedParsedFile) -> HashSet<String> {
    parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .filter_map(|r| match r.get_attr("role_name")? {
            Value::Concrete(ConcreteValue::String(s)) => Some(s.expose_plaintext().to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn test_anonymous_module_calls_get_distinct_prefixes() {
    let call_a = ModuleCall::test("github".to_string(), None, {
        let mut args = HashMap::new();
        args.insert(
            "github_repo".to_string(),
            Value::Concrete(ConcreteValue::String("carina-rs/infra".into())),
        );
        module_arguments(args)
    });
    let call_b = ModuleCall::test("github".to_string(), None, {
        let mut args = HashMap::new();
        args.insert(
            "github_repo".to_string(),
            Value::Concrete(ConcreteValue::String("carina-rs/other".into())),
        );
        module_arguments(args)
    });

    let a = instance_prefix_for_call(&call_a);
    let b = instance_prefix_for_call(&call_b);
    assert_ne!(a, b);
    assert!(
        a.starts_with("github_"),
        "expected `github_<16hex>`, got {a}"
    );
    assert_eq!(
        a.len(),
        "github_".len() + 16,
        "expected 16 hex chars in {a}"
    );
}

// SimHash is locality-sensitive: editing one argument must flip only a few
// bits so reconciliation can find the state entry. Assert the Hamming
// distance is below the reconciliation threshold.
#[test]
fn test_anonymous_module_call_prefix_is_locality_sensitive() {
    let make = |repo: &str| {
        ModuleCall::test("github".to_string(), None, {
            let mut args = HashMap::new();
            args.insert(
                "github_repo".to_string(),
                Value::Concrete(ConcreteValue::String(repo.to_string().into())),
            );
            args.insert(
                "role_name".to_string(),
                Value::Concrete(ConcreteValue::String("github-actions".into())),
            );
            args.insert(
                "managed_policy_arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String(
                        "arn:aws:iam::aws:policy/AdministratorAccess"
                            .to_string()
                            .into(),
                    ),
                )])),
            );
            module_arguments(args)
        })
    };

    let a = instance_prefix_for_call(&make("carina-rs/infra"));
    let b = instance_prefix_for_call(&make("carina-rs/other"));
    let parse = |p: &str| parse_synthetic_instance_prefix(p).unwrap().1;
    let distance = parse(&a).distance(parse(&b));
    assert!(
        distance < crate::identifier::SIMHASH_HAMMING_THRESHOLD,
        "small edit should stay inside the reconciliation threshold, got distance {distance}",
    );
}

#[test]
fn test_named_module_call_uses_binding_name() {
    let call = ModuleCall::test(
        "github".to_string(),
        Some("prod".to_string()),
        HashMap::new(),
    );
    assert_eq!(instance_prefix_for_call(&call), "prod");
}

#[test]
fn test_anonymous_module_calls_expand_into_distinct_instances() {
    let parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'alpha' }
thing { name = 'beta'  }
"#,
    );

    let role_addresses: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.identity_str().expect("resolved identity").to_string())
        .collect();
    assert_eq!(role_addresses.len(), 2, "got {:?}", role_addresses);

    assert_eq!(
        role_names(&parsed),
        ["alpha".to_string(), "beta".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
    );
}

#[test]
fn unavailable_builtin_handle_arguments_are_prefixed_per_module_instance() {
    let mut origin = Resource::with_provider("mock", "test.resource", "origin", None);
    origin.binding = Some("origin".to_string());

    // Model the parsed `upper(origin)` call directly: ordinary parsing rejects
    // a bare handle in strict mode, while the permissive directory pass keeps
    // the failed call as Unavailable for diagnostics and identity material.
    let handle = crate::eval_value::StructuralHandle::new(
        crate::eval_value::StructuralHandleKind::Resource(crate::parser::ResourceTypePath::new(
            "mock",
            "test.resource",
        )),
        "origin",
    );
    let mut dependent = Resource::pending_with_provider("mock", "test.resource", None);
    dependent.set_attr(
        "replacement_key".to_string(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![crate::resource::FunctionArgument::from_handle(handle)],
            source_line: (7).into(),
        }),
    );
    let mut module = ParsedFile {
        resources: vec![origin, dependent],
        ..ParsedFile::default()
    };
    crate::parser::resolve_resource_refs_defer_unknown_functions(
        &mut module,
        &ProviderContext::default(),
    )
    .expect("permissive resolution must retain the failed built-in");

    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver.imported_modules.insert("m".to_string(), module);
        resolver
    };
    let call = ModuleCall::test("m".to_string(), None, HashMap::new());
    let mut resources = Vec::new();
    for instance in ["first", "second"] {
        resources.extend(
            resolver
                .expand_module_call(&call, instance)
                .expect("expand module instance")
                .resources,
        );
    }

    let dependents = resources
        .iter()
        .filter(|resource| resource.get_attr("replacement_key").is_some())
        .collect::<Vec<_>>();
    assert_eq!(dependents.len(), 2, "expected one dependent per instance");

    let argument_bindings = dependents
        .iter()
        .map(|resource| {
            let Some(Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::upper,
                args,
                ..
            })) = resource.get_attr("replacement_key")
            else {
                panic!(
                    "upper(handle) must remain an unavailable built-in: {:?}",
                    resource.get_attr("replacement_key")
                );
            };
            let [argument] = args.as_slice() else {
                panic!("upper() must retain one argument: {args:?}");
            };
            let Value::Deferred(DeferredValue::BindingRef { binding }) = argument.walker_value()
            else {
                panic!("upper() must retain its handle-shaped argument: {argument:?}");
            };
            binding.clone()
        })
        .collect::<HashSet<_>>();
    assert_eq!(
        argument_bindings,
        HashSet::from(["first.origin".to_string(), "second.origin".to_string()]),
    );

    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "mock",
        (crate::schema::ResourceSchema::new("test.resource").attribute(
            crate::schema::AttributeSchema::optional(
                "replacement_key",
                crate::schema::AttributeType::string(),
            )
            .create_only()
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    crate::identifier::compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &registry,
        &|_| Vec::new(),
    )
    .expect("compute anonymous identities");

    let identities = resources
        .iter()
        .filter(|resource| resource.get_attr("replacement_key").is_some())
        .map(|resource| {
            resource
                .id
                .identity_str()
                .expect("computed identity")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_ne!(identities[0], identities[1]);
    let hash_bases = identities
        .iter()
        .map(|identity| {
            identity
                .split_once('.')
                .expect("module-prefixed identity")
                .1
                .to_string()
        })
        .collect::<HashSet<_>>();
    assert_eq!(
        hash_bases.len(),
        2,
        "instance-prefixed unavailable arguments must produce distinct identity hashes: {identities:?}",
    );
}

#[test]
fn test_mixed_named_and_anonymous_module_calls_coexist() {
    let parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

let named = thing { name = 'named-call' }
thing              { name = 'anon-call'  }
"#,
    );

    assert_eq!(
        role_names(&parsed),
        ["named-call".to_string(), "anon-call".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
    );

    let addrs: Vec<&str> = parsed
        .resources
        .iter()
        .map(|r| r.id.identity_str().expect("resolved identity"))
        .collect();
    assert!(addrs.iter().any(|n| n.starts_with("named.")), "{:?}", addrs);
    assert!(addrs.iter().any(|n| n.starts_with("thing_")), "{:?}", addrs);
}

// Reconciliation: an argument edit moves the SimHash prefix by a few bits;
// if the old prefix is in state and the new one is not, the reconciler
// must rewrite the expanded resources to use the state address.
#[test]
fn test_reconcile_anonymous_module_instances_remaps_close_prefix() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    );

    let before = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let (new_prefix, _) = before.split_once('.').unwrap();
    let new_prefix = new_prefix.to_string();
    let (module, new_hash) = parse_synthetic_instance_prefix(&new_prefix).unwrap();
    assert_eq!(module, "thing");

    // Model a nested call retained under the anonymous instance. Its address,
    // binding, and argument ref must move with the managed resource.
    let mut nested = parsed.compositions[0].clone();
    nested.id = nested
        .id
        .instantiate_with_identity(crate::resource::ResourceIdentity::new(format!(
            "{new_prefix}.nested"
        )));
    nested.binding = Some(format!("{new_prefix}.nested"));
    nested.signature.arguments.insert(
        "role_name".to_string(),
        crate::resource::LiveCompositionArgument::from_value(
            Value::resource_ref(
                format!("{new_prefix}.role"),
                "role_name".to_string(),
                vec![],
            ),
            consumer_type(TypeExpr::String),
            ArgumentOrigin::Defaulted,
        ),
    );
    parsed.compositions.push(nested);

    // Fabricate a state entry whose SimHash is within threshold of the
    // current one (flip one bit).
    let state_hash = new_hash.with_flipped_mask_for_test(1);
    let state_prefix = format!("thing_{state_hash:016x}");
    let state_name = format!("{state_prefix}.role");
    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];

    reconcile_anonymous_module_instances(
        &mut parsed.resources,
        &mut parsed.compositions,
        &state_lookup,
    );

    let role = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap();
    assert_eq!(
        role.id.identity_str().expect("resolved identity"),
        state_name
    );
    assert_eq!(
        role.binding.as_deref(),
        Some(state_name.as_str()),
        "binding should be remapped too",
    );
    let expanded = format!("{:?}", (&parsed.resources, &parsed.compositions));
    assert!(
        !expanded.contains(&new_prefix),
        "the superseded synthetic prefix must not leak from expanded output: {expanded}",
    );
    assert!(expanded.contains(&state_prefix));

    let outer = parsed
        .compositions
        .iter()
        .find(|composition| composition.binding.is_none())
        .unwrap();
    assert_eq!(outer.id.identity_str(), state_prefix);
    assert_eq!(outer.instance, state_prefix);

    let nested = parsed
        .compositions
        .iter()
        .find(|composition| composition.binding.is_some())
        .unwrap();
    assert_eq!(nested.id.identity_str(), format!("{state_prefix}.nested"));
    assert_eq!(
        nested.binding.as_deref(),
        Some(format!("{state_prefix}.nested").as_str())
    );
    assert_eq!(
        nested
            .signature
            .arguments
            .get("role_name")
            .map(|argument| argument.value()),
        Some(&Value::resource_ref(
            format!("{state_prefix}.role"),
            "role_name".to_string(),
            vec![],
        )),
    );
}

#[test]
fn test_module_instance_reconcile_skips_claimed_prefix() {
    let mut claimed_from = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    );
    let before_name = claimed_from
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    let (current_prefix, _) = before_name.split_once('.').unwrap();
    let (_, current_hash) = parse_synthetic_instance_prefix(current_prefix).unwrap();
    let state_hash = current_hash.with_flipped_mask_for_test(1);
    let state_name = format!("thing_{:016x}.role", state_hash);
    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];
    let claims = crate::identifier::StateBlockClaims::new(
        [crate::parser::StateBlockAddress::new(
            "awscc",
            "iam.Role",
            ResourceIdentity::try_from(state_name.clone()).unwrap(),
        )]
        .into_iter()
        .collect(),
        HashSet::new(),
    );

    crate::module_resolver::reconcile_anonymous_module_instances(
        &mut claimed_from.resources,
        &mut claimed_from.compositions,
        &state_lookup,
        &claims,
    );
    assert_eq!(
        claimed_from
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .identity_str()
            .unwrap_or(""),
        before_name,
        "a claimed state child must exclude the whole state prefix from orphan candidates",
    );

    let mut claimed_to = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    );
    let before_name = claimed_to
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    let (current_prefix, _) = before_name.split_once('.').unwrap();
    let (_, current_hash) = parse_synthetic_instance_prefix(current_prefix).unwrap();
    let state_hash = current_hash.with_flipped_mask_for_test(1);
    let state_name = format!("thing_{:016x}.role", state_hash);
    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];
    let claims = crate::identifier::StateBlockClaims::new(
        HashSet::new(),
        [crate::parser::StateBlockAddress::new(
            "awscc",
            "iam.Role",
            ResourceIdentity::try_from(before_name.clone()).unwrap(),
        )]
        .into_iter()
        .collect(),
    );

    crate::module_resolver::reconcile_anonymous_module_instances(
        &mut claimed_to.resources,
        &mut claimed_to.compositions,
        &state_lookup,
        &claims,
    );
    assert_eq!(
        claimed_to
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .identity_str()
            .unwrap_or(""),
        before_name,
        "a claimed desired child must pin the whole current prefix",
    );
}

// Reconciliation must not cross module names: a `foo_<hash>` state entry
// has nothing to do with a current `bar_<hash>` DSL instance.
#[test]
fn test_reconcile_anonymous_module_instances_ignores_other_modules() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
    );

    let before_name: String = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    // State entry uses a different module name.
    let state_lookup = |_: &str, _: &str| vec!["other_0000000000000001.role".to_string()];
    reconcile_anonymous_module_instances(
        &mut parsed.resources,
        &mut parsed.compositions,
        &state_lookup,
    );

    let after_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(before_name, after_name);
}

// Regression for #2211: a single anonymous module instance whose module
// expands to multiple resource types means the same state prefix shows up
// once per resource type when `find_state_names_by_type` is queried per
// (provider, type). The reconciler must treat repeated identical hashes
// as the same candidate, not as multiple ambiguous candidates.
#[test]
fn test_reconcile_anonymous_module_instances_dedups_state_prefixes_across_types() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/thing");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let provider_res = awscc.iam.OidcProvider {
  url             = 'https://example.com'
  client_id_list  = ['x']
  thumbprint_list = ['y']
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let mut parsed = resolve_modules(parsed, &root_dir).expect("resolve_modules should succeed");

    // Discover the new prefix from the parsed Role.
    let role_name_before = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    let (new_prefix, _) = role_name_before.split_once('.').unwrap();
    let (_, new_hash) = parse_synthetic_instance_prefix(new_prefix).unwrap();

    // State holds the *same* instance prefix at two resource types, one
    // bit away from the current SimHash — i.e. a small argument edit.
    let state_hash = new_hash.with_flipped_mask_for_test(1);
    let state_lookup = move |_: &str, resource_type: &str| match resource_type {
        "iam.OidcProvider" => vec![format!("thing_{:016x}.provider_res", state_hash)],
        "iam.Role" => vec![format!("thing_{:016x}.role", state_hash)],
        _ => vec![],
    };

    reconcile_anonymous_module_instances(
        &mut parsed.resources,
        &mut parsed.compositions,
        &state_lookup,
    );

    let role_after = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap();
    assert_eq!(
        role_after.id.identity_str().expect("resolved identity"),
        format!("thing_{:016x}.role", state_hash),
        "Role address must be remapped to the state prefix",
    );
    let provider_after = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.OidcProvider")
        .unwrap();
    assert_eq!(
        provider_after.id.identity_str().expect("resolved identity"),
        format!("thing_{:016x}.provider_res", state_hash),
        "OidcProvider address must be remapped to the state prefix",
    );
}

// Reconciliation must not run when there are multiple candidate state
// prefixes within threshold — ambiguity means we can't tell which is the
// "same instance."
#[test]
fn test_reconcile_anonymous_module_instances_skips_ambiguous() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
    );

    let before_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    let (prefix, _) = before_name.split_once('.').unwrap();
    let (_, cur_hash) = parse_synthetic_instance_prefix(prefix).unwrap();

    // Two state entries at the same Hamming distance — ambiguous.
    let state_lookup = move |_: &str, _: &str| {
        vec![
            format!(
                "thing_{:016x}.role",
                cur_hash.with_flipped_mask_for_test(0b1)
            ),
            format!(
                "thing_{:016x}.role",
                cur_hash.with_flipped_mask_for_test(0b10)
            ),
        ]
    };
    reconcile_anonymous_module_instances(
        &mut parsed.resources,
        &mut parsed.compositions,
        &state_lookup,
    );

    let after_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(before_name, after_name, "ambiguous match must not remap");
}

// When state holds prefix A and the DSL has both A (unchanged) and a new
// A' (a new anonymous call with similar args), A' must not be remapped
// onto A — they are two distinct instances even though their SimHashes
// are close. State prefixes already in use by current DSL must not serve
// as remap candidates.
#[test]
fn test_reconcile_anonymous_module_instances_does_not_steal_in_use_prefix() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'unchanged' }
thing { name = 'unchanged-but-different' }
"#,
    );

    let prefixes_before: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| {
            r.id.identity_str()
                .unwrap_or("")
                .split_once('.')
                .unwrap()
                .0
                .to_string()
        })
        .collect();
    assert_eq!(prefixes_before.len(), 2);
    let mut iter = prefixes_before.iter();
    let first = iter.next().unwrap().clone();
    let _second = iter.next().unwrap().clone();

    // State only holds the *first* prefix. The reconciler must not
    // remap the second instance onto it.
    let first_clone = first.clone();
    let state_lookup = move |_: &str, _: &str| vec![format!("{}.role", first_clone)];
    reconcile_anonymous_module_instances(
        &mut parsed.resources,
        &mut parsed.compositions,
        &state_lookup,
    );

    let prefixes_after: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| {
            r.id.identity_str()
                .unwrap_or("")
                .split_once('.')
                .unwrap()
                .0
                .to_string()
        })
        .collect();
    assert_eq!(
        prefixes_after, prefixes_before,
        "in-use state prefix must not be reassigned to a different DSL instance",
    );
}

#[test]
fn test_missing_required_argument() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        HashMap::new(), // Missing vpc_id
    );

    let result = resolver.expand_module_call(&call, "my_instance");
    assert!(matches!(
        result.as_ref().map_err(|error| error.root_cause()),
        Err(ModuleError::MissingArgument { .. })
    ));
}

#[test]
fn test_expand_module_call_uses_dot_path_addressing() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".into())),
            );
            module_arguments(args)
        },
    );

    let expanded = resolver
        .expand_module_call(&call, "my_instance")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let sg = &expanded[0];
    // Resource name should use dot notation, not underscore
    assert_eq!(
        sg.id.identity_str().expect("resolved identity"),
        "my_instance.sg"
    );
}

#[test]
fn test_module_dot_path_bindings_and_refs() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("net".to_string(), create_module_with_intra_refs());
        r
    };

    let call = ModuleCall::test("net".to_string(), Some("prod".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver
        .expand_module_call(&call, "prod")
        .unwrap()
        .resources;

    // Resource names should use dot notation
    assert_eq!(
        expanded[0].id.identity_str().expect("resolved identity"),
        "prod.main_vpc"
    );
    assert_eq!(
        expanded[1].id.identity_str().expect("resolved identity"),
        "prod.sub"
    );

    // binding should use dot notation
    assert_eq!(expanded[0].binding, Some("prod.vpc".to_string()));
    assert_eq!(expanded[1].binding, Some("prod.subnet".to_string()));

    // Intra-module ResourceRef should use dot notation
    assert_eq!(
        expanded[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "prod.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
    );
}

/// carina#3061: a `wait` block declared inside a module must survive
/// expansion. Every binding-name field is instance-prefixed, and a
/// module resource that references the wait binding has that reference
/// prefixed too (so the dependency edge to the `Effect::Wait` forms).
#[test]
fn test_expand_module_call_propagates_and_prefixes_wait_bindings() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let module = {
        let mut m = create_module_with_intra_refs();
        // A resource that consumes the wait binding the same way the
        // real CloudFront Distribution consumes `cert_issued`.
        m.resources.push(Resource {
            id: ResourceId::with_identity("cloudfront.Distribution", "distribution"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "acm_certificate_arn".to_string(),
                    Value::resource_ref(
                        "cert_issued".to_string(),
                        "certificate_arn".to_string(),
                        vec![],
                    ),
                );
                attrs.into_iter().collect()
            },
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("distribution".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        });
        m.wait_bindings.push(WaitBinding {
            binding: "cert_issued".into(),
            target: "main_vpc".into(),
            until_raw: "main_vpc.state == \"available\"".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["main_vpc".to_string(), "state".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("available".into())),
            },
            timeout_secs: Some(300),
            depends_on: vec!["subnet".into()],
            line: 1,
        });
        m
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("net".to_string(), module);
        r
    };

    let call = ModuleCall::test("net".to_string(), Some("prod".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver.expand_module_call(&call, "prod").unwrap();

    // The wait binding survived expansion and every binding-name field
    // is instance-prefixed.
    assert_eq!(expanded.wait_bindings.len(), 1);
    let wb = &expanded.wait_bindings[0];
    assert_eq!(wb.binding, "prod.cert_issued");
    assert_eq!(wb.target, "prod.main_vpc");
    assert_eq!(
        wb.until_predicate.lhs_segments,
        vec!["prod.main_vpc".to_string(), "state".to_string()]
    );
    assert_eq!(wb.depends_on, vec!["prod.subnet".to_string()]);
    // RHS value and surface text are not binding names — unchanged.
    assert_eq!(
        wb.until_predicate.rhs,
        Value::Concrete(ConcreteValue::String("available".into()))
    );
    assert_eq!(wb.timeout_secs, Some(300));

    // The downstream resource's reference to the wait binding was
    // rewritten to the prefixed name, so the dependency edge to the
    // Effect::Wait can form at plan time.
    let dist = expanded
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("distribution resource present");
    assert_eq!(
        dist.get_attr("acm_certificate_arn"),
        Some(&Value::resource_ref(
            "prod.cert_issued".to_string(),
            "certificate_arn".to_string(),
            vec![]
        )),
        "the module resource's `cert_issued` ref must be instance-prefixed"
    );
}

#[test]
fn test_module_composition_dot_path_refs() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        HashMap::new(),
    );

    let expanded = resolver.expand_module_call(&call, "web").unwrap();

    // carina#3181 PR C: the composition resource lives in the typed slice.
    let composition_res = expanded
        .compositions
        .first()
        .expect("Virtual resource should exist");

    // The security_group attribute should reference dot-notation binding
    assert_eq!(
        composition_res
            .signature
            .attributes
            .get("security_group")
            .map(|a| a.to_value()),
        Some(Value::resource_ref(
            "web.sg".to_string(),
            "id".to_string(),
            vec![]
        ))
    );
}

#[test]
fn test_substitute_arguments_interpolation() {
    use crate::resource::InterpolationPart;

    let mut inputs = HashMap::new();
    inputs.insert(
        "env_name".to_string(),
        Value::Concrete(ConcreteValue::String("dev".into())),
    );

    // Interpolation like "prefix-${env_name}-suffix" where env_name is a module argument
    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
            binding: "env_name".to_string(),
        })),
        InterpolationPart::Literal("-suffix".into()),
    ]));
    let result = substitute_arguments(&value, &inputs).expect("substitution");

    // After substitution, the ResourceRef should be replaced with the argument value
    assert_eq!(
        result,
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("dev".into()))),
            InterpolationPart::Literal("-suffix".into()),
        ]))
    );
}

#[test]
fn test_unknown_argument_rejected() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".into())),
            );
            // Unknown argument: not declared in the module
            args.insert(
                "unknown_arg".to_string(),
                Value::Concrete(ConcreteValue::String("should-fail".into())),
            );
            module_arguments(args)
        },
    );

    let result = resolver.expand_module_call(&call, "my_instance");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::UnknownArgument { .. })
        ),
        "Expected UnknownArgument error, got {:?}",
        result
    );
}

#[test]
fn test_substitute_arguments_function_call() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "cidr".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    // FunctionCall like cidr_subnet(cidr, 8, 0) where cidr is a module argument
    let value = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::cidr_subnet,
        args: vec![
            Value::Deferred(DeferredValue::BindingRef {
                binding: "cidr".to_string(),
            })
            .into(),
            Value::Concrete(ConcreteValue::Int(8)).into(),
            Value::Concrete(ConcreteValue::Int(0)).into(),
        ],
        source_line: (0).into(),
    });
    let result = substitute_arguments(&value, &inputs).expect("substitution");

    assert_eq!(
        result,
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::cidr_subnet,
            args: vec![
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())).into(),
                Value::Concrete(ConcreteValue::Int(8)).into(),
                Value::Concrete(ConcreteValue::Int(0)).into(),
            ],
            source_line: (0).into(),
        })
    );
}

#[test]
fn test_substitute_arguments_unavailable_builtin_call() {
    let inputs = HashMap::from([(
        "source".to_string(),
        Value::Concrete(ConcreteValue::String("resolved".into())),
    )]);
    let value = Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
                name: "source".to_string(),
                source_line: (0).into(),
                diagnostic_name: None,
            }))
            .into(),
        ],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "fixture failure".to_string(),
        ),
        source_line: (7).into(),
    });

    let substituted = substitute_arguments(&value, &inputs).expect("substitution");
    let Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::upper,
        args,
        reason,
        source_line,
    }) = substituted
    else {
        panic!("substitution must preserve Unavailable: {substituted:?}");
    };
    assert_eq!(
        args[0].value(),
        Some(&Value::Concrete(ConcreteValue::String("resolved".into())))
    );
    assert_eq!(reason.as_str(), "fixture failure");
    assert_eq!(source_line.get(), Some(7));
}

/// Module with interpolation in resource attributes to test argument substitution
fn create_module_with_interpolation() -> ParsedFile {
    use crate::resource::InterpolationPart;

    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![Resource {
            id: ResourceId::with_identity("ec2.Vpc", "vpc"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "cidr_block".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "cidr_block".to_string(),
                    }),
                );
                attrs.insert(
                    "name".to_string(),
                    Value::Deferred(DeferredValue::Interpolation(vec![
                        InterpolationPart::Literal("test-".into()),
                        InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
                            binding: "env_name".to_string(),
                        })),
                    ])),
                );
                attrs.insert(
                    "env".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "env_name".to_string(),
                    }),
                );
                attrs.into_iter().collect()
            },
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("vpc".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "cidr_block".to_string(),
                static_type: known(TypeExpr::String),
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "env_name".to_string(),
                static_type: known(TypeExpr::String),
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_expand_module_call_with_interpolation() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("vpc_mod".to_string(), create_module_with_interpolation());
        r
    };

    let call = ModuleCall::test("vpc_mod".to_string(), Some("dev_vpc".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        args.insert(
            "env_name".to_string(),
            Value::Concrete(ConcreteValue::String("dev".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver
        .expand_module_call(&call, "dev_vpc")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let vpc = &expanded[0];

    // Simple argument substitution should work
    assert_eq!(
        vpc.get_attr("cidr_block"),
        Some(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".into()
        )))
    );
    assert_eq!(
        vpc.get_attr("env"),
        Some(&Value::Concrete(ConcreteValue::String("dev".into())))
    );

    // Interpolation with argument substitutes and canonicalizes back to
    // a flat `String` so downstream `Value::Concrete(ConcreteValue::String)` consumers (state
    // diff, plan rendering) see the resolved value. Without the post-
    // substitution canonicalize, this would stay as
    // `Interpolation([Literal("test-"), Expr(String("dev"))])` (#2815, #2817).
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("test-dev".into()))),
    );
}

#[test]
fn test_nested_module_two_level() {
    // outer_module imports inner_module
    // resolve_modules on root.crn should expand both levels
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    let parsed = resolve_modules(parsed, &fixtures_dir).unwrap();

    // Should have resources from both inner_module (vpc) and outer_module (sg)
    let resource_types: Vec<&str> = parsed
        .resources
        .iter()
        .map(|resource| resource.id.resource_type.as_str())
        .collect();

    assert!(
        resource_types.iter().any(|t| t.ends_with(".Vpc")),
        "Should contain VPC resource from inner module, got: {:?}",
        resource_types
    );
    assert!(
        resource_types.iter().any(|t| t.ends_with(".SecurityGroup")),
        "Should contain security group from outer module, got: {:?}",
        resource_types
    );
    assert!(
        parsed
            .resources
            .iter()
            .all(|resource| resource.attributes.keys().all(|key| !key.starts_with('_'))),
        "module expansion must not introduce host metadata into resource attributes"
    );
}

#[test]
fn nested_module_expansion_remaps_bare_use_owners_and_keeps_list_map_paths() {
    let fixture = tempfile::tempdir().expect("nested bare-use fixture");
    let outer_dir = fixture.path().join("outer");
    let inner_dir = fixture.path().join("inner");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        fixture.path().join("main.crn"),
        r#"let outer_module = use { source = "outer" }
let root = outer_module {}
"#,
    )
    .unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"let inner_module = use { source = "../inner" }
let outer_value = "forwarded"
attributes { result = outer_value }
let nested = inner_module { param = outer_value }
"#,
    )
    .unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"arguments { param: String }
mock.test.Consumer {
  name = "consumer"
  values = [param]
  mapping = { primary = param }
}
"#,
    )
    .unwrap();

    let root = fs::read_to_string(fixture.path().join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&root, &ProviderContext::default()).unwrap();
    let parsed = resolve_modules(parsed, fixture.path()).expect("expand both module levels");

    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "test.Consumer")
        .expect("expanded inner consumer");
    assert_eq!(
        consumer.get_attr("values"),
        Some(&Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("forwarded".into()),)
        ]))),
    );

    let inner_uses = parsed
        .bare_binding_uses()
        .uses()
        .filter(|use_site| use_site.name == "param")
        .collect::<Vec<_>>();
    assert_eq!(inner_uses.len(), 2, "one list and one map leaf");
    assert!(
        inner_uses
            .iter()
            .all(|use_site| use_site.source_file.path().ends_with("inner/main.crn"))
    );
    assert!(inner_uses.iter().all(|use_site| matches!(
        &use_site.owner,
        BindingUseOwner::Resource(address) if address.resource_id() == &consumer.id
    )));
    assert!(inner_uses.iter().any(|use_site| matches!(
        use_site.path.segments(),
        [AttributePathSegment::Attribute(root), AttributePathSegment::ListIndex(0)]
            if root == "values"
    )));
    assert!(inner_uses.iter().any(|use_site| matches!(
        use_site.path.segments(),
        [AttributePathSegment::Attribute(root), AttributePathSegment::Key(key)]
            if root == "mapping" && key == "primary"
    )));
}

#[test]
fn test_nested_module_intra_ref_to_module_call_is_prefixed() {
    // carina#3243 regression: `outer_module` declares
    //   let net = inner { ... }          (module call → composition resource)
    //   let sg  = ... { vpc_id = net.vpc_id }
    // When the *outer* module is itself expanded (root.crn calls
    // `outer` with binding `web`), the `net.vpc_id` reference inside
    // `sg` must be rewritten to `web.net.vpc_id`. Pre-fix, only
    // `module.resources` / `module.data_sources` / `module.wait_bindings`
    // were treated as intra-module bindings, so a module-call binding
    // like `net` was left bare and the downstream validation step
    // reported `unknown binding 'net' in reference net.vpc_id`.
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    let parsed = resolve_modules(parsed, &fixtures_dir).unwrap();

    // Find the SecurityGroup resource and read its `vpc_id` attribute.
    let sg = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "ec2.SecurityGroup")
        .expect("SecurityGroup from outer module must be present");
    let vpc_id = sg
        .get_attr("vpc_id")
        .expect("SecurityGroup must carry the vpc_id attribute");

    // After expansion the binding must be instance-prefixed (`web.net`),
    // not the bare intra-module name (`net`).
    match vpc_id {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(
                path.binding(),
                "web.net",
                "expected instance-prefixed binding, got: {}",
                path.binding()
            );
            assert_eq!(path.attribute(), "vpc_id");
        }
        other => panic!("expected ResourceRef for sg.vpc_id, got: {:?}", other),
    }

    let nested = parsed
        .compositions
        .iter()
        .find(|composition| composition.module_name == "inner")
        .expect("inner module composition must be retained");
    let immediate_call = nested.call_path().immediate();
    let root_call = nested.call_path().root();
    assert_eq!(immediate_call.binding.as_deref(), Some("web.net"));
    assert_eq!(immediate_call.instance, "web.net");
    assert_eq!(immediate_call.module_name, "inner");
    assert_eq!(root_call.binding.as_deref(), Some("web"));
    assert_eq!(root_call.instance, "web");
    assert_eq!(root_call.module_name, "outer");
}

#[test]
fn test_nested_module_three_level() {
    // root -> middle_module -> inner_module
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root_three_level.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    let parsed = resolve_modules(parsed, &fixtures_dir).unwrap();

    // Should have the VPC resource from inner_module (through middle_module)
    let resource_types: Vec<&str> = parsed
        .resources
        .iter()
        .map(|resource| resource.id.resource_type.as_str())
        .collect();

    assert!(
        resource_types.iter().any(|t| t.ends_with(".Vpc")),
        "Should contain VPC resource from inner module (3 levels deep), got: {:?}",
        resource_types
    );
}

#[test]
fn test_nested_module_cycle_detection() {
    // cycle_a imports cycle_b, cycle_b imports cycle_a
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root_cycle.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    let result = resolve_modules(parsed, &fixtures_dir);
    assert!(
        result.is_err(),
        "Should detect circular import, but got: {:?}",
        result
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err.root_cause(), ModuleError::CircularImport(_)),
        "Expected CircularImport error, got: {:?}",
        err
    );
}

#[test]
fn test_expand_module_call_with_function_call_argument() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("vpc_mod".to_string(), create_module_with_interpolation());
        r
    };

    // Pass a FunctionCall as an argument value
    let call = ModuleCall::test("vpc_mod".to_string(), Some("dev_vpc".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr_block".to_string(),
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::cidr_subnet,
                args: vec![
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())).into(),
                    Value::Concrete(ConcreteValue::Int(8)).into(),
                    Value::Concrete(ConcreteValue::Int(0)).into(),
                ],
                source_line: (0).into(),
            }),
        );
        args.insert(
            "env_name".to_string(),
            Value::Concrete(ConcreteValue::String("dev".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver
        .expand_module_call(&call, "dev_vpc")
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let vpc = &expanded[0];

    // FunctionCall argument should be substituted as-is (resolved at apply time)
    assert_eq!(
        vpc.get_attr("cidr_block"),
        Some(&Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::cidr_subnet,
            args: vec![
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())).into(),
                Value::Concrete(ConcreteValue::Int(8)).into(),
                Value::Concrete(ConcreteValue::Int(0)).into(),
            ],
            source_line: (0).into(),
        }))
    );
}

#[test]
fn test_load_module_missing_path_cleans_resolving_set() {
    // Nonexistent import path => descriptive IO error (NotFound), not the
    // single-file-module contract error. The resolving set must be cleaned
    // up so a retry does not masquerade as a circular import.
    let tmp_dir = std::env::temp_dir().join("carina_test_missing_path_cleanup");
    let _ = fs::create_dir_all(&tmp_dir);

    let mut resolver = ModuleResolver::new(&tmp_dir);

    let err = resolver
        .load_module("nonexistent")
        .expect_err("expected error");
    assert!(
        matches!(&err, ModuleError::Io(_)),
        "expected Io error for a nonexistent path, got: {err:?}"
    );

    let err = resolver
        .load_module("nonexistent")
        .expect_err("expected error");
    assert!(
        matches!(&err, ModuleError::Io(_)),
        "expected Io error on second attempt, not CircularImport, got: {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_load_module_parse_error_cleans_resolving_set() {
    let tmp_root = std::env::temp_dir().join("carina_test_parse_error_cleanup");
    let _ = fs::remove_dir_all(&tmp_root);
    let bad_module_dir = tmp_root.join("bad_module");
    fs::create_dir_all(&bad_module_dir).unwrap();
    fs::write(
        bad_module_dir.join("main.crn"),
        "this is not valid carina syntax {{{{",
    )
    .unwrap();

    let mut resolver = ModuleResolver::new(&tmp_root);

    // First attempt: parse error on a directory module with a bad .crn file.
    let result = resolver.load_module("bad_module");
    assert!(
        result.is_err(),
        "expected error but got: {:?}",
        result.unwrap()
    );
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ModuleError::ParseInFile { .. }),
        "expected located Parse error on first attempt, got: {err:?}"
    );

    // Second attempt: should still get parse error, not circular import
    // (the resolving set must have been cleaned up).
    let result = resolver.load_module("bad_module");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ModuleError::ParseInFile { .. }),
        "expected located Parse error on second attempt, not CircularImport, got: {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_load_module_rejects_state_blocks() {
    let cases = [
        (
            "moved",
            r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
}
"#,
        ),
        (
            "removed",
            r#"removed {
  from = mock.test.Resource 'old'
}
"#,
        ),
        (
            "import",
            r#"import {
  to = mock.test.Resource 'existing'
  id = 'resource-123'
}
"#,
        ),
    ];

    for (kind, state_block) in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        let module_dir = tmp.path().join("module");
        fs::create_dir(&module_dir).expect("module directory");
        fs::write(
            module_dir.join("arguments.crn"),
            "arguments {\n  name: String = 'test'\n}\n",
        )
        .expect("module arguments");
        fs::write(module_dir.join("state.crn"), state_block).expect("module state block");

        let mut resolver = ModuleResolver::new(tmp.path());
        let err = resolver
            .load_module("module")
            .expect_err("state block inside a module must be rejected");

        assert!(
            matches!(&err, ModuleError::StateBlockInModule),
            "expected StateBlockInModule for {kind}, got {err:?}",
        );
        assert_eq!(
            err.to_string(),
            "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.",
            "unexpected resolver error for {kind}",
        );
    }
}

#[test]
fn test_load_module_rejects_backend_and_cleans_resolving_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("module");
    fs::create_dir(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("arguments.crn"),
        "arguments {\n  name: String = 'test'\n}\n",
    )
    .expect("module arguments");
    fs::write(
        module_dir.join("backend.crn"),
        "backend local {\n  path = 'module.state.json'\n}\n",
    )
    .expect("module backend");

    let mut resolver = ModuleResolver::new(tmp.path());
    for attempt in 1..=2 {
        let err = resolver
            .load_module("module")
            .expect_err("backend inside a module must be rejected");
        assert!(
            matches!(&err, ModuleError::BackendInModule),
            "expected BackendInModule on attempt {attempt}, got {err:?}",
        );
        assert_eq!(
            err.to_string(),
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level.",
            "unexpected resolver error on attempt {attempt}",
        );
    }
}

#[test]
fn test_load_module_rejects_upstream_state_and_cleans_resolving_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("module");
    fs::create_dir(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("arguments.crn"),
        "arguments {\n  name: String = 'test'\n}\n",
    )
    .expect("module arguments");
    fs::write(
        module_dir.join("upstream.crn"),
        "let up = upstream_state { source = '../other' }\n",
    )
    .expect("module upstream_state");

    let mut resolver = ModuleResolver::new(tmp.path());
    for attempt in 1..=2 {
        let err = resolver
            .load_module("module")
            .expect_err("upstream_state inside a module must be rejected");
        assert!(
            matches!(&err, ModuleError::UpstreamStateInModule),
            "expected UpstreamStateInModule on attempt {attempt}, got {err:?}",
        );
        assert_eq!(
            err.to_string(),
            "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level.",
            "unexpected resolver error on attempt {attempt}",
        );
    }
}

#[test]
fn test_load_module_rejects_exports_and_cleans_resolving_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("module");
    fs::create_dir(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("arguments.crn"),
        "arguments {\n  name: String = 'test'\n}\n",
    )
    .expect("module arguments");
    fs::write(
        module_dir.join("exports.crn"),
        "exports {\n  module_value = 'module'\n}\n",
    )
    .expect("module exports");

    let mut resolver = ModuleResolver::new(tmp.path());
    for attempt in 1..=2 {
        let err = resolver
            .load_module("module")
            .expect_err("exports inside a module must be rejected");
        assert!(
            matches!(&err, ModuleError::ExportsInModule),
            "expected ExportsInModule on attempt {attempt}, got {err:?}",
        );
        assert_eq!(
            err.to_string(),
            "exports blocks are not allowed inside modules. Define exports at the root configuration level.",
            "unexpected resolver error on attempt {attempt}",
        );
    }
}

#[test]
fn test_load_module_rejects_file_path() {
    // Issue #1997: Modules must be directories. A single `.crn` file as a
    // module target should be rejected with NotADirectory instead of being
    // parsed as a one-file module.
    let tmp_root = std::env::temp_dir().join("carina_test_module_rejects_file");
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).unwrap();
    fs::write(tmp_root.join("single.crn"), "arguments {\n  x: String\n}\n").unwrap();

    let mut resolver = ModuleResolver::new(&tmp_root);
    let err = resolver
        .load_module("single.crn")
        .expect_err("a single .crn file must not be loadable as a module");
    assert!(
        matches!(&err, ModuleError::NotADirectory { .. }),
        "expected NotADirectory, got {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

/// Helper to create a module with a validated port argument
fn create_module_with_port_validation() -> ParsedFile {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "port".to_string(),
            static_type: known(TypeExpr::Int),
            default: Some(Value::Concrete(ConcreteValue::Int(8080))),
            description: Some("Web server port".to_string()),
            validations: vec![ValidationBlock::dummy(
                ValidateExpr::And(
                    Box::new(ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("port".to_string())),
                        op: CompareOp::Gte,
                        rhs: Box::new(ValidateExpr::Int(1)),
                    }),
                    Box::new(ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("port".to_string())),
                        op: CompareOp::Lte,
                        rhs: Box::new(ValidateExpr::Int(65535)),
                    }),
                ),
                Some("Port must be between 1 and 65535".to_string()),
            )],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn test_argument_validation_passes_with_valid_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall::test("web_server".to_string(), Some("web".to_string()), {
        let mut args = HashMap::new();
        args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(443)));
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "web").unwrap();
    assert!(
        result.compositions[0]
            .signature
            .pending_constraints
            .is_empty(),
        "satisfied constraints must not be persisted"
    );
}

#[test]
fn test_argument_validation_with_resource_reference_is_pending() {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};

    let mut module = create_module_with_port_validation();
    module.arguments[0].static_type = known(TypeExpr::String);
    module.arguments[0].validations = vec![ValidationBlock::dummy(
        ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::FunctionCall {
                name: "length".to_string(),
                args: vec![ValidateExpr::Var("port".to_string())],
            }),
            op: CompareOp::Gt,
            rhs: Box::new(ValidateExpr::Int(0)),
        },
        Some("port must not be empty".to_string()),
    )];
    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver
            .imported_modules
            .insert("web_server".to_string(), module);
        resolver
    };
    let call = ModuleCall::test(
        "web_server".to_string(),
        Some("web".to_string()),
        module_arguments([(
            "port".to_string(),
            Value::resource_ref("producer", "port", Vec::new()),
        )]),
    );

    let expanded = resolver
        .expand_module_call(&call, "web")
        .expect("a reference-valued constraint must remain pending");
    let constraints = &expanded.compositions[0].signature.pending_constraints;
    assert_eq!(constraints.len(), 1);
    assert_eq!(
        constraints[0].id,
        crate::resource::ModuleConstraintId::argument_validation("port", 0)
    );
    assert_eq!(
        referenced_constraint_arguments(&constraints[0].expression),
        ["port"]
    );
}

#[test]
fn nested_pending_constraint_keeps_local_names_while_value_is_rewritten() {
    use crate::parser::{CompareOp, ValidateExpr};
    use crate::resource::{
        CompositionCall, CompositionCallPath, LiveComposition, LiveCompositionArgument,
        LiveCompositionSignature, ModuleConstraintId, PendingModuleConstraint,
    };

    let parsed_calls = crate::parser::parse_file(
        r#"let inner = use { source = './inner' }
let outer = use { source = './outer' }
inner {}
let root = outer { outer_port = producer.port }
"#,
        std::path::Path::new("/tmp/root.crn"),
        &ProviderContext::default(),
    )
    .expect("parse distinct inner and outer call identities")
    .module_calls;
    let inner_id = parsed_calls[0].id().clone();
    let call = parsed_calls[1].clone();

    let inner_call = CompositionCall {
        module_name: "inner".to_string(),
        binding: Some("inner".to_string()),
        instance: "inner".to_string(),
        module_source: None,
        module_directory: None,
        loop_origin: None,
        id: inner_id.clone(),
    };
    let inner = LiveComposition {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "_virtual", "inner",
        )),
        signature: LiveCompositionSignature {
            arguments: IndexMap::from([(
                "port".to_string(),
                LiveCompositionArgument::from_value(
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "outer_port".to_string(),
                    }),
                    consumer_type(TypeExpr::String),
                    ArgumentOrigin::Defaulted,
                ),
            )]),
            attributes: IndexMap::new(),
            pending_constraints: vec![PendingModuleConstraint {
                id: ModuleConstraintId::argument_validation("port", 0),
                expression: ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::FunctionCall {
                        name: "length".to_string(),
                        args: vec![ValidateExpr::Var("port".to_string())],
                    }),
                    op: CompareOp::Gt,
                    rhs: Box::new(ValidateExpr::Int(0)),
                },
                message: "port must not be empty".to_string(),
            }],
        },
        binding: Some("inner".to_string()),
        dependency_bindings: BTreeSet::new(),
        module_name: "inner".to_string(),
        instance: "inner".to_string(),
        call_path: CompositionCallPath::direct(inner_call),
        quoted_string_attrs: HashSet::new(),
    };
    let outer = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![inner],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "outer_port".to_string(),
            static_type: known(TypeExpr::String),
            default: None,
            description: None,
            validations: vec![],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };
    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver.imported_modules.insert("outer".to_string(), outer);
        resolver
    };
    let expanded = resolver.expand_module_call(&call, "root").unwrap();
    let nested = expanded
        .compositions
        .iter()
        .find(|composition| composition.module_name == "inner")
        .expect("nested composition");
    assert_eq!(nested.instance, "root.inner");
    assert_eq!(nested.call_path().calls().count(), 2);
    assert_eq!(nested.call_path().root().module_name, "outer");
    assert_eq!(nested.call_path().immediate().module_name, "inner");
    assert_eq!(nested.call_path().immediate().instance, "root.inner");
    assert_eq!(&nested.call_path().root().id, call.id());
    assert_eq!(&nested.call_path().immediate().id, &inner_id);
    assert_ne!(
        nested.call_path().root().id,
        nested.call_path().immediate().id,
    );
    assert_eq!(
        nested
            .signature
            .arguments
            .get("port")
            .map(LiveCompositionArgument::value),
        Some(&Value::resource_ref("producer", "port", Vec::new()))
    );
    let constraint = &nested.signature.pending_constraints[0];
    assert_eq!(
        constraint.id,
        ModuleConstraintId::argument_validation("port", 0)
    );
    assert_eq!(
        referenced_constraint_arguments(&constraint.expression),
        ["port"]
    );
}

#[test]
fn test_argument_validation_passes_with_default_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall::test(
        "web_server".to_string(),
        Some("web".to_string()),
        HashMap::new(), // Uses default 8080
    );

    let result = resolver.expand_module_call(&call, "web");
    assert!(result.is_ok());
}

#[test]
fn test_argument_validation_fails_with_invalid_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall::test("web_server".to_string(), Some("web".to_string()), {
        let mut args = HashMap::new();
        args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(0)));
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "web");
    assert!(result.is_err());
    let err = result.unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("module 'web_server'"), "{rendered}");
    assert!(rendered.contains("call 'web'"), "{rendered}");
    assert!(rendered.contains("argument 'port'"), "{rendered}");
    assert!(
        rendered.contains("Port must be between 1 and 65535"),
        "{rendered}"
    );
    assert!(rendered.contains("got 0"), "{rendered}");
    match err.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.module, "web_server");
            assert_eq!(diagnostic.arguments, ["port"]);
            assert_eq!(diagnostic.message, "Port must be between 1 and 65535");
            assert_eq!(diagnostic.actuals, [("port".to_string(), "0".to_string())]);
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_argument_validation_fails_with_negative_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall::test("web_server".to_string(), Some("web".to_string()), {
        let mut args = HashMap::new();
        args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(-1)));
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "web");
    assert!(result.is_err());
}

#[test]
fn test_argument_validation_fails_too_large() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall::test("web_server".to_string(), Some("web".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(70000)),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "web");
    assert!(result.is_err());
}

#[test]
fn test_argument_validation_no_message_uses_default() {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "count".to_string(),
            static_type: known(TypeExpr::Int),
            default: None,
            description: None,
            validations: vec![ValidationBlock::dummy(
                ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("count".to_string())),
                    op: CompareOp::Gt,
                    rhs: Box::new(ValidateExpr::Int(0)),
                },
                None,
            )],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("counter".to_string(), module);
        r
    };

    let call = ModuleCall::test("counter".to_string(), Some("c".to_string()), {
        let mut args = HashMap::new();
        args.insert("count".to_string(), Value::Concrete(ConcreteValue::Int(0)));
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "c");
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.message, "validation failed for argument 'count'");
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_argument_validation_len_with_list() {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "tags".to_string(),
            static_type: known(TypeExpr::List(Box::new(TypeExpr::String))),
            default: None,
            description: None,
            validations: vec![ValidationBlock::dummy(
                ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::FunctionCall {
                        name: "len".to_string(),
                        args: vec![ValidateExpr::Var("tags".to_string())],
                    }),
                    op: CompareOp::Gte,
                    rhs: Box::new(ValidateExpr::Int(1)),
                },
                Some("At least one tag is required".to_string()),
            )],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("tagged".to_string(), module);
        r
    };

    // Valid: non-empty list
    let call = ModuleCall::test("tagged".to_string(), Some("t".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("env:prod".into()),
            )])),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "t").is_ok());

    // Invalid: empty list
    let call = ModuleCall::test("tagged".to_string(), Some("t".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::List(vec![])),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call, "t");
    assert!(result.is_err());
    let error = result.unwrap_err();
    match error.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.message, "At least one tag is required");
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_require_block_passes() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "enable_https".to_string(),
                static_type: known(TypeExpr::Bool),
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "cert_arn".to_string(),
                static_type: known(TypeExpr::String),
                default: Some(Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:123:cert/abc".into(),
                ))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock::dummy(
            // !enable_https || cert_arn != null
            ValidateExpr::Or(
                Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                    "enable_https".to_string(),
                )))),
                Box::new(ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("cert_arn".to_string())),
                    op: CompareOp::Ne,
                    rhs: Box::new(ValidateExpr::Null),
                }),
            ),
            "cert_arn is required when HTTPS is enabled".to_string(),
        )],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("web".to_string(), module);
        r
    };

    // HTTPS enabled with cert_arn provided: should pass
    let call = ModuleCall::test("web".to_string(), Some("w".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "enable_https".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        );
        args.insert(
            "cert_arn".to_string(),
            Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:us-east-1:123:cert/abc".into(),
            )),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "w").is_ok());
}

#[test]
fn test_require_block_fails_with_not_expr() {
    use crate::parser::{RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "enable_https".to_string(),
                static_type: known(TypeExpr::Bool),
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "has_cert".to_string(),
                static_type: known(TypeExpr::Bool),
                default: Some(Value::Concrete(ConcreteValue::Bool(false))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock::dummy(
            // !enable_https || has_cert
            ValidateExpr::Or(
                Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                    "enable_https".to_string(),
                )))),
                Box::new(ValidateExpr::Var("has_cert".to_string())),
            ),
            "cert is required when HTTPS is enabled".to_string(),
        )],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("web".to_string(), module);
        r
    };

    // HTTPS enabled but has_cert is false: should fail
    let call = ModuleCall::test("web".to_string(), Some("w".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "enable_https".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        );
        args.insert(
            "has_cert".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call, "w");
    assert!(result.is_err());
    let error = result.unwrap_err();
    match error.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.message, "cert is required when HTTPS is enabled");
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_require_block_len_function() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "subnet_ids".to_string(),
            static_type: known(TypeExpr::List(Box::new(TypeExpr::String))),
            default: None,
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock::dummy(
            // len(subnet_ids) >= 2
            ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::FunctionCall {
                    name: "len".to_string(),
                    args: vec![ValidateExpr::Var("subnet_ids".to_string())],
                }),
                op: CompareOp::Gte,
                rhs: Box::new(ValidateExpr::Int(2)),
            },
            "ALB requires at least two subnets".to_string(),
        )],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("alb".to_string(), module);
        r
    };

    // Two subnets: should pass
    let call = ModuleCall::test("alb".to_string(), Some("lb".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "subnet_ids".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("subnet-a".into())),
                Value::Concrete(ConcreteValue::String("subnet-b".into())),
            ])),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "lb").is_ok());

    // One subnet: should fail
    let call = ModuleCall::test("alb".to_string(), Some("lb".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "subnet_ids".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("subnet-a".into()),
            )])),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call, "lb");
    assert!(result.is_err());
    let error = result.unwrap_err();
    match error.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.message, "ALB requires at least two subnets");
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_require_block_multiple_constraints() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "min_size".to_string(),
                static_type: known(TypeExpr::Int),
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "max_size".to_string(),
                static_type: known(TypeExpr::Int),
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock::dummy(
            // min_size <= max_size
            ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::Var("min_size".to_string())),
                op: CompareOp::Lte,
                rhs: Box::new(ValidateExpr::Var("max_size".to_string())),
            },
            "min_size must be <= max_size".to_string(),
        )],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("asg".to_string(), module);
        r
    };

    // min_size < max_size: should pass
    let call = ModuleCall::test("asg".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "min_size".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        );
        args.insert(
            "max_size".to_string(),
            Value::Concrete(ConcreteValue::Int(5)),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "a").is_ok());

    // min_size > max_size: should fail
    let call = ModuleCall::test("asg".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "min_size".to_string(),
            Value::Concrete(ConcreteValue::Int(10)),
        );
        args.insert(
            "max_size".to_string(),
            Value::Concrete(ConcreteValue::Int(5)),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call, "a");
    assert!(result.is_err());
    let error = result.unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("module 'asg'"), "{rendered}");
    assert!(rendered.contains("call 'a'"), "{rendered}");
    assert!(rendered.contains("max_size"), "{rendered}");
    assert!(rendered.contains("min_size"), "{rendered}");
    assert!(rendered.contains("max_size = 5"), "{rendered}");
    assert!(rendered.contains("min_size = 10"), "{rendered}");
    assert!(
        rendered.contains("min_size must be <= max_size"),
        "{rendered}"
    );
    match error.root_cause() {
        ModuleError::Constraint(diagnostic) => {
            assert_eq!(diagnostic.message, "min_size must be <= max_size");
        }
        other => panic!("Expected module constraint failure, got {other:?}"),
    }
}

#[test]
fn test_argument_type_mismatch_int_for_string() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        {
            let mut args = HashMap::new();
            // vpc_id expects string, pass int
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::Int(42)),
            );
            module_arguments(args)
        },
    );

    let result = resolver.expand_module_call(&call, "my_instance");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::InvalidArgumentType { .. })
        ),
        "Expected InvalidArgumentType error, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_mismatch_string_for_bool() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("my_instance".to_string()),
        {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            );
            // enable_flag expects bool, pass string
            args.insert(
                "enable_flag".to_string(),
                Value::Concrete(ConcreteValue::String("not-a-bool".into())),
            );
            module_arguments(args)
        },
    );

    let result = resolver.expand_module_call(&call, "my_instance");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::InvalidArgumentType { .. })
        ),
        "Expected InvalidArgumentType error, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_custom_validator() {
    // Create a loaded ProviderContext with a custom "arn" validator
    let identity = TypeIdentity::bare("Arn");
    let owned = crate::parser::OwnedTypeIdentity::provider("test", identity.clone());
    let validator_identity = identity.clone();
    let config = LoadedProviderContext::for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            std::collections::HashSet::from([owned.clone()]),
            Box::new(move |target, value| {
                if target.identity() != &validator_identity {
                    return Ok(());
                }
                let Value::Concrete(ConcreteValue::String(s)) = value else {
                    return Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        "Expected string",
                    ));
                };
                let s = s.expose_plaintext();
                if s.starts_with("arn:") {
                    Ok(())
                } else {
                    Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        format!("expected ARN format, got '{s}'"),
                    ))
                }
            }),
        ),
        ..ProviderContext::default()
    });

    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "policy_arn".to_string(),
        static_type: known(TypeExpr::SchemaType { owned }),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let schemas = crate::schema::SchemaRegistry::new();
    let resolver = {
        let mut r = ModuleResolver::with_config_and_schemas(".", &config, &schemas);
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    // Valid ARN passes
    let call = ModuleCall::test("test_module".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "policy_arn".to_string(),
            Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:policy/MyPolicy"
                    .to_string()
                    .into(),
            )),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "a").is_ok());

    // Invalid ARN fails
    let call_bad = ModuleCall::test("test_module".to_string(), Some("b".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "policy_arn".to_string(),
            Value::Concrete(ConcreteValue::String("not-an-arn".into())),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call_bad, "b");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::InvalidArgumentType { .. })
        ),
        "Expected InvalidArgumentType error for invalid ARN, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_list_of_custom_type() {
    let identity = TypeIdentity::bare("Arn");
    let owned = crate::parser::OwnedTypeIdentity::provider("test", identity.clone());
    let validator_identity = identity.clone();
    let config = LoadedProviderContext::for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            std::collections::HashSet::from([owned.clone()]),
            Box::new(move |target, value| {
                if target.identity() != &validator_identity {
                    return Ok(());
                }
                let Value::Concrete(ConcreteValue::String(s)) = value else {
                    return Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        "Expected string",
                    ));
                };
                let s = s.expose_plaintext();
                if s.starts_with("arn:") {
                    Ok(())
                } else {
                    Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        format!("expected ARN format, got '{s}'"),
                    ))
                }
            }),
        ),
        ..ProviderContext::default()
    });

    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "policy_arns".to_string(),
        static_type: known(TypeExpr::List(Box::new(TypeExpr::SchemaType { owned }))),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let schemas = crate::schema::SchemaRegistry::new();
    let resolver = {
        let mut r = ModuleResolver::with_config_and_schemas(".", &config, &schemas);
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    // Valid list of ARNs
    let call = ModuleCall::test("test_module".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "policy_arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("arn:aws:iam::123:policy/A".into())),
                Value::Concrete(ConcreteValue::String("arn:aws:iam::123:policy/B".into())),
            ])),
        );
        module_arguments(args)
    });
    assert!(resolver.expand_module_call(&call, "a").is_ok());

    // List with invalid ARN fails
    let call_bad = ModuleCall::test("test_module".to_string(), Some("b".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "policy_arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("arn:aws:iam::123:policy/A".into())),
                Value::Concrete(ConcreteValue::String("not-an-arn".into())),
            ])),
        );
        module_arguments(args)
    });
    let result = resolver.expand_module_call(&call_bad, "b");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::InvalidArgumentType { .. })
        ),
        "Expected InvalidArgumentType for list with invalid ARN, got {:?}",
        result
    );
}

#[test]
fn test_load_module_directory_merges_sibling_files_with_main() {
    // A directory-based module that splits definitions across main.crn and
    // sibling files (arguments.crn, exports.crn, resources.crn) must be
    // parsed as a whole. The previous behavior returned only main.crn's
    // contents when main.crn existed, silently dropping siblings.
    let tmp_dir = std::env::temp_dir().join("carina_test_load_module_sibling_merge");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    fs::write(tmp_dir.join("main.crn"), "# main module file\n").unwrap();
    fs::write(
        tmp_dir.join("arguments.crn"),
        "arguments {\n  env: String\n}\n",
    )
    .unwrap();
    fs::write(
        tmp_dir.join("exports.crn"),
        "exports {\n  region = \"ap-northeast-1\"\n}\n",
    )
    .unwrap();

    let parsed = load_module(&tmp_dir)
        .expect("expected module to load because arguments.crn declares an argument");

    assert_eq!(
        parsed.arguments.len(),
        1,
        "arguments declared in arguments.crn must be preserved when main.crn exists"
    );
    assert_eq!(parsed.arguments[0].name, "env");
    assert_eq!(
        parsed.export_params.len(),
        1,
        "exports declared in exports.crn must be preserved when main.crn exists"
    );
    assert_eq!(parsed.export_params[0].name, "region");

    let _ = fs::remove_dir_all(&tmp_dir);
}

/// Helper for #2393 regression tests: write a module body and a calling
/// root body to a tempdir, parse and resolve, return the parsed root with
/// modules expanded. Mirrors the directory-scoped fixture shape required by
/// CLAUDE.md so single-file thinking can't sneak back in.
fn resolve_default_arg_fixture(module_body: &str, root_body: &str) -> ModuleResolvedParsedFile {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/m");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(module_dir.join("main.crn"), module_body).unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir).expect("resolve_modules should succeed")
}

fn role_attr<'a>(parsed: &'a ModuleResolvedParsedFile, attr: &str) -> &'a Value {
    parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .expect("Role resource should exist")
        .get_attr(attr)
        .unwrap_or_else(|| panic!("Role.{attr} should exist"))
}

// Regression for #2393. A module argument default that interpolates another
// argument (`["repo:${github_repo}:*"]`) must resolve `${github_repo}` against
// the caller-provided value, not leave it as the literal binding name.
#[test]
fn test_argument_default_interpolates_other_arguments() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  github_repo     : String
  subject_patterns: list(String) = ["repo:${github_repo}:*"]
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {
    patterns = subject_patterns
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  github_repo = 'carina-rs/infra'
}
"#,
    );

    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("assume_role_policy_document should be a Map, got {policy:?}");
    };
    let patterns = policy_map.get("patterns").expect("patterns should exist");
    let Value::Concrete(ConcreteValue::List(items)) = patterns else {
        panic!("patterns should be a List, got {patterns:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0],
        Value::Concrete(ConcreteValue::String("repo:carina-rs/infra:*".into())),
        "default's ${{github_repo}} interpolation must resolve against the caller's value"
    );
}

// #2393 — block-form default with `${other_arg}` interpolation must also
// resolve, not just the simple `name: T = expr` form.
#[test]
fn test_argument_default_block_form_interpolates_other_arguments() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  github_repo: String
  subject_pattern: String {
    description = "subject pattern"
    default     = "repo:${github_repo}:*"
  }
}

let role = awscc.iam.Role {
  role_name                   = subject_pattern
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  github_repo = 'carina-rs/infra'
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("repo:carina-rs/infra:*".into())),
        "block-form default's `${{github_repo}}` must resolve against the caller's value"
    );
}

// #2393 — bare-identifier default (`b: String = a`, no `${}` wrapping) is a
// `Value::Deferred(DeferredValue::ResourceRef{ binding: "a" })` after parse and must resolve to the
// caller-supplied value of `a`.
#[test]
fn test_argument_default_bare_identifier_resolves() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  primary  : String
  secondary: String = primary
}

let role = awscc.iam.Role {
  role_name                   = secondary
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  primary = 'p'
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("p".into())),
        "bare-identifier default `secondary = primary` must resolve to caller's `primary`"
    );
}

#[test]
fn module_argument_postfix_access_resolves_across_directory_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/selector");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("arguments.crn"),
        r#"
arguments {
  items: list(String)
  cfg: map(String)
}
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("resource.crn"),
        r#"
let selected = mock.test.Thing {
  name          = items[1]
  bracket_value = cfg["k"]
  field_value   = cfg.k
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let selector = use { source = "../modules/selector" }
let child = selector {
  items = ["a", "b"]
  cfg   = { k = "value" }
}
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let parsed = crate::parser::parse(root_body, &ProviderContext::default()).unwrap();
    let parsed = resolve_modules(parsed, &root_dir).expect("module postfix accesses must expand");

    let selected = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("child.selected"))
        .expect("expanded selected resource");
    assert_eq!(
        selected.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("b".into())))
    );
    assert_eq!(
        selected.get_attr("bracket_value"),
        Some(&Value::Concrete(ConcreteValue::String("value".into())))
    );
    assert_eq!(
        selected.get_attr("field_value"),
        Some(&Value::Concrete(ConcreteValue::String("value".into())))
    );
}

#[test]
fn module_argument_postfix_access_reports_known_miss_during_expansion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/selector");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("arguments.crn"),
        "arguments { items: list(String) }\n",
    )
    .unwrap();
    fs::write(
        module_dir.join("resource.crn"),
        r#"
let selected = mock.test.Thing {
  name = items[5]
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let selector = use { source = "../modules/selector" }
let child = selector { items = ["x", "y"] }
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let parsed = crate::parser::parse(root_body, &ProviderContext::default()).unwrap();
    let error = resolve_modules(parsed, &root_dir)
        .expect_err("known module-argument misses must fail during module expansion");
    let message = error.to_string();
    assert!(
        message.contains("index 5 out of range for list of length 2"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("selector")
            && message.contains("modules/selector")
            && message.contains("line 3"),
        "module access errors must identify the module source and authored line: {message}"
    );
}

#[test]
fn module_argument_postfix_access_preserves_deferred_caller_value() {
    use crate::resource::{PathSegment, Subscript};

    let value = crate::resource::apply_postfix_access(
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument {
                name: "items".to_string(),
                source_line: (0).into(),
                diagnostic_name: None,
            },
        )),
        PathSegment::Subscript {
            index: Subscript::Int { index: 1 },
        },
        3,
        crate::resource::AccessMissPolicy::Error,
    )
    .unwrap();
    let caller_value = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::split,
        args: vec![
            Value::Concrete(ConcreteValue::String(":".into())).into(),
            Value::resource_ref("source", "name", vec![]).into(),
        ],
        source_line: (0).into(),
    });

    let substituted = substitute_arguments(
        &value,
        &HashMap::from([("items".to_string(), caller_value)]),
    )
    .expect("substitution");

    assert!(matches!(
        substituted,
        Value::Deferred(DeferredValue::Access(access))
            if access.source_line() == 3
            && matches!(access.step(), PathSegment::Subscript { index: Subscript::Int { index: 1 } })
            && matches!(access.base(), Value::Deferred(DeferredValue::FunctionCall { name, args, .. })
            if name == "split" && matches!(args[1].walker_value(), Value::Deferred(DeferredValue::ResourceRef { path }) if path.binding() == "source"))
    ));
}

#[test]
fn ref_function_parameter_substitutes_a_module_parameter_handle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/selector");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { vpc: mock.test.Thing }

fn id_of(resource: mock.test.Thing): String {
  resource.identifier
}

let identifier = id_of(vpc)
let selected = mock.test.Consumer { parent = identifier }
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let source = mock.test.Thing { name = "source" }
let selector = use { source = "../modules/selector" }
let child = selector { vpc = source }
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let mut context = ProviderContext::default();
    context
        .resource_types
        .insert(("mock".to_string(), "test.Thing".to_string()));
    let parsed = crate::parser::parse(root_body, &context).unwrap();
    let parsed = resolve_modules_with_config(
        parsed,
        &root_dir,
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("a Ref-typed function parameter must substitute a module handle");

    let selected = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("child.selected"))
        .expect("expanded consumer");
    assert_eq!(
        selected.get_attr("parent"),
        Some(&Value::resource_ref("source", "identifier", vec![])),
    );
}

#[test]
fn module_parameter_handle_rewrites_depends_on_to_the_caller_binding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/consumer");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("arguments.crn"),
        "arguments { peer: mock.test.Thing }\n",
    )
    .unwrap();
    fs::write(
        module_dir.join("resource.crn"),
        r#"
let consumer = mock.test.Consumer {
  name = "consumer"
  directives { depends_on = [peer] }
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let source = mock.test.Thing { name = "peer" }
let imported = use { source = "../modules/consumer" }
let child = imported { peer = source }
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let mut context = ProviderContext::default();
    context
        .resource_types
        .insert(("mock".to_string(), "test.Thing".to_string()));
    let parsed = crate::parser::parse(root_body, &context).unwrap();
    let parsed = resolve_modules_with_config(
        parsed,
        &root_dir,
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("a module handle in depends_on must expand to the caller binding");

    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("child.consumer"))
        .expect("expanded consumer");
    assert_eq!(consumer.directives.depends_on, vec!["source"]);
    let dependencies = crate::deps::get_resource_dependencies(consumer);
    assert!(dependencies.contains("source"));
    assert!(!dependencies.contains("child.peer"));
}

#[test]
fn module_dependency_substitution_error_reports_the_call_argument_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/consumer");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"arguments { peer: mock.test.Thing }
let consumer = mock.test.Consumer {
  name = "consumer"
  directives { depends_on = [peer] }
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let source = mock.test.Thing { name = "peer" }
let imported = use { source = "../modules/consumer" }
let child = imported {
  peer = source.identifier
}
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let mut context = ProviderContext::default();
    context
        .resource_types
        .insert(("mock".to_string(), "test.Thing".to_string()));
    let parsed = crate::parser::parse(root_body, &context).unwrap();
    let error = resolve_modules_with_config(
        parsed,
        &root_dir,
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect_err("a value cannot satisfy a module dependency handle");

    let message = error.to_string();
    assert!(message.contains("at line 5"), "unexpected error: {message}");
    assert!(
        message.contains("depends_on parameter 'peer' received a value"),
        "unexpected error: {message}",
    );
}

#[test]
fn module_value_argument_binding_collision_is_not_treated_as_a_handle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/consumer");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { peer: mock.test.Thing }
let consumer = mock.test.Consumer { parent = peer.name }
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let peer = mock.test.Thing { name = "peer" }
let imported = use { source = "../modules/consumer" }
let child = imported { peer = peer.identifier }
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let mut context = ProviderContext::default();
    context
        .resource_types
        .insert(("mock".to_string(), "test.Thing".to_string()));
    let parsed = crate::parser::parse(root_body, &context).unwrap();
    let parsed = resolve_modules_with_config(
        parsed,
        &root_dir,
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("the typed Value variant must survive a binding-name collision");

    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("child.consumer"))
        .expect("expanded consumer");
    assert_eq!(
        consumer.get_attr("parent"),
        Some(&Value::resource_ref(
            "peer",
            "identifier",
            vec!["name".to_string()],
        )),
    );
}

// #2393 — transitive default chain `a → b → c`. Each default is resolved
// against arguments already in scope, so by the time `c = "${b}!"` is
// resolved, `b` has been canonicalized to a flat string `"X-X"`. Pinning
// this is important because removing the `canonicalize_in_place` call in
// the expander would produce a nested `Interpolation` in `c` that no
// downstream consumer flattens.
#[test]
fn test_argument_default_transitive_chain_resolves() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  a: String = 'X'
  b: String = "${a}-${a}"
  c: String = "${b}!"
}

let role = awscc.iam.Role {
  role_name                   = c
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("X-X!".into())),
        "transitive default chain `a → b → c` must collapse to a flat string"
    );
}

// #2393 — interpolation nested inside a Map value within a list default
// must be resolved by `substitute_arguments`'s recursion through
// List/Map/Interpolation arms.
#[test]
fn test_argument_default_nested_collection_interpolates() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  region : String
  tags   : map(String) = {
    managed_by = 'carina'
    region     = "${region}-tag"
  }
}

let role = awscc.iam.Role {
  role_name                   = 'r'
  assume_role_policy_document = {
    tags = tags
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  region = 'ap-northeast-1'
}
"#,
    );

    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("policy should be Map, got {policy:?}");
    };
    let tags = policy_map.get("tags").expect("tags should exist");
    let Value::Concrete(ConcreteValue::Map(tag_map)) = tags else {
        panic!("tags should be Map, got {tags:?}");
    };
    assert_eq!(
        tag_map.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1-tag".into()
        ))),
        "interpolation inside a nested map default must resolve recursively"
    );
}

// #2393 — module with arguments but no defaults at all must work; the new
// substitute+canonicalize pass is a no-op on caller-supplied scalars.
#[test]
fn test_argument_no_defaults_caller_scalars_pass_through() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  x: String
  y: Int
}

let role = awscc.iam.Role {
  role_name                   = x
  assume_role_policy_document = {
    y_value = y
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  x = 'hello'
  y = 42
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("hello".into())),
    );
    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("policy should be Map, got {policy:?}");
    };
    assert_eq!(
        policy_map.get("y_value"),
        Some(&Value::Concrete(ConcreteValue::Int(42)))
    );
}

// #2393 — argument defaults whose RHS references *another* argument
// (`prefix: String = later`) used to be resolved in strict declaration
// order by `substitute_arguments` in `expander.rs`. A forward
// reference therefore degraded — first to a literal
// `Value::Concrete(ConcreteValue::String("later"))` (pre-#2817), then to an unresolved
// `Value::Deferred(DeferredValue::ResourceRef("later"))` (post-#2817 PR1).
//
// This pass adds a fixed-point loop around `substitute_arguments`
// (#2817 follow-up): each iteration replaces every `ResourceRef`
// whose binding matches an already-resolved argument; the loop
// terminates when an iteration produces no changes. Cycles
// (`a = b`, `b = a`) hit a hard iteration cap and leave the still-
// unresolved refs in place for the post-merge scope check to flag.
#[test]
fn test_argument_default_forward_ref_resolves_under_fixpoint() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  prefix: String = later
  later : String = 'L'
}

let role = awscc.iam.Role {
  role_name                   = prefix
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("L".into())),
        "forward-ref default `prefix = later` resolves to `later`'s value `'L'` \
         once the fix-point loop runs in `expander.rs::substitute_arguments`"
    );
}

/// Argument-default fix-point loop must terminate even when the
/// dependency graph is cyclic. With `a: String = b` and `b: String = a`
/// neither side ever reduces, so each iteration leaves both as
/// `ResourceRef`. The hard iteration cap stops the loop and surfaces
/// the unresolved refs to the scope check.
#[test]
fn test_argument_default_cycle_terminates_with_unresolved_refs() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  a: String = b
  b: String = a
}

let role = awscc.iam.Role {
  role_name                   = a
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    let role_name = role_attr(&parsed, "role_name");
    match role_name {
        // The cycle leaves both `a` and `b` as typed argument placeholders; which
        // one surfaces as `role_name` depends on substitute_arguments's
        // walk order. Either is fine — the contract is that the loop
        // terminates without panicking and preserves a structured,
        // statically typed placeholder rather than inventing a value.
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument { name, .. })) => {
            assert!(
                name == "a" || name == "b",
                "expected cyclic argument to point at `a` or `b`, got: {name:?}"
            );
        }
        other => panic!("expected unresolved module argument from cycle; got {other:?}"),
    }
}

// #2393 — caller-supplied ResourceRefs (cross-module data flow) must NOT be
// substituted by the new default-resolution pass: their binding names are
// not argument names, so they should pass through untouched until the outer
// resolver rewrites them.
#[test]
fn test_argument_caller_resource_ref_passes_through() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  source_arn: String
}

let role = awscc.iam.Role {
  role_name                   = source_arn
  assume_role_policy_document = {}
}
"#,
        r#"
let upstream = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

let m = use { source = '../modules/m' }

m {
  source_arn = upstream.arn
}
"#,
    );

    // Find the module's expanded `role` (the upstream Vpc has type Vpc, not Role).
    let role_name = role_attr(&parsed, "role_name");
    match role_name {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(
                path.binding(),
                "upstream",
                "caller's `upstream.arn` must remain a ResourceRef with binding=upstream"
            );
            assert_eq!(path.attribute(), "arn");
        }
        other => panic!("role_name should be a ResourceRef, got {other:?}"),
    }
}

#[test]
fn test_load_module_directory_merge_order_is_deterministic() {
    // Merged vectors must be ordered by file path so that downstream
    // first-match-wins lookups (hover, completion, diagnostics) do not
    // depend on filesystem iteration order.
    let tmp_dir = std::env::temp_dir().join("carina_test_load_module_merge_order");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // Create files out of lexicographic order to make the sort observable.
    fs::write(tmp_dir.join("z_last.crn"), "arguments {\n  c: String\n}\n").unwrap();
    fs::write(tmp_dir.join("a_first.crn"), "arguments {\n  a: String\n}\n").unwrap();
    fs::write(
        tmp_dir.join("m_middle.crn"),
        "arguments {\n  b: String\n}\n",
    )
    .unwrap();

    let parsed = load_module(&tmp_dir).expect("module should load");
    let names: Vec<&str> = parsed.arguments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a", "b", "c"],
        "arguments must be merged in sorted filename order"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

/// Regression for #2549: a `list(String)` argument forwarded unchanged
/// from an outer (usecase) module to an inner module call must type-check.
#[test]
fn test_list_argument_passthrough_between_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Inner module: declares a list(String) argument.
    let inner_dir = tmp.path().join("modules/github_oidc");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  github_repo:      String
  subject_patterns: list(String) = ["repo:${github_repo}:*"]
}

let role = awscc.iam.Role {
  role_name = github_repo
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    // Outer (usecase) module: receives the same list(String) and forwards
    // it unchanged to the inner module.
    let outer_dir = tmp.path().join("usecases/bootstrap");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  github_repo:      String
  subject_patterns: list(String) = [
    'repo:carina-rs/infra:ref:refs/heads/main',
    'repo:carina-rs/infra:pull_request',
  ]
}

let github = use {
  source = '../../modules/github_oidc'
}

let bootstrap = github {
  github_repo      = github_repo
  subject_patterns = subject_patterns
}
"#,
    )
    .unwrap();

    // Caller of the usecase.
    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let bootstrap = use {
  source = '../usecases/bootstrap'
}

let prod = bootstrap {
  github_repo = 'carina-rs/infra'
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir)
        .expect("list(String) should flow through usecase to inner module");
}

/// Real infra shape: `let subjects = subject_patterns` rebinding before
/// pass-through. The forwarded value's binding name is `subjects`, not
/// `subject_patterns`, so the arg-table lookup must follow the alias.
#[test]
fn test_list_passthrough_through_let_alias() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/github_oidc");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  subject_patterns: list(String) = [
    'repo:carina-rs/infra:ref:refs/heads/main',
  ]
}

let github = use {
  source = '../../modules/github_oidc'
}

let subjects = subject_patterns
let bound = github {
  patterns = subjects
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir)
        .expect("list(String) via let alias should pass-through to inner module");
}

/// Same shape as the list-pass-through test but for `map(String)`. The fix
/// has to cover every container type, not only list — root cause is shared.
#[test]
fn test_map_argument_passthrough_between_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  tags: map(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  tags: map(String) = { env = 'prod' }
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  tags = tags
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir)
        .expect("map(String) should flow through usecase to inner module");
}

/// The checked-graph gate runs BindingIndex before expansion, including when a
/// product command has skipped provider/resource validation.
#[test]
fn test_mismatched_argument_passthrough_is_rejected_before_expansion() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  s: String
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  patterns = s
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  s = 'hello'
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let error = resolve_modules(parsed, &root_dir)
        .expect_err("the checked graph must reject String -> list(String)");
    assert!(
        error
            .to_string()
            .contains("cannot assign String to 'patterns'"),
        "{error}"
    );
}

/// Cross-scalar forwarding is rejected by the same BindingIndex authority.
#[test]
fn test_int_argument_forwarded_to_string_arg_is_rejected_before_expansion() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  count: Int
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  name = count
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  count = 3
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let error = resolve_modules(parsed, &root_dir)
        .expect_err("the checked graph must reject Int -> String");
    assert!(
        error.to_string().contains("cannot assign Int to 'name'"),
        "{error}"
    );
}

#[test]
fn integer_arithmetic_crosses_a_module_argument_boundary_as_typed_int() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/child");
    fs::create_dir_all(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments { count: Int }
let target = mock.test.Target { count = count }
"#,
    )
    .expect("module source");

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).expect("root directory");
    let root_source = r#"
let source = mock.test.Source {}
let child = use { source = "../modules/child" }
let instance = child { count = source.count + 1 }
"#;
    fs::write(root_dir.join("main.crn"), root_source).expect("root source");

    let mut config = ProviderContext::default();
    for resource_type in ["test.Source", "test.Target"] {
        config
            .resource_types
            .insert(("mock".to_string(), resource_type.to_string()));
    }
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Source")
            .attribute(AttributeSchema::provider_populated("count", AttributeType::int()).build())
            .validate()
            .expect("source schema"),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Target")
            .attribute(AttributeSchema::required("count", AttributeType::int()).build())
            .validate()
            .expect("target schema"),
    );

    let parsed = crate::parser::parse(root_source, &config).expect("root parse");
    let parsed = resolve_modules_with_config(parsed, &root_dir, &config, &schemas)
        .expect("typed arithmetic module argument must expand");
    let count = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("instance.target"))
        .and_then(|resource| resource.get_attr("count"))
        .expect("expanded target count");
    assert!(
        matches!(count, Value::Deferred(DeferredValue::IntegerArithmetic(_))),
        "module substitution must preserve typed arithmetic, got {count:?}",
    );
}

/// Nested container declarations are checked by BindingIndex before expansion.
#[test]
fn test_list_inner_element_mismatch_is_rejected_before_expansion() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  numbers: list(Int)
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  patterns = numbers
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  numbers = [1, 2, 3]
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let error = resolve_modules(parsed, &root_dir)
        .expect_err("the checked graph must reject list(Int) -> list(String)");
    assert!(
        error
            .to_string()
            .contains("cannot assign List<Int> to 'patterns'"),
        "{error}"
    );
}

fn resolve_ref_forwarding_fixture(inner_type: &str) -> Result<(), ModuleError> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let outer_dir = tmp.path().join("outer");
    let inner_dir = outer_dir.join("inner");
    fs::create_dir_all(&inner_dir).expect("nested module directories");
    fs::write(
        inner_dir.join("main.crn"),
        format!("arguments {{\n  x: {inner_type}\n}}\n"),
    )
    .expect("inner module");
    fs::write(
        outer_dir.join("main.crn"),
        r#"let inner = use { source = './inner' }
arguments {
  a: mock.test.resource
}
inner {
  x = a
}
"#,
    )
    .expect("outer module");
    fs::write(
        tmp.path().join("main.crn"),
        r#"let r = mock.test.resource { name = 'r' }
let outer = use { source = './outer' }
outer {
  a = r
}
"#,
    )
    .expect("root configuration");

    let mut config = ProviderContext::default();
    config
        .resource_types
        .insert(("mock".to_string(), "test.resource".to_string()));
    let content = fs::read_to_string(tmp.path().join("main.crn")).expect("root source");
    let parsed = crate::parser::parse(&content, &config).expect("root configuration must parse");
    resolve_modules_with_config(
        parsed,
        tmp.path(),
        &config,
        &crate::schema::SchemaRegistry::new(),
    )?;
    Ok(())
}

/// A declared resource reference is a typed handle for boundary checking even
/// though its runtime transport remains the resource id string (#3803).
#[test]
fn ref_argument_forwarded_to_scalar_is_rejected_before_expansion() {
    let error = resolve_ref_forwarding_fixture("Int")
        .expect_err("a resource handle must not flow into an Int argument");
    assert!(
        error
            .to_string()
            .contains("cannot assign mock.test.resource to 'x'")
            && error.to_string().contains("expected Int"),
        "{error}"
    );
}

#[test]
fn ref_argument_forwarded_to_string_is_rejected_as_a_bare_handle() {
    let error = resolve_ref_forwarding_fixture("String")
        .expect_err("a bare handle cannot cross into a String parameter");
    assert!(
        error.to_string().contains("'a' is a resource, not a value"),
        "{error}",
    );
}

#[test]
fn ref_argument_forwarded_to_ref_remains_typed() {
    resolve_ref_forwarding_fixture("mock.test.resource")
        .expect("Ref-typed arguments remain handles across module boundaries");
}

#[test]
fn ref_typed_module_argument_field_access_uses_the_caller_binding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("child");
    fs::create_dir_all(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("main.crn"),
        r#"arguments {
  source: mock.test.resource
}
let consumer = mock.test.consumer {
  parent = source.name
}
"#,
    )
    .expect("module source");
    let root_source = r#"let source = mock.test.resource { name = 'root' }
let child = use { source = './child' }
let instance = child { source = source }
"#;
    fs::write(tmp.path().join("main.crn"), root_source).expect("root source");

    let mut config = ProviderContext::default();
    config
        .resource_types
        .insert(("mock".to_string(), "test.resource".to_string()));
    let parsed = crate::parser::parse(root_source, &config).expect("root parse");
    let parsed = resolve_modules_with_config(
        parsed,
        tmp.path(),
        &config,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("module expansion");

    let value = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("instance.consumer"))
        .and_then(|resource| resource.get_attr("parent"))
        .expect("expanded consumer parent");
    assert!(
        matches!(
            value,
            Value::Deferred(DeferredValue::ResourceRef { path })
                if path.binding() == "source" && path.attribute() == "name"
        ),
        "module argument access must retain the caller binding, got {value:?}",
    );
}

#[test]
fn ref_typed_module_argument_used_bare_is_rejected_in_module_context() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("child");
    fs::create_dir_all(&module_dir).expect("module directory");
    fs::write(
        module_dir.join("main.crn"),
        r#"arguments {
  source: mock.test.resource
}
let consumer = mock.test.consumer {
  parent = source
}
"#,
    )
    .expect("module source");
    let root_source = r#"let source = mock.test.resource { name = 'root' }
let child = use { source = './child' }
let instance = child { source = source }
"#;

    let mut config = ProviderContext::default();
    config
        .resource_types
        .insert(("mock".to_string(), "test.resource".to_string()));
    let parsed = crate::parser::parse(root_source, &config).expect("root parse");
    let error = resolve_modules_with_config(
        parsed,
        tmp.path(),
        &config,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect_err("bare module handle use must fail");
    let message = error.to_string();
    assert!(
        message.contains("'source' is a resource handle, not a value")
            && message.contains("line 5"),
        "unexpected error: {message}",
    );
}

/// Three-deep pass-through: outer → middle → inner all forwarding the
/// same `list(String)` arg. Each enclosing scope contributes its own
/// arg signature; the chain must hold across both module-call hops.
#[test]
fn test_list_passthrough_three_levels_deep() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let middle_dir = tmp.path().join("modules/middle");
    fs::create_dir_all(&middle_dir).unwrap();
    fs::write(
        middle_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let inner = use {
  source = '../inner'
}

let bound = inner {
  patterns = patterns
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let middle = use {
  source = '../../modules/middle'
}

let bound = middle {
  patterns = patterns
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  patterns = ['a', 'b']
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(parsed, &root_dir).expect("3-deep list pass-through should resolve");
}

// carina-rs/carina#2611: closed-set string types in `arguments`.
// These tests cover the *caller* side — when a module is declared
// with `env: 'dev' | 'prod' = 'dev'`, the call site
// `mod_call { env = 'dpv' }` must be rejected by typecheck. Without
// the StringLiteral/Union arms in `validation::typecheck`, the
// typo would slip through to plan because the parser's pre-#2611
// fallback was `String`.

fn create_module_with_environment_union() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        data_sources: vec![],
        compositions: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "environment".to_string(),
            static_type: known(TypeExpr::Union(vec![
                TypeExpr::StringLiteral("dev".to_string()),
                TypeExpr::StringLiteral("prod".to_string()),
            ])),
            default: Some(Value::Concrete(ConcreteValue::String("dev".into()))),
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: vec![],
        deferred_for_phase: ParsedDeferredForPhase::default(),
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
    }
}

#[test]
fn caller_side_typo_against_string_literal_union_is_rejected() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "env_module".to_string(),
            create_module_with_environment_union(),
        );
        r
    };

    let call = ModuleCall::test("env_module".to_string(), Some("my_env".to_string()), {
        let mut args = HashMap::new();
        // Typo: 'dpv' is outside the declared 'dev' | 'prod' union.
        args.insert(
            "environment".to_string(),
            Value::Concrete(ConcreteValue::String("dpv".into())),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "my_env");
    assert!(
        matches!(
            result.as_ref().map_err(|error| error.root_cause()),
            Err(ModuleError::InvalidArgumentType { .. })
        ),
        "Caller passing 'dpv' to `'dev' | 'prod'` must be rejected — \
         carina-rs/carina#2611. Got: {:?}",
        result
    );
}

#[test]
fn caller_side_value_in_string_literal_union_is_accepted() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "env_module".to_string(),
            create_module_with_environment_union(),
        );
        r
    };

    let call = ModuleCall::test("env_module".to_string(), Some("my_env".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "environment".to_string(),
            Value::Concrete(ConcreteValue::String("prod".into())),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "my_env");
    assert!(
        result.is_ok(),
        "Caller passing 'prod' (in the declared union) must be accepted. Got: {:?}",
        result
    );
}

#[test]
fn escaped_string_literal_union_matches_decoded_module_arguments() {
    let module = crate::parser::parse(
        r#"
arguments {
  environment: 'it\'s' | 'b'
}
"#,
        &ProviderContext::default(),
    )
    .expect("module argument type should parse");
    let mut resolver = ModuleResolver::new(".");
    resolver
        .imported_modules
        .insert("env_module".to_string(), module);

    let parse_value = |literal: &str| {
        let source = format!("let value = {literal}\n");
        crate::parser::parse(&source, &ProviderContext::default())
            .expect("argument value should parse")
            .variables
            .get("value")
            .expect("value binding should exist")
            .clone()
    };
    let call = |value| {
        ModuleCall::test(
            "env_module".to_string(),
            Some("instance".to_string()),
            module_arguments([("environment".to_string(), value)]),
        )
    };

    for literal in [r"'it\'s'", r#""it's""#] {
        let result = resolver.expand_module_call(&call(parse_value(literal)), "instance");
        assert!(
            result.is_ok(),
            "{literal} should satisfy the decoded type: {result:?}"
        );
    }

    let error = resolver
        .expand_module_call(&call(parse_value("'x'")), "instance")
        .expect_err("a value outside the literal union must be rejected");
    assert!(
        error
            .to_string()
            .contains(r#"expected "it's" | 'b', got string 'x'"#),
        "the decoded apostrophe must use the shared renderer: {error}"
    );
}

/// Acceptance test for #2815 / #2817. An `arguments {}` block declared in
/// `main.crn` must be visible to identifier references in *sibling* `.crn`
/// files in the same module directory. Before #2817, the per-file
/// `ParseContext` could not see sibling-defined symbols, so `${env}` in
/// `role.crn` lowered to a literal `Value::Concrete(ConcreteValue::String("env"))` — `role_name`
/// rendered as `"test-role-env"` instead of `"test-role-dev"`. The
/// directory-aware parse pipeline seeds every file's `ParseContext` with
/// the union of declared names from all sibling files, so the normal
/// `ctx.get_variable` path resolves the reference uniformly.
#[test]
fn test_arguments_visible_to_sibling_crn_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/usecase");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  env: String
}
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("role.crn"),
        r#"
let role = awscc.iam.Role {
  role_name = "test-role-${env}"
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let uc = use { source = '../modules/usecase' }
let r  = uc { env = 'dev' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let parsed = resolve_modules(parsed, &root_dir).expect("resolve_modules should succeed");

    assert_eq!(
        role_names(&parsed),
        ["test-role-dev".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
        "argument `env` declared in `main.crn` must substitute into `${{env}}` \
         interpolation in sibling `role.crn`",
    );
}

/// Regression for the seeding-vs-local-duplicate corner of #2817.
///
/// Inside the directory-aware Pass-2 parse, every binding name from
/// the merged Pass-1 result is seeded into the per-file
/// `ParseContext`. The seeded name MUST NOT mask a real in-file
/// duplicate (`arguments { foo }` and `let foo = ...` in the same
/// file). The shadow logic is to drop the seed mark the moment a
/// local declaration overwrites it; if a *second* local declaration
/// then appears, the regular duplicate-binding error must still
/// trigger.
#[test]
fn test_local_duplicate_still_detected_under_seeded_parse() {
    // The duplicate is `let foo = "y"` colliding with `arguments { foo }`,
    // which both register `foo` as a binding. Seeding (a sibling file
    // with `let foo = ...`) primed the seed mark; the `arguments` block
    // must drop it so the subsequent `let foo` is still a duplicate.
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/dup");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  foo: String
}

let foo = "y"
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("sibling.crn"),
        "# also has foo via let later\nlet foo_other = \"z\"\n",
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let m = use { source = '../modules/dup' }
m { foo = 'x' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let err = resolve_modules(parsed, &root_dir).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("DuplicateBinding") && msg.contains("foo"),
        "expected DuplicateBinding for `foo`; got {msg}"
    );
}

/// carina#3126 / PR-A structural invariant: a `for` over an
/// unresolved iterable declared **inside a module** must survive
/// `expand_module_call` and reach the caller. Before the single
/// merge surface, the expansion result carried only `resources` +
/// `wait_bindings`, so a module-internal `deferred_for_expressions`
/// entry was silently dropped — invisible to validate/plan/apply
/// (the real `carina-rs/infra usecases/registry/acm.crn` case).
///
/// PR-A only asserts the entry is *propagated* (no longer dropped).
/// Instance-prefixing of its binding-name fields is PR-B; this test
/// deliberately does not assert prefixing so it stays green across
/// the PR-A → PR-B boundary.
#[test]
fn test_expand_module_call_propagates_deferred_for_expressions() {
    use crate::parser::{DeferredForTarget, ForBinding};

    let module = {
        let mut m = create_module_with_intra_refs();
        // The loop iterates `cert.domain_validation_options`, so `cert`
        // must be a real module-internal binding for the
        // intra-module-conditional prefix to apply (same condition as
        // `rewrite_intra_module_refs`). Add it so this unit faithfully
        // models the real `let cert = aws.acm.Certificate { … }` case.
        m.resources.push(Resource {
            id: ResourceId::with_identity("acm.Certificate", "cert"),
            attributes: HashMap::new().into_iter().collect(),
            directives: Directives::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("cert".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        });
        m.parsed_deferred_for_expressions_mut()
            .push(parsed_deferred_resource(DeferredForExpression {
                file: None,
                line: 7,
                header: "for _, opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.RecordSet".to_string(),
                attributes: vec![],
                target: DeferredForTarget::Bound {
                    binding: "_domain_validation_options".to_string(),
                },
                iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
                binding: ForBinding::Map("_".to_string(), "opt".to_string()),
                template_resource: Resource {
                    id: ResourceId::with_identity("route53.RecordSet", "placeholder"),
                    attributes: {
                        // The loop body references the module-internal
                        // `cert` binding — the part of the template
                        // treatment that actually SURVIVES materialization
                        // (`substitute_attrs` does not re-run ref-rewrite).
                        // PR-B must prefix this to `r.cert` so the
                        // generated RecordSet wires to the prefixed module
                        // certificate; without it the loop body silently
                        // dangles. Round-3 review test gap.
                        let mut a = IndexMap::new();
                        a.insert(
                            "validated_cert".to_string(),
                            Value::resource_ref("cert".to_string(), "arn".to_string(), vec![]),
                        );
                        a
                    },
                    directives: Directives::default(),
                    resolved_ignore_changes: Default::default(),
                    prefixes: HashMap::new(),
                    binding: None,
                    dependency_bindings: BTreeSet::new(),
                    module_source: None,
                    default_tag_keys: Default::default(),
                    loop_provenance: Default::default(),
                    quoted_string_attrs: std::collections::HashSet::new(),
                },
            }));
        m
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("registry".to_string(), module);
        r
    };

    let call = ModuleCall::test("registry".to_string(), Some("r".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver.expand_module_call(&call, "r").unwrap();

    // The structural invariant: the module-internal deferred-for
    // reached the caller instead of vanishing at the expansion
    // boundary.
    assert_eq!(
        expanded.parsed_deferred_for_expressions().len(),
        1,
        "module-internal deferred-for must survive expand_module_call (carina#3126)"
    );
    let d = &expanded.parsed_deferred_for_expressions()[0];
    assert!(d.root_bindings().contains("r.cert"));
    // carina#3126 PR-B: binding-name fields are instance-prefixed so
    // the loop-generated resources are isolated per module instance
    // and the iterable resolves against the (now prefixed) module
    // resource. Call instance is "r".
    //
    // `binding_name` is the generated-resource address prefix →
    // prefixed unconditionally (mirrors `Resource.binding`).
    assert_eq!(
        d.target.bound_binding(),
        Some("r._domain_validation_options"),
        "PR-B must instance-prefix binding_name"
    );
    // `cert` is a module-internal `let cert` binding → prefixed
    // (same caller-collision guard `rewrite_intra_module_refs`
    // applies to a `ResourceRef` head; a caller-shared name would
    // NOT be prefixed — see prefix_parsed_deferred_for_expression).
    assert_eq!(
        d.root_bindings(),
        std::collections::BTreeSet::from(["r.cert".to_string()]),
        "PR-B must instance-prefix the intra-module iterable root"
    );

    // The part of the template treatment that SURVIVES materialization
    // (`substitute_attrs` does not re-run ref-rewrite): the loop body's
    // intra-module ref must be prefixed, and module_source stamped, so
    // the generated RecordSet wires to the prefixed module certificate.
    // Round-3 review found this load-bearing path had zero coverage.
    let DeferredForBody::Resource(template_resource) = &d.body else {
        panic!("deferred test body must remain a resource")
    };
    let validated_cert = template_resource
        .attributes
        .get("validated_cert")
        .expect("template attr preserved");
    match validated_cert {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(
                path.binding(),
                "r.cert",
                "loop body's intra-module ref must be instance-prefixed"
            );
            assert_eq!(path.attribute(), "arn");
        }
        other => panic!("expected prefixed ResourceRef, got {other:?}"),
    }
    assert!(
        matches!(
            template_resource.module_source,
            Some(crate::resource::ModuleSource::Module(ref source)) if source.instance() == "r"
        ),
        "template_resource must carry the module instance source; got: {:?}",
        template_resource.module_source
    );
}

#[test]
fn module_deferred_template_instantiated_twice_has_distinct_pending_ids() {
    use crate::parser::{DeferredForTarget, ForBinding};

    let mut module = create_test_module_with_anonymous_resource();
    module.resources.clear();
    module
        .parsed_deferred_for_expressions_mut()
        .push(parsed_deferred_resource(DeferredForExpression {
            file: None,
            line: 1,
            header: "for _, account in accounts.items".to_string(),
            resource_type: "awscc.sso.Assignment".to_string(),
            attributes: Vec::new(),
            target: DeferredForTarget::Bound {
                binding: "assignments".to_string(),
            },
            iterable: Value::resource_ref("accounts", "items", Vec::new()),
            binding: ForBinding::Map("_".to_string(), "account".to_string()),
            template_resource: Resource::pending_with_provider("awscc", "sso.Assignment", None),
        }));

    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver
            .imported_modules
            .insert("assignments".to_string(), module);
        resolver
    };
    let call = ModuleCall::test("assignments".to_string(), None, HashMap::new());

    let first_expanded = resolver.expand_module_call(&call, "first").unwrap();
    let first = first_expanded
        .into_parsed_deferred_for_expressions()
        .into_iter()
        .next()
        .unwrap();
    let DeferredForBody::Resource(first) = first.body else {
        panic!("deferred test body must remain a resource")
    };
    let second_expanded = resolver.expand_module_call(&call, "second").unwrap();
    let second = second_expanded
        .into_parsed_deferred_for_expressions()
        .into_iter()
        .next()
        .unwrap();
    let DeferredForBody::Resource(second) = second.body else {
        panic!("deferred test body must remain a resource")
    };

    assert_eq!(first.id.identity_str(), None);
    assert_eq!(second.id.identity_str(), None);
    assert_ne!(first.id, second.id);
}

/// carina#3126 PR-B negative case: an `iterable_binding` that is NOT a
/// module-internal binding (a caller-passed / argument binding that
/// merely shares a name) must **not** be instance-prefixed — the same
/// caller-collision guard `rewrite_intra_module_refs` applies to a
/// `ResourceRef` head. This locks the `else` arm of the conditional
/// and is the safety property the deliberate divergence from the
/// design's unconditional-prefix table protects. Without this test a
/// future "simplification" back to unconditional prefixing would pass
/// CI while silently breaking caller-passed iterables.
#[test]
fn deferred_for_iterable_binding_not_prefixed_when_not_module_internal() {
    use crate::parser::{DeferredForTarget, ForBinding};

    let module = {
        let mut m = create_module_with_intra_refs();
        // No module resource/wait is bound `accounts` — it is meant to
        // come from the caller (e.g. an argument), so it is NOT in
        // `intra_module_bindings` and must survive unprefixed.
        m.parsed_deferred_for_expressions_mut()
            .push(parsed_deferred_resource(DeferredForExpression {
                file: None,
                line: 3,
                header: "for _, a in accounts.list".to_string(),
                resource_type: "awscc.sso.Assignment".to_string(),
                attributes: vec![],
                target: DeferredForTarget::Bound {
                    binding: "_list".to_string(),
                },
                iterable: Value::resource_ref("accounts", "list", Vec::new()),
                binding: ForBinding::Map("_".to_string(), "a".to_string()),
                template_resource: Resource {
                    id: ResourceId::with_identity("sso.Assignment", "placeholder"),
                    attributes: HashMap::new().into_iter().collect(),
                    directives: Directives::default(),
                    resolved_ignore_changes: Default::default(),
                    prefixes: HashMap::new(),
                    binding: None,
                    dependency_bindings: BTreeSet::new(),
                    module_source: None,
                    default_tag_keys: Default::default(),
                    loop_provenance: Default::default(),
                    quoted_string_attrs: std::collections::HashSet::new(),
                },
            }));
        m
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("net".to_string(), module);
        r
    };

    let call = ModuleCall::test("net".to_string(), Some("prod".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "cidr".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        module_arguments(args)
    });

    let expanded = resolver.expand_module_call(&call, "prod").unwrap();
    let d = &expanded.parsed_deferred_for_expressions()[0];
    // `binding_name` is a synthesized address prefix → always prefixed.
    assert_eq!(d.target.bound_binding(), Some("prod._list"));
    // `accounts` is NOT module-internal → must stay UNPREFIXED
    // (caller-collision guard). A regression to unconditional
    // prefixing would make this `prod.accounts` and fail here.
    assert_eq!(
        d.root_bindings(),
        std::collections::BTreeSet::from(["accounts".to_string()]),
        "a non-module-internal iterable binding must NOT be prefixed"
    );
}

/// carina#3126 / PR-A end-to-end: `wait_bindings` AND
/// `deferred_for_expressions` declared inside an **imported module
/// directory** must survive the *full* `resolve_modules` pipeline —
/// i.e. through the new single merge surface (`relabel_export_phase`
/// + `merge_parsed_file`), not just `expand_module_call` in isolation.
///
/// The other module tests call `expand_module_call` directly and so
/// bypass the merge surface this PR introduces; a classified-but-
/// mis-merged field (destructured then forgotten in the rebuild)
/// still compiles and those tests would not catch it. This test
/// drives `resolve_modules` so the merge path is actually exercised
/// — the [[feedback_unit_test_path_is_not_apply_path]] guard.
///
/// Mirrors the real `carina-rs/infra usecases/registry` shape: a
/// module with a `wait` (carina#3061) and a `for` over a provider-read
/// attribute (carina#3126), consumed via a module call.
#[test]
fn resolve_modules_propagates_module_wait_and_deferred_for_through_merge() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let module_dir = tmp.path().join("modules/registry");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  domain_name: String
}

let cert = aws.acm.Certificate {
  domain_name       = domain_name
  validation_method = dns
}

for _, opt in cert.domain_validation_options {
  aws.route53.RecordSet {
    hosted_zone_id   = "Z123"
    name             = opt.resource_record.name
    type             = cname
    ttl              = 300
    resource_records = [opt.resource_record.value]
  }
}

let cert_issued = wait cert {
  until   = cert.status == "ISSUED"
  timeout = 75min
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let registry = use {
  source = "../modules/registry"
}

let r = registry {
  domain_name = "registry-dev.example.com"
}
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let parsed = crate::parser::parse(root_body, &ProviderContext::default()).unwrap();
    let parsed = resolve_modules(parsed, &root_dir).expect("resolve_modules should succeed");

    // carina#3126: the module-internal deferred-for survived the full
    // resolve_modules pipeline (through relabel_export_phase +
    // merge_parsed_file), not just expand_module_call.
    assert_eq!(
        parsed.deferred_for_expressions().len(),
        1,
        "module-internal deferred-for must reach the caller through the \
         single merge surface (carina#3126); got: {:?}",
        parsed.deferred_for_expressions()
    );
    let d = &parsed.deferred_for_expressions()[0];
    assert_eq!(d.resource_type, "aws.route53.RecordSet");
    assert!(
        d.reference_candidates()
            .contains("r.cert.domain_validation_options")
    );
    // carina#3126 PR-B: through the full resolve_modules pipeline the
    // deferred-for's iterable resolves against the prefixed module `cert`.
    // carina#4109: because this loop has no `let` binding, its generated
    // resources stay anonymous; only the migration-only legacy base receives
    // the module-instance prefix.
    let crate::parser::DeferredForTarget::Unbound { template } = &d.target else {
        panic!("unbound module loop must remain an unbound deferred target")
    };
    assert_eq!(
        template.legacy_base().exact_state_name(),
        "r._domain_validation_options"
    );
    assert_eq!(
        d.root_bindings(),
        std::collections::BTreeSet::from(["r.cert".to_string()])
    );

    // carina#3061: the module-internal wait binding still survives the
    // same pipeline and is instance-prefixed (the existing invariant,
    // now exercised *through* the shared merge, not bypassing it).
    assert_eq!(
        parsed.wait_bindings.len(),
        1,
        "module-internal wait must reach the caller through the single \
         merge surface (carina#3061); got: {:?}",
        parsed.wait_bindings
    );
    assert_eq!(parsed.wait_bindings[0].binding, "r.cert_issued");
    assert_eq!(parsed.wait_bindings[0].target, "r.cert");
}

#[test]
fn resolve_modules_rebuilds_deferred_for_from_typed_parser_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let parsed = crate::parser::parse(
        r#"
let source = mock.test.resource { name = "source" }

let first = for item in source.first {
  mock.test.resource { name = item comment = "first" }
}

let second = for item in source.second {
  mock.test.resource { name = item comment = "second" }
}
"#,
        &ProviderContext::default(),
    )
    .expect("deferred resource loops parse");

    let parsed = resolve_modules(parsed, tmp.path()).expect("deferred loops finalize");

    let comments = parsed
        .deferred_for_expressions()
        .iter()
        .map(|deferred| deferred.template_resource.get_attr("comment"))
        .collect::<Vec<_>>();
    assert_eq!(
        comments,
        [
            Some(&Value::Concrete(ConcreteValue::String("first".into()))),
            Some(&Value::Concrete(ConcreteValue::String("second".into()))),
        ],
        "finalization must derive order and templates from the typed list alone"
    );
}

#[test]
fn finalize_rejects_undefined_iterable_for_managed_resource_body() {
    let fixture = tempfile::tempdir().expect("undefined managed iterable fixture");
    let source_file = fixture.path().join("main.crn");
    let source = r#"let items = ["one"]
let resources = for item in itmes {
  mock.test.resource { name = item }
}
"#;
    fs::write(&source_file, source).expect("write undefined iterable fixture");
    let loaded = crate::config_loader::load_configuration_with_config(
        fixture.path(),
        &ProviderContext::default(),
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("undefined iterable remains deferred until finalization");
    let identifier_scope_errors = crate::parser::check_identifier_scope(&loaded.parsed);
    assert!(matches!(
        identifier_scope_errors.as_slice(),
        [crate::parser::ParseError::Located { error, .. }]
            if matches!(error.as_ref(), crate::parser::ParseError::UndefinedDeferredIterable { name, .. } if name == "itmes")
    ));

    let error = super::resolve_modules_with_providerless_config(
        loaded.parsed,
        fixture.path(),
        &ProviderContext::default(),
        &crate::schema::SchemaRegistry::new(),
    )
    .expect_err("finalization must reject an undefined managed-resource iterable");
    let ModuleError::ParseInFile { path, error } = error.root_cause() else {
        panic!("expected a file-owned parse error, got {error:?}");
    };
    assert_eq!(path, &source_file);
    assert!(matches!(
        error.as_ref(),
        crate::parser::ParseError::UndefinedDeferredIterable {
            name,
            line: 2,
            span,
            ..
        } if name == "itmes" && span.start_line() == 2 && span.end_line() == 4
    ));
    assert!(error.to_string().contains("Did you mean `items`"));
}

#[test]
fn finalize_reports_every_undefined_iterable_for_managed_resource_bodies() {
    let parsed = crate::parser::parse(
        r#"let first = for item in frist_items {
  mock.test.resource { name = item }
}
let second = for item in secnd_items {
  mock.test.resource { name = item }
}
"#,
        &ProviderContext::default(),
    )
    .expect("both undefined iterables remain deferred until finalization");

    let error = super::resolve_modules_providerless(parsed, std::path::Path::new("."))
        .expect_err("finalization must reject both undefined iterables");
    let message = error.display_relative_to(std::path::Path::new("."));

    assert!(
        message.contains("Undefined identifier `frist_items`"),
        "{message}"
    );
    assert!(
        message.contains("Undefined identifier `secnd_items`"),
        "{message}"
    );
}

#[test]
fn resolve_modules_with_config_populates_and_prefixes_directory_module_dependencies() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let module_dir = tmp.path().join("modules/registry");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("arguments.crn"),
        r#"
arguments {
  label: String
}

attributes {
  target_name = target.role_name
}
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("resources.crn"),
        r#"
let target = mock.iam.Role {
  role_name = "module-target"
}

let consumer = mock.iam.Role {
  role_name   = "module-consumer"
  description = "${target.role_name}:${label}"
}
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("reads.crn"),
        r#"
let roles = read mock.iam.Roles {
  name_regex = "^${target.role_name}$"
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let registry = use {
  source = "../modules/registry"
}

let registry_publish = registry {
  label = "caller-label"
}
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let config = ProviderContext::default();
    let parsed = crate::parser::parse(root_body, &config).expect("root must parse");
    let parsed = resolve_modules_with_config(
        parsed,
        &root_dir,
        &config,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("production module resolution must succeed");

    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("registry_publish.consumer"))
        .expect("expanded module consumer must exist");
    assert!(
        consumer
            .dependency_bindings
            .contains("registry_publish.target"),
        "module resource dependency must be populated and prefixed: {:?}",
        consumer.dependency_bindings,
    );
    assert!(
        !consumer
            .dependency_bindings
            .contains("registry_publish.label"),
        "module arguments must not be mistaken for intra-module resources: {:?}",
        consumer.dependency_bindings,
    );
    assert_eq!(
        consumer.attributes.get("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "module-target:caller-label".into(),
        ))),
        "module-local references must resolve before argument substitution"
    );

    let roles = parsed
        .data_sources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("registry_publish.roles"))
        .expect("expanded module data source must exist");
    assert_eq!(
        roles.dependency_bindings,
        BTreeSet::from(["registry_publish.target".to_string()]),
        "module data-source dependency must be populated and prefixed"
    );
    assert_eq!(
        roles.attributes.get("name_regex"),
        Some(&Value::Concrete(ConcreteValue::String(
            "^module-target$".into(),
        ))),
        "module data-source input must resolve through the production loader"
    );

    let composition = parsed
        .compositions
        .iter()
        .find(|composition| composition.binding.as_deref() == Some("registry_publish"))
        .expect("module output composition must exist");
    let target_name = composition
        .signature
        .attributes
        .get("target_name")
        .expect("module output must exist");
    let path = target_name.forwarded_path().unwrap_or_else(|| {
        panic!("module output must remain a deferred forwarded path: {target_name:?}")
    });
    assert_eq!(path.binding(), "registry_publish.target");
}

#[test]
fn nested_directory_module_dependencies_follow_both_instance_prefixes() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
attributes {
  target_name = target.role_name
}
"#,
    )
    .unwrap();
    fs::write(
        inner_dir.join("resources.crn"),
        r#"
let target = mock.iam.Role {
  role_name = "module-target"
}

let consumer = mock.iam.Role {
  role_name   = "module-consumer"
  description = target.role_name
}
"#,
    )
    .unwrap();
    fs::write(
        inner_dir.join("reads.crn"),
        r#"
let roles = read mock.iam.Roles {
  name_regex = target.role_name
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("modules/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
attributes {
  target_name = inner.target_name
}
"#,
    )
    .unwrap();
    fs::write(
        outer_dir.join("inner.crn"),
        r#"
let inner_module = use {
  source = "../inner"
}

let inner = inner_module {}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    let root_body = r#"
let outer_module = use {
  source = "../modules/outer"
}

let outer = outer_module {}
"#;
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let config = ProviderContext::default();
    let parsed = crate::parser::parse(root_body, &config).expect("root must parse");
    let parsed = resolve_modules_with_config(
        parsed,
        &root_dir,
        &config,
        &crate::schema::SchemaRegistry::new(),
    )
    .expect("nested production module resolution must succeed");

    let target = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("outer.inner.target"))
        .expect("twice-expanded target must exist");
    assert_eq!(
        target.id.identity_str().expect("resolved identity"),
        "outer.inner.target"
    );

    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("outer.inner.consumer"))
        .expect("twice-expanded consumer must exist");
    assert_eq!(
        consumer.dependency_bindings,
        BTreeSet::from(["outer.inner.target".to_string()]),
        "the outer pass must prefix the inner dependency once, matching the final target binding"
    );

    let roles = parsed
        .data_sources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("outer.inner.roles"))
        .expect("twice-expanded data source must exist");
    assert_eq!(
        roles.dependency_bindings,
        BTreeSet::from(["outer.inner.target".to_string()]),
        "nested data-source dependency metadata must match the twice-expanded target binding"
    );
}

/// carina#3238: a `list(T)`-typed module argument must accept a bare
/// `ResourceRef` whose runtime value is a list (e.g. the `arns` output
/// of `read aws.iam.Roles`). Before the fix the `TypeExpr::List` arm of
/// the module-arg typecheck only accepted `ConcreteValue::List`, so
/// passing `roles.arns` was rejected with a misleading
/// `expected list(...)` error — forcing the user to wrap it in a
/// literal `[roles.arns[0]]` that silently drops every element past
/// index 0. The scalar arms (`String`, `Simple`, `Ref`, `SchemaType`)
/// already accepted `ResourceRef`; only the collection arms didn't.
#[test]
fn argument_type_list_accepts_resource_ref() {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "role_arns".to_string(),
        static_type: known(TypeExpr::List(Box::new(TypeExpr::Simple(
            "arn".to_string(),
        )))),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    let call = ModuleCall::test("test_module".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "role_arns".to_string(),
            Value::resource_ref("admin_access_roles", "arns", Vec::new()),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "a");
    assert!(
        result.is_ok(),
        "list(T) argument should accept ResourceRef (carina#3238); got {:?}",
        result
    );
}

/// carina#3238 sibling case: `map(T)` arguments must also accept a
/// `ResourceRef` whose runtime value is a map. Same root cause and same
/// fix as the list case — the collection arms were symmetric in
/// rejecting deferred refs.
#[test]
fn argument_type_map_accepts_resource_ref() {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "tags".to_string(),
        static_type: known(TypeExpr::Map(Box::new(TypeExpr::String))),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    let call = ModuleCall::test("test_module".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "tags".to_string(),
            Value::resource_ref("some_resource", "tags", Vec::new()),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "a");
    assert!(
        result.is_ok(),
        "map(T) argument should accept ResourceRef (carina#3238); got {:?}",
        result
    );
}

/// carina#3238 sibling case: `struct { ... }` arguments must also
/// accept a `ResourceRef`. Same root cause and same fix as List/Map —
/// the collection arms were symmetric in rejecting deferred refs.
#[test]
fn argument_type_struct_accepts_resource_ref() {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "options".to_string(),
        static_type: known(TypeExpr::Struct {
            fields: vec![("name".to_string(), TypeExpr::String)],
        }),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    let call = ModuleCall::test("test_module".to_string(), Some("a".to_string()), {
        let mut args = HashMap::new();
        args.insert(
            "options".to_string(),
            Value::resource_ref("some_resource", "options", Vec::new()),
        );
        module_arguments(args)
    });

    let result = resolver.expand_module_call(&call, "a");
    assert!(
        result.is_ok(),
        "struct argument should accept ResourceRef (carina#3238); got {:?}",
        result
    );
}

#[test]
fn argument_type_int_accepts_resource_ref_for_later_ref_type_validation() {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "count".to_string(),
        static_type: known(TypeExpr::Int),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut resolver = ModuleResolver::new(".");
        resolver
            .imported_modules
            .insert("test_module".to_string(), module);
        resolver
    };
    let call = ModuleCall::test(
        "test_module".to_string(),
        Some("a".to_string()),
        module_arguments([(
            "count".to_string(),
            Value::resource_ref("source", "count", Vec::new()),
        )]),
    );

    let result = resolver.expand_module_call(&call, "a");
    assert!(
        result.is_ok(),
        "ResourceRef type is checked by the BindingIndex pass; expansion must defer it: {result:?}"
    );
}

/// carina#3238 end-to-end: a multi-file directory fixture that mirrors
/// the real `infra-deploy` reproducer — a usecase module declares
/// `list(T)` and `map(T)` arguments, and the root caller feeds them
/// from a `read` data source's attributes (a list-typed and a
/// map-typed `ResourceRef`). Before the fix, `resolve_modules` failed
/// with `Invalid argument type ... expected list(...)`; the workaround
/// was `[xs[0]]` which silently dropped every element past index 0.
#[test]
fn list_and_map_args_accept_read_attribute_passthrough_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Usecase module: declares list(String) + map(String) arguments,
    // split across the multi-file shape used in carina-rs/infra (the
    // issue's real reproducer lived in `usecases/registry/infra-deploy`).
    let usecase_dir = tmp.path().join("usecases/infra_deploy");
    fs::create_dir_all(&usecase_dir).unwrap();
    fs::write(
        usecase_dir.join("arguments.crn"),
        r#"
arguments {
  sso_admin_role_arns: list(String)
  account_tags:        map(String)
}
"#,
    )
    .unwrap();
    fs::write(
        usecase_dir.join("main.crn"),
        r#"
let r = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    // Root caller: feeds a `read` data source's list-typed and map-typed
    // attributes directly into the usecase's list/map args. Pre-fix, the
    // module-arg typecheck rejected the bare ResourceRef.
    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let admin_access_roles = read aws.iam.Roles {
  path_prefix = '/aws-reserved/sso.amazonaws.com/'
  name_regex  = '^AWSReservedSSO_AdministratorAccess_[0-9a-f]{16}$'
}

let infra_deploy = use {
  source = '../usecases/infra_deploy'
}

let rd = infra_deploy {
  sso_admin_role_arns = admin_access_roles.arns
  account_tags        = admin_access_roles.tags
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let result = resolve_modules(parsed, &root_dir);
    assert!(
        result.is_ok(),
        "list(T)/map(T) usecase arguments must accept a `read`'s \
         list/map-typed attribute via bare ResourceRef (carina#3238); \
         got {:?}",
        result
    );
}

/// carina#3238: the error message for a true list-vs-scalar mismatch
/// must show the actual value, not just the expected type. The
/// previous wording (`expected list(aws.iam.Role.Arn)`) sent the
/// reporter hunting for an element-type mismatch when the actual cause
/// was a string value passed where a list was required. The fix surfaces
/// both the expected type and the offending value.
#[test]
fn argument_type_mismatch_error_shows_actual_value() {
    let msg = module_argument_type_error(
        "role_arns",
        TypeExpr::List(Box::new(TypeExpr::Simple("arn".to_string()))),
        Value::Concrete(ConcreteValue::String("arn:aws:iam::123:role/A".into())),
    );

    assert!(
        msg.contains("got string 'arn:aws:iam::123:role/A'"),
        "error should show the actual string value for a string-into-list mismatch (carina#3238); got: {msg}"
    );
}

fn module_argument_type_error(arg_name: &str, type_expr: TypeExpr, value: Value) -> String {
    module_argument_type_error_with_config(arg_name, type_expr, value, &ProviderContext::default())
}

fn module_argument_type_error_with_config(
    arg_name: &str,
    type_expr: TypeExpr,
    value: Value,
    config: &ProviderContext,
) -> String {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: arg_name.to_string(),
        static_type: known(type_expr),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut resolver = ModuleResolver::with_config(".", config);
        resolver
            .imported_modules
            .insert("web_tier".to_string(), module);
        resolver
    };

    let call = ModuleCall::test(
        "web_tier".to_string(),
        Some("web".to_string()),
        module_arguments([(arg_name.to_string(), value)]),
    );

    let result = resolver.expand_module_call(&call, "web");
    let Err(err) = result else {
        panic!("expected InvalidArgumentType error, got Ok");
    };
    err.to_string()
}

fn environment_type() -> TypeExpr {
    TypeExpr::Union(vec![
        TypeExpr::StringLiteral("dev".to_string()),
        TypeExpr::StringLiteral("stg".to_string()),
        TypeExpr::StringLiteral("prod".to_string()),
    ])
}

#[test]
fn argument_type_mismatch_error_shows_string_literal_value() {
    let value = Value::Concrete(ConcreteValue::String("production".into()));

    let msg = module_argument_type_error("environment", environment_type(), value);

    assert_eq!(
        msg,
        "Invalid argument type for 'environment' in module 'web_tier': expected 'dev' | 'stg' | 'prod', got string 'production'",
        "the #3804 root diagnostic must remain stable"
    );
}

#[test]
fn argument_type_mismatch_error_shows_int_literal_value() {
    let value = Value::Concrete(ConcreteValue::Int(5));

    let msg = module_argument_type_error("environment", TypeExpr::String, value);

    assert!(
        msg.contains("got int (5)"),
        "error should include the mismatched int value; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_redacts_secret_value() {
    let secret = "issue-3804-secret-plaintext";
    let value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String(secret.to_string().into()),
    ))));

    let msg = module_argument_type_error("environment", TypeExpr::Int, value);

    assert!(
        msg.contains("got secret"),
        "error should describe a deferred secret by shape; got: {msg}"
    );
    assert!(
        !msg.contains(secret),
        "error must not reveal secret plaintext; got: {msg}"
    );
}

#[test]
fn decrypted_module_argument_is_secret_by_value_type() {
    let plaintext = "hunter2SECRET";
    let config = ProviderContext {
        decryptor: Some(Box::new(move |_, _| Ok(plaintext.to_string()))),
        ..ProviderContext::default()
    };
    let value = crate::builtins::evaluate_builtin_with_config_to_value(
        "decrypt",
        &[Value::Concrete(ConcreteValue::String("ciphertext".into()))],
        &config,
    )
    .expect("test decryptor");

    let message = module_argument_type_error_with_config("enabled", TypeExpr::Bool, value, &config);

    assert!(message.contains("got secret"), "message={message}");
    assert!(!message.contains(plaintext), "message={message}");
}

#[test]
fn argument_type_mismatch_error_shows_nested_list_element_value() {
    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("dev".into())),
        Value::Concrete(ConcreteValue::String("production".into())),
    ]));

    let msg = module_argument_type_error(
        "environments",
        TypeExpr::List(Box::new(environment_type())),
        value,
    );

    assert!(
        msg.contains("at list element 1: expected 'dev' | 'stg' | 'prod', got string 'production'"),
        "error should include the mismatched list element and path; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_string_list_element_value() {
    let type_expr = TypeExpr::List(Box::new(TypeExpr::Union(vec![
        TypeExpr::StringLiteral("dev".to_string()),
        TypeExpr::StringLiteral("stg".to_string()),
    ])));
    let value = Value::Concrete(ConcreteValue::StringList(vec![
        "dev".into(),
        "production".into(),
    ]));

    let msg = module_argument_type_error("environments", type_expr, value);

    assert!(
        msg.contains("at list element 1: expected 'dev' | 'stg', got string 'production'"),
        "error should include the mismatched StringList element and path; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_complete_nested_list_path() {
    let type_expr = TypeExpr::List(Box::new(TypeExpr::List(Box::new(TypeExpr::StringLiteral(
        "dev".to_string(),
    )))));
    let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String(
            "production".into(),
        ))]),
    )]));

    let msg = module_argument_type_error("environments", type_expr, value);

    assert!(
        msg.contains(
            "at list element 0 -> list element 0: expected 'dev', got string 'production'"
        ),
        "error should report the complete path to the innermost mismatched list element; got: {msg}"
    );
}

#[test]
fn binding_ref_argument_defers_to_binding_index() {
    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "environment".to_string(),
        static_type: known(TypeExpr::Union(vec![
            TypeExpr::StringLiteral("dev".to_string()),
            TypeExpr::StringLiteral("prod".to_string()),
        ])),
        default: None,
        description: None,
        validations: Vec::new(),
    }];
    let mut resolver = ModuleResolver::new(".");
    resolver
        .imported_modules
        .insert("inner".to_string(), module);
    let call = ModuleCall::test(
        "inner".to_string(),
        Some("inner_instance".to_string()),
        module_arguments([(
            "environment".to_string(),
            Value::Deferred(DeferredValue::BindingRef {
                binding: "outer_environment".to_string(),
            }),
        )]),
    );
    // Expansion only checks concrete value shapes. BindingIndex owns the
    // declared source-to-sink relation for binding references.
    assert!(resolver.expand_module_call(&call, "inner_instance").is_ok());
}

#[test]
fn directory_module_loader_preserves_parameter_diagnostic_sources() {
    let temp = tempfile::tempdir().unwrap();
    let attributes_path = temp.path().join("attributes.crn");
    let exports_path = temp.path().join("exports.crn");
    std::fs::write(
        &attributes_path,
        "attributes {\n  output: String = 'value'\n}\n",
    )
    .unwrap();
    std::fs::write(&exports_path, "exports {\n  result: String = 'value'\n}\n").unwrap();

    let parsed = load_directory_module(temp.path()).expect("directory module");
    let attribute = &parsed.attribute_params[0];
    assert_eq!(attribute.source_file(), attributes_path.as_path());
    assert_eq!(attribute.source_span().start_line(), 2);
    let export = &parsed.export_params[0];
    assert_eq!(export.source_file(), exports_path.as_path());
    assert_eq!(export.source_span().start_line(), 2);
}

#[test]
fn argument_type_mismatch_error_redacts_secret_list_element() {
    let secret = "issue-3804-nested-secret-plaintext";
    let value = Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
        DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::String(
            secret.to_string().into(),
        )))),
    )]));

    let msg = module_argument_type_error("counts", TypeExpr::List(Box::new(TypeExpr::Int)), value);

    assert!(
        msg.contains("at list element 0: expected Int, got secret"),
        "error should describe a secret list element by shape; got: {msg}"
    );
    assert!(
        !msg.contains(secret),
        "error must not reveal nested secret plaintext; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_nested_map_value() {
    let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "primary".to_string(),
            Value::Concrete(ConcreteValue::String("dev".into())),
        ),
        (
            "secondary".to_string(),
            Value::Concrete(ConcreteValue::String("production".into())),
        ),
    ])));

    let msg = module_argument_type_error(
        "environments",
        TypeExpr::Map(Box::new(environment_type())),
        value,
    );

    assert!(
        msg.contains(
            "at map key 'secondary': expected 'dev' | 'stg' | 'prod', got string 'production'"
        ),
        "error should include the mismatched map value and path; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_nested_struct_field_value() {
    let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "environment".to_string(),
        Value::Concrete(ConcreteValue::String("production".into())),
    )])));
    let type_expr = TypeExpr::Struct {
        fields: vec![("environment".to_string(), environment_type())],
    };

    let msg = module_argument_type_error("settings", type_expr, value);

    assert!(
        msg.contains(
            "at struct field 'environment': expected 'dev' | 'stg' | 'prod', got string 'production'"
        ),
        "error should include the mismatched struct field value and path; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_escapes_string_literal_value() {
    let value = Value::Concrete(ConcreteValue::String("production\"\nnext".into()));

    let msg = module_argument_type_error("environment", environment_type(), value);

    assert!(
        msg.contains("got string \"production\\\"\\nnext\""),
        "error should escape quotes and newlines in string values; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_preserves_float_fraction() {
    let value = Value::Concrete(ConcreteValue::Float(1.0));

    let msg = module_argument_type_error("count", TypeExpr::Int, value);

    assert!(
        msg.contains("got float (1.0)"),
        "error should preserve the float's decimal point; got: {msg}"
    );
}

#[test]
fn argument_type_validation_error_shows_actual_string_literal() {
    let value = Value::Concrete(ConcreteValue::String("not-an-ipv4-cidr".into()));

    let msg =
        module_argument_type_error("network", TypeExpr::Simple("ipv4_cidr".to_string()), value);

    assert!(
        msg.contains("got string 'not-an-ipv4-cidr'"),
        "validation errors should include the actual scalar literal; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_enum_identifier_value() {
    let value = Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
        "aws.foo.Bar.baz",
    )));

    let msg = module_argument_type_error("count", TypeExpr::Int, value);

    assert!(
        msg.contains("got enum identifier aws.foo.Bar.baz"),
        "error should include the enum identifier; got: {msg}"
    );
}

#[test]
fn argument_type_mismatch_error_shows_canonical_enum_value() {
    let value = Value::Concrete(ConcreteValue::CanonicalEnum(
        CanonicalEnumValue::new_for_test(
            TypeIdentity::from_schema_type("aws", "foo", "Bar"),
            "baz",
        ),
    ));

    let msg = module_argument_type_error("count", TypeExpr::Int, value);

    assert!(
        msg.contains("got canonical enum aws.foo.Bar.baz"),
        "error should include the canonical enum value; got: {msg}"
    );
}

#[test]
fn resolved_module_signature_groups_all_owners_of_a_dotted_annotation() {
    let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
    let context = ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([
                crate::parser::OwnedTypeIdentity::provider("aws", identity.clone()),
                crate::parser::OwnedTypeIdentity::provider("awscc", identity),
            ]),
            Box::new(|target, _value| {
                Err(
                    crate::parser::CustomTypeValidationError::MissingDeclaration {
                        owner: "fixture".to_string(),
                        identity: target.identity().clone(),
                    },
                )
            }),
        ),
        ..ProviderContext::default()
    };
    let parsed = crate::parser::parse(
        "arguments {\n  roles: list(aws.iam.Role.Arn)\n}\n",
        &ProviderContext::default(),
    )
    .expect("module signature fixture must parse");

    let signature = super::signature::resolved_signature(&parsed, &context);
    let resolved = &signature.arguments[0].static_type;

    assert!(
        matches!(
            resolved,
            crate::static_type::StaticType::Known(resolved)
                if matches!(resolved.type_expr(), TypeExpr::List(inner)
                    if matches!(inner.as_ref(), TypeExpr::SchemaType { .. }))
        ),
        "module signatures must not retain DottedUnresolved for a known shared identity: {resolved:?}"
    );
}

#[test]
fn resolved_module_signature_preserves_unresolvable_attribute_type_for_best_effort_snapshot() {
    let parsed = crate::parser::parse(
        "attributes {\n  out: missing.types.Identifier = 'value'\n}\n",
        &ProviderContext::default(),
    )
    .expect("module signature fixture must parse");

    let signature = super::signature::resolved_signature(&parsed, &ProviderContext::default());
    let static_type = signature.attributes["out"]
        .as_ref()
        .expect("declared attribute type");

    assert!(
        matches!(
            static_type,
            crate::static_type::StaticType::Unchecked(reason)
                if reason.original()
                    == &TypeExpr::DottedUnresolved(ResourceTypePath::new(
                        "missing",
                        "types.Identifier",
                    ))
                    && matches!(
                        reason.resolution_error(),
                        crate::static_type::StaticTypeResolutionError::UnknownCustomType { .. }
                    )
        ),
        "best-effort module signature must retain the typed unchecked reason: {static_type:?}",
    );
}
