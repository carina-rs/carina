use super::*;
use crate::resource::{BindingAddress, ConcreteValue, DeferredValue, Value, ValueKind};

#[derive(Default)]
struct Count {
    scalars: usize,
    secrets: usize,
    paths: Vec<(crate::schema::LeafPath, bool)>,
    numbers: std::collections::BTreeMap<i64, usize>,
    leaves: Vec<LeafKind>,
    expressions: Vec<ExpressionKind>,
    scalar_kinds: Vec<ScalarKind>,
    reject_secret: bool,
    placeholder_count: usize,
    reference_count: usize,
}

impl Algebra<Inspect> for Count {
    type Output = usize;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Inspect, P, B>,
    ) -> Step<Inspect, usize, P, B> {
        self.scalars += 1;
        self.scalar_kinds.push(value.kind());
        self.paths.push((at.path.clone(), at.is_secret()));
        if let ScalarInput::Int(number) = &value.input {
            *self.numbers.entry(**number).or_default() += 1;
        }
        value.finish(1)
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        items: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, usize, P, B> {
        items.inspect(self, |children| 1 + children.into_iter().sum::<usize>())
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        entries: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, usize, P, B> {
        entries.inspect(self, |children| 1 + children.into_iter().sum::<usize>())
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        expr: ExprChildren<'_, Inspect, P, B>,
    ) -> Step<Inspect, usize, P, B> {
        self.expressions.push(expr.kind());
        expr.inspect(self, |children| 1 + children.into_iter().sum::<usize>())
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        leaf: ScopedLeaf<'_, Inspect, P, B>,
    ) -> Step<Inspect, usize, P, B> {
        self.leaves.push(leaf.kind());
        self.placeholder_count += usize::from(leaf.with_placeholder(|slot| slot.is_some()));
        self.reference_count += usize::from(leaf.with_reference(|reference| reference.is_some()));
        leaf.finish(1)
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Inspect, P, B>,
    ) -> SecretStep<Inspect, usize, P, B> {
        self.secrets += 1;
        if self.reject_secret {
            return child.reject();
        }
        child.inspect(self).map(|count| 1 + count)
    }
}

fn every_edge_fixture() -> crate::resource::AuthoredValue {
    use crate::{
        builtins::BuiltinFunction,
        integer_arithmetic::{IntegerOp, IntegerOperatorSpan, ValidatedIntegerExpr},
        parser::{DeferredIfCondition, DeferredValueIf},
        resource::{
            AuthoredValue, DiagnosticSourceLine, FunctionArgument, InterpolationPart,
            ParserPlaceholder, PathSegment, RelativeBindingName, UnavailableReason,
        },
    };
    fn atom(n: i64) -> AuthoredValue {
        Value::Concrete(ConcreteValue::Int(n))
    }
    let reference = || AuthoredValue::resource_ref("producer", "number", vec![]);
    let access = crate::resource::apply_postfix_access(
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "record".into(),
        })),
        PathSegment::Field {
            name: "field".into(),
        },
        7,
        crate::resource::AccessMissPolicy::Error,
    )
    .unwrap();
    let parsed = crate::parser::parse_file(
        "fn captured(input: Int = 71): Int {\n let local = 72\n 73\n}\n",
        std::path::Path::new("captured.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let function = std::sync::Arc::new(parsed.user_functions["captured"].clone());
    let source_line = DiagnosticSourceLine::authored(9);
    Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "entry".into() => atom(1) },
        )),
        Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
            DeferredIfCondition::new(
                Value::Concrete(ConcreteValue::Bool(true)),
                "if.crn".into(),
                1,
            ),
            atom(2),
            atom(3),
        )))),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(atom(4)),
            default: Box::new(atom(5)),
        }),
        Value::Deferred(DeferredValue::IntegerArithmetic(
            ValidatedIntegerExpr::new(
                atom(6),
                IntegerOp::Add,
                atom(7),
                IntegerOperatorSpan::synthetic(),
            )
            .unwrap(),
        )),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix".into()),
            InterpolationPart::Expr(atom(8)),
        ])),
        Value::Deferred(DeferredValue::FunctionCall {
            name: BuiltinFunction::join,
            args: vec![FunctionArgument::from_value(atom(9))],
            source_line,
        }),
        Value::Deferred(DeferredValue::Unavailable {
            name: BuiltinFunction::env,
            args: vec![FunctionArgument::from_value(atom(10))],
            reason: UnavailableReason::from_builtin_error("unavailable".into()),
            source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: std::sync::Arc::new(std::collections::HashMap::from([(
                "captured".into(),
                (*function).clone(),
            )])),
            args: vec![FunctionArgument::from_value(atom(11))],
            source_line,
        }),
        access,
        Value::Deferred(DeferredValue::Secret(Box::new(atom(12)).into())),
        reference(),
        Value::Deferred(DeferredValue::BindingRef {
            binding: RelativeBindingName::new("producer"),
        }),
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "input".into(),
        })),
    ]))
}

#[test]
fn kernel_covers_every_value_bearing_edge() {
    let value = every_edge_fixture();
    let mut count = Count::default();
    assert_eq!(
        inspect(WalkRoot::syntactic(), &value, &mut count)
            .unwrap()
            .into_output(),
        35
    );
    // 12 numbered operand leaves, the condition, and three leaves in each
    // of the two distinct captured function definitions.
    assert_eq!(count.scalars, 19, "a value-bearing edge was skipped");
    assert_eq!(count.secrets, 1);
    assert_eq!(count.placeholder_count, 2);
    assert_eq!(count.reference_count, 2);
    assert_eq!(
        count.numbers,
        (1..=12)
            .map(|n| (n, 1))
            .chain((71..=73).map(|n| (n, 2)))
            .collect()
    );
    assert_eq!(
        count.leaves,
        [
            LeafKind::Placeholder,
            LeafKind::ResourceRef,
            LeafKind::BindingRef,
            LeafKind::Unknown,
            LeafKind::Placeholder
        ]
    );
    assert_eq!(
        count.expressions,
        [
            ExpressionKind::ModuleIf,
            ExpressionKind::Coalesce,
            ExpressionKind::IntegerArithmetic,
            ExpressionKind::Interpolation,
            ExpressionKind::FunctionCall,
            ExpressionKind::Unavailable,
            ExpressionKind::ResolvedFunctionCall,
            ExpressionKind::Access
        ]
    );
}

#[test]
fn transform_rewrites_all_operands_and_captured_function_values() {
    let value = every_edge_fixture();
    let rewritten = transform(WalkRoot::syntactic(), value, &mut ReplaceInts::default())
        .unwrap()
        .into_value();
    let mut count = Count::default();
    inspect(WalkRoot::syntactic(), &rewritten, &mut count).unwrap();
    assert_eq!(count.numbers, [(41, 18)].into());
    assert_eq!(count.secrets, 1);
}

#[test]
fn rejecting_secret_cannot_reach_its_child_or_finish_successfully() {
    let value = secret(int(2));
    let mut count = Count {
        reject_secret: true,
        ..Count::default()
    };
    assert!(matches!(
        inspect(WalkRoot::syntactic(), &value, &mut count),
        Err(WalkError::Rejected)
    ));
    assert_eq!((count.secrets, count.scalars), (1, 0));
}

#[test]
fn located_summary_remembers_secret_descendants() {
    let value = Value::Concrete(ConcreteValue::List(vec![secret(int(2))]));
    assert!(
        inspect(WalkRoot::syntactic(), &value, &mut Count::default())
            .unwrap()
            .is_secret()
    );
}

#[test]
fn transform_cannot_forge_validated_arithmetic_with_nonnumeric_operands() {
    use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, ValidatedIntegerExpr};
    let value = secret(Value::Deferred(DeferredValue::IntegerArithmetic(
        ValidatedIntegerExpr::new(
            int(1),
            IntegerOp::Add,
            int(2),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap(),
    )));
    assert!(
        transform(
            WalkRoot::syntactic(),
            value,
            &mut ReplaceInts { invalid: true }
        )
        .is_err()
    );
}

#[test]
fn every_concrete_variant_is_visited_once() {
    use crate::resource::{CanonicalEnumValue, RawEnumIdentifier};
    let values: Vec<Value> = vec![
        Value::Concrete(ConcreteValue::String("text".into())),
        Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
            "choice",
        ))),
        Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(crate::schema::TypeIdentity::bare("Choice"), "choice"),
        )),
        int(1),
        Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
        Value::Concrete(ConcreteValue::Bool(true)),
        Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(30))),
        Value::Concrete(ConcreteValue::StringList(vec!["one".into(), "two".into()])),
        Value::Concrete(ConcreteValue::List(vec![int(2)])),
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "key".into() => int(3) },
        )),
    ];
    let mut count = Count::default();
    let visited = inspect(
        WalkRoot::syntactic(),
        &Value::Concrete(ConcreteValue::List(values)),
        &mut count,
    )
    .unwrap()
    .into_output();
    assert_eq!(visited, 13);
    assert_eq!(count.scalars, 10); // StringList is one Value, not two child Values.
    assert_eq!(
        count.scalar_kinds,
        [
            ScalarKind::String,
            ScalarKind::EnumIdentifier,
            ScalarKind::CanonicalEnum,
            ScalarKind::Int,
            ScalarKind::Float,
            ScalarKind::Bool,
            ScalarKind::Duration,
            ScalarKind::StringList,
            ScalarKind::Int,
            ScalarKind::Int
        ]
    );
}

#[test]
fn secret_keeps_nested_list_map_and_struct_paths() {
    use crate::schema::{
        AttributeSchema, AttributeType, LeafPath, LeafPathSegment as S, ResourceSchema, StructField,
    };
    let schema = ResourceSchema::new("test.Record")
        .attribute(
            AttributeSchema::required(
                "config",
                AttributeType::struct_(
                    "Config",
                    vec![
                        StructField::required(
                            "items",
                            AttributeType::list(AttributeType::map(AttributeType::struct_(
                                "Item",
                                vec![StructField::required("count", AttributeType::int()).build()],
                            ))),
                        )
                        .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let value = secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "items".into() => Value::Concrete(ConcreteValue::List(vec![Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "a.b[0]".into() => Value::Concrete(ConcreteValue::Map(indexmap::indexmap! { "count".into() => int(1) }))
        }))]))
    })));
    let mut count = Count::default();
    inspect(
        WalkRoot::attribute(&schema, "config").unwrap(),
        &value,
        &mut count,
    )
    .unwrap();
    assert_eq!(
        count.paths,
        [(
            LeafPath::from_segments([
                S::StructField("items".into()),
                S::ListIndex(0),
                S::MapKey("a.b[0]".into()),
                S::StructField("count".into()),
            ]),
            true
        )]
    );
}

