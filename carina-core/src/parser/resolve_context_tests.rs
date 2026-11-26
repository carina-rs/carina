//! HEAD's three failure modes and strict deferred-body checks are contracts.
use super::*;
use crate::builtins::BuiltinFunction;
use crate::parser::ast::with_user_function_binding_map_count;
use crate::resource::{FunctionArgument, same_plain_representation};

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

#[derive(Clone, Copy, Debug)]
enum ShareFnShape {
    Coalesce,
    Conditional,
    Interpolation,
    Join,
}

fn share_fn_definitions(depth: usize, shape: ShareFnShape) -> Vec<String> {
    let mut definitions = vec!["fn f0(x: String): String { x }".to_string()];
    for level in 1..=depth {
        let call = format!("f{}(x)", level - 1);
        let body = match shape {
            ShareFnShape::Coalesce => format!("{call} ?? {call}"),
            ShareFnShape::Conditional => format!("if true {{ {call} }} else {{ {call} }}"),
            ShareFnShape::Interpolation => format!("\"${{{call}}}${{{call}}}\""),
            ShareFnShape::Join => format!("join('', [{call}, {call}])"),
        };
        definitions.push(format!("fn f{level}(x: String): String {{ {body} }}"));
    }
    definitions
}

fn share_fn_counts(shape: ShareFnShape, depth: usize, directory: bool) -> (usize, usize) {
    let definitions = share_fn_definitions(depth, shape).join("\n");
    let consumer = format!("let v = f{depth}('a')\n");
    let (((), mappings), walks) = crate::value_walk::with_function_definition_walk_count(|| {
        with_user_function_binding_map_count(|| {
            let parsed = if directory {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("a_functions.crn"), &definitions).unwrap();
                std::fs::write(dir.path().join("main.crn"), &consumer).unwrap();
                crate::config_loader::parse_directory(
                    &crate::paths::ResolvedPath::new(dir.path()).unwrap(),
                    &context(),
                )
                .unwrap()
                .into_parsed()
            } else {
                crate::parser::parse(&format!("{definitions}\n{consumer}"), &context()).unwrap()
            };
            let result = &parsed.variables["v"];
            if matches!(shape, ShareFnShape::Interpolation | ShareFnShape::Join) {
                assert!(
                    matches!(result, Value::Concrete(ConcreteValue::String(value))
                    if value.expose_plaintext() == "a".repeat(1 << depth))
                );
            }
            let _ = parsed.resolve_root_for_test();
        })
    });
    (mappings, walks)
}

fn share_fn_assert_polynomial(shape: ShareFnShape, directory: bool) {
    // Deep debug parser/evaluator frames exceed libtest's default thread stack.
    // Keep the full depth and count assertions on an explicitly sized stack.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || share_fn_assert_polynomial_on_thread(shape, directory))
        .unwrap()
        .join()
        .unwrap();
}

fn share_fn_assert_polynomial_on_thread(shape: ShareFnShape, directory: bool) {
    let mut previous = None;
    for depth in [2, 4, 8, 12] {
        let counts = share_fn_counts(shape, depth, directory);
        if let Some((old_depth, (bindings, transforms))) = previous {
            // Measured doubling ratios stay below 6.2, leaving headroom at 8.
            // Pure c*2^depth growth has a ratio of 16 from depth 4 to 8 and fails.
            if depth == 2 * old_depth {
                assert!(
                    counts.0 <= bindings * 8,
                    "{shape:?} directory={directory} binding mappings must grow polynomially: depth {old_depth}: {bindings}, depth {depth}: {}",
                    counts.0
                );
                assert!(
                    counts.1 <= transforms * 8,
                    "{shape:?} directory={directory} transform definition walks must grow polynomially: depth {old_depth}: {transforms}, depth {depth}: {}",
                    counts.1
                );
            }
        }
        // At depth 12 the fixed code maps at most 158,569 definitions and walks
        // 258. These caps leave at least 1.5x headroom over those measured maxima.
        assert!(
            counts.0 < 250_000 && counts.1 < 1_024,
            "{shape:?} directory={directory} definition walks at depth {depth} must stay bounded: {counts:?}"
        );
        previous = Some((depth, counts));
    }
}

#[test]
fn share_fn_coalesce_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Coalesce, false);
}

#[test]
fn share_fn_conditional_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Conditional, false);
}

#[test]
fn share_fn_interpolation_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Interpolation, false);
}

#[test]
fn share_fn_join_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Join, false);
}

#[test]
fn share_fn_directory_sibling_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Interpolation, true);
}

#[derive(Clone, Copy, Debug)]
struct ShareFnWalkCounts {
    bindings: usize,
    transforms: usize,
    inspections: usize,
    runtime: usize,
    knowledge: usize,
}

// Count argument operands on the surface available to Inspect's callers.
// A positive result prevents the zero-definition assertion from passing because
// a validation path stopped inspecting retained calls altogether.
#[derive(Default)]
struct OperandVisits;

impl crate::value_walk::Algebra<crate::value_walk::Inspect> for OperandVisits {
    type Output = usize;

    fn scalar<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        at: &crate::value_walk::At<'_>,
        value: crate::value_walk::ScopedScalar<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::Step<crate::value_walk::Inspect, usize, P, B> {
        value.finish(operand_visit(at))
    }

    fn list<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        at: &crate::value_walk::At<'_>,
        items: crate::value_walk::Children<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::Step<crate::value_walk::Inspect, usize, P, B> {
        let own = operand_visit(at);
        items.inspect(self, |children| own + children.into_iter().sum::<usize>())
    }

    fn map<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        at: &crate::value_walk::At<'_>,
        items: crate::value_walk::Children<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::Step<crate::value_walk::Inspect, usize, P, B> {
        let own = operand_visit(at);
        items.inspect(self, |children| own + children.into_iter().sum::<usize>())
    }

    fn expression<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        at: &crate::value_walk::At<'_>,
        expr: crate::value_walk::ExprChildren<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::Step<crate::value_walk::Inspect, usize, P, B> {
        let own = operand_visit(at);
        expr.inspect(self, |children| own + children.into_iter().sum::<usize>())
    }

    fn placeholder<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        at: &crate::value_walk::At<'_>,
        value: crate::value_walk::ScopedLeaf<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::Step<crate::value_walk::Inspect, usize, P, B> {
        value.finish(operand_visit(at))
    }

