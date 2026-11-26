# Value traversal kernel contract

## Dispatch and mode-specific calls

[The engine](engine.rs) has one exhaustive Value/ConcreteValue/DeferredValue
variant list. Dispatch destructures each variant once and passes typed fields
to cursors. There is no raw Value paired with a separate variant tag, catch-all
variant arm, runtime traversal flag, or consumer opt-in for call definitions.
The source guard checks the kernel directory for panicking fallbacks and
unchecked production unwrap calls.

The private sealed Mode trait chooses both Input and ResolvedCall. The call
projection is constructed at the shared dispatch boundary:

| Mode | ResolvedCall projection | Traversal and child length |
|---|---|---|
| Inspect | InspectCall contains only a borrowed argument vector. | Argument values, including auxiliary provenance, in their existing order. |
| Transform | TransformCall owns the callee Arc, captured-scope Arc, arguments and source line. | Callee defaults/locals/body, sorted captured definitions, then argument values. |

Parts::ResolvedCall contains that associated projection, not generic definition
fields. The Inspect fold can destructure only InspectCall { args }; it cannot
obtain a callee, captured scope, source line, or definition iterator. Every
syntactic, typed, authored-name, declaration and rootless root dispatches through
this same projection. Switching to another Inspect algebra at a cursor uses the
same shape. The call's ExpressionKind remains on ExprChildren.

Children::len and is_empty delegate to Parts::len, which delegates resolved-call
length to the same mode-specific projection. Inspect length counts argument
values through FunctionArgument::try_visit_values, just like traversal. Transform
length includes one output per parameter default, local and body, for the callee
and each distinct stored scope definition, plus argument values. Names do not
deduplicate stored definitions: a callee and equally named scope entry may have
different bodies. Inspect enters zero definitions; its work depends on operand
visits, path copying and map sorting, independently of definition graph size.

## Scope, order and rewriting

Inspect borrows operands and folds restricted summaries through Algebra<Inspect>.
The six arms include a required secret arm. Successful Secret completion enters
its child through the kernel; rejection is allowed. There is no successful skip.
Cursors expose engine operations rather than raw child values. WalkMode and
WalkOutput are sealed. Approved outputs are (), bool, usize, i64, Option/Vec of
approved outputs, and Evidence of approved outputs. Strings, Values and arbitrary
errors cannot leave through that result channel. Existing public Value enums
and side effects still allow independent walkers outside this kernel.

Locations retain LeafPath's StructField, MapKey and ListIndex segments. Secret
adds protection ancestry without a path segment. At and Scoped expose a public
path only outside Secret. Protection belongs to each site; inspect returns only
the folded output, and there is no walk-wide secret flag or locator emission.
ReferenceRef exposes only nonrecursive reference identity. The kernel does not
follow bindings or provide detached re-entry or expression-edge roots.

Expression ancestry is independent of container paths. Then, Else, CoalesceValue
and CoalesceDefault retain the destination effective type. Conditions, argument
values, arithmetic operands, access bases and definition inputs clear it.
Argument(index) and ArgumentValue(index) retain their existing ordinals, including
auxiliary operands. Map traversal and outputs retain insertion order; sorted key
ranks are used only for internal ancestry checks. Source lines never sort errors.

The internal BoundTransform adapter consumes values and rebuilds typed fields.
A value operation returns Value, an argument operation FunctionArgument, and a
definition operation UserFunction; only summaries share a vector. Scalar, leaf
and whole-argument replacement stop at the replacement. There is no preliminary
inventory of discarded arguments. Normal Secret completion restores its wrapper
and retains per-site error protection. SecretPayload's existing owned adapter
copies its child because there is no consuming payload accessor. Other operands
move through their owning adapters; integer operands retain post-validation
checks. Change flags do not depend on Value equality.

An unchanged callee and captured scope retain both original Arcs, including when
only arguments change. Scope maps allocate replacements only after an entry
changes. Definition traversal enters through engine/definitions.rs and applies
FunctionDefinition or FunctionScope before the internal memo. Parser rewrites
suppress lexical substitution under these edges. The #4604 memo still shares
completion and storage-preserving candidates; it no longer records or replays
aggregate secret discovery. Memo cross-checks compare candidate storage, output
and changed flags.