fn int(value: i64) -> Value {
    Value::Concrete(ConcreteValue::Int(value))
}
fn secret(value: Value) -> Value {
    Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
}

#[test]
fn kernel_requires_secret_reentry() {
    let value = Value::Concrete(ConcreteValue::List(vec![int(1), secret(int(2))]));
    let mut count = Count::default();
    assert_eq!(
        inspect(WalkRoot::syntactic(), &value, &mut count)
            .unwrap()
            .into_output(),
        4
    );
    assert_eq!((count.scalars, count.secrets), (2, 1));
    assert_eq!(
        count
            .paths
            .iter()
            .map(|(_, scope)| *scope)
            .collect::<Vec<_>>(),
        [false, true]
    );
}

#[test]
fn transform_restores_secret_wrapper() {
    let result = transform(
        WalkRoot::syntactic(),
        secret(int(1)),
        &mut ReplaceInts::default(),
    )
    .unwrap()
    .into_value();
    assert!(matches!(result,
        Value::Deferred(DeferredValue::Secret(payload))
            if payload.plaintext_for_test() == &int(41)
    ));
}

#[derive(Default)]
struct ReplaceInts {
    invalid: bool,
}
impl Algebra<Transform> for ReplaceInts {
    type Output = ();
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        scalar: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, (), P, B> {
        if scalar.kind() == ScalarKind::Int {
            if self.invalid {
                scalar.replace(
                    Value::Concrete(ConcreteValue::String("invalid-operand".into())),
                    (),
                )
            } else {
                scalar.replace(Value::Concrete(ConcreteValue::Int(41)), ())
            }
        } else {
            scalar.keep(())
        }
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, (), P, B> {
        children.transform(self, |_| ())
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, (), P, B> {
        children.transform(self, |_| ())
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, (), P, B> {
        children.transform(self, |_| ())
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, (), P, B> {
        leaf.keep(())
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, (), P, B> {
        child.transform(self)
    }
}

// The same owner uses inspection and rewriting. Its inventories are external;
// only handles and kernel-issued positions travel through the fold result.
#[derive(Default)]
struct Carry {
    issued: usize,
    references: HashMap<usize, String>,
    expressions: Vec<ExpressionKind>,
    secret_entries: usize,
}
impl Carry {
    fn issue(&mut self) -> usize {
        self.issued += 1;
        self.issued
    }
}
macro_rules! carry_algebra {
    ($mode:ty, $finish:ident, $walk:ident) => {
        impl Algebra<$mode> for Carry {
            type Output = Evidence<usize, usize>;
            fn scalar<P: ValueKind, B: BindingAddress>(
                &mut self,
                at: &At<'_>,
                value: ScopedScalar<'_, $mode, P, B>,
            ) -> Step<$mode, Self::Output, P, B> {
                let id = self.issue();
                value.$finish(Evidence::at(at, [id], [id + 1000]))
            }
            fn list<P: ValueKind, B: BindingAddress>(
                &mut self,
                _: &At<'_>,
                children: Children<'_, $mode, P, B>,
            ) -> Step<$mode, Self::Output, P, B> {
                children.$walk(self, Evidence::merge)
            }
            fn map<P: ValueKind, B: BindingAddress>(
                &mut self,
                _: &At<'_>,
                children: Children<'_, $mode, P, B>,
            ) -> Step<$mode, Self::Output, P, B> {
                children.$walk(self, Evidence::merge)
            }
            fn expression<P: ValueKind, B: BindingAddress>(
                &mut self,
                _: &At<'_>,
                expression: ExprChildren<'_, $mode, P, B>,
            ) -> Step<$mode, Self::Output, P, B> {
                self.expressions.push(expression.kind());
                let id = self.issue();
                expression.with_evidence([id], [id + 1000]).$walk(self)
            }
            fn placeholder<P: ValueKind, B: BindingAddress>(
                &mut self,
                at: &At<'_>,
                leaf: ScopedLeaf<'_, $mode, P, B>,
            ) -> Step<$mode, Self::Output, P, B> {
                let id = self.issue();
                leaf.with_reference(|reference| {
                    let binding = match reference {
                        Some(ReferenceRef::Resource(path)) => Some(path.binding()),
                        Some(ReferenceRef::Binding(binding)) => Some(binding.as_str()),
                        None => None,
                    };
                    if let Some(binding) = binding {
                        self.references.insert(id, binding.to_owned());
                    }
                });
                leaf.$finish(Evidence::at(at, [id], [id + 1000]))
            }
            fn secret<P: ValueKind, B: BindingAddress>(
                &mut self,
                _: &SecretAt<'_>,
                child: SecretChild<'_, $mode, P, B>,
            ) -> SecretStep<$mode, Self::Output, P, B> {
                self.secret_entries += 1;
                child.$walk(self)
            }
        }
    };
}
carry_algebra!(Inspect, finish, inspect);
carry_algebra!(Transform, keep, transform);

type EvidenceSnapshot = Vec<(usize, LeafPath, Vec<ExpressionEdge>, bool)>;
fn evidence_snapshot(items: &[Scoped<usize>]) -> EvidenceSnapshot {
    items
        .iter()
        .map(|item| {
            (
                *item.output(),
                item.path.clone(),
                item.expression_edges().to_vec(),
                item.is_secret(),
            )
        })
        .collect()
}

#[test]
fn transform_preserves_evidence_on_every_container_argument_and_expression_edge() {
    let value = every_edge_fixture();
    let inspected = inspect(WalkRoot::syntactic(), &value, &mut Carry::default())
        .unwrap()
        .into_output();
    let result = transform(WalkRoot::syntactic(), value.clone(), &mut Carry::default()).unwrap();
    assert!(result.is_secret());
    let (rewritten, located) = result.into_parts();
    assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
    assert!(located.is_secret());
    let output = located.into_output();
    assert_eq!(
        evidence_snapshot(output.obligations()),
        evidence_snapshot(inspected.obligations())
    );
    assert_eq!(
        evidence_snapshot(output.receipts()),
        evidence_snapshot(inspected.receipts())
    );
    assert_eq!(output.receipts().len(), 32);
    assert!(output.receipts().iter().any(|receipt| {
        receipt
            .expression_edges()
            .contains(&ExpressionEdge::Argument(0))
    }));
}

#[test]
fn generic_expression_edge_separates_value_obligations_receipts_and_protection() {
    let value = secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "private.key[0]".into() => Value::Concrete(ConcreteValue::List(vec![int(5)])),
    })));
    let result = transform_expression_edge(
        WalkRoot::syntactic(),
        ExpressionEdge::EvidenceOperand,
        value.clone(),
        [10_000_usize],
        [20_000_usize],
        &mut Carry::default(),
    )
    .unwrap();
    let (rewritten, located) = result.into_parts();
    assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
    assert!(located.is_secret());
    let output = located.into_output();
    assert_eq!(*output.obligations()[0].output(), 10_000);
    assert_eq!(*output.receipts()[0].output(), 20_000);
    // The outer obligation is authored outside Secret, while operand evidence
    // retains its protected scope and the exact result path inside the value.
    assert!(!output.obligations()[0].is_secret());
    assert_eq!(
        output.obligations()[0].public_path().unwrap().segments(),
        []
    );
    for item in [&output.obligations()[1], &output.receipts()[1]] {
        assert!(item.is_secret());
        assert!(item.public_path().is_none());
        assert_eq!(item.expression_edges(), [ExpressionEdge::EvidenceOperand]);
        assert_eq!(
            item.path,
            LeafPath::from_segments([
                LeafPathSegment::MapKey("private.key[0]".into()),
                LeafPathSegment::ListIndex(0),
            ])
        );
    }
    let mut carry = Carry::default();
    let inspected = inspect_expression_edge(
        WalkRoot::syntactic(),
        ExpressionEdge::EvidenceOperand,
        &value,
        [10_000_usize],
        [20_000_usize],
        &mut carry,
    )
    .unwrap()
    .into_output();
    assert_eq!(
        evidence_snapshot(inspected.receipts()),
        evidence_snapshot(output.receipts())
    );
    assert_eq!(carry.secret_entries, 1);
}

#[test]
fn authored_evidence_visits_unselected_branches_and_fallback_without_evaluation() {
    use crate::{
        builtins::BuiltinFunction, parser::DeferredValueIf, resource::DiagnosticSourceLine,
    };
    let value: Value = Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
        false_condition("branch.crn", 1),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(int(10)),
            default: Box::new(Value::Deferred(DeferredValue::Unavailable {
                name: BuiltinFunction::env,
                args: vec![int(11).into()],
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "must not be evaluated".into(),
                ),
                source_line: DiagnosticSourceLine::authored(3),
            })),
        }),
        int(12),
    ))));
    let mut carry = Carry::default();
    let output = inspect(WalkRoot::syntactic(), &value, &mut carry)
        .unwrap()
        .into_output();
    assert_eq!(
        carry.expressions,
        [
            ExpressionKind::ModuleIf,
            ExpressionKind::Coalesce,
            ExpressionKind::Unavailable
        ]
    );
    assert_eq!(output.obligations().len(), 7);
    assert!(
        output
            .obligations()
            .iter()
            .any(|item| item.expression_edges().contains(&ExpressionEdge::Then))
    );
    assert!(
        output
            .obligations()
            .iter()
            .any(|item| item.expression_edges().contains(&ExpressionEdge::Else))
    );
    assert!(output.obligations().iter().any(|item| {
        item.expression_edges()
            .contains(&ExpressionEdge::CoalesceDefault)
    }));
    let rewritten = transform(WalkRoot::syntactic(), value.clone(), &mut Carry::default())
        .unwrap()
        .into_value();
    assert_eq!(
        format!("{rewritten:?}"),
        format!("{value:?}"),
        "structural traversal must retain both unselected computations"
    );
}

#[test]
fn per_element_directive_edge_exposes_unevaluated_body_and_stage_slots() {
    use crate::{
        parser::DeferredValueIf,
        resource::{LifecycleDirective, LoopSlot, RelativeBindingName},
    };
    let value: Value<LoopSlot, RelativeBindingName> =
        Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
            false_condition("loop.crn", 4),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value))),
                default: Box::new(Value::Concrete(ConcreteValue::Int(2))),
            }),
            Value::Concrete(ConcreteValue::Int(3)),
        ))));
    let edge = ExpressionEdge::DirectiveBody(LifecycleDirective::PreventDestroy);
    let inspected = inspect_expression_edge(
        WalkRoot::syntactic(),
        edge.clone(),
        &value,
        [100_usize],
        [],
        &mut Carry::default(),
    )
    .unwrap()
    .into_output();
    assert_eq!(inspected.obligations().len(), 7);
    assert!(
        inspected
            .obligations()
            .iter()
            .skip(1)
            .all(|item| item.expression_edges().first() == Some(&edge))
    );
    let (rewritten, output) = transform_expression_edge(
        WalkRoot::syntactic(),
        edge,
        value.clone(),
        [100_usize],
        [],
        &mut Carry::default(),
    )
    .unwrap()
    .into_parts();
    assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
    assert_eq!(
        evidence_snapshot(inspected.obligations()),
        evidence_snapshot(output.into_output().obligations())
    );
}

