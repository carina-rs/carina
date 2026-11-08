# Value traversal kernel contract

## Structural dispatch and scope

[The engine](engine.rs) declares one exhaustive variant list over Value,
ConcreteValue, and DeferredValue. A macro instantiates that list for each mode's
input type: borrowed fields for Inspect and owned fields for Transform. Each
variant is destructured once and its typed fields are passed directly to its
cursor. There is no catch-all arm:
adding a variant or a field to an existing variant requires a kernel decision.
There is no raw Value plus a separately asserted variant tag. Scalar and leaf
cursors retain their actual typed payloads; kind and reference/placeholder views
are derived from those payloads. No cursor downcasts a Value again.

Parts is indexed by the sealed mode. Inspect parts contain references and have
no rebuilding operation or closure. Transform parts own their fields and have
typed operations: a value rewrite returns Value, an argument rewrite returns
FunctionArgument, and a function rewrite returns UserFunction. Only fold outputs
share a flat vector. Fixed-arity expressions rebuild from named fields;
interpolations rebuild each literal or expression directly, with no positional
extraction. The source guard scans the whole value_walk directory for explicit
panicking fallbacks and production code for unchecked unwrap calls.

The existing algebra signatures are unchanged; the additional kernel entry points
below address protected redaction, persisted positions, receipts, and type roots. Cursor declarations make
the existing borrowed-payload outlives bound (`P: 'v`) explicit; the private
sealed mode trait selects borrowed versus owned input fields.

Inspect borrows a tree and folds summaries. Transform consumes a tree, rebuilds
its structure, and returns its value **and** folded output. Its container and
expression operations require an explicit output fold, so identity-returning
Argument/List/Map/Any preparation can carry receipts through normalization.
Scalar/leaf replacement applies to that node; it does not traverse newly
inserted replacement trees. A rewrite is a candidate, not schema admission or
conversion validation.

The algebra has six required arms, including a distinct secret arm. Successful
Secret completion requires entering its child through the kernel. Transform
restores the Secret wrapper by default. Its explicit redact operation first
inspects the child and then replaces the whole Secret with a typed digest leaf,
retaining the child evidence. Rejection is permitted; a successful skip is not.
The existing SecretPayload adapter clones for an owned rewrite because main
has no consuming payload accessor. Arguments, arithmetic operands and access
bases move through small crate-only owning-type adapters. Arithmetic replacements
still pass through from_post_validation, preserving the existing operand checks.
Argument traversal preserves all auxiliary operands and metadata. No existing
walker is migrated; the prior RootlessType::effective_type addition remains the
only schema edit, and schema/mod.rs is unchanged in r4. Replacement is tracked
explicitly, never by Value equality (which is not reflexive for Unknown and some
placeholders).

Locations reuse LeafPath and its StructField, MapKey, and ListIndex segments.
Secret adds protection ancestry, never a path segment. Schema context distinguishes
struct fields from map keys. Expression operands keep the destination path.
Then, Else, CoalesceValue and CoalesceDefault retain its effective type because
their results have that type. Conditions, arguments, arithmetic operands, access
bases and the other input edges clear it. The edge match is exhaustive, so a new
edge requires an explicit type-context decision. ExpressionEdge ancestry is a
separate axis. Protected paths may contain private map keys and are never
publicly exposed.

### Changed access, function identity and fold lengths

A changed Access first uses ValueKind::rebuild_access. The three evaluable stages
delegate to AccessExpr::rebuild_with_base with AccessMissPolicy::Error, which
calls the existing rebuild_result primitive. SnapshotLeaf uses its existing
apply_snapshot_path_segment primitive. Both reuse the original metadata-restoring
operation, extracted once on AccessExpr. A successful canonical selection can
collapse an access; only an unsuccessful attempt retains a dormant access via
the existing retain_deferred_miss_display_root operation. This matches the
canonical-first/fallback ordering in parser/ast.rs. Unchanged access shells keep
their source and rooted-loop metadata. The retry needs one base copy only when
changed, because the existing consuming access primitive does not return its
input on failure; traversal itself moves the owned base.