    fn secret<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        _: &crate::value_walk::SecretAt<'_>,
        child: crate::value_walk::SecretChild<'_, crate::value_walk::Inspect, P, B>,
    ) -> crate::value_walk::SecretStep<crate::value_walk::Inspect, usize, P, B> {
        child.inspect(self)
    }
}

fn operand_visit(at: &crate::value_walk::At<'_>) -> usize {
    use crate::value_walk::ExpressionEdge;
    let edges = at.expression_edges();
    usize::from(
        edges
            .iter()
            .any(|edge| matches!(edge, ExpressionEdge::ArgumentValue(_)))
            && !edges.iter().any(|edge| {
                matches!(
                    edge,
                    ExpressionEdge::FunctionDefinition | ExpressionEdge::FunctionScope(_)
                )
            }),
    )
}

fn assert_retained_chain<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
    call: &GenericValue<P, B>,
    depth: usize,
) {
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) = call else {
        panic!("depth {depth}: validation must retain the outer call")
    };
    let mut function = function.as_ref();
    for level in (0..=depth).rev() {
        assert_eq!(function.name, format!("f{level}"));
        if level == 0 {
            break;
        }
        let body = match &function.body.value {
            GenericValue::Deferred(DeferredValue::Coalesce { value, .. }) => value.as_ref(),
            GenericValue::Deferred(DeferredValue::ModuleIf(conditional)) => {
                conditional.then_value()
            }
            body => body,
        };
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { function: next, .. }) =
            body
        else {
            panic!(
                "depth {depth}: f{} must remain in the captured call chain",
                level - 1
            )
        };
        function = next;
    }
}

fn share_fn_walk_counts(run: impl FnOnce()) -> ShareFnWalkCounts {
    let ((((((), bindings), transforms), inspections), runtime), knowledge) =
        crate::resource::knowledge::with_function_definition_knowledge_count(|| {
            crate::resolver::with_runtime_function_definition_walk_count(|| {
                crate::value_walk::with_function_definition_inspect_count(|| {
                    crate::value_walk::with_function_definition_walk_count(|| {
                        with_user_function_binding_map_count(run)
                    })
                })
            })
        });
    ShareFnWalkCounts {
        bindings,
        transforms,
        inspections,
        runtime,
        knowledge,
    }
}

fn share_fn_retained_counts(shape: ShareFnShape, depth: usize) -> ShareFnWalkCounts {
    use crate::binding_index::BindingIndex;
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
    };

    let definitions = share_fn_definitions(depth, shape).join("\n");
    let source = format!(
        "arguments {{ input: String }}\n{definitions}\nlet v = f{depth}(input)\nlet sink = mock.test.Sink {{ value = \"prefix-${{v}}\" }}\n"
    );
    let config = context();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Sink")
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    share_fn_walk_counts(|| {
        let mut parsed = crate::parser::parse(&source, &config).unwrap();
        assert!(
            matches!(
                &parsed.variables["v"],
                Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. })
                    if function.name == format!("f{depth}")
                    && args[0].pending_parameter() == Some("input")
            ),
            "{shape:?} depth {depth}: the unknown module argument must retain the call"
        );
        let mut call = &parsed.variables["v"];
        for level in (0..=depth).rev() {
            let Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) = call else {
                panic!("{shape:?} depth {depth}: f{level} must remain in the captured call chain")
            };
            assert_eq!(function.name, format!("f{level}"));
            call = match &function.body.value {
                Value::Deferred(DeferredValue::Coalesce { value, .. }) => value,
                Value::Deferred(DeferredValue::ModuleIf(conditional)) => conditional.then_value(),
                body => body,
            };
        }

        crate::validation::deferred_types::with_session_for_test(&config, |session| {
            let report =
                crate::validation::resolve_file_type_exprs_report(&mut parsed, session, None)
                    .unwrap();
            assert!(
                report.diagnostics().is_empty(),
                "{shape:?} depth {depth}: {:?}",
                report.diagnostics()
            );
            crate::parser::resolve_resource_refs_in_session(&mut parsed, session).unwrap();
        });
        let bindings = BindingIndex::from_parsed(&parsed, &schemas, &config);
        crate::validation::validate_user_function_ref_types_with_bindings(&parsed, &bindings)
            .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
        assert!(
            configuration.judged().report().findings().is_empty(),
            "{shape:?} depth {depth}: {:?}",
            configuration.judged().report().findings()
        );
        let findings = configuration
            .judged()
            .function_type_promise_findings(&bindings);
        assert!(findings.is_empty(), "{shape:?} depth {depth}: {findings:?}");

        let resolved = parsed.resolve_root_for_test();
        crate::test_utils::legacy_validate_resources(
            &resolved,
            &schemas,
            &std::collections::HashSet::from(["mock".to_string()]),
            &config,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            matches!(
                &resolved.variables["v"],
                GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { .. })
            ),
            "{shape:?} depth {depth}: validation must keep the unknown call deferred"
        );
        assert_retained_chain(&resolved.variables["v"], depth);
        let visits = crate::value_walk::inspect(
            crate::value_walk::WalkRoot::syntactic(),
            &resolved.variables["v"],
            &mut OperandVisits,
        )
        .unwrap()
        .into_output();
        eprintln!("{shape:?} depth {depth}: operand visits={visits}");
        assert!(
            visits > 0,
            "{shape:?} depth {depth}: operand visits: {visits}"
        );
    })
}