#[test]
fn obligation_binding_worklist_preserves_scope_and_leaves_cycle_policy_with_owner() {
    use crate::resource::RelativeBindingName;
    type V = Value<NoPlaceholder, RelativeBindingName>;
    fn binding(name: &str) -> V {
        Value::Deferred(DeferredValue::BindingRef {
            binding: name.into(),
        })
    }
    let values = HashMap::from([
        ("a", binding("b")),
        (
            "b",
            Value::Concrete(ConcreteValue::List(vec![
                binding("a"),
                Value::Concrete(ConcreteValue::Int(7)),
            ])),
        ),
    ]);
    let root = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "private-map-key".into() => binding("a"),
    }))
    .into_secret();
    let mut carry = Carry::default();
    let seed = inspect(WalkRoot::syntactic(), &root, &mut carry)
        .unwrap()
        .into_output();
    let mut pending = seed.obligations().to_vec();
    let mut visited = std::collections::HashSet::new();
    let mut hops = 0;
    let mut receipts = Vec::new();
    while let Some(site) = pending.pop() {
        let Some(binding) = carry.references.get(site.output()).cloned() else {
            continue;
        };
        assert!(site.is_secret());
        if !visited.insert((binding.clone(), site.is_secret())) {
            continue;
        }
        hops += 1;
        let located = site.inspect(&values[binding.as_str()], &mut carry).unwrap();
        assert!(located.is_secret());
        let output = located.into_output();
        pending.extend_from_slice(output.obligations());
        receipts.extend_from_slice(output.receipts());
    }
    assert_eq!(hops, 2);
    assert_eq!(visited.len(), 2);
    assert_eq!(carry.secret_entries, 1);
    assert!(
        receipts
            .iter()
            .all(|item| item.is_secret() && item.public_path().is_none())
    );
    assert!(
        receipts
            .iter()
            .any(|item| item.path.segments().last() == Some(&LeafPathSegment::ListIndex(1)))
    );
}

#[test]
fn identity_rewrite_preserves_knowledge_classification_on_a_value_table() {
    use crate::resource::{RelativeBindingName, knowledge::KnowledgeContext};
    type V = Value<NoPlaceholder, RelativeBindingName>;
    let reference = V::resource_ref("producer", "id", vec![]);
    let values: Vec<V> = vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::String("literal".into())),
        Value::Concrete(ConcreteValue::Int(2)).into_secret(),
        reference.clone(),
        Value::Concrete(ConcreteValue::List(vec![reference.clone()])),
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "member".into() => reference.clone() },
        )),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(reference),
            default: Box::new(Value::Concrete(ConcreteValue::Int(3))),
        }),
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
    ];
    for value in values {
        let before = value
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let (rewritten, _) = transform(WalkRoot::syntactic(), value.clone(), &mut Carry::default())
            .unwrap()
            .into_parts();
        let after = rewritten
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        assert_eq!(
            std::mem::discriminant(&before),
            std::mem::discriminant(&after)
        );
        assert_eq!(before.into_known().is_ok(), after.into_known().is_ok());
        assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
    }
}

#[test]
fn protected_rejection_errors_contain_no_value_or_path_text() {
    let value = secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "private-key".into() => int(1),
    })));
    let error = inspect(
        WalkRoot::syntactic(),
        &value,
        &mut Count {
            reject_secret: true,
            ..Count::default()
        },
    )
    .err()
    .unwrap();
    assert_eq!(error, WalkError::Rejected);
    assert!(!format!("{error:?} {error}").contains("private"));
}

fn false_condition<P: ValueKind, B: BindingAddress>(
    file: &str,
    line: usize,
) -> crate::parser::DeferredIfCondition<P, B> {
    let authored = crate::parser::DeferredIfCondition::new(
        Value::Concrete(ConcreteValue::Bool(false)),
        file.into(),
        line,
    );
    crate::parser::DeferredIfCondition::new_with_source_expression(
        Value::Concrete(ConcreteValue::Bool(false)),
        file.into(),
        line,
        authored.source_expression,
    )
}

#[test]
fn identity_rewrite_preserves_authored_loop_access_metadata_without_value_equality() {
    use crate::resource::{LoopSlot, PathSegment};
    let value = crate::resource::parser_deferred_for_access(
        "item".into(),
        LoopSlot::Value,
        PathSegment::Field {
            name: "attribute".into(),
        },
        44,
    );
    let (rewritten, output) = transform(WalkRoot::syntactic(), value, &mut Carry::default())
        .unwrap()
        .into_parts();
    assert!(matches!(rewritten,
        Value::Deferred(DeferredValue::Access(access))
            if access.display_root() == Some("item")
                && access.source_line() == 44
                && matches!(access.base(), Value::Deferred(DeferredValue::Placeholder(
                    crate::resource::ParserPlaceholder::Loop(LoopSlot::Value)
                )))
    ));
    assert_eq!(output.into_output().receipts().len(), 2);
}

#[test]
fn kernel_sources_have_no_panicking_dispatch_fallbacks() {
    let forbidden =
        regex::Regex::new(r"\b(?:unreachable|panic|todo|unimplemented)\s*!|\bexpect\s*\(").unwrap();
    let unwrap = regex::Regex::new(r"\.\s*unwrap\s*\(").unwrap();
    for name in ["unreachable", "panic", "todo", "unimplemented"] {
        assert!(forbidden.is_match(&format!("{name}!()")));
        assert!(forbidden.is_match(&format!("{name} \n !()")));
    }
    assert!(forbidden.is_match(&format!("value.{}(message)", "expect")));
    assert!(unwrap.is_match(&format!("value.{}()", "unwrap")));
    let mut pending = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/value_walk")];
    let mut checked = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "md")
            {
                let source = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !forbidden.is_match(&source),
                    "forbidden panic fallback in {}",
                    path.display()
                );
                if path.extension().is_some_and(|ext| ext == "rs")
                    && path.file_name().is_some_and(|name| name != "tests.rs")
                {
                    assert!(
                        !unwrap.is_match(&source),
                        "unchecked extraction in {}",
                        path.display()
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(
        checked >= 4,
        "the guard must cover the entire kernel directory"
    );
}

#[test]
fn typed_interpolation_and_container_parts_preserve_empty_and_mixed_shapes() {
    use crate::resource::InterpolationPart::{Expr, Literal};
    let values: Vec<Value> = vec![
        Value::Concrete(ConcreteValue::List(Vec::new())),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::new())),
        Value::Deferred(DeferredValue::Interpolation(Vec::new())),
        Value::Deferred(DeferredValue::Interpolation(vec![
            Literal("one".into()),
            Literal("two".into()),
        ])),
        Value::Deferred(DeferredValue::Interpolation(vec![
            Expr(int(1)),
            Literal("separator".into()),
            Expr(secret(int(2))),
            Literal("tail".into()),
            Expr(int(3)),
        ])),
    ];
    for value in values {
        let inspected = inspect(WalkRoot::syntactic(), &value, &mut Carry::default()).unwrap();
        let (rewritten, transformed) =
            transform(WalkRoot::syntactic(), value.clone(), &mut Carry::default())
                .unwrap()
                .into_parts();
        assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
        assert_eq!(inspected.is_secret(), transformed.is_secret());
        let inspected = inspected.into_output();
        let transformed = transformed.into_output();
        assert_eq!(
            evidence_snapshot(inspected.obligations()),
            evidence_snapshot(transformed.obligations())
        );
        assert_eq!(
            evidence_snapshot(inspected.receipts()),
            evidence_snapshot(transformed.receipts())
        );
    }
}

#[derive(Default)]
struct Redactor {
    inventory: Carry,
    hashed: usize,
}
impl Algebra<Transform> for Redactor {
    type Output = Evidence<usize, usize>;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        Algebra::<Transform>::scalar(&mut self.inventory, at, value)
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        children.transform(self, Evidence::merge)
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        children.transform(self, Evidence::merge)
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        expression: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        let id = self.inventory.issue();
        expression.with_evidence([id], [id + 1000]).transform(self)
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        Algebra::<Transform>::placeholder(&mut self.inventory, at, leaf)
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, Self::Output, P, B> {
        let hashed = &mut self.hashed;
        child.redact(&mut self.inventory, |protected| {
            *hashed += 1;
            protected.hash_snapshot::<NoPlaceholder>(None)
        })
    }
}

fn protected_expression_fixture() -> Value {
    Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "public".into() => int(1),
        "payload".into() => secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "private.map.key".into() => Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::join,
                args: vec![Value::Concrete(ConcreteValue::String("private-value".into())).into()],
                source_line: crate::resource::DiagnosticSourceLine::synthetic(),
            }),
            "another.private.key".into() => secret(int(2)),
        }))),
    }))
}

#[test]
fn secret_redaction_enters_first_retains_protected_evidence_and_outputs_only_hashes() {
    let original = protected_expression_fixture();
    let snapshot = original.to_snapshot();
    let expected = inspect(WalkRoot::syntactic(), &snapshot, &mut Carry::default())
        .unwrap()
        .into_output();
    let mut redactor = Redactor::default();
    let (redacted, located) = transform(WalkRoot::syntactic(), snapshot, &mut redactor)
        .unwrap()
        .into_parts();
    assert!(located.is_secret());
    let output = located.into_output();
    assert_eq!(
        evidence_snapshot(output.obligations()),
        evidence_snapshot(expected.obligations())
    );
    assert_eq!(
        evidence_snapshot(output.receipts()),
        evidence_snapshot(expected.receipts())
    );
    assert_eq!(redactor.hashed, 1);
    assert_eq!(
        redactor.inventory.secret_entries, 1,
        "nested Secret is inspected before the enclosing hash is produced"
    );
    assert!(output.obligations().iter().skip(1).all(Scoped::is_secret));
    let json = serde_json::to_string(&redacted).unwrap();
    let debug = format!("{redacted:?}");
    for forbidden in ["private.map.key", "another.private.key", "private-value"] {
        assert!(!json.contains(forbidden));
        assert!(!debug.contains(forbidden));
    }
    let hash = redacted.get("payload").unwrap().secret_hash().unwrap();
    let Value::Concrete(ConcreteValue::Map(map)) = original else {
        return;
    };
    assert!(
        matches!(&map["payload"], Value::Deferred(DeferredValue::Secret(payload))
        if payload.verify_hash(hash).unwrap())
    );
}