An unchanged resolved callee retains its original Arc, and an unchanged lexical
scope retains its original map Arc, even when call arguments change. A candidate
uses its own change flag rather than an enclosing traversal's flag. A replacement
scope map is allocated only after at least one scope entry changes. The callee
Arc and the by-value UserFunction entries in its functions map are distinct
stored definitions, each visited once at its own expression edge. They cannot
be deduplicated by a function name: equal names can contain different bodies.
This keeps the existing all-edges/evidence contract and preserves knowledge
sharing by Arc identity when values are unchanged.

Parts::len counts argument values through the same try_visit_values API as the
fold, and function operands through the same function_values iterator as the
borrowed fold. Defaults, locals and bodies contribute separate outputs; a
resolved call does not count an entire function as one output.

## Knowledge remains the classifier

The kernel has **no** known/unknown classification, is-known predicate, pending
phase calculation, or expression evaluator. ScalarKind, LeafKind, and
ExpressionKind describe representation only. In particular, a Concrete List or
Map can contain pending values, and Secret can contain a known value. Any owner
that needs semantic knowledge must call Value::knowledge with its existing
KnowledgeContext. The identity-rewrite table in [the tests](tests.rs) checks
classification and known-value admission before and after a kernel rewrite.

## Evidence and unevaluated expression edges

Evidence<O, R> carries two independent sequences: authored obligation handles
and result-path receipt handles. Each Scoped handle retains the exact LeafPath,
expression ancestry, and Secret scope where it was attached. Owners retain the
typed evidence in their own inventories and use restricted summary handles
(such as usize indices) across the fold. The kernel does not mint a conversion
proof or interpret either handle. Evidence::merge appends both channels without
rebasing paths or combining their meanings.

ExprChildren::with_evidence returns ExpressionWithEvidence. Its inspect and
transform operations schedule the operands through the same dispatcher, then
hand off the attached evidence together with the child outputs. The standalone
inspect_expression_edge and transform_expression_edge adapters provide this
same operand-plus-evidence form for externally owned expressions. For example,
a retained conversion operand uses EvidenceOperand, and a directive visitor
uses DirectiveBody(LifecycleDirective) with its stored body value.

A located inspection exposes its folded output and whether the traversal
encountered protected values. MarkedTransform::into_parts returns the rewritten
value and that located output separately. This preserves four independent
pieces: value, scoped authored obligations, result-path receipts, and protected
provenance. The aggregate protection flag records traversal ancestry/encounters;
it is not an evaluation or taint inference. An obligation attached outside
Secret stays in its authored public scope even if its operand contains Secret.
An obligation attached inside Secret retains protected scope and a hidden path.

ModuleIf schedules Condition, Then, and Else. Coalesce schedules CoalesceValue
and CoalesceDefault, including an unused fallback. Calls include their argument
values and access metadata; resolved calls include captured function defaults,
locals, bodies, and scope definitions. Nothing selects a branch, invokes a
builtin, substitutes a slot, or materializes a directive. Thus an owner can
inventory obligations in unselected computations without evaluating them.

ReferenceRef exposes only nonrecursive reference identity through the restricted
summary callback. Scoped::inspect re-enters an owner-resolved binding value at
a saved location, retaining its path, expression edges, and Secret ancestry.
It clears schema context because the resolved value has its own type. Owners
keep the visited set/worklist; the kernel never follows reference graphs or
chooses a cycle policy. The binding-worklist test covers a protected cyclic graph.

## Protected projection, freezing, and selected results

### G1: enter before redaction

SecretChild<Transform>::redact takes an Inspect algebra and an owner callback.
It enters the original payload through the same dispatcher and completes that
inspection successfully before invoking the callback. The callback receives an
opaque ProtectedSecret, with no plaintext projection, Clone, or Debug, and may
return only RedactedSecretHash. The kernel calls the stage's existing
from_redacted_secret API and returns the hash leaf plus the unchanged inspection
output. It never copies a protected map or its keys into the replacement.

A redaction owner first uses the existing Value::to_snapshot stage conversion,
then supplies its evidence-inspection algebra. A non-snapshot output stage
rejects the replacement with RedactionStage. Nested Secret nodes inside a whole
protected container are inspected and retain their obligations even though the
entire enclosing container becomes one digest. If inspection rejects, hashing
is not invoked and no successful Secret step is minted.