fn share_fn_assert_retained_polynomial(shape: ShareFnShape) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut counts = Vec::new();
            for depth in [2, 4, 8, 12] {
                let current = share_fn_retained_counts(shape, depth);
                counts.push((depth, current));
                eprintln!("{shape:?} retained depth {depth}: {current:?}");
                assert_eq!(
                    current.inspections, 0,
                    "{shape:?} retained Inspect must never enter definitions: {counts:?}"
                );
                if counts.len() > 1 {
                    let (old_depth, old) = counts[counts.len() - 2];
                    // These intervals allow cubic growth up to 8x, while pure
                    // c*2^depth growth gives 16x from depth 4 to 8.
                    for (traversal, old_count, count) in [
                        ("binding", old.bindings, current.bindings),
                        ("transform", old.transforms, current.transforms),
                        ("runtime", old.runtime, current.runtime),
                        ("knowledge", old.knowledge, current.knowledge),
                    ] {
                        assert!(
                            count <= old_count * 8,
                            "{shape:?} retained {traversal} definition walks must grow polynomially: depth {old_depth}: {old_count}, depth {depth}: {count}; all counts: {counts:?}"
                        );
                    }
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn share_fn_retained_coalesce_definition_walks_grow_polynomially() {
    share_fn_assert_retained_polynomial(ShareFnShape::Coalesce);
}

#[test]
fn share_fn_retained_conditional_definition_walks_grow_polynomially() {
    share_fn_assert_retained_polynomial(ShareFnShape::Conditional);
}

fn share_fn_retained_module_counts(
    shape: ShareFnShape,
    depth: usize,
    split: bool,
) -> ShareFnWalkCounts {
    use crate::binding_index::BindingIndex;
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
    };

    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let definitions = share_fn_definitions(depth, shape).join("\n");
    let consumer = format!(
        "let v = f{depth}(input)\nlet module_sink = mock.test.Sink {{ value = \"prefix-${{v}}\" }}\n"
    );
    for (path, source) in [
        (directory.path().join("providers.crn"), "provider mock {}\n".to_string()),
        (directory.path().join("main.crn"), "let origin = mock.test.Source { name = 'origin' }\nlet child = use { source = './module' }\nlet instance = child { input = origin.identifier }\nlet sink = mock.test.Sink { value = \"prefix-${instance.output}\" }\n".to_string()),
        (directory.path().join("exports.crn"), "attributes { output: String = instance.output }\n".to_string()),
        (module.join("main.crn"), format!("arguments {{ input: String }}\n{}\n{consumer}", if split { "" } else { &definitions })),
        (module.join("functions.crn"), if split { definitions } else { String::new() }),
        (module.join("exports.crn"), "attributes { output: String = v }\n".to_string()),
    ] {
        std::fs::write(path, source).unwrap();
    }
    let config = context();
    let mut schemas = SchemaRegistry::new();
    for (name, attribute) in [("test.Source", "name"), ("test.Sink", "value")] {
        let schema = PatternResourceSchema::new(name).attribute(
            PatternAttributeSchema::optional(attribute, PatternAttributeType::string()).build(),
        );
        let schema = if name == "test.Source" {
            schema.attribute(
                PatternAttributeSchema::provider_populated(
                    "identifier",
                    PatternAttributeType::string(),
                )
                .build(),
            )
        } else {
            schema
        };
        schemas.insert("mock", schema.validate().unwrap());
    }
    share_fn_walk_counts(|| {
        let mut loaded_module = crate::config_loader::parse_directory(
            &crate::paths::ResolvedPath::new(&module).unwrap(),
            &config,
        )
        .unwrap()
        .into_parsed();
        crate::validation::deferred_types::with_session_for_test(&config, |session| {
            let report = crate::validation::resolve_file_type_exprs_report(
                &mut loaded_module,
                session,
                None,
            )
            .unwrap();
            assert!(
                report.diagnostics().is_empty(),
                "{:?}",
                report.diagnostics()
            );
            crate::parser::resolve_resource_refs_in_session(&mut loaded_module, session).unwrap();
        });
        let bindings = BindingIndex::from_parsed(&loaded_module, &schemas, &config);
        crate::validation::validate_user_function_ref_types_with_bindings(
            &loaded_module,
            &bindings,
        )
        .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&loaded_module, &schemas, &config);
        assert!(
            configuration.judged().report().findings().is_empty(),
            "{:?}",
            configuration.judged().report().findings()
        );
        assert!(
            configuration
                .judged()
                .function_type_promise_findings(&bindings)
                .is_empty()
        );
        assert_retained_chain(&loaded_module.variables["v"], depth);
        let visits = crate::value_walk::inspect(
            crate::value_walk::WalkRoot::syntactic(),
            &loaded_module.variables["v"],
            &mut OperandVisits,
        )
        .unwrap()
        .into_output();
        eprintln!("{shape:?} module split={split} depth {depth}: loaded operand visits={visits}");
        assert!(visits > 0, "loaded module must visit operands");

        let root = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let loaded = crate::config_loader::load_configuration(&root, &config).unwrap();
        let expanded =
            crate::validation::deferred_types::with_session_for_test(&config, |session| {
                crate::module_resolver::resolve_modules_with_providerless_config(
                    loaded.parsed,
                    &root,
                    session,
                    &schemas,
                )
                .unwrap()
            });
        let bindings = BindingIndex::from_parsed(&expanded, &schemas, &config);
        let configuration =
            crate::config_loader::load_buffer_configuration(&expanded, &schemas, &config);
        assert!(
            configuration.judged().report().findings().is_empty(),
            "{:?}",
            configuration.judged().report().findings()
        );
        assert!(
            configuration
                .judged()
                .function_type_promise_findings(&bindings)
                .is_empty()
        );
        let sink = expanded
            .resources
            .iter()
            .find(|resource| {
                resource
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.as_str() == "sink")
            })
            .expect("parent sink");
        let value = sink.get_attr("value").unwrap();
        let GenericValue::Deferred(DeferredValue::Interpolation(parts)) = value else {
            panic!("parent sink must retain its interpolation: {value:?}")
        };
        let operand = parts
            .iter()
            .find_map(|part| match part {
                InterpolationPart::Expr(value) => Some(value),
                _ => None,
            })
            .expect("parent output operand");
        // Expansion keeps the parent's reference to the composition output.
        // Follow the same binding view used by validation to its retained call.
        let GenericValue::Deferred(DeferredValue::ResourceRef { path }) = operand else {
            panic!("parent must reference the module output: {operand:?}")
        };
        let values = crate::binding_index::ResolvedBindings::for_expanded_validation(&expanded);
        let call = values
            .get(path.binding())
            .expect("module output binding")
            .get(path.attribute())
            .expect("module output attribute")
            .to_value_at("parent output")
            .unwrap();
        // Substituting a resource reference evaluates fN's outer body once.
        // Its f(N-1)..f0 chain must still be deferred after validation.
        let first_call = match (&call, shape) {
            (
                GenericValue::Deferred(DeferredValue::Coalesce { value, default }),
                ShareFnShape::Coalesce,
            ) => {
                assert_retained_chain(value, depth - 1);
                assert_retained_chain(default, depth - 1);
                value.as_ref()
            }
            (
                GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { .. }),
                ShareFnShape::Conditional,
            ) => {
                assert_retained_chain(&call, depth - 1);
                &call
            }
            _ => panic!("{shape:?} depth {depth}: expansion must retain the deferred expression"),
        };
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = first_call
        else {
            unreachable!()
        };
        assert!(
            matches!(args[0].value(), Some(GenericValue::Deferred(DeferredValue::ResourceRef { path })) if path.binding() == "origin" && path.attribute() == "identifier")
        );
        let visits = crate::value_walk::inspect(
            crate::value_walk::WalkRoot::syntactic(),
            &call,
            &mut OperandVisits,
        )
        .unwrap()
        .into_output();
        eprintln!("{shape:?} module split={split} depth {depth}: parent operand visits={visits}");
        assert!(visits > 0, "expanded parent must visit operands");
        crate::test_utils::legacy_validate_resources(
            &expanded,
            &schemas,
            &std::collections::HashSet::from(["mock".to_string()]),
            &config,
            &mut Vec::new(),
        )
        .unwrap();
        assert_retained_chain(first_call, depth - 1);
    })
}