#[test]
fn redaction_does_not_invoke_the_owner_after_an_inner_rejection() {
    let payload = protected_expression_fixture().to_snapshot();
    let child = SecretChild::<Transform, _, _> {
        payload: Box::new(payload).into(),
        at: WalkRoot::syntactic().at().protected(),
    };
    let mut calls = 0;
    let step = child.redact(
        &mut Count {
            reject_secret: true,
            ..Count::default()
        },
        |protected| {
            calls += 1;
            protected.hash_snapshot::<NoPlaceholder>(None)
        },
    );
    assert!(matches!(step.step.result, Err(WalkError::Rejected)));
    assert_eq!(calls, 0);
}

#[test]
fn protected_locator_freezes_without_keys_and_resolves_after_verified_rehydration() {
    let original = protected_expression_fixture().to_snapshot();
    let (redacted, output) = transform(
        WalkRoot::syntactic(),
        original.clone(),
        &mut Redactor::default(),
    )
    .unwrap()
    .into_parts();
    let output = output.into_output();
    let sites: Vec<_> = output
        .obligations()
        .iter()
        .filter(|site| site.is_secret())
        .collect();
    assert!(!sites.is_empty());
    let frozen: Vec<_> = sites
        .iter()
        .map(|site| (*site.output(), site.protected_locator().unwrap().clone()))
        .collect();
    let saved = serde_json::to_string(&(redacted, &frozen)).unwrap();
    for forbidden in ["private.map.key", "another.private.key", "private-value"] {
        assert!(!saved.contains(forbidden));
        assert!(!format!("{frozen:?}").contains(forbidden));
    }
    let (mut rehydrated, frozen): (
        crate::resource::SnapshotValue,
        Vec<(usize, ProtectedLocator)>,
    ) = serde_json::from_str(&saved).unwrap();
    for (id, locator) in &frozen {
        assert!(
            locator
                .resolve(WalkRoot::syntactic(), &rehydrated, *id)
                .is_err(),
            "hash placeholders are not rehydrated positions"
        );
    }
    // The owner verifies and restores the original Secret material before
    // asking the kernel to reattach the frozen evidence under the apply lock.
    let hash = rehydrated
        .get("payload")
        .unwrap()
        .secret_hash()
        .unwrap()
        .clone();
    let original_secret = original.get("payload").unwrap();
    let expected_hash = transform(
        WalkRoot::syntactic(),
        original_secret.clone(),
        &mut Redactor::default(),
    )
    .unwrap()
    .into_value();
    let material_hash = expected_hash.secret_hash().unwrap();
    assert!(
        matches!(original_secret, Value::Deferred(DeferredValue::Secret(payload))
        if ProtectedSecret { payload }.hash_snapshot::<NoPlaceholder>(Some(&hash)).unwrap() == hash)
    );
    assert!(!material_hash.encoded_digest().is_empty());
    assert!(matches!(rehydrated, Value::Concrete(ConcreteValue::Map(_))));
    if let Value::Concrete(ConcreteValue::Map(map)) = &mut rehydrated {
        map.insert("payload".into(), original_secret.clone());
    }
    for ((id, locator), expected) in frozen.into_iter().zip(sites) {
        let restored = locator
            .resolve(WalkRoot::syntactic(), &rehydrated, id)
            .unwrap();
        assert!(restored.is_secret());
        assert!(restored.public_path().is_none());
        assert_eq!(restored.path, expected.path);
        assert_eq!(restored.expression_edges(), expected.expression_edges());
        assert_eq!(restored.output(), expected.output());
    }
    let forged: ProtectedLocator = serde_json::from_str("[1,[],[]]").unwrap();
    assert!(
        forged
            .resolve(WalkRoot::syntactic(), &rehydrated, 1_usize)
            .is_err()
    );
}

#[derive(Default)]
struct LookupReceipts {
    inventory: Carry,
}
impl Algebra<Inspect> for LookupReceipts {
    type Output = Evidence<usize, usize>;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        Algebra::<Inspect>::scalar(&mut self.inventory, at, value)
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        children.inspect(self, Evidence::merge)
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        children.inspect(self, Evidence::merge)
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        expression
            .inspect(&mut self.inventory, Evidence::merge)
            .try_map_output(|evidence| {
                evidence.reroot_receipts(at, 0, |receipt| *receipt.output() == 1001)
            })
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        Algebra::<Inspect>::placeholder(&mut self.inventory, at, leaf)
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Inspect, P, B>,
    ) -> SecretStep<Inspect, Self::Output, P, B> {
        child.inspect(self)
    }
}

impl Algebra<Transform> for LookupReceipts {
    type Output = Evidence<usize, usize>;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        Algebra::<Transform>::scalar(&mut self.inventory, at, value)
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        children.transform(self, Evidence::merge)
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        children.transform(self, Evidence::merge)
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        expression
            .transform(&mut self.inventory, Evidence::merge)
            .try_map_output(|evidence| {
                evidence.reroot_receipts(at, 0, |receipt| *receipt.output() == 1001)
            })
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        Algebra::<Transform>::placeholder(&mut self.inventory, at, leaf)
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, Self::Output, P, B> {
        child.transform(self)
    }
}

#[test]
fn lookup_reroots_only_selected_receipts_and_never_removes_secret_provenance() {
    let value: Value = Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
        DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::lookup,
            args: vec![
                Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                    "v".into() => secret(Value::Deferred(DeferredValue::FunctionCall {
                        name: crate::builtins::BuiltinFunction::join, args: vec![int(7).into()],
                        source_line: crate::resource::DiagnosticSourceLine::synthetic(),
                    })),
                }))
                .into(),
                Value::Concrete(ConcreteValue::String("v".into())).into(),
                Value::Concrete(ConcreteValue::String("".into())).into(),
            ],
            source_line: crate::resource::DiagnosticSourceLine::synthetic(),
        },
    )]));
    let output = inspect(
        WalkRoot::syntactic(),
        &value,
        &mut LookupReceipts::default(),
    )
    .unwrap()
    .into_output();
    let rewritten = transform(
        WalkRoot::syntactic(),
        value.clone(),
        &mut LookupReceipts::default(),
    )
    .unwrap();
    let (rewritten, owned_output) = rewritten.into_parts();
    assert_eq!(format!("{rewritten:?}"), format!("{value:?}"));
    let owned_output = owned_output.into_output();
    assert_eq!(
        evidence_snapshot(owned_output.receipts()),
        evidence_snapshot(output.receipts())
    );
    assert_eq!(
        evidence_snapshot(owned_output.obligations()),
        evidence_snapshot(output.obligations())
    );
    let selected = output
        .receipts()
        .iter()
        .find(|receipt| *receipt.output() == 1001)
        .unwrap();
    assert_eq!(
        selected.path,
        LeafPath::from_segments([LeafPathSegment::ListIndex(0)])
    );
    assert!(selected.expression_edges().is_empty());
    assert!(selected.is_secret());
    assert!(selected.public_path().is_none());
    let obligation = output
        .obligations()
        .iter()
        .find(|obligation| *obligation.output() == 1)
        .unwrap();
    assert_eq!(obligation.path.segments().len(), 2);
    assert!(
        obligation
            .expression_edges()
            .contains(&ExpressionEdge::Argument(0))
    );
    assert!(
        output
            .receipts()
            .iter()
            .filter(|receipt| *receipt.output() != 1001)
            .all(|receipt| !receipt.expression_edges().is_empty())
    );
}

#[test]
fn rebasing_rejects_a_receipt_from_another_argument() {
    let at = WalkRoot::syntactic().at();
    let other_argument = at.operand(ExpressionEdge::Argument(1));
    let evidence: Evidence<usize, usize> = Evidence::at(&other_argument, [], [9]);
    assert!(matches!(
        evidence.reroot_receipts(&at, 0, |_| true),
        Err(WalkError::ReceiptOrigin)
    ));
    let call = at.list_child(0);
    let actual_argument = call.operand(ExpressionEdge::Argument(0));
    let evidence: Evidence<usize, usize> = Evidence::at(&actual_argument, [], [10]);
    assert!(
        matches!(
            evidence.reroot_receipts(&at, 0, |_| true),
            Err(WalkError::ReceiptOrigin)
        ),
        "a container above the call is not the call result position"
    );
}

#[test]
fn config_and_all_declaration_sources_get_typed_roots_without_a_syntactic_fallback() {
    use crate::{
        parser::{ProviderContext, TypeExpr},
        schema::{AttributeType, StructField, ValidatedConfigSchema},
    };
    let config = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([
        (
            "provider".into(),
            AttributeType::struct_(
                "Provider",
                vec![StructField::required("field", AttributeType::int()).build()],
            ),
        ),
        (
            "backend".into(),
            AttributeType::struct_(
                "Backend",
                vec![StructField::required("field", AttributeType::int()).build()],
            ),
        ),
    ]))
    .unwrap();
    let value: Value = Value::Concrete(ConcreteValue::Map(
        indexmap::indexmap! { "field".into() => int(1) },
    ));
    for name in ["provider", "backend"] {
        let mut count = Count::default();
        inspect(
            WalkRoot::config_attribute(&config, name).unwrap(),
            &value,
            &mut count,
        )
        .unwrap();
        assert_eq!(
            count.paths[0].0,
            LeafPath::from_segments([LeafPathSegment::StructField("field".into())])
        );
    }
    assert!(WalkRoot::config_attribute(&config, "missing").is_none());
    let parsed = crate::parser::parse_file(
        "arguments { options: struct { field: Int } }\nattributes { annotated: struct { field: Int } = { field = 1 } }\nexports { exported: struct { field: Int } = { field = 1 } }\n",
        std::path::Path::new("roots.crn"), &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).unwrap();
    let declared_type = TypeExpr::Struct {
        fields: vec![("field".into(), TypeExpr::Int)],
    };
    let inferred = crate::static_type::ExportStaticType::Inferred(
        crate::static_type::StaticType::Known(declared_type.clone()),
    );
    let upstream = crate::upstream_exports::UpstreamExportType::Static(
        crate::static_type::ConsumerExportStaticType::Declared(
            crate::static_type::StaticType::Known(crate::static_type::ConsumerResolvedType::new(
                declared_type.clone(),
            )),
        ),
    );
    let inferred_upstream = crate::upstream_exports::UpstreamExportType::Static(
        crate::static_type::ConsumerExportStaticType::Inferred(
            crate::static_type::StaticType::Known(crate::static_type::ConsumerResolvedType::new(
                declared_type,
            )),
        ),
    );
    assert!(inferred_upstream.declared_known().is_none());
    assert!(DeclarationRoot::new(&TypeExpr::Union(Vec::new())).is_none());
    let sources = [
        &parsed.arguments[0].static_type,
        parsed.attribute_params[0].static_type.as_ref().unwrap(),
        parsed.export_params[0].static_type.as_ref().unwrap(),
        inferred.known().unwrap(),
        upstream.declared_known().unwrap(),
    ];
    for source in sources {
        let declaration = DeclarationRoot::new(source).unwrap();
        let mut count = Count::default();
        inspect(declaration.root(), &value, &mut count).unwrap();
        assert_eq!(
            count.paths[0].0,
            LeafPath::from_segments([LeafPathSegment::StructField("field".into())])
        );
    }
}