ProtectedSecret::hash_snapshot<S> bridges the snapshot's leaf marker to the
owner's expected evaluable stage S (NoPlaceholder, LoopSlot, or ParserPlaceholder)
with a fallible, private serde marker decode and the existing stage/binding
mapping APIs. Only the leaf marker is decoded; Value is never serialized through
an unprotected public callback. Existing canonical_secret_hash_material and
RedactedSecretHash remain the hash format. Matching previous hashes can be
reused. Invalid/mismatched markers and already-redacted nested material produce
a payload-free SecretHash error. This adapter neither classifies knowledge nor
mints evaluation/admission authority. Its compatibility test covers all three
stages and binding references.

### G2: persist and reattach protected obligations

For a fresh inspect/transform root, Scoped::protected_locator yields an opaque
ProtectedLocator for a physical position below a Secret in that root. Its versioned serde form contains
only numbers: structural routes to the enclosing Secret and to the position.
No authored field/key text or value is stored, serialized, or shown by Debug.
List indices, fixed expression-edge tags, and canonical map-key ranks form the
routes. Key ranks use sorted keys while iteration and output order stay intact.
Thus a JSON round trip or verified map-key reordering does not silently attach
an obligation to a sibling. Secret scope remains separate from LeafPath; a
private locator hop does not add a Secret segment to LeafPath.

The owner freezes its obligation handle/metadata with the locator, then verifies
and rehydrates the saved value under its apply lock. ProtectedLocator::resolve
uses an inspection algebra over the single dispatcher to return a Scoped handle
at that actual protected position. Hash-only trees, public positions, unknown
versions, and absent coordinates fail with a payload-free error. public_path
stays None after resolution. A locator is a structural coordinate, not plan
admission or cryptographic identity: the owner must validate the saved evidence
and rehydrated content and use the same typed root and naming constructor. Resolving each locator walks
the root; batching is left to a future measured optimization.

Owner-resolved binding worklist hops retain protection but start a detached
value root. Its locator carries a private Reentered origin, serialized as numeric
tag 2, distinct from a fresh inspect/transform root's Walk tag 1. Inherited Secret
scope also retains this tagged locator when the referenced value has no new
Secret wrapper. ProtectedLocator::resolve rejects a Reentered origin with the
specific WalkError::ReenteredLocator before traversing any supplied tree. Thus a
persisted re-entry coordinate cannot silently attach to another root's node.
To freeze a physical location in a referenced value, the owner starts a fresh
inspect/transform with that value's actual root; Scoped::inspect is the logical
binding-worklist operation. As before, deserialization is not admission authority
or protection against tampering with serialized coordinates.

### G3: move result receipts explicitly

Evidence::reroot_receipts(call_at, argument_index, select) moves only selected
receipts from that exact argument subtree to the call's result position. It
checks structural ancestry as well as expression/path ancestry; another
argument or a container above the call is rejected. Unselected receipts and all
authored obligations retain their locations. Receipt protection is the maximum
of its old and destination scopes, so a public call cannot declassify a receipt
from a Secret argument. Moved receipts discard their old physical locator;
protected provenance and logical result position remain independent.

Step::try_map_output applies this checked operation after an Inspect or Transform
fold. Transform keeps the rewritten value and change flag while mapping its
output. The lookup regression moves the receipt from Argument(0)/map key v onto
the call result and checks identical results in both modes.

## Typed roots beyond resource attributes

| Source | Kernel root and existing source API |
|---|---|
| Resource attribute / nested schema node | WalkRoot::attribute or typed(EffectiveType); preserves resource ancestry. |
| Provider config | WalkRoot::config_attribute(ValidatedConfigSchema, name), using effective_type(name). |
| Backend config | Same validated-config constructor, or an existing EffectiveType passed to typed. |
| Module arguments | DeclarationRoot::new(&parameter.static_type).root(); consumer parameters use static_type.known() first. |
| Annotated attributes | DeclarationRoot::new(attribute.static_type.as_ref()?).root(); consumer annotations use their known resolved TypeExpr. |
| Exports | DeclarationRoot::new(export.static_type.known()?).root(); parsed explicit exports use their optional annotation. Local known inferred exports are supported. |
| Upstream exports | DeclarationRoot::new(entry.static_type.declared_known()?).root(); upstream inferred types deliberately supply no declaration authority. |
| RootlessType | WalkRoot::rootless(&RootlessType), using the approved crate-only effective_type accessor to retain its validated root and named definitions. |