fn share_fn_assert_retained_module_polynomial(shape: ShareFnShape, split: bool) {
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(move || {
        let mut previous: Option<(usize, ShareFnWalkCounts)> = None;
        for depth in [2, 4, 8, 12] {
            let current = share_fn_retained_module_counts(shape, depth, split);
            eprintln!("{shape:?} module split={split} depth {depth}: {current:?}");
            assert_eq!(current.inspections, 0, "{shape:?} module split={split} depth {depth}: Inspect must never enter definitions: {current:?}");
            if let Some((old_depth, old)) = previous {
                for (traversal, old_count, count) in [
                    ("binding", old.bindings, current.bindings),
                    ("transform", old.transforms, current.transforms),
                    ("runtime", old.runtime, current.runtime),
                    ("knowledge", old.knowledge, current.knowledge),
                ] {
                    assert!(count <= old_count * 8, "{shape:?} module split={split} {traversal}: depth {old_depth}: {old_count}, depth {depth}: {count}; {current:?}");
                }
            }
            previous = Some((depth, current));
        }
    }).unwrap().join().unwrap();
}

#[test]
fn share_fn_retained_module_compact_coalesce_definition_walks_grow_polynomially() {
    share_fn_assert_retained_module_polynomial(ShareFnShape::Coalesce, false);
}

#[test]
fn share_fn_retained_module_split_coalesce_definition_walks_grow_polynomially() {
    share_fn_assert_retained_module_polynomial(ShareFnShape::Coalesce, true);
}

#[test]
fn share_fn_retained_module_compact_conditional_definition_walks_grow_polynomially() {
    share_fn_assert_retained_module_polynomial(ShareFnShape::Conditional, false);
}

#[test]
fn share_fn_retained_module_split_conditional_definition_walks_grow_polynomially() {
    share_fn_assert_retained_module_polynomial(ShareFnShape::Conditional, true);
}

#[test]
fn share_fn_crosscheck_nested_chains() {
    let (_, bindings, transforms) = crate::function_memo_test::verify(|| {
        for shape in [
            ShareFnShape::Coalesce,
            ShareFnShape::Conditional,
            ShareFnShape::Interpolation,
            ShareFnShape::Join,
        ] {
            for depth in [2, 3] {
                share_fn_counts(shape, depth, false);
            }
        }
        share_fn_counts(ShareFnShape::Interpolation, 3, true);
    });
    assert!(bindings > 0, "binding memo hits must be cross-checked");
    assert!(transforms > 0, "transform memo hits must be cross-checked");
}

#[test]
fn share_fn_crosscheck_outer_bindings_defaults_locals_and_secrets() {
    let (_, bindings, transforms) = crate::function_memo_test::verify(|| {
        let parsed = crate::parser::parse(
            r#"arguments { input: String }
let outer = secret('protected-4495')
fn leaf(x: String, suffix: String = 'default'): String { secret("${x}-${outer}-${suffix}") }
fn middle(x: String): String {
  let local = leaf(x)
  "${local}${leaf(x)}"
}
fn top(x: String): String { middle(x) ?? middle(x) }
let result = top(input)
"#,
            &context(),
        )
        .unwrap();
        assert!(crate::value::contains_secret(
            &parsed.user_functions["leaf"].body.value
        ));
        // The unknown module argument defers evaluation before substitution.
        // Exercise the captured definitions through fresh rewrites explicitly.
        let definitions = crate::value_walk::DefinitionMemo::default();
        for line in [1, 99] {
            let rewritten = crate::value_walk::substitute_function_parameters(
                &parsed.variables["result"],
                &std::collections::HashMap::new(),
                line,
                crate::value_walk::RewritePosition::Value,
                &definitions,
            )
            .unwrap();
            assert!(same_plain_representation(
                &parsed.variables["result"],
                &rewritten
            ));
        }
        let mut resolved = parsed.resolve_root_for_test();
        resolved
            .variables
            .get_mut("result")
            .unwrap()
            .map_absolute_bindings(&mut |binding| binding.clone());
        assert!(crate::value::contains_secret(
            &resolved.user_functions["leaf"].body.value
        ));
        assert!(!format!("{:?}", resolved.variables["result"]).contains("protected-4495"));

        share_fn_forward_file_function_body_captures_bootstrap_callee_and_scope();
        share_fn_forward_directory_function_body_captures_bootstrap_callee_and_scope();
    });
    assert!(bindings > 0, "binding memo hits must be cross-checked");
    assert!(transforms > 0, "transform memo hits must be cross-checked");
}

#[test]
fn share_fn_forward_file_root_consumer_before_declaration_remains_a_call() {
    let parsed = crate::parser::parse(
        r#"let v = g("a")
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
"#,
        &context(),
    )
    .unwrap();

    // A seeded function is not evaluated, even with a static argument.
    assert!(matches!(
        &parsed.variables["v"],
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, source_line, .. })
            if function.name == "g"
                && source_line.get() == Some(1)
                && args.len() == 1
                && matches!(args[0].value(), Some(Value::Concrete(ConcreteValue::String(s))) if s == "a")
    ));

    // Root binding resolution changes addresses; it does not evaluate the call.
    let resolved = parsed.resolve_root_for_test();
    assert!(matches!(
        &resolved.variables["v"],
        GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { function, args, source_line, .. })
            if function.name == "g"
                && source_line.get() == Some(1)
                && args.len() == 1
                && matches!(args[0].value(), Some(GenericValue::Concrete(ConcreteValue::String(s))) if s == "a")
    ));
}