#[test]
fn rootless_roots_preserve_validated_types_and_named_definitions() {
    use crate::schema::{
        AttributeType, CompiledAttributeType, CompiledStructField, RootlessType, StructField,
        ValidatedConfigSchema,
    };
    use std::collections::BTreeMap;

    let config = ValidatedConfigSchema::validate(BTreeMap::from([(
        "options".into(),
        AttributeType::struct_(
            "Options",
            vec![
                StructField::required(
                    "items",
                    AttributeType::list(AttributeType::struct_(
                        "Item",
                        vec![StructField::required("field", AttributeType::int()).build()],
                    )),
                )
                .build(),
            ],
        ),
    )]))
    .unwrap();
    let config_root = config.rootless_type("options").unwrap();
    let named_root = RootlessType::with_defs(
        CompiledAttributeType::ref_("Options"),
        BTreeMap::from([
            (
                "Options".into(),
                CompiledAttributeType::struct_(
                    "Options",
                    vec![
                        CompiledStructField::required(
                            "items",
                            CompiledAttributeType::list(CompiledAttributeType::ref_("Item")),
                        )
                        .build(),
                    ],
                ),
            ),
            (
                "Item".into(),
                CompiledAttributeType::struct_(
                    "Item",
                    vec![
                        CompiledStructField::required("field", CompiledAttributeType::int())
                            .build(),
                    ],
                ),
            ),
        ]),
    );
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "items".into() => Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                "field".into() => int(1),
            })),
        ])),
    }));
    for rootless in [&config_root, &named_root] {
        let root = WalkRoot::rootless(rootless);
        let at = root.clone().at();
        let effective = at.effective_type().unwrap();
        assert!(effective.path().is_empty());
        assert!(matches!(
            effective.shape(),
            Shape::Struct { name: "Options" }
        ));
        effective.validate(&value).unwrap();
        let mut count = Count::default();
        inspect(root, &value, &mut count).unwrap();
        assert_eq!(
            count.paths,
            vec![(
                LeafPath::from_segments([
                    LeafPathSegment::StructField("items".into()),
                    LeafPathSegment::ListIndex(0),
                    LeafPathSegment::StructField("field".into()),
                ]),
                false,
            )]
        );
    }
}

#[test]
fn opaque_snapshot_hashing_preserves_existing_stage_and_binding_hash_material() {
    use crate::resource::{LoopSlot, ParserPlaceholder, PlaceholderKind};
    fn check<S: PlaceholderKind + serde::de::DeserializeOwned>(value: Value<S>) {
        let payload: SecretPayload<S, AbsoluteBindingAddress> = Box::new(value).into();
        let expected = payload.hash(None).unwrap();
        let snapshot = Value::Deferred(DeferredValue::Secret(payload)).to_snapshot();
        assert!(
            matches!(snapshot, Value::Deferred(DeferredValue::Secret(payload))
            if ProtectedSecret { payload: &payload }.hash_snapshot::<S>(Some(&expected)).unwrap() == expected)
        );
    }
    check(Value::<NoPlaceholder>::Concrete(ConcreteValue::String(
        "private-runtime".into(),
    )));
    check(Value::<LoopSlot>::Deferred(DeferredValue::Placeholder(
        LoopSlot::Value,
    )));
    check(Value::<ParserPlaceholder>::Deferred(
        DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "parameter".into(),
        }),
    ));
    check(Value::<NoPlaceholder>::root_resource_ref(
        "producer",
        "id",
        Vec::new(),
    ));
}

#[test]
fn protected_locator_survives_verified_map_key_reordering() {
    let original: Value = secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "z-private".into() => int(1), "a-private".into() => int(2),
    })));
    let reordered: Value = secret(Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "a-private".into() => int(2), "z-private".into() => int(1),
    })));
    assert!(matches!((&original, &reordered),
        (Value::Deferred(DeferredValue::Secret(left)), Value::Deferred(DeferredValue::Secret(right)))
            if right.verify_hash(&left.hash(None).unwrap()).unwrap()));
    let output = inspect(WalkRoot::syntactic(), &original, &mut Carry::default())
        .unwrap()
        .into_output();
    let site = &output.obligations()[0];
    let wire = serde_json::to_string(site.protected_locator().unwrap()).unwrap();
    let restored: ProtectedLocator = serde_json::from_str(&wire).unwrap();
    let restored = restored
        .resolve(WalkRoot::syntactic(), &reordered, *site.output())
        .unwrap();
    assert_eq!(restored.path, site.path);
    assert_eq!(restored.expression_edges(), site.expression_edges());
}

// R4 regressions are appended so the previous contract tests remain unchanged.
#[derive(Default)]
struct R4Rewrite {
    replace_placeholders: bool,
    replace_with_loop_slot: bool,
    increment_integers: bool,
    integers: usize,
    pointers: Vec<usize>,
    sites: Vec<Vec<ExpressionEdge>>,
}
impl Algebra<Transform> for R4Rewrite {
    type Output = usize;
    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, usize, P, B> {
        if let ScalarInput::String(text) = &value.input {
            self.pointers
                .push(text.expose_plaintext().as_ptr() as usize);
        }
        if value.kind() == ScalarKind::Int {
            self.integers += 1;
            self.sites.push(at.expression_edges().to_vec());
            if self.increment_integers {
                return value.replace(Value::Concrete(ConcreteValue::Int(self.integers as i64)), 1);
            }
        }
        value.keep(1)
    }
    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, usize, P, B> {
        children.transform(self, |outputs| 1 + outputs.into_iter().sum::<usize>())
    }
    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, usize, P, B> {
        children.transform(self, |outputs| 1 + outputs.into_iter().sum::<usize>())
    }
    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        expression: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, usize, P, B> {
        expression.transform(self, |outputs| 1 + outputs.into_iter().sum::<usize>())
    }
    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, usize, P, B> {
        leaf.with_reference(|reference| {
            if let Some(reference) = reference {
                let text = match reference {
                    ReferenceRef::Resource(path) => path.binding(),
                    ReferenceRef::Binding(binding) => binding.as_str(),
                };
                self.pointers.push(text.as_ptr() as usize);
            }
        });
        if leaf.kind() == LeafKind::Placeholder && self.replace_placeholders {
            return leaf.replace(
                Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                    "field".into() => Value::Concrete(ConcreteValue::Int(41)),
                })),
                1,
            );
        }
        if leaf.kind() == LeafKind::Placeholder && self.replace_with_loop_slot {
            return match P::from_loop_slot(crate::resource::LoopSlot::Key) {
                Ok(slot) => leaf.replace(Value::Deferred(DeferredValue::Placeholder(slot)), 1),
                Err(_) => leaf.reject(),
            };
        }
        leaf.keep(1)
    }
    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, usize, P, B> {
        child.transform(self)
    }
}

#[test]
fn r4_changed_access_uses_canonical_selection_before_lazy_fallback() {
    use crate::resource::{AccessMissPolicy, ParserPlaceholder, PathSegment, RelativeBindingName};
    let base: Value<ParserPlaceholder, RelativeBindingName> =
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "record".into(),
        }));
    let value = crate::resource::apply_postfix_access(
        base,
        PathSegment::Field {
            name: "field".into(),
        },
        42,
        AccessMissPolicy::Error,
    )
    .unwrap();
    let result = transform(
        WalkRoot::syntactic(),
        value,
        &mut R4Rewrite {
            replace_placeholders: true,
            ..Default::default()
        },
    )
    .unwrap()
    .into_value();
    assert_eq!(result, Value::Concrete(ConcreteValue::Int(41)));
}

#[test]
fn r4_changed_loop_access_preserves_rooted_metadata_when_still_deferred() {
    use crate::resource::{LoopSlot, PathSegment};
    let original = crate::resource::parser_deferred_for_access(
        "item".into(),
        LoopSlot::Value,
        PathSegment::Field {
            name: "field".into(),
        },
        42,
    );
    let result = transform(
        WalkRoot::syntactic(),
        original,
        &mut R4Rewrite {
            replace_with_loop_slot: true,
            ..Default::default()
        },
    );
    assert!(
        result.is_ok(),
        "a valid loop-root rewrite must use the existing rebuild seam"
    );
    let result = result.unwrap().into_value();
    assert!(
        matches!(&result, Value::Deferred(DeferredValue::Access(access))
        if access.display_root() == Some("item") && access.source_line() == 42
            && access.deferred_miss_display_root().is_none())
    );
}

#[test]
fn r4_reentered_secret_locator_cannot_resolve_to_an_unrelated_root_position() {
    let source = secret(int(111));
    let source_output = inspect(WalkRoot::syntactic(), &source, &mut Carry::default())
        .unwrap()
        .into_output();
    let inherited_secret = &source_output.obligations()[0];
    let reentered_plain = inherited_secret
        .inspect(&int(222), &mut Carry::default())
        .unwrap()
        .into_output();
    assert!(reentered_plain.obligations()[0].is_secret());
    assert!(
        reentered_plain.obligations()[0]
            .protected_locator()
            .is_some(),
        "an inherited protected position must retain a typed locator origin"
    );

    let public = inspect(WalkRoot::syntactic(), &int(0), &mut Carry::default())
        .unwrap()
        .into_output();
    let referenced = secret(int(333));
    let reentered = public.obligations()[0]
        .inspect(&referenced, &mut Carry::default())
        .unwrap()
        .into_output();
    let site = &reentered.obligations()[0];
    let frozen = serde_json::to_string(site.protected_locator().unwrap()).unwrap();
    let locator: ProtectedLocator = serde_json::from_str(&frozen).unwrap();
    assert!(
        locator
            .resolve(WalkRoot::syntactic(), &source, *site.output())
            .is_err(),
        "a persisted re-entry coordinate must never attach to another root's secret"
    );
}