All existing typed WalkRoot constructors select canonical field names.
Authored DSL aliases require attribute_by_authored_name,
typed_by_authored_name, config_attribute_by_authored_name or
rootless_by_authored_name; declaration roots offer root_by_authored_name.
The choice lives in the opaque root and follows descendants automatically.
Canonical lookup matches EffectiveField::name only. Only the explicitly named
find_field_by_authored_name path calls the existing alias-aware accessor.
Both domains emit StructField segments using the selected field's canonical name,
while Transform retains input map keys. A missing field in the selected domain
returns the text-free UnknownStructField error rather than inventing a typed
position for an authored alias.

The field-candidate accessor is called only after shape() is Struct. In the
existing schema API that arm takes exactly one ShapeWalkBudget step and extends
its fields without recursing into their types. A fresh budget of 64 therefore
cannot exhaust on this path. The r4 test pins this with 96 fields behind 96
reference hops and a one-step budget. Transparent List/Union searches are not
used here; extending the kernel to those searches would require an explicit
fallible exhaustion contract, not treating exhaustion as an absent field.

DeclarationRoot owns the existing definition-free LiftedType from the public
lift_type_expr API. Its root borrows that type through the existing crate API
EffectiveType::for_value_walk and empty definitions; no new lifting/classification
walk is introduced. The type cursor retains identified owners and constraints.
Unliftable declarations return None instead of silently using a syntactic root.

WalkRoot::rootless borrows the validated root and its complete definition map
through RootlessType::effective_type. It does not rebuild a declaration or
discard named references. The accessor delegates to EffectiveType::for_value_walk
and is the only change in schema/mod.rs. The rootless test covers both a public
ValidatedConfigSchema::rootless_type source and nested named definitions,
retaining StructField paths through a list.

## #4260 hand-off: seven seams

These are the seven points from carina-00's hand-off note. This PR changes none
of their implementations. CheckedConversion and ExpressionEvidence do not exist
on this main revision. Adding a value/expression representation variant must
first fail the exhaustive engine dispatch; #4260 must add an explicit expression
arm with operand recursion and evidence hand-off. Existing consumers should not
add another value recursion for it. The generic operand/evidence shape is
implemented and exercised here already.

| # | Consumer seam | Exact kernel entry point / arm | Evidence and scope hand-off |
|---|---|---|---|
| 1 | conversion_inventory / contains_conversion (visit_value_expressions) | inspect; Algebra::expression; ExprChildren::with_evidence(...).inspect; inspect_expression_edge with EvidenceOperand for an externally owned operand | Inventory owner supplies obligation/receipt handles; Evidence retains each scoped handle. Both branch and fallback edges are visited. |
| 2 | admission::collect_projection_obligations (path-carrying ExpressionVisitor) | WalkRoot::attribute / typed / config_attribute / rootless, or their *_by_authored_name constructors; DeclarationRoot::new(...).root() / root_by_authored_name(); Algebra::expression and its At; Evidence::at / ExprChildren::with_evidence | Authored obligations retain their scope and LeafPath, independently of result receipts. Expression ancestry distinguishes branch operands at the same destination path. |
| 3 | admission::walk_obligation_bindings (cycle-robust worklist) | Algebra::placeholder; ScopedLeaf::with_reference; Scoped::inspect | Owner queues reference identities and Scoped obligation handles, deduplicates binding/scope identities, then re-enters resolved values without resetting Secret ancestry. Detached locators retain a Reentered origin and reject physical resolution. |
| 4 | admission::freeze_conversion_candidates (shared backend Freeze visitor) | transform / transform_expression_edge; ExpressionWithEvidence::transform; SecretChild::redact (G1); Scoped::protected_locator and ProtectedLocator::resolve (G2); MarkedTransform::into_parts | Freeze owns interpretation. Normal rewrites restore Secret wrappers. G1 inventories first, then replaces the complete protected node with a digest. G2 persists opaque numeric coordinates and resolves scoped obligations only on the verified, rehydrated tree. |
| 5 | builtin argument preparation (Argument/List/Map/Any identity paths) | Argument / ArgumentValue edges; Children::transform(algebra, Evidence::merge); Step::try_map_output and Evidence::reroot_receipts (G3) | Identity paths merge receipts. Selection paths explicitly reroot only chosen receipts from the exact argument ancestor to the call result; Secret ancestry is joined and authored obligations stay put. Unchanged argument shells retain auxiliary metadata. |
| 6 | parser loop substitution (retained conversions and Coalesce fallback) | Algebra::placeholder; ScopedLeaf::with_placeholder; Algebra::expression; CoalesceValue / CoalesceDefault; EvidenceOperand | Slot substitution belongs to the owner. Structural rewriting retains Coalesce and its unevaluated fallback and transports both evidence channels. |
| 7 | Directives::try_visit_expressions (including PerElement) | inspect_expression_edge / transform_expression_edge with DirectiveBody(directive), on the directive owner's stored Value<LoopSlot, B> | The owner forwards its private stored body from its own visitor. No new payload accessor is needed for this port. The fold visits every nested condition, branch, and fallback before any per-element evaluation. |