#[test]
fn share_fn_forward_file_root_consumer_after_declaration_evaluates_to_interpolation() {
    let parsed = crate::parser::parse(
        r#"let s = "outer"
fn g(x: String): String { "${x}-${s}" }
let v = g("a")
"#,
        &context(),
    )
    .unwrap();

    // Function bodies do not capture root lets. The unbound s is an enum
    // identifier, which interpolation canonicalization does not stringify.
    assert!(matches!(
        &parsed.variables["v"],
        Value::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));

    let resolved = parsed.resolve_root_for_test();
    assert!(matches!(
        &resolved.variables["v"],
        GenericValue::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(GenericValue::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));
}

#[test]
fn share_fn_forward_file_deferred_consumers_capture_bootstrap_or_final_body() {
    for (before_declaration, source) in [
        (
            true,
            r#"arguments { input: String }
let v = g(input)
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
"#,
        ),
        (
            false,
            r#"arguments { input: String }
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
let v = g(input)
"#,
        ),
    ] {
        let parsed = crate::parser::parse(source, &context()).unwrap();
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &parsed.variables["v"]
        else {
            panic!("g(input) must remain a deferred user-function call")
        };
        assert_eq!(function.name, "g");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert!(matches!(
            &function.body.value,
            Value::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Expr(Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::FnParam { name },
                        ))),
                        InterpolationPart::Literal(separator),
                        InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if name == "x" && separator == "-" && s.as_str() == "s"
                )
        ));

        let final_body = &parsed.user_functions["g"].body;
        assert!(same_plain_representation(
            &function.body.value,
            &final_body.value
        ));
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert_eq!(
            function.body.source_file.as_deref(),
            before_declaration.then_some(std::path::Path::new("test.crn"))
        );
        assert!(final_body.source_file.is_none());
        // These are all four body fields: only source_file differs for a
        // forward call. An after-declaration call has the final body.
        assert_eq!(
            function.body.source_file == final_body.source_file,
            !before_declaration
        );
        assert_eq!(
            functions.keys().map(String::as_str).collect::<Vec<_>>(),
            ["g"]
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);

        let resolved = parsed.resolve_root_for_test();
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &resolved.variables["v"]
        else {
            panic!("root binding resolution must retain g(input)")
        };
        let final_body = &resolved.user_functions["g"].body;
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert_eq!(function.body.value, final_body.value);
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert_eq!(
            function.body.source_file.as_deref(),
            before_declaration.then_some(std::path::Path::new("test.crn"))
        );
        assert!(final_body.source_file.is_none());
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }
}

#[test]
fn share_fn_forward_file_function_body_captures_bootstrap_callee_and_scope() {
    let parsed = crate::parser::parse(
        r#"let s = "outer"
fn outer(x: String): String { inner(x) }
fn inner(x: String): String { "${x}-${s}" }
"#,
        &context(),
    )
    .unwrap();
    let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        args,
        ..
    }) = &parsed.user_functions["outer"].body.value
    else {
        panic!("outer must retain its forward call to inner")
    };
    assert_eq!(function.name, "inner");
    assert_eq!(args.len(), 1);
    assert!(matches!(
        args[0].value(),
        Some(Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam { name }))) if name == "x"
    ));
    let final_body = &parsed.user_functions["inner"].body;
    assert!(same_plain_representation(
        &function.body.value,
        &final_body.value
    ));
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert_eq!(
        function.body.source_file.as_deref(),
        Some(std::path::Path::new("test.crn"))
    );
    assert!(final_body.source_file.is_none());
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["outer", "inner"]
    );
    // The captured outer is itself the prepass copy: its inner call was
    // parsed when only inner had been added to the bootstrap table.
    let Value::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("bootstrap outer must contain a resolved inner call")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );

    let resolved = parsed.resolve_root_for_test();
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &resolved.user_functions["outer"].body.value
    else {
        panic!("root binding resolution must retain outer's call")
    };
    let final_body = &resolved.user_functions["inner"].body;
    assert_eq!(function.body.value, final_body.value);
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert_eq!(
        function.body.source_file.as_deref(),
        Some(std::path::Path::new("test.crn"))
    );
    assert!(final_body.source_file.is_none());
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("root binding resolution must retain bootstrap outer's scope")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );
}

#[test]
fn share_fn_forward_directory_root_consumers_before_and_after_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let main_path = directory.path().join("main.crn");
    let helpers_path = directory.path().join("z_helpers.crn");
    let main = "let v_before = g(\"a\")\nlet s = \"outer\"\n";
    let helpers = "fn g(x: String): String { \"${x}-${s}\" }\nlet v_after = g(\"a\")\n";
    std::fs::write(&main_path, main).unwrap();
    std::fs::write(&helpers_path, helpers).unwrap();

    // Inspect the per-file parser output before the directory loader's
    // separate function-evaluation pass removes the forward static call.
    let files = crate::config_loader::parse_directory_files(
        &[(main_path, main.into()), (helpers_path, helpers.into())],
        &context(),
    )
    .unwrap();
    assert!(matches!(
        &files[0].1.parsed().variables["v_before"],
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. })
            if function.name == "g" && args.len() == 1
                && matches!(args[0].value(), Some(Value::Concrete(ConcreteValue::String(s))) if s == "a")
    ));
    assert!(matches!(
        &files[1].1.parsed().variables["v_after"],
        Value::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));

    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();
    // To confirm by running: the loader's evaluation pass retains the enum
    // operand instead of folding either result to a concrete string.
    for name in ["v_before", "v_after"] {
        assert!(matches!(
            &parsed.variables[name],
            Value::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Literal(prefix),
                        InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if prefix == "a-" && s.as_str() == "s"
                )
        ));
    }

    let resolved = parsed.resolve_root_for_test();
    for name in ["v_before", "v_after"] {
        assert!(matches!(
            &resolved.variables[name],
            GenericValue::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Literal(prefix),
                        InterpolationPart::Expr(GenericValue::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if prefix == "a-" && s.as_str() == "s"
                )
        ));
    }
}