fn r4_resolved_fixture() -> crate::resource::AuthoredValue {
    let parsed = crate::parser::parse_file(
        "fn called(input: Int = 71): Int {\n let local = 72\n 73\n}\nfn helper(): Int {\n 74\n}\n",
        std::path::Path::new("r4-functions.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function: Arc::new(parsed.user_functions["called"].clone()),
        functions: Arc::new(parsed.user_functions),
        args: vec![crate::resource::FunctionArgument::from_value(
            Value::Concrete(ConcreteValue::Int(1)),
        )],
        source_line: crate::resource::DiagnosticSourceLine::authored(9),
    })
}

#[test]
fn r4_unchanged_resolved_call_preserves_both_arc_identities() {
    let value = r4_resolved_fixture();
    assert!(matches!(
        &value,
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
    ));
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &value
    {
        let original_function = function.clone();
        let original_scope = functions.clone();
        let rewritten = transform(WalkRoot::syntactic(), value, &mut Carry::default())
            .unwrap()
            .into_value();
        assert!(matches!(
            &rewritten,
            Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        ));
        if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            ..
        }) = &rewritten
        {
            assert!(
                Arc::ptr_eq(function, &original_function),
                "unchanged callee evidence shares this Arc"
            );
            assert!(
                Arc::ptr_eq(functions, &original_scope),
                "unchanged captured scope must retain its Arc"
            );
        }
    }
}

#[test]
fn r4_distinct_captured_definition_occurrences_are_rewritten_once_each() {
    let mut value = r4_resolved_fixture();
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) = &mut value {
        Arc::make_mut(functions).remove("called");
    }
    let mut rewrite = R4Rewrite {
        increment_integers: true,
        ..Default::default()
    };
    let rewritten = transform(WalkRoot::syntactic(), value, &mut rewrite)
        .unwrap()
        .into_value();
    assert_eq!(
        rewrite.integers, 5,
        "three callee fields, one helper body, one argument"
    );
    assert_eq!(
        rewrite
            .sites
            .iter()
            .filter(|edges| edges.first() == Some(&ExpressionEdge::FunctionDefinition))
            .count(),
        3
    );
    assert_eq!(
        rewrite
            .sites
            .iter()
            .filter(|edges| edges.first() == Some(&ExpressionEdge::FunctionScope(0)))
            .count(),
        1
    );
    let mut count = Count::default();
    inspect(WalkRoot::syntactic(), &rewritten, &mut count).unwrap();
    assert_eq!(count.numbers, (1..=5).map(|number| (number, 1)).collect());
}

#[test]
fn r4_canonical_root_does_not_accept_an_authored_block_alias() {
    use crate::schema::{AttributeType, StructField, ValidatedConfigSchema};
    let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
        "root".into(),
        AttributeType::struct_(
            "Root",
            vec![
                StructField::required(
                    "canonical",
                    AttributeType::struct_(
                        "Inner",
                        vec![StructField::required("field", AttributeType::int()).build()],
                    ),
                )
                .with_block_name("alias")
                .build(),
            ],
        ),
    )]))
    .unwrap();
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "alias".into() => Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "field".into() => int(1),
        })),
    }));
    assert!(
        inspect(
            WalkRoot::config_attribute(&schema, "root").unwrap(),
            &value,
            &mut Count::default()
        )
        .is_err(),
        "canonical typed roots must not silently resolve authored-only aliases"
    );
}

#[test]
fn r4_nested_module_snapshot_hash_is_identical_to_existing_state_hash() {
    use crate::resource::{ModuleInstanceScope, RelativeBindingName};
    let parent = ModuleInstanceScope::for_call("outer", Some("outer"), "outer");
    let child = ModuleInstanceScope::for_call("inner", Some("inner"), "inner");
    let scope = parent.with_child(&child);
    let relative: Value<NoPlaceholder, RelativeBindingName> = Value::Concrete(ConcreteValue::Map(
        indexmap::indexmap! {
            "reference".into() => Value::resource_ref("producer", "id", vec![]),
            "binding".into() => Value::Deferred(DeferredValue::BindingRef { binding: "producer".into() }),
        },
    ));
    let absolute = relative.resolve_module(&scope);
    assert!(format!("{absolute:?}").contains("outer.inner.producer"));
    let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> = Box::new(absolute).into();
    let existing = payload.hash(None).unwrap();
    let snapshot = Value::Deferred(DeferredValue::Secret(payload)).to_snapshot();
    assert!(matches!(
        &snapshot,
        Value::Deferred(DeferredValue::Secret(_))
    ));
    if let Value::Deferred(DeferredValue::Secret(payload)) = &snapshot {
        let protected = ProtectedSecret { payload };
        assert_eq!(
            protected
                .hash_snapshot::<NoPlaceholder>(Some(&existing))
                .unwrap(),
            existing,
            "the previous state digest must be reused byte-for-byte for nested module bindings"
        );
    }
}

#[test]
fn r4_parts_length_matches_the_actual_resolved_call_fold_outputs() {
    let value = r4_resolved_fixture();
    assert!(matches!(
        &value,
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
    ));
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        args,
        source_line,
    }) = &value
    {
        let parts: Parts<
            '_,
            Inspect,
            crate::resource::ParserPlaceholder,
            crate::resource::RelativeBindingName,
        > = Parts::ResolvedCall {
            function,
            functions,
            args,
            source_line,
        };
        let length = parts.len();
        let actual = parts
            .inspect(WalkRoot::syntactic().at(), &mut Count::default())
            .unwrap();
        assert_eq!(length, actual.len());
    }
}

#[test]
fn r4_result_branches_retain_destination_types_but_inputs_do_not() {
    use crate::schema::{AttributeType, StructField, ValidatedConfigSchema};
    let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
        "root".into(),
        AttributeType::struct_(
            "Root",
            vec![StructField::required("field", AttributeType::int()).build()],
        ),
    )]))
    .unwrap();
    let record = || {
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "field".into() => int(1) },
        ))
    };
    let values: [Value; 2] = [
        Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(false_condition("r4.crn", 1), record(), record()),
        ))),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(record()),
            default: Box::new(record()),
        }),
    ];
    for value in values {
        let mut count = Count::default();
        inspect(
            WalkRoot::config_attribute(&schema, "root").unwrap(),
            &value,
            &mut count,
        )
        .unwrap();
        let fields: Vec<_> = count
            .paths
            .into_iter()
            .filter(|(path, _)| !path.segments().is_empty())
            .collect();
        assert_eq!(
            fields,
            vec![
                (
                    LeafPath::from_segments([LeafPathSegment::StructField("field".into())]),
                    false
                );
                2
            ]
        );
    }
    let root = WalkRoot::config_attribute(&schema, "root").unwrap().at();
    for edge in [
        ExpressionEdge::Condition,
        ExpressionEdge::Argument(0),
        ExpressionEdge::ArithmeticLeft,
        ExpressionEdge::AccessBase,
        ExpressionEdge::EvidenceOperand,
    ] {
        assert!(root.operand(edge).effective_type().is_none());
    }
}

#[test]
fn r4_struct_lookup_spends_one_budget_step_even_with_many_fields_and_reference_hops() {
    use crate::schema::{
        CompiledAttributeType as Ty, CompiledStructField as Field, RootlessType, ShapeWalkBudget,
    };
    let mut defs = std::collections::BTreeMap::new();
    for index in 0..96 {
        defs.insert(format!("Hop{index}"), Ty::ref_(format!("Hop{}", index + 1)));
    }
    defs.insert(
        "Hop96".into(),
        Ty::struct_(
            "Wide",
            (0..96)
                .map(|index| Field::required(format!("field{index}"), Ty::int()).build())
                .collect(),
        ),
    );
    let rootless = RootlessType::with_defs(Ty::ref_("Hop0"), defs);
    let at = WalkRoot::rootless(&rootless).at();
    let effective = at.effective_type().unwrap();
    assert!(matches!(effective.shape(), Shape::Struct { name: "Wide" }));
    let mut budget = ShapeWalkBudget::new(1);
    assert_eq!(
        effective.field_candidates_with_budget(&mut budget).len(),
        96
    );
    assert!(
        !budget.take(),
        "the struct accessor consumed exactly one step"
    );
    let value: Value = Value::Concrete(ConcreteValue::Map(
        indexmap::indexmap! { "field95".into() => int(1) },
    ));
    let mut count = Count::default();
    inspect(WalkRoot::rootless(&rootless), &value, &mut count).unwrap();
    assert_eq!(
        count.paths,
        vec![(
            LeafPath::from_segments([LeafPathSegment::StructField("field95".into())]),
            false
        )]
    );
}

#[test]
fn r4_owned_argument_arithmetic_and_access_operands_move_without_copying() {
    use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, ValidatedIntegerExpr};
    use crate::resource::{
        AccessMissPolicy, DiagnosticSourceLine, PathSegment, RelativeBindingName, RootScope,
        ValueString,
    };
    let argument = ValueString::plain("argument-allocation");
    let access_default = ValueString::plain("access-base-allocation");
    let binding = RootScope::new().resolve(RelativeBindingName::new("arithmetic-binding"));
    let expected = vec![
        argument.expose_plaintext().as_ptr() as usize,
        binding.as_str().as_ptr() as usize,
        access_default.expose_plaintext().as_ptr() as usize,
    ];
    let values: Value = Value::Concrete(ConcreteValue::List(vec![
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String(argument)).into()],
            source_line: DiagnosticSourceLine::synthetic(),
        }),
        Value::Deferred(DeferredValue::IntegerArithmetic(
            ValidatedIntegerExpr::new(
                Value::Deferred(DeferredValue::BindingRef { binding }),
                IntegerOp::Add,
                int(1),
                IntegerOperatorSpan::synthetic(),
            )
            .unwrap(),
        )),
        crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(int(1)),
                default: Box::new(Value::Concrete(ConcreteValue::String(access_default))),
            }),
            PathSegment::Field {
                name: "field".into(),
            },
            1,
            AccessMissPolicy::Error,
        )
        .unwrap(),
    ]));
    let mut rewrite = R4Rewrite::default();
    transform(WalkRoot::syntactic(), values, &mut rewrite).unwrap();
    assert_eq!(
        rewrite.pointers, expected,
        "owned operands must reach the algebra without cloning their strings"
    );
}