The promised find_unavailable_evaluation CheckedConversion behavior maps to the
same explicit expression arm: schedule its operand through EvidenceOperand,
then hand off the conversion evidence. Unavailable is a structural expression
here, so inspecting it does not return its diagnostic or execute it. Selection
and unavailable-evaluation policy remain consumer work after #4260.

## Public surface

The complete public signatures are in [mod.rs](mod.rs). The core types are:

```text
trait Algebra<M: WalkMode> {
    type Output: WalkOutput;
    scalar(At, ScopedScalar) -> Step;
    list(At, Children) -> Step;
    map(At, Children) -> Step;
    expression(At, ExprChildren) -> Step;
    placeholder(At, ScopedLeaf) -> Step;
    secret(SecretAt, SecretChild) -> SecretStep;  // required, no default
}
inspect(root, &Value<P, B>, &mut A) -> Result<Located<A::Output>, WalkError>
transform(root, Value<P, B>, &mut A)
    -> Result<MarkedTransform<A::Output, P, B>, WalkError>
inspect_expression_edge(root, edge, &value, obligations, receipts, &mut A)
    -> Result<Located<Evidence<O, R>>, WalkError>
transform_expression_edge(root, edge, value, obligations, receipts, &mut A)
    -> Result<MarkedTransform<Evidence<O, R>, P, B>, WalkError>
ExprChildren::with_evidence(obligations, receipts) -> ExpressionWithEvidence
Evidence::at(&At, obligations, receipts) -> Evidence<O, R>
Evidence::merge(outputs) -> Evidence<O, R>
MarkedTransform::into_parts() -> (Value<P, B>, Located<O>)
Scoped<T>::inspect(&Value<P, B>, &mut A) -> Result<Located<A::Output>, WalkError>
SecretChild<Transform>::redact(&mut inspector, ProtectedSecret -> Result<RedactedSecretHash, WalkError>)
    -> SecretStep<Transform, inspector::Output, P, B>
ProtectedSecret::hash_snapshot<S>(previous_hash) -> Result<RedactedSecretHash, WalkError>
Scoped<T>::protected_locator() -> Option<&ProtectedLocator>
ProtectedLocator::resolve(root, &rehydrated, output) -> Result<Scoped<T>, WalkError>
Evidence::reroot_receipts(&call_at, argument, select) -> Result<Self, WalkError>
Step::try_map_output(output -> Result<output, WalkError>) -> Step
WalkRoot::config_attribute(&ValidatedConfigSchema, name) -> Option<WalkRoot>
WalkRoot::rootless(&RootlessType) -> WalkRoot
WalkRoot::attribute_by_authored_name(&ValidatedResourceSchema, name) -> Option<WalkRoot>
WalkRoot::typed_by_authored_name(EffectiveType) -> WalkRoot
WalkRoot::config_attribute_by_authored_name(&ValidatedConfigSchema, name) -> Option<WalkRoot>
WalkRoot::rootless_by_authored_name(&RootlessType) -> WalkRoot
DeclarationRoot::new(&TypeExpr) -> Option<DeclarationRoot>
DeclarationRoot::root() -> WalkRoot
DeclarationRoot::root_by_authored_name() -> WalkRoot
```