#[test]
fn share_fn_forward_directory_deferred_consumers_capture_bootstrap_or_local_body() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "arguments { input: String }\nlet v_before = g(input)\nlet s = \"outer\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("z_helpers.crn"),
        "fn g(x: String): String { \"${x}-${s}\" }\nlet v_after = g(input)\n",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();

    for name in ["v_before", "v_after"] {
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &parsed.variables[name]
        else {
            panic!("{name} must remain a deferred g(input) call")
        };
        assert_eq!(function.name, "g");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        let final_body = &parsed.user_functions["g"].body;
        assert!(same_plain_representation(
            &function.body.value,
            &final_body.value
        ));
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert!(final_body.source_file.is_some());
        // To confirm by running: directory metadata stamps the stored final
        // definition after calls have captured it. The bootstrap body was
        // already stamped, so it equals the final body; the local copy lacks it.
        assert_eq!(function.body.source_file.is_some(), name == "v_before");
        assert_eq!(
            function.body.source_file == final_body.source_file,
            name == "v_before"
        );
        assert_eq!(
            functions.keys().map(String::as_str).collect::<Vec<_>>(),
            ["g"]
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }

    let resolved = parsed.resolve_root_for_test();
    for name in ["v_before", "v_after"] {
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &resolved.variables[name]
        else {
            panic!("root binding resolution must retain {name}")
        };
        let final_body = &resolved.user_functions["g"].body;
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert_eq!(function.body.value, final_body.value);
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert!(final_body.source_file.is_some());
        assert_eq!(function.body.source_file.is_some(), name == "v_before");
        assert_eq!(
            function.body.source_file == final_body.source_file,
            name == "v_before"
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }
}

#[test]
fn share_fn_forward_directory_function_body_captures_bootstrap_callee_and_scope() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "arguments { input: String }\nlet v = outer(input)\nlet s = \"outer\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("z_helpers.crn"),
        "fn outer(x: String): String { inner(x) }\nfn inner(x: String): String { \"${x}-${s}\" }\n",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();
    let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        args,
        ..
    }) = &parsed.user_functions["outer"].body.value
    else {
        panic!("outer must retain its forward call to inner")
    };
    assert_eq!(function.name, "inner");
    assert_eq!(args.len(), 1);
    assert!(matches!(
        args[0].value(),
        Some(Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam { name }))) if name == "x"
    ));
    let final_body = &parsed.user_functions["inner"].body;
    assert!(same_plain_representation(
        &function.body.value,
        &final_body.value
    ));
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert!(final_body.source_file.is_some());
    assert_eq!(function.body.source_file, final_body.source_file);
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["outer", "inner"]
    );
    assert!(!same_plain_representation(
        &functions["outer"].body.value,
        &parsed.user_functions["outer"].body.value
    ));
    let Value::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("bootstrap outer must retain the earlier function table")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );

    let resolved = parsed.resolve_root_for_test();
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &resolved.user_functions["outer"].body.value
    else {
        panic!("root binding resolution must retain outer's call")
    };
    let final_body = &resolved.user_functions["inner"].body;
    assert_eq!(function.body.value, final_body.value);
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert!(final_body.source_file.is_some());
    assert_eq!(function.body.source_file, final_body.source_file);
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("root binding resolution must retain bootstrap outer's scope")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );
}

fn parsed(expression: &str) -> ParsedFile {
    crate::parser::parse(
        &format!("let item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
        &context(),
    )
    .unwrap()
}

fn failed_call() -> Value {
    Value::Deferred(DeferredValue::FunctionCall {
        name: BuiltinFunction::length,
        args: vec![FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::Bool(true),
        ))],
        source_line: 1.into(),
    })
}

fn failed_user_call() -> ParsedFile {
    let mut file = crate::parser::parse(
        "fn fail(name: String): String { env(name) }\nlet item = mock.test.resource { name = 'item' }",
        &context(),
    )
    .unwrap();
    let functions = std::sync::Arc::new(file.user_functions.clone());
    let call = Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function: std::sync::Arc::new(file.user_functions["fail"].clone()),
        functions,
        args: vec![FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::String("CARINA_3979_MISSING_USER_CALL".into()),
        ))],
        source_line: 2.into(),
    });
    file.resources[0].attributes.insert("comment".into(), call);
    file
}

#[test]
fn builtin_failure_modes_match_head() {
    for (resolution, accepted) in [
        (strict_resolution_context(), false),
        (analysis_resolution_context(), true),
        (diagnostics_resolution_context(), false),
        (strict_resolution_context().retained(), true),
    ] {
        for expression in ["length(true)", "secret(length(true))"] {
            let result = resolve_resource_refs_in_context(
                &mut parsed(expression),
                &context(),
                None,
                resolution,
            );
            assert_eq!(result.is_ok(), accepted, "{resolution:?}: {result:?}");
        }
    }
}

#[test]
fn failed_user_call_modes_match_head() {
    for (resolution, accepted) in [
        (strict_resolution_context(), false),
        (analysis_resolution_context(), false),
        (diagnostics_resolution_context(), true),
        (strict_resolution_context().retained(), true),
    ] {
        let mut file = failed_user_call();
        let result = resolve_resource_refs_in_context(&mut file, &context(), None, resolution);
        assert_eq!(result.is_ok(), accepted, "{resolution:?}: {result:?}");
        if accepted {
            assert!(matches!(
                file.resources[0].get_attr("comment"),
                Some(Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }))
            ));
        }
    }
    assert!(
        resolve_resource_refs_defer_unknown_functions(&mut failed_user_call(), &context()).is_err()
    );
    assert!(
        resolve_snapshot_function_calls_in_context(
            &mut failed_user_call(),
            &context(),
            analysis_resolution_context()
        )
        .is_err()
    );
    assert!(resolve_resource_refs_for_diagnostics(&mut failed_user_call(), &context()).is_ok());
}

#[test]
fn strict_entry_points_and_diagnostics_still_reject_builtin_failures() {
    assert!(resolve_resource_refs_with_config(&mut parsed("length(true)"), &context()).is_err());
    assert!(
        resolve_resource_refs_for_diagnostics(&mut parsed("length(true)"), &context()).is_err()
    );
    crate::validation::deferred_types::with_session_for_test(&context(), |session| {
        let file = parsed("length(true)");
        assert!(resolve_resource_refs_in_session(&mut file.clone(), session).is_err());
        let tree = super::super::AnalysisTree::try_from_file(&file).unwrap();
        assert!(resolve_resource_refs_from_analysis(&tree, session).is_err());
    });
    assert!(
        resolve_resource_refs_defer_unknown_functions(&mut parsed("length(true)"), &context())
            .is_ok()
    );
    assert!(
        resolve_snapshot_function_calls_in_context(
            &mut parsed("length(true)"),
            &context(),
            analysis_resolution_context()
        )
        .is_ok()
    );
}