#[test]
fn r4_persisted_reentry_locator_never_attaches_to_a_different_secret() {
    let source = secret(int(111));
    let public = inspect(WalkRoot::syntactic(), &int(0), &mut Carry::default())
        .unwrap()
        .into_output();
    let referenced = secret(int(333));
    let reentered = public.obligations()[0]
        .inspect(&referenced, &mut Carry::default())
        .unwrap()
        .into_output();
    let site = &reentered.obligations()[0];
    let frozen = serde_json::to_string(site.protected_locator().unwrap()).unwrap();
    let locator: ProtectedLocator = serde_json::from_str(&frozen).unwrap();
    assert!(
        locator
            .resolve(WalkRoot::syntactic(), &source, *site.output())
            .is_err(),
        "these relative coordinates otherwise silently resolve to the unrelated secret(111)"
    );
}

#[test]
fn r4_equal_named_callee_and_scope_entries_are_distinct_stored_definitions() {
    let mut value = r4_resolved_fixture();
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &mut value
    {
        assert!(!std::ptr::eq(function.as_ref(), &functions["called"]));
        Arc::make_mut(functions)
            .get_mut("called")
            .unwrap()
            .body
            .value = Value::Concrete(ConcreteValue::Int(999));
    }
    let rewritten = transform(WalkRoot::syntactic(), value, &mut R4Rewrite::default())
        .unwrap()
        .into_value();
    let mut count = Count::default();
    inspect(WalkRoot::syntactic(), &rewritten, &mut count).unwrap();
    assert_eq!(
        count.numbers,
        [(1, 1), (71, 2), (72, 2), (73, 1), (74, 1), (999, 1)].into()
    );
}

#[test]
fn r4_authored_root_constructors_resolve_aliases_to_canonical_leaf_paths() {
    use crate::schema::{AttributeType, StructField, ValidatedConfigSchema};
    let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
        "root".into(),
        AttributeType::struct_(
            "Root",
            vec![
                StructField::required(
                    "canonical",
                    AttributeType::struct_(
                        "Inner",
                        vec![StructField::required("field", AttributeType::int()).build()],
                    ),
                )
                .with_block_name("alias")
                .build(),
            ],
        ),
    )]))
    .unwrap();
    let rootless = schema.rootless_type("root").unwrap();
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "alias".into() => Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "field".into() => int(1),
        })),
    }));
    let roots = [
        WalkRoot::config_attribute_by_authored_name(&schema, "root").unwrap(),
        WalkRoot::typed_by_authored_name(schema.effective_type("root").unwrap()),
        WalkRoot::rootless_by_authored_name(&rootless),
    ];
    for root in roots {
        let mut count = Count::default();
        inspect(root.clone(), &value, &mut count).unwrap();
        assert_eq!(
            count.paths,
            vec![(
                LeafPath::from_segments([
                    LeafPathSegment::StructField("canonical".into()),
                    LeafPathSegment::StructField("field".into()),
                ]),
                false
            )]
        );
        let rewritten = transform(root, value.clone(), &mut Carry::default())
            .unwrap()
            .into_value();
        assert_eq!(
            rewritten, value,
            "path normalization must preserve authored input keys"
        );
    }
}

#[test]
fn r4_argument_only_rewrite_preserves_unchanged_function_and_scope_arcs() {
    let mut value = r4_resolved_fixture();
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &mut value
    {
        let callee = Arc::make_mut(function);
        callee.params.clear();
        callee.local_lets.clear();
        callee.body.value = Value::Concrete(ConcreteValue::Bool(false));
        for definition in Arc::make_mut(functions).values_mut() {
            definition.params.clear();
            definition.local_lets.clear();
            definition.body.value = Value::Concrete(ConcreteValue::Bool(false));
        }
    }
    assert!(matches!(
        &value,
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
    ));
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &value
    {
        let old_function = function.clone();
        let old_scope = functions.clone();
        let rewritten = transform(WalkRoot::syntactic(), value, &mut ReplaceInts::default())
            .unwrap()
            .into_value();
        if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &rewritten
        {
            assert!(Arc::ptr_eq(function, &old_function));
            assert!(Arc::ptr_eq(functions, &old_scope));
            assert_eq!(
                args[0].value(),
                Some(&Value::Concrete(ConcreteValue::Int(41)))
            );
        } else {
            assert!(matches!(
                &rewritten,
                Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
            ));
        }
    }
}

#[test]
fn r4_call_and_unavailable_lengths_use_the_same_argument_value_visitor_as_the_fold() {
    use crate::resource::{DiagnosticSourceLine, FunctionArgument, UnavailableReason};
    let args = vec![
        FunctionArgument::from_value(int(1)),
        FunctionArgument::from_value(int(2)),
    ];
    let name = crate::builtins::BuiltinFunction::join;
    let source_line = DiagnosticSourceLine::synthetic();
    let reason = UnavailableReason::from_builtin_error("unavailable".into());
    let parts: [Parts<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>; 2] = [
        Parts::Call {
            name: &name,
            args: &args,
            source_line: &source_line,
        },
        Parts::Unavailable {
            name: &name,
            args: &args,
            source_line: &source_line,
            reason: &reason,
        },
    ];
    for parts in parts {
        let length = parts.len();
        let actual = parts
            .inspect(WalkRoot::syntactic().at(), &mut Count::default())
            .unwrap();
        assert_eq!(length, actual.len());
    }
}

#[test]
fn r4_changed_snapshot_access_uses_the_existing_snapshot_selection_seam() {
    use crate::resource::{AccessMissPolicy, ParserPlaceholder, PathSegment, RelativeBindingName};
    let base: Value<ParserPlaceholder, RelativeBindingName> =
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "record".into(),
        }));
    let value = crate::resource::apply_postfix_access(
        base,
        PathSegment::Field {
            name: "field".into(),
        },
        42,
        AccessMissPolicy::Error,
    )
    .unwrap()
    .to_snapshot();
    let result = transform(
        WalkRoot::syntactic(),
        value,
        &mut R4Rewrite {
            replace_placeholders: true,
            ..Default::default()
        },
    )
    .unwrap()
    .into_value();
    assert_eq!(result, Value::Concrete(ConcreteValue::Int(41)));
}

#[test]
fn r4_reentry_locator_reports_its_typed_origin_after_persistence() {
    let source = secret(int(111));
    let inventory = inspect(WalkRoot::syntactic(), &source, &mut Carry::default())
        .unwrap()
        .into_output();
    let output = inventory.obligations()[0]
        .inspect(&int(222), &mut Carry::default())
        .unwrap()
        .into_output();
    let saved =
        serde_json::to_string(output.obligations()[0].protected_locator().unwrap()).unwrap();
    let restored: ProtectedLocator = serde_json::from_str(&saved).unwrap();
    assert!(matches!(
        restored.resolve(WalkRoot::syntactic(), &source, 1_usize),
        Err(WalkError::ReenteredLocator)
    ));
    assert!(!saved.contains("111") && !saved.contains("222"));
}

#[test]
fn r4_changed_access_only_retains_a_dormant_miss_after_canonical_selection_fails() {
    use crate::resource::{LoopSlot, PathSegment};
    let value = crate::resource::parser_deferred_for_access(
        "item".into(),
        LoopSlot::Value,
        PathSegment::Field {
            name: "missing".into(),
        },
        42,
    );
    let result = transform(
        WalkRoot::syntactic(),
        value,
        &mut R4Rewrite {
            replace_placeholders: true,
            ..Default::default()
        },
    )
    .unwrap()
    .into_value();
    assert!(
        matches!(&result, Value::Deferred(DeferredValue::Access(access))
        if access.display_root().is_none() && access.deferred_miss_display_root() == Some("item")
            && access.source_line() == 42
            && matches!(access.base(), Value::Concrete(ConcreteValue::Map(_))))
    );
}

#[test]
fn r4_authored_resource_root_requires_its_named_constructor() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    let schema = ResourceSchema::new("test.Root")
        .attribute(
            AttributeSchema::optional(
                "options",
                AttributeType::struct_(
                    "Options",
                    vec![
                        StructField::required("canonical", AttributeType::int())
                            .with_block_name("field_alias")
                            .build(),
                    ],
                ),
            )
            .with_block_name("option")
            .build(),
        )
        .validate()
        .unwrap();
    assert!(WalkRoot::attribute(&schema, "option").is_none());
    let value: Value = Value::Concrete(ConcreteValue::Map(
        indexmap::indexmap! { "field_alias".into() => int(1) },
    ));
    let mut count = Count::default();
    inspect(
        WalkRoot::attribute_by_authored_name(&schema, "option").unwrap(),
        &value,
        &mut count,
    )
    .unwrap();
    assert_eq!(
        count.paths,
        vec![(
            LeafPath::from_segments([LeafPathSegment::StructField("canonical".into())]),
            false
        )]
    );
}

fn unavailable_candidate() -> crate::resource::AuthoredValue {
    Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "private-unavailable-reason".into(),
        ),
        source_line: 7.into(),
    })
}

#[test]
fn unavailable_search_preserves_saved_scope_and_coalesce_policies() {
    let unavailable = unavailable_candidate();
    let fallback = |value| {
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("origin", "comment", vec![])),
            default: Box::new(value),
        })
    };
    for (candidate, inside) in [
        (unavailable.clone(), false),
        (unavailable.clone().into_secret(), true),
        (fallback(unavailable.clone()), false),
        (fallback(unavailable.clone().into_secret()), true),
    ] {
        assert!(contains_unavailable(&candidate, UnavailableSearchPolicy::SavedCandidate).unwrap());
        assert_eq!(
            contains_unavailable(&candidate, UnavailableSearchPolicy::SavedSecret).unwrap(),
            inside
        );
    }
    assert!(
        find_required_unavailable(
            &fallback(unavailable),
            crate::config_loader::strict_resolution_context()
        )
        .unwrap()
        .is_none()
    );
    // OnlyInside ignored an outer public failure but still searched its args.
    let outer = Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![crate::resource::FunctionArgument::from_value(
            unavailable_candidate().into_secret(),
        )],
        reason: crate::resource::UnavailableReason::from_builtin_error("outer".into()),
        source_line: 1.into(),
    });
    assert!(contains_unavailable(&outer, UnavailableSearchPolicy::SavedSecret).unwrap());
}