The stage adapter added on the owning ValueKind trait has the signature
`rebuild_access<B: BindingAddress>(&AccessExpr<Self, B>, Value<Self, B>) ->
Result<Value<Self, B>, ValueAccessError>`. It delegates to the existing access
seam for that stage; it adds no classifier. The crate-only move adapters are
FunctionArgument::try_map_values, AccessExpr::try_map_base and
ValidatedIntegerExpr::into_parts. Their only kernel caller owns the operands.

Each algebra method is generic over ValueKind and BindingAddress so that
captured parser-stage function definitions use the same dispatcher as an
expanded call. WalkMode is sealed to Inspect and Transform. WalkOutput is
sealed to (), bool, usize, i64, Option/Vec of approved outputs, and Evidence of
approved outputs. Strings, raw values, and arbitrary error payloads cannot be
returned through this result channel. Existing public Value enums, callbacks,
and algebra side effects still permit independent code outside this kernel;
this preparatory PR does not claim to seal the whole representation.

## Compile-time contracts


An otherwise complete algebra cannot omit `secret`:

```compile_fail,E0046
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
struct Count;
impl Algebra<Inspect> for Count {
    type Output = usize;
    fn scalar<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedScalar<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.finish(1) }
    fn list<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn map<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn expression<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ExprChildren<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn placeholder<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedLeaf<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.finish(0) }
}
```

Its compiling twin borrows values, folds all children, and explicitly re-enters
Secret. The location passed to an arm is issued by the engine:

```
use carina_core::{resource::{BindingAddress, ConcreteValue, DeferredValue, Value, ValueKind}, value_walk::*};
struct Count;
impl Algebra<Inspect> for Count {
    type Output = usize;
    fn scalar<P: ValueKind, B: BindingAddress>(&mut self, at: &At<'_>, v: ScopedScalar<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> {
        assert!(at.is_secret());
        assert!(at.public_path().is_none());
        v.finish(1)
    }
    fn list<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn map<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn expression<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ExprChildren<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.inspect(self, |v| v.into_iter().sum()) }
    fn placeholder<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedLeaf<'_, Inspect, P, B>) -> Step<Inspect, usize, P, B> { v.finish(0) }
    fn secret<P: ValueKind, B: BindingAddress>(&mut self, _: &SecretAt<'_>, v: SecretChild<'_, Inspect, P, B>) -> SecretStep<Inspect, usize, P, B> { v.inspect(self) }
}
let value: Value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::Int(5))).into()));
assert_eq!(inspect(WalkRoot::syntactic(), &value, &mut Count)?.into_output(), 1);
# Ok::<(), WalkError>(())
```

A Secret step cannot be forged from an ordinary completed step:

```compile_fail,E0451
use carina_core::value_walk::{Inspect, Step, SecretStep};
fn forge(step: Step<Inspect, ()>) -> SecretStep<Inspect, ()> {
    SecretStep { step }
}
```

The compiling twin obtains the step only by entering the supplied child:

```
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
fn enter<P: ValueKind, B: BindingAddress, A: Algebra<Inspect>>(
    child: SecretChild<'_, Inspect, P, B>, algebra: &mut A,
) -> SecretStep<Inspect, A::Output, P, B> {
    child.inspect(algebra)
}
```

`At` cannot be fabricated from a path or used to reset protected ancestry:

```compile_fail,E0451
use carina_core::{schema::LeafPath, value_walk::At};
let _ = At {
    effective: None,
    path: LeafPath::from_segments([]),
    expressions: vec![],
    secret_depth: 0,
    encountered_secret: std::rc::Rc::new(std::cell::Cell::new(false)),
    position: vec![],
    secret_anchor: None,
};
```

The compiling twin accepts an engine-issued position:

```
use carina_core::{schema::LeafPath, value_walk::At};
fn public_location<'a>(at: &'a At<'_>) -> Option<&'a LeafPath> {
    at.public_path()
}
```

A root cannot be forged by setting its authority fields:

```compile_fail,E0451
use carina_core::value_walk::WalkRoot;
let _ = WalkRoot { effective: None };
```

The compiling twin explicitly chooses a syntactic root or obtains one from a
validated schema:

```
use carina_core::{schema::{AttributeSchema, AttributeType, ResourceSchema}, value_walk::WalkRoot};
let _ = WalkRoot::syntactic();
let schema = ResourceSchema::new("test.Example")
    .attribute(AttributeSchema::required("count", AttributeType::int()).build())
    .validate().unwrap();
let _ = WalkRoot::attribute(&schema, "count").unwrap();
```

Transform consumes the input and restores the wrapper around a replacement:

```
use carina_core::{resource::{BindingAddress, ConcreteValue, DeferredValue, Value, ValueKind}, value_walk::*};
struct Rewrite;
impl Algebra<Transform> for Rewrite {
    type Output = ();
    fn scalar<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedScalar<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.replace(Value::Concrete(ConcreteValue::Int(7)), ()) }
    fn list<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn map<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn expression<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ExprChildren<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn placeholder<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedLeaf<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.keep(()) }
    fn secret<P: ValueKind, B: BindingAddress>(&mut self, _: &SecretAt<'_>, v: SecretChild<'_, Transform, P, B>) -> SecretStep<Transform, (), P, B> { v.transform(self) }
}
let value: Value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::Int(5))).into()));
let rewritten = transform(WalkRoot::syntactic(), value, &mut Rewrite)?.into_value();
assert!(matches!(rewritten, Value::Deferred(DeferredValue::Secret(_))));
assert!(!format!("{rewritten:?}").contains('7'));
# Ok::<(), WalkError>(())
```

The rewrite algebra also requires its own Secret arm:

```compile_fail,E0046
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
struct Rewrite;
impl Algebra<Transform> for Rewrite {
    type Output = ();
    fn scalar<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedScalar<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.keep(()) }
    fn list<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn map<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: Children<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn expression<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ExprChildren<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.transform(self, |_| ()) }
    fn placeholder<P: ValueKind, B: BindingAddress>(&mut self, _: &At<'_>, v: ScopedLeaf<'_, Transform, P, B>) -> Step<Transform, (), P, B> { v.keep(()) }
}
```

External code cannot select raw children from the cursors and match past the
dispatcher. This restriction covers scalars, leaves, containers, expressions,
and the protected payload separately:

```compile_fail,E0616
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn bypass(value: ScopedScalar<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>) {
    let _raw = value.input;
}
```

```compile_fail,E0616
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn bypass(value: ScopedLeaf<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>) {
    let _raw = value.input;
}
```

```compile_fail,E0616
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn bypass(children: Children<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>) {
    let _raw = children.parts;
}
```

```compile_fail,E0616
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn bypass(expression: ExprChildren<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>) {
    let _raw = expression.children;
}
```

```compile_fail,E0616
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn bypass(child: SecretChild<'_, Inspect, NoPlaceholder, AbsoluteBindingAddress>) {
    let _raw = child.payload;
}
```

An ordinary step cannot be fabricated, nor can a scope witness be attached to
an arbitrary path:

```compile_fail,E0451
use carina_core::value_walk::{Inspect, Step};
let _ = Step::<Inspect, ()> { result: Ok(()) };
```

```compile_fail,E0451
use carina_core::{schema::LeafPath, value_walk::Scoped};
let _ = Scoped {
    output: 1_usize,
    path: LeafPath::from_segments([]),
    expressions: vec![],
    secret_depth: 0,
    locator: None,
    position: vec![],
};
```

A Secret child cannot issue a successful keep without re-entry:

```compile_fail,E0599
use carina_core::{resource::{NoPlaceholder, AbsoluteBindingAddress}, value_walk::*};
fn skip(child: SecretChild<'_, Transform, NoPlaceholder, AbsoluteBindingAddress>) -> SecretStep<Transform, (), NoPlaceholder, AbsoluteBindingAddress> {
    child.keep(())
}
```