#[test]
fn strict_deferred_value_if_checks_both_branches() {
    for expression in [
        "if flag { length(true) } else { 'ok' }",
        "if flag { 'ok' } else { length(true) }",
    ] {
        let mut file = crate::parser::parse(
            &format!("arguments {{ flag: Bool }}\nlet item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
            &context(),
        ).unwrap();
        assert!(matches!(
            file.resources[0].get_attr("comment"),
            Some(Value::Deferred(DeferredValue::ModuleIf(_)))
        ));
        let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
        assert!(error.to_string().contains("length"), "{error}");
    }
}

#[test]
fn strict_deferred_structural_if_checks_both_body_directives() {
    for branch_index in 0..2 {
        let mut file = crate::parser::parse(
            "arguments { flag: Bool }\nlet choice = if flag { mock.test.resource { name = 'a' directives { force_delete = true } } } else { mock.test.resource { name = 'b' directives { force_delete = true } } }",
            &context(),
        ).unwrap();
        let branch = file.deferred_module_ifs[0]
            .branches_mut()
            .nth(branch_index)
            .unwrap();
        let super::super::DeferredIfBody::Resource(resource) = branch else {
            panic!("resource branch")
        };
        resource.directives.force_delete.as_mut().unwrap().value = failed_call();
        let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
        assert!(error.to_string().contains("length"), "{error}");
    }
}

#[test]
fn strict_deferred_for_template_rejects_known_builtin_failure() {
    let mut file = crate::parser::parse(
        "let origin = mock.test.resource { name = 'origin' }\nfor item in origin.items { mock.test.resource { name = item comment = length(true) } }",
        &context(),
    ).unwrap();
    assert_eq!(file.parsed_deferred_for_expressions().len(), 1);
    let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
    assert!(error.to_string().contains("length"), "{error}");
}

#[test]
fn strict_coalesce_default_preserves_failures_and_folds_successes() {
    for expression in [
        "origin.comment ?? length(true)",
        "\"${origin.comment ?? length(true)}\"",
    ] {
        let mut file = crate::parser::parse(
            &format!("let origin = mock.test.resource {{ name = 'origin' }}\nlet item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
            &context(),
        ).unwrap();
        resolve_resource_refs_with_config(&mut file, &context()).unwrap();
    }
    let mut file = parsed("'unused'");
    file.resources[0].attributes.insert(
        "comment".into(),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("origin", "comment", vec![])),
            default: Box::new(Value::Deferred(DeferredValue::FunctionCall {
                name: BuiltinFunction::length,
                args: vec![FunctionArgument::from_value(Value::Concrete(
                    ConcreteValue::String("ok".into()),
                ))],
                source_line: 1.into(),
            })),
        }),
    );
    resolve_resource_refs_with_config(&mut file, &context()).unwrap();
    let Some(Value::Deferred(DeferredValue::Coalesce { default, .. })) =
        file.resources[0].get_attr("comment")
    else {
        panic!("retained coalesce")
    };
    assert_eq!(default.as_ref(), &Value::Concrete(ConcreteValue::Int(2)));
}

#[test]
fn retained_snapshot_does_not_require_failed_user_calls() {
    let mut file = failed_user_call();
    resolve_snapshot_function_calls_in_context(
        &mut file,
        &context(),
        analysis_resolution_context().retained(),
    )
    .unwrap();
    assert!(matches!(
        file.resources[0].get_attr("comment"),
        Some(Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }))
    ));
}

fn inspect_4495_parser_missing(line: usize, reason: &str) -> Value {
    Value::Deferred(DeferredValue::Unavailable {
        name: BuiltinFunction::env,
        args: vec![],
        reason: UnavailableReason::from_builtin_error(reason.into()),
        source_line: line.into(),
    })
}

fn inspect_4495_assert_required(value: Value, expected: Option<(usize, &str, bool)>) {
    // Call the production interpolation guard; the policy's Evidence/error
    // inventory and ordinal checks live beside UnavailableSearch's tests.
    let result = reject_required_interpolation_unavailable(
        &[
            InterpolationPart::Literal("prefix".into()),
            InterpolationPart::Expr(value),
        ],
        strict_resolution_context(),
    );
    if let Some((line, message, secret)) = expected {
        let error = result.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Invalid expression at line {line}: {message}")
        );
        if secret {
            assert!(matches!(error, ParseError::InsideSecret { line: actual } if actual == line));
            assert!(!format!("{error:?}").contains("private-4495"));
        } else {
            assert!(
                matches!(error, ParseError::InvalidExpression { line: actual, .. } if actual == line)
            );
        }
    } else {
        result.unwrap();
    }
}

#[test]
fn inspect_4495_required_interpolation_directory_definition_and_primary_pins() {
    let directory = tempfile::tempdir().unwrap();
    for (name, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "main.crn",
            "arguments { input: String }\nlet result = retained(input)\n",
        ),
        ("exports.crn", "attributes { output: String = result }\n"),
        (
            "functions.crn",
            "fn unused(): String { 'unused' }\nfn retained(x: String, fallback: String = 'default'): String {\n let local = 'local'\n x\n}\n",
        ),
    ] {
        std::fs::write(directory.path().join(name), source).unwrap();
    }
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();
    for location in ["default", "local", "body", "captured"] {
        let mut call = parsed.variables["result"].clone();
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            ..
        }) = &mut call
        else {
            panic!("directory fixture must retain its call")
        };
        assert!(functions.contains_key("unused"));
        let failure = || inspect_4495_parser_missing(5, "private-4495 definition").into_secret();
        match location {
            "default" => std::sync::Arc::make_mut(function).params[1].default = Some(failure()),
            "local" => std::sync::Arc::make_mut(function).local_lets[0].1 = failure(),
            "body" => std::sync::Arc::make_mut(function).body.value = failure(),
            "captured" => {
                std::sync::Arc::make_mut(functions)
                    .get_mut("unused")
                    .unwrap()
                    .body
                    .value = failure()
            }
            _ => unreachable!(),
        }
        inspect_4495_assert_required(call.clone(), None);
        let Value::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = &mut call else {
            unreachable!()
        };
        args.clear();
        args.push(FunctionArgument::from_value(inspect_4495_parser_missing(
            17,
            "first primary",
        )));
        args.push(FunctionArgument::from_value(
            inspect_4495_parser_missing(19, "private-4495 second").into_secret(),
        ));
        inspect_4495_assert_required(call, Some((17, "env(): first primary", false)));
    }
}