Changed access first uses ValueKind::rebuild_access and the existing canonical
selection seam. Only a failed canonical rebuild retains a dormant access with
its original source/root metadata. Unchanged access shells preserve metadata.
The stage adapters remain crate-only; they introduce no knowledge classifier.

## Evidence and availability

Evidence<O, R> keeps authored obligation handles and result receipts in separate
sequences. Each Scoped handle retains its exact LeafPath, expression ancestry
and Secret depth. Owners keep diagnostic text or other payloads in private
inventories and pass restricted handles through the fold. Evidence::merge appends
both channels without rebasing them. ExprChildren::with_evidence(...).inspect
attaches evidence at the expression before child outputs, without evaluating it.

ModuleIf visits Condition, Then and Else; Coalesce visits value and unused default;
calls visit primary arguments and auxiliary access provenance. Inspect never
visits captured definitions. The internal parser policy may select or evaluate
operands under its existing domain rules; structural inspection performs neither.
The kernel leaves known/unknown classification to Value::knowledge and the existing
KnowledgeContext. Identity rewrite tests check classification remains unchanged.

All three availability policies use the operand projection. They exclude auxiliary
ArgumentValue(index != 0) provenance from obligations. Required follows resolution
demand and defers an unselected coalesce default; SavedCandidate includes defaults;
SavedSecret includes failures only at protected sites. Evidence preserves error
preorder, insertion order, source lines and handle ordinals. Only the selected
first failure's site controls masking. Unavailable data inside definitions cannot
allocate handles during Inspect; actual body evaluation still has its own errors
and Secret results.

Evidence::reroot_receipts moves selected receipts from the exact argument subtree
to a call's result. Structural and expression/path ancestry are checked, and
protection is joined, never cleared. Authored obligations and unselected receipts
stay at their original sites. Step::try_map_output preserves completion while
checking such output changes. The kernel does not interpret conversion evidence,
mint admission authority, hash protected values, or persist scoped positions.

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
without a second type walker. The rootless test covers both a public
ValidatedConfigSchema::rootless_type source and nested named definitions,
retaining StructField paths through a list.

## Retained entry points

The public borrowed entry point is inspect(root, &value, &mut algebra), used by
availability and test_utils. It returns the algebra's output directly. Cursors
provide inspect, finish, reject and evidence operations for Algebra<Inspect>.
Owned dispatch and definition traversal are internal to the kernel; parser and
module owners call concrete substitution/evaluation functions.

The ValueKind stage adapter rebuild_access delegates to the existing access seam.
FunctionArgument::try_map_values, AccessExpr::try_map_base and
ValidatedIntegerExpr::into_parts move owned operands without adding classifiers.
New representation variants must update the exhaustive dispatcher explicitly.

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
assert_eq!(inspect(WalkRoot::syntactic(), &value, &mut Count)?, 1);
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
use carina_core::value_walk::At;
fn reset(at: At<'_>) {
    let _ = At { secret_depth: 0, ..at };
}
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
fn reset(root: WalkRoot<'_>) {
    let _ = WalkRoot { effective: None, ..root };
}
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

A protected path stays private:

```compile_fail,E0616
use carina_core::value_walk::Scoped;
fn bypass(scope: &Scoped<usize>) { let _ = &scope.path; }
```

Protection ancestry cannot be cleared by external code:

```compile_fail,E0616
use carina_core::value_walk::Scoped;
fn unprotect(mut receipt: Scoped<usize>) { receipt.secret_depth = 0; }
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


The root's naming domain cannot be rewritten by external code:

```compile_fail,E0616
use carina_core::value_walk::WalkRoot;
fn change_names(mut root: WalkRoot<'_>) {
    root.field_names = root.field_names;
}
```

An Inspect call cursor has no callee or captured scope handle. Its sealed
projection stores only arguments, regardless of the algebra or root. The seal
is the pub(super) Parts type and InspectCall, which has no callee or captured
scope fields. The types enforce this restriction; it is not pinned by a
compile-fail doctest. The public cursor already hid these fields before the
operand-only projection, so accessing them would also fail at the base revision.