Modes and result channels are sealed:

```compile_fail,E0277
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
struct Bypass;
impl WalkMode for Bypass {
    type Product<O: WalkOutput, P: ValueKind, B: BindingAddress> = O;
}
```

```compile_fail,E0277
use carina_core::value_walk::WalkOutput;
fn plaintext() -> impl WalkOutput { String::new() }
```

The value of a completed rewrite and its folded evidence can only be read after
completion, through MarkedTransform::into_parts. A protected path stays private:

```compile_fail,E0616
use carina_core::value_walk::Scoped;
fn bypass(scope: &Scoped<usize>) { let _ = &scope.path; }
```


Borrowed container cursors cannot call the owned rebuilding operation:

```compile_fail,E0599
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
fn cannot_rebuild<P: ValueKind, B: BindingAddress, A: Algebra<Transform, Output = ()>>(
    children: Children<'_, Inspect, P, B>, algebra: &mut A,
) {
    children.transform(algebra, |_| ());
}
```

The same separation applies to protected operands:

```compile_fail,E0599
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
fn cannot_rebuild_secret<P: ValueKind, B: BindingAddress, A: Algebra<Transform, Output = ()>>(
    child: SecretChild<'_, Inspect, P, B>, algebra: &mut A,
) {
    child.transform(algebra);
}
```

Redaction requires an inspection algebra; a digest alone cannot mint completion:

```compile_fail
use carina_core::{resource::{BindingAddress, ValueKind, RedactedSecretHash}, value_walk::*};
fn skip<P: ValueKind, B: BindingAddress>(child: SecretChild<'_, Transform, P, B>, hash: RedactedSecretHash) {
    child.redact(hash);
}
```

The protected hashing handle exposes no payload for copying or formatting:

```compile_fail,E0616
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::*};
fn plaintext<P: ValueKind, B: BindingAddress>(protected: ProtectedSecret<'_, P, B>) {
    let _ = protected.payload;
}
```

```compile_fail,E0308
use carina_core::{resource::{BindingAddress, ValueKind, Value, ConcreteValue}, value_walk::*};
fn copy<P: ValueKind, B: BindingAddress, A: Algebra<Inspect>>(child: SecretChild<'_, Transform, P, B>, algebra: &mut A) {
    child.redact(algebra, |_| Ok(Value::<P, B>::Concrete(ConcreteValue::Map(Default::default()))));
}
```

Protected locators and protection ancestry cannot be fabricated by field access:

```compile_fail,E0423
use carina_core::value_walk::ProtectedLocator;
let _ = ProtectedLocator(1, Vec::new(), Vec::new());
```

```compile_fail,E0616
use carina_core::value_walk::Scoped;
fn unprotect(mut receipt: Scoped<usize>) { receipt.secret_depth = 0; }
```


The opaque protected hashing handle cannot be formatted:

```compile_fail,E0277
use carina_core::{resource::{BindingAddress, ValueKind}, value_walk::ProtectedSecret};
fn format<P: ValueKind, B: BindingAddress>(protected: ProtectedSecret<'_, P, B>) {
    let _ = format!("{protected:?}");
}
```

External code enters a validated rootless type through WalkRoot::rootless;
the schema accessor remains crate-only:

```compile_fail,E0624
use carina_core::schema::RootlessType;
fn bypass(rootless: &RootlessType) {
    let _ = rootless.effective_type();
}
```

A declaration root cannot erase arbitrary resource-schema ancestry:

```compile_fail,E0308
use carina_core::{schema::CompiledAttributeType, value_walk::DeclarationRoot};
fn erase(ty: &CompiledAttributeType) { let _ = DeclarationRoot::new(ty); }
```


The naming domain and persisted locator origin cannot be rewritten by external
code; their constructors and resolver enforce the choice:

```compile_fail,E0616
use carina_core::value_walk::WalkRoot;
fn change_names(mut root: WalkRoot<'_>) {
    root.field_names = root.field_names;
}
```

```compile_fail,E0616
use carina_core::value_walk::ProtectedLocator;
fn change_origin(mut locator: ProtectedLocator) {
    locator.0 = locator.0;
}
```