#[test]
fn inspect_4495_required_interpolation_order_masking_auxiliary_and_coalesce_pins() {
    for reversed in [false, true] {
        let nested = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("private-4495 prefix".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Secret(
                Box::new(inspect_4495_parser_missing(31, "private-4495 first").into_secret())
                    .into(),
            ))),
            InterpolationPart::Expr(inspect_4495_parser_missing(32, "second")),
        ]));
        let mut entries = vec![
            ("a".into(), nested),
            (
                "z".into(),
                inspect_4495_parser_missing(9, "earlier line, later key"),
            ),
        ];
        if reversed {
            entries.reverse();
        }
        // map_entries restores insertion order (value_walk/engine.rs:409); errors follow map visits, not source lines.
        let expected = if reversed {
            (9, "env(): earlier line, later key", false)
        } else {
            (31, "invalid value inside secret(...)", true)
        };
        inspect_4495_assert_required(
            Value::Concrete(ConcreteValue::Map(entries.into_iter().collect())),
            Some(expected),
        );
    }
    let protected = inspect_4495_parser_missing(41, "private-4495 fallback").into_secret();
    for primary in [
        Value::Concrete(ConcreteValue::String("selected".into())),
        Value::resource_ref("origin", "identifier", vec![]),
    ] {
        inspect_4495_assert_required(
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(primary),
                default: Box::new(protected.clone()),
            }),
            None,
        );
    }
    inspect_4495_assert_required(
        protected,
        Some((41, "invalid value inside secret(...)", true)),
    );
    let auxiliary = Value::Deferred(DeferredValue::FunctionCall {
        name: BuiltinFunction::upper,
        args: vec![FunctionArgument::with_auxiliary_for_inspect_test(
            Value::Concrete(ConcreteValue::String("selected operand".into())),
            inspect_4495_parser_missing(3, "private-4495 auxiliary").into_secret(),
        )],
        source_line: 1.into(),
    });
    inspect_4495_assert_required(auxiliary, None);
    // Interpolation-part order outranks the source line of a later part.
    let error = reject_required_interpolation_unavailable(
        &[
            InterpolationPart::Expr(inspect_4495_parser_missing(51, "first part")),
            InterpolationPart::Expr(inspect_4495_parser_missing(2, "later part")),
        ],
        strict_resolution_context(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid expression at line 51: env(): first part"
    );
    assert!(matches!(
        error,
        ParseError::InvalidExpression { line: 51, .. }
    ));
}

#[test]
fn inspect_4495_evaluated_body_failure_and_secret_result_pins() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("functions.crn");
    let source = "fn fail(x: String): String {\n  env(x)\n}\nfn protect(x: String): String {\n  secret(\"${x}-suffix\")\n}\n";
    std::fs::write(&file, source).unwrap();
    let parsed = crate::parser::parse_file(source, &file, &context()).unwrap();
    let scope = std::sync::Arc::new(parsed.user_functions.clone());
    let error = super::super::functions::evaluate_resolved_user_function(
        &parsed.user_functions["fail"],
        &scope,
        &[FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::String("CARINA_INSPECT_4495_MISSING".into()),
        ))],
        &context(),
        None,
        99,
    )
    .unwrap_err();
    let diagnostic = error.element_diagnostic().expect("body owns its failure");
    assert_eq!(diagnostic.location().file().path(), file);
    assert_eq!(diagnostic.location().span().start_line(), 2);
    assert_eq!(
        diagnostic.detail().to_string(),
        "Invalid expression at line 2: environment variable 'CARINA_INSPECT_4495_MISSING' is not set"
    );
    assert!(matches!(
        diagnostic.detail(),
        ParseError::InvalidExpression { line: 2, .. }
    ));
    assert_eq!(
        error.to_string(),
        format!(
            "{}:2:3: Invalid expression at line 2: environment variable 'CARINA_INSPECT_4495_MISSING' is not set",
            file.display()
        )
    );

    let protected = super::super::functions::evaluate_resolved_user_function(
        &parsed.user_functions["protect"],
        &scope,
        &[FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::String("private-4495".into()),
        ))],
        &context(),
        None,
        99,
    )
    .unwrap();
    let Value::Deferred(DeferredValue::Secret(inner)) = &protected else {
        panic!("evaluated Secret result lost protection")
    };
    assert!(
        matches!(inner.expose_plaintext(), Value::Concrete(ConcreteValue::String(text)) if text == "private-4495-suffix")
    );
    assert!(!format!("{protected:?}").contains("private-4495"));
    // Binding expansion and saved verification must preserve the evaluated
    // Secret too; use the normal root resolver to cross that phase boundary.
    let mut parsed = parsed;
    parsed.variables.insert("protected".into(), protected);
    let runtime = parsed.resolve_root_for_test().variables["protected"]
        .clone()
        .into_expanded()
        .unwrap();
    let saved = crate::value::redact_secrets_in_value(&runtime).unwrap();
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("private-4495")
    );
    let restored = crate::value::rehydrate_redacted_secrets(&saved, &runtime).unwrap();
    assert!(matches!(
        restored,
        GenericValue::Deferred(DeferredValue::Secret(_))
    ));
    assert!(!format!("{restored:?}").contains("private-4495"));
}

#[test]
fn inspect_4495_required_directory_attempted_input_pin() {
    let directory = tempfile::tempdir().unwrap();
    for (name, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "functions.crn",
            "fn unused(): String { secret(env('PRIVATE_UNUSED_4495')) }\n",
        ),
        (
            "main.crn",
            "// directory input fixture\nlet result = \"prefix-${env('CARINA_INSPECT_4495_MISSING')}\"\n",
        ),
        ("exports.crn", "attributes { output: String = result }\n"),
    ] {
        std::fs::write(directory.path().join(name), source).unwrap();
    }
    let failure = crate::config_loader::load_configuration_report(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
        &crate::schema::SchemaRegistry::new(),
    )
    .err()
    .expect("a demanded input must fail");
    assert_eq!(failure.diagnostics().len(), 1);
    let crate::config_loader::ConfigurationLoadDiagnostic::Parse { file, error } =
        &failure.diagnostics()[0]
    else {
        panic!("expected the original ParseError")
    };
    assert_eq!(
        file.as_deref().or_else(|| error.source_file()),
        Some(directory.path().join("main.crn").as_path())
    );
    let first = error
        .element_diagnostic()
        .map_or(error.as_ref(), |finding| finding.detail());
    assert_eq!(
        first.to_string(),
        "Invalid expression at line 2: env(): environment variable 'CARINA_INSPECT_4495_MISSING' is not set"
    );
    assert!(matches!(
        first,
        ParseError::InvalidExpression { line: 2, .. }
    ));
}