#[test]
fn unavailable_search_checks_both_module_if_branches() {
    for (then_value, else_value) in [
        (
            unavailable_candidate(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            Value::Concrete(ConcreteValue::Bool(true)),
            unavailable_candidate(),
        ),
    ] {
        let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    Value::Concrete(ConcreteValue::Bool(true)),
                    "if.crn".into(),
                    1,
                ),
                then_value,
                else_value,
            ),
        )));
        assert!(
            find_required_unavailable(&value, crate::config_loader::strict_resolution_context())
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn unavailable_search_ignores_captured_code_and_checks_call_arguments() {
    let file = crate::parser::parse(
        "fn captured(): String { 'ok' }",
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let mut function = file.user_functions["captured"].clone();
    function.body.value = unavailable_candidate().into_secret();
    let mut scope_function = function.clone();
    scope_function.name = "unused".into();
    let functions = Arc::new(HashMap::from([("unused".into(), scope_function)]));
    let mut value = Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function: Arc::new(function),
        functions,
        args: vec![],
        source_line: 1.into(),
    });
    assert!(!contains_unavailable(&value, UnavailableSearchPolicy::SavedCandidate).unwrap());
    assert!(!contains_unavailable(&value, UnavailableSearchPolicy::SavedSecret).unwrap());
    assert!(
        find_required_unavailable(&value, crate::config_loader::strict_resolution_context())
            .unwrap()
            .is_none()
    );
    if let Value::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = &mut value {
        args.push(crate::resource::FunctionArgument::from_value(
            unavailable_candidate().into_secret(),
        ));
    }
    assert!(matches!(
        value,
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
    ));
    assert!(contains_unavailable(&value, UnavailableSearchPolicy::SavedCandidate).unwrap());
    assert!(contains_unavailable(&value, UnavailableSearchPolicy::SavedSecret).unwrap());
    assert!(
        find_required_unavailable(&value, crate::config_loader::strict_resolution_context())
            .unwrap()
            .is_some()
    );
}

#[test]
fn unavailable_search_protects_secret_diagnostics() {
    let error = find_required_unavailable(
        &unavailable_candidate().into_secret(),
        crate::config_loader::strict_resolution_context(),
    )
    .unwrap()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains(crate::resource::INSIDE_SECRET_MESSAGE)
    );
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(
            !rendered.contains("private-unavailable-reason"),
            "{rendered}"
        );
    }
}

#[test]
fn parser_substitution_keeps_nested_call_error_location_and_protection() {
    use crate::{
        eval_value::UnexpandedStructuralHandle,
        parser::{ParseError, ResourceTypePath},
        resource::{AuthoredValue, FunctionArgument, ParserPlaceholder},
    };

    let parameter =
        AuthoredValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "input".into(),
        }));
    let value = AuthoredValue::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::List(vec![parameter]),
        ))],
        source_line: 29.into(),
    });
    let substitutions = HashMap::from([(
        "input".into(),
        FunctionArgument::from_handle(UnexpandedStructuralHandle::module_parameter(
            "private-handle-2715",
            ResourceTypePath::new("mock", "test.resource"),
        )),
    )]);
    let error = crate::value_walk::substitute_function_parameters(
        &value,
        &substitutions,
        1,
        crate::value_walk::RewritePosition::Value,
    )
    .unwrap_err();
    assert!(
        matches!(error, ParseError::InvalidExpression { line: 29, .. }),
        "{error}"
    );
    let error = crate::value_walk::substitute_function_parameters(
        &value.into_secret(),
        &substitutions,
        1,
        crate::value_walk::RewritePosition::Value,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(crate::resource::INSIDE_SECRET_MESSAGE)
    );
    assert!(!format!("{error:?} {error}").contains("private-handle-2715"));
}

#[test]
fn parser_substitution_preserves_captured_lexical_scopes_and_arc_identity() {
    use crate::resource::{AuthoredValue, FunctionArgument, ParserPlaceholder};

    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let file = crate::parser::parse(
        "fn nested(input: String): String { let local = input secret(local) }",
        &context,
    )
    .unwrap();
    let function = Arc::new(file.user_functions["nested"].clone());
    let functions = Arc::new(file.user_functions);
    let parameter =
        AuthoredValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "input".into(),
        }));
    let call = AuthoredValue::Deferred(DeferredValue::ResolvedFunctionCall {
        function: function.clone(),
        functions: functions.clone(),
        args: vec![FunctionArgument::from_value(parameter)],
        source_line: 17.into(),
    })
    .into_secret();
    let replacement =
        AuthoredValue::Concrete(ConcreteValue::String("private-lexical-input".into()));
    let substitutions = HashMap::from([(
        "input".into(),
        FunctionArgument::from_value(replacement.clone()),
    )]);
    let rewritten = crate::value_walk::substitute_function_parameters(
        &call,
        &substitutions,
        1,
        crate::value_walk::RewritePosition::Value,
    )
    .unwrap();
    assert!(!format!("{rewritten:?}").contains("private-lexical-input"));
    assert!(matches!(
        &rewritten,
        Value::Deferred(DeferredValue::Secret(_))
    ));
    if let Value::Deferred(DeferredValue::Secret(payload)) = &rewritten {
        assert!(matches!(
            payload.plaintext_for_test(),
            Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        ));
        if let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: callee,
            functions: scope,
            args,
            ..
        }) = payload.plaintext_for_test()
        {
            assert!(Arc::ptr_eq(callee, &function));
            assert!(Arc::ptr_eq(scope, &functions));
            assert_eq!(args[0].walker_value(), &replacement);
        }
    }
}

#[test]
fn parser_policy_selection_preserves_secret_and_does_not_evaluate_inactive_operands() {
    use crate::{
        parser::{DeferredIfCondition, DeferredValueIf, ParseError},
        resource::{AuthoredValue, ParserPlaceholder, RelativeBindingName},
    };

    struct Select;
    impl ParserRewrite<RelativeBindingName> for Select {
        type Error = ParseError;
        fn error(&mut self, error: ParseError) -> ParseError {
            error
        }
        fn source_line(&self) -> usize {
            1
        }
        fn leaf(
            &mut self,
            _: &LeafInput<'_, Transform, ParserPlaceholder, RelativeBindingName>,
            _: RewritePosition,
        ) -> Result<Option<AuthoredValue>, ParseError> {
            Err(ParseError::UserFunctionError(
                "private-inactive-operand".into(),
            ))
        }
        fn select(
            &mut self,
            kind: ExpressionKind,
            _: &AuthoredValue,
            _: Option<(&std::path::Path, usize)>,
        ) -> Result<Option<usize>, ParseError> {
            Ok(if kind == ExpressionKind::ModuleIf {
                Some(1)
            } else if kind == ExpressionKind::Coalesce {
                Some(0)
            } else {
                None
            })
        }
    }
    let inactive =
        AuthoredValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: "unused".into(),
        }))
        .into_secret();
    let selected = AuthoredValue::Concrete(ConcreteValue::Int(42)).into_secret();
    let conditional =
        AuthoredValue::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
            DeferredIfCondition::new(
                AuthoredValue::Concrete(ConcreteValue::Bool(true)),
                "if.crn".into(),
                1,
            ),
            selected.clone(),
            inactive.clone(),
        ))));
    let coalesce = AuthoredValue::Deferred(DeferredValue::Coalesce {
        value: Box::new(selected.clone()),
        default: Box::new(inactive),
    });
    for expression in [conditional, coalesce] {
        let result =
            rewrite_parser_value(&expression, RewritePosition::Value, &mut Select).unwrap();
        assert_eq!(result, selected);
    }
}

#[test]
fn parser_policy_callbacks_are_confined_to_the_kernel() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let adapter = std::fs::read_to_string(root.join("value_walk/substitution.rs")).unwrap();
    assert!(adapter.contains("pub(super) trait ParserRewrite"));
    assert!(adapter.contains("pub(super) fn rewrite_parser_value"));
    for owner in ["parser/functions.rs", "module_resolver/expander.rs"] {
        let source = std::fs::read_to_string(root.join(owner)).unwrap();
        assert!(
            !source.contains("ParserRewrite"),
            "{owner} can receive raw protected nodes"
        );
        assert!(
            !source.contains("rewrite_parser_value"),
            "{owner} can supply raw-node callbacks"
        );
    }
}

#[test]
fn substitution_if_branches_inside_access_bases_remain_value_positions() {
    use crate::eval_value::UnexpandedStructuralHandle;
    use crate::parser::{DeferredIfCondition, DeferredValueIf, ResourceTypePath};
    use crate::resource::{AuthoredValue, FunctionArgument, ParserPlaceholder, PathSegment};
    let parameter = |name: &str| {
        AuthoredValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam {
            name: name.into(),
        }))
    };
    let conditional =
        AuthoredValue::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
            DeferredIfCondition::new(parameter("c"), "branches.crn".into(), 7),
            parameter("p"),
            parameter("q"),
        ))));
    let body = crate::resource::apply_postfix_access(
        conditional,
        PathSegment::Field {
            name: "field".into(),
        },
        7,
        crate::resource::AccessMissPolicy::Error,
    )
    .unwrap();
    let substitutions = HashMap::from([
        (
            "c".into(),
            FunctionArgument::from_value(AuthoredValue::Concrete(ConcreteValue::Bool(true))),
        ),
        (
            "p".into(),
            FunctionArgument::from_handle(UnexpandedStructuralHandle::module_parameter(
                "resource",
                ResourceTypePath::new("mock", "test.resource"),
            )),
        ),
        (
            "q".into(),
            FunctionArgument::from_value(AuthoredValue::Concrete(ConcreteValue::Map(
                indexmap::indexmap! {
                    "field".into() => AuthoredValue::Concrete(ConcreteValue::String("value".into())),
                },
            ))),
        ),
    ]);
    let error = crate::value_walk::substitute_function_parameters(
        &body,
        &substitutions,
        7,
        crate::value_walk::RewritePosition::Value,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid expression at line 7: 'p' is a resource handle, not a value; reference one of its attributes, e.g. 'p.<attribute>'"
    );
}

#[test]
fn r5_function_path_substitution_preserves_authored_reference_locations() {
    use crate::resource::{AuthoredValue, FunctionArgument};
    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_file(
        "let source = mock.test.resource {}\nlet input = source.field\n",
        std::path::Path::new("reference.crn"),
        &context,
    )
    .unwrap();
    let input = &parsed.variables["input"];
    assert!(
        matches!(
            input,
            AuthoredValue::Deferred(DeferredValue::ResourceRef { .. })
        ),
        "expected reference: {input:?}"
    );
    if let AuthoredValue::Deferred(DeferredValue::ResourceRef { path }) = input {
        let locations = path.reference_locations().cloned().unwrap();
        let replacement = AuthoredValue::resource_ref("target", "name", vec![]);
        let substitutions = HashMap::from([(
            "source".into(),
            FunctionArgument::from_value(AuthoredValue::Concrete(ConcreteValue::Map(
                indexmap::IndexMap::from([("field".into(), replacement)]),
            ))),
        )]);
        let rewritten = crate::value_walk::substitute_function_parameters(
            input,
            &substitutions,
            2,
            crate::value_walk::RewritePosition::Value,
        )
        .unwrap();
        assert!(
            matches!(rewritten, AuthoredValue::Deferred(DeferredValue::ResourceRef { path }) if path.binding() == "target" && path.reference_locations() == Some(&locations))
        );
    }
}
