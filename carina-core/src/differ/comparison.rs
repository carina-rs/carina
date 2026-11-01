//! Type-aware comparison logic for diffing resource attributes.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

use crate::explicit::{ExplicitFields, ExplicitProjection};
use crate::provider::{InitializedProvider, NestedWriteOnlyRecord, ReferenceScope};
use crate::resource::{
    ApplyTimeUnknown, CanonicalComparisonAttributes, ConcreteValue, DeferredValue,
    InterpolationPart, MaskedComparisonAttributes, ResolvedIgnoreChanges, SavedValueViews, Value,
    ValueString, merge_with_saved, value_at_leaf,
};
use crate::schema::{
    CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema, DslTransform,
    LeafPath, OwnerBoundResourceSchema, ReferenceForm, ReferenceForms, TypeIdentity,
    empty_defs_for_schema_walks,
};
use crate::value::{PersistedValue, canonical_secret_hash_material, contains_secret};

/// Scope and schema forms resolved from one initialized provider. This is not
/// a comparison context: callers must explicitly choose planning or observation.
#[derive(Debug, Clone, Copy)]
pub struct ComparisonSchema<'a> {
    scope: &'a ReferenceScope,
    reference_forms: &'a ReferenceForms,
}

/// Shared comparison core parameterized by an explicit reference equality policy.
/// Planning entry points accept only [`PlanningComparison`].
#[derive(Debug, Clone, Copy)]
pub struct ComparisonContext<'a, R: ReferenceEquality> {
    schema: ComparisonSchema<'a>,
    references: R,
    path: &'a str,
}

pub type PlanningComparison<'a> = ComparisonContext<'a, PlanningEquality<'a>>;
pub type ObservationComparison<'a> = ComparisonContext<'a, ObservationEquality>;
pub type AuthoringComparison<'a> = ComparisonContext<'a, AuthoringEquality<'a>>;
pub(crate) type SavedPlanComparison<'a> = ComparisonContext<'a, DisplayEquality>;

mod sealed {
    pub trait Sealed {}
}

/// A closed set of policies for shared structural comparison helpers.
pub trait ReferenceEquality: sealed::Sealed + std::fmt::Debug + Copy {
    #[doc(hidden)]
    fn equal(&self, path: &str, identity: &TypeIdentity, a: &ValueString, b: &ValueString) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct PlanningEquality<'a>(&'a ReferenceComparisonSession<'a>);

/// Observation changes are byte changes, including reference-typed leaves.
#[derive(Debug, Clone, Copy)]
pub struct ObservationEquality;

/// Reuse prepared identities solely for authoring correspondence during state
/// writeback. Missing evidence stays unequal and never blocks persistence of
/// a provider operation that has already completed.
#[derive(Debug, Clone, Copy)]
pub struct AuthoringEquality<'a> {
    references: Option<&'a crate::references::ResolvedReferences>,
    resource: &'a crate::resource::ResourceId,
}

/// Byte equality for rendering recorded effects. This policy cannot enter a
/// planning API and never resolves references or changes recorded effects.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DisplayEquality;

impl sealed::Sealed for PlanningEquality<'_> {}
impl sealed::Sealed for ObservationEquality {}
impl sealed::Sealed for AuthoringEquality<'_> {}
impl sealed::Sealed for DisplayEquality {}

impl ReferenceEquality for PlanningEquality<'_> {
    fn equal(&self, path: &str, identity: &TypeIdentity, a: &ValueString, b: &ValueString) -> bool {
        match self
            .0
            .references
            .equal(self.0.resource, path, identity, a, b)
        {
            Ok(equal) => equal,
            Err(error) => {
                self.0.error.borrow_mut().get_or_insert(error);
                false
            }
        }
    }
}

impl ReferenceEquality for ObservationEquality {
    fn equal(&self, _: &str, _: &TypeIdentity, a: &ValueString, b: &ValueString) -> bool {
        a == b
    }
}

impl ReferenceEquality for AuthoringEquality<'_> {
    fn equal(&self, path: &str, identity: &TypeIdentity, a: &ValueString, b: &ValueString) -> bool {
        self.references.map_or(a == b, |references| {
            references
                .equal(self.resource, path, identity, a, b)
                .unwrap_or(false)
        })
    }
}

impl ReferenceEquality for DisplayEquality {
    fn equal(&self, _: &str, _: &TypeIdentity, a: &ValueString, b: &ValueString) -> bool {
        a == b
    }
}

#[derive(Debug)]
struct ReferenceComparisonSession<'a> {
    references: &'a crate::references::ResolvedReferences,
    resource: &'a crate::resource::ResourceId,
    error: std::cell::RefCell<Option<Box<crate::references::ReferencePlanError>>>,
}

impl<'a> ComparisonSchema<'a> {
    pub(crate) fn from_initialized_provider(
        provider: &'a InitializedProvider,
        schema: OwnerBoundResourceSchema<'a>,
    ) -> Self {
        Self::from_scope_and_forms(provider.reference_scope(), schema.reference_forms())
    }

    pub(crate) fn from_scope_and_forms(
        scope: &'a ReferenceScope,
        reference_forms: &'a ReferenceForms,
    ) -> Self {
        Self {
            scope,
            reference_forms,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(scope: &'a ReferenceScope, reference_forms: &'a ReferenceForms) -> Self {
        Self::from_scope_and_forms(scope, reference_forms)
    }

    /// Explicitly compare refreshed observations or state writeback bytes.
    pub fn observation(self) -> ObservationComparison<'a> {
        ComparisonContext {
            schema: self,
            references: ObservationEquality,
            path: "",
        }
    }

    pub fn authoring<'r>(
        &'r self,
        references: Option<&'r crate::references::ResolvedReferences>,
        resource: &'r crate::resource::ResourceId,
    ) -> AuthoringComparison<'r> {
        ComparisonContext {
            schema: *self,
            references: AuthoringEquality {
                references,
                resource,
            },
            path: "",
        }
    }

    /// Only this scope constructs a planning context. Its higher-ranked
    /// callback cannot release output until missing witness entries have been
    /// checked, and cannot return the context itself.
    pub fn with_references<T>(
        &self,
        references: &crate::references::ResolvedReferences,
        resource: &crate::resource::ResourceId,
        compare: impl for<'session> FnOnce(&PlanningComparison<'session>) -> T,
    ) -> Result<T, Box<crate::references::ReferencePlanError>> {
        let session = ReferenceComparisonSession {
            references,
            resource,
            error: std::cell::RefCell::new(None),
        };
        let context = ComparisonContext {
            schema: *self,
            references: PlanningEquality(&session),
            path: "",
        };
        let result = compare(&context);
        match session.error.into_inner() {
            Some(error) => Err(error),
            None => Ok(result),
        }
    }

    pub(crate) fn with_recorded_effects<T>(
        &self,
        compare: impl FnOnce(SavedPlanComparison<'_>) -> T,
    ) -> T {
        compare(ComparisonContext {
            schema: *self,
            references: DisplayEquality,
            path: "",
        })
    }
}

impl<'a> ObservationComparison<'a> {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(scope: &'a ReferenceScope, reference_forms: &'a ReferenceForms) -> Self {
        ComparisonSchema::for_test(scope, reference_forms).observation()
    }
}

impl PlanningComparison<'_> {
    pub(crate) fn prepared_references(&self) -> &crate::references::ResolvedReferences {
        self.references.0.references
    }
}

impl<R: ReferenceEquality> ComparisonContext<'_, R> {
    pub(crate) fn at_path<'p>(&'p self, path: &'p str) -> ComparisonContext<'p, R> {
        ComparisonContext {
            schema: self.schema,
            references: self.references,
            path,
        }
    }

    pub(crate) fn field_path(&self, field: &str) -> String {
        if self.path.is_empty() {
            field.into()
        } else {
            format!("{}.{field}", self.path)
        }
    }

    pub(crate) fn index_path(&self, index: usize) -> String {
        format!("{}[{index}]", self.path)
    }

    pub(crate) fn map_key_path(&self, key: &str) -> String {
        format!(
            "{}[{}]",
            self.path,
            crate::parser::display_string_literal(key)
        )
    }

    pub(crate) fn entry_path(
        &self,
        key: &str,
        ty: Option<&AttributeType>,
        defs: &BTreeMap<String, AttributeType>,
    ) -> String {
        if matches!(
            ty.map(|ty| ty.shape_with_defs(defs)),
            Some(crate::schema::Shape::Map { .. })
        ) {
            self.map_key_path(key)
        } else {
            self.field_path(key)
        }
    }

    pub(crate) fn leaf_path(&self, leaf: &LeafPath) -> String {
        let mut path = self.path.to_string();
        for segment in leaf.segments() {
            path = match segment {
                crate::schema::LeafPathSegment::StructField(name) => format!("{path}.{name}"),
                crate::schema::LeafPathSegment::MapKey(key) => {
                    format!("{path}[{}]", crate::parser::display_string_literal(key))
                }
                crate::schema::LeafPathSegment::ListIndex(index) => format!("{path}[{index}]"),
            };
        }
        path
    }

    fn reference_form(&self, identity: &TypeIdentity) -> Option<&ReferenceForm> {
        self.schema.reference_forms.get(identity)
    }

    /// Canonicalize the protocol's unqualified reference form.
    ///
    /// `{name}` denotes exactly one bare segment: it contains neither `:` nor
    /// `/`. Values containing either delimiter are already qualified or path
    /// forms and compare exactly; they are never expanded and can never equal
    /// a bare name through reference-form canonicalization.
    pub(crate) fn canonical_string<'b>(
        &self,
        value: &'b str,
        identity: Option<&TypeIdentity>,
        to_dsl: Option<&DslTransform>,
    ) -> Cow<'b, str> {
        let transformed = to_dsl
            .map(|transform| transform.apply(value))
            .unwrap_or(Cow::Borrowed(value));
        let Some(form) = identity.and_then(|identity| self.reference_form(identity)) else {
            return transformed;
        };
        if transformed.is_empty() || transformed.contains(':') || transformed.contains('/') {
            return transformed;
        }
        match form {
            ReferenceForm::Template { long } => long
                .render(self.schema.scope, transformed.as_ref())
                .map(Cow::Owned)
                .unwrap_or(transformed),
        }
    }
}

/// Type-aware semantic comparison of two Values.
///
/// When an `AttributeType` is provided, the comparison uses type information
/// to detect semantically equivalent values that differ textually:
/// - Int/Float coercion: `Int(1)` equals `Float(1.0)` for numeric types
/// - List/Map: recurse with inner element type
/// - Struct: recurse with per-field type information, tolerating extra fields
///   with default values (e.g., `false` for Bool)
/// - Enum: resolve typed enum identifiers and compare API spellings
///   case-insensitively (e.g., `awscc.s3.Bucket.Type.AES256` equals `"AES256"`)
///
/// Without type information, falls back to `Value::semantically_equal()`.
///
/// # Invariant: `Union[String, list(String)]` is canonicalized upstream
///
/// For attributes typed as `Union[String, list(String)]` (the IAM-style
/// `string_or_list_of_strings` shape — see #2481), **both** `a` and
/// `b` reach this function as the canonical `Value::Concrete(ConcreteValue::StringList)` form,
/// never as a mix of `Value::Concrete(ConcreteValue::String)` and `Value::Concrete(ConcreteValue::List([String]))`. The
/// canonicalization happens upstream in
/// `value::canonicalize_resources_with_schemas` (#2511) for the
/// desired side and `value::canonicalize_states_with_schemas` (#2513)
/// for the actual side, both run before the differ. Persisted values that
/// `merge_with_saved` adds to the effective desired side are canonicalized by
/// `provider::RawSavedAttrs::lift` at the raw-to-lifted seam (carina#3740).
///
/// A `StringList` pair compared under any List shape uses that List's `ordered`
/// flag to select positional or multiset comparison and compares each element
/// through the inner type. The `Union[String, list(String)]` List member is the
/// main case (carina-rs/carina-provider-aws#535).
///
/// **Do not add a special-case equality here that treats `"x"` and
/// `["x"]` as equal for this Union type.** Doing so would mask the
/// canonicalization invariant: if a non-canonical value reaches the
/// differ, that is an upstream bug and should fail the comparison so
/// the diff surfaces it. Special-case equality at the comparator
/// hides the divergence and leaves state files recording the
/// non-canonical shape, so plan diffs would keep firing on the next
/// run — exactly the phantom-diff regression that #2481 set out to
/// eliminate.
// `pub(crate)` (not `pub(super)`): the plan detail-row renderer
// (`crate::detail_rows`) reuses this exact function so the rendered
// rows agree with `find_changed_attributes`' own verdict. Reusing it
// — never reimplementing — is the carina#3073 fix; a second equality
// notion is precisely the drift this avoids.
pub(crate) fn type_aware_equal<R: ReferenceEquality>(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> bool {
    type_aware_equal_on_path(a, b, attr_type, defs, &[], context)
}

fn type_aware_equal_on_path<'a, R: ReferenceEquality>(
    a: &Value,
    b: &Value,
    attr_type: Option<&'a AttributeType>,
    defs: &'a BTreeMap<String, AttributeType>,
    visited_refs: &[&'a str],
    context: &ComparisonContext<'_, R>,
) -> bool {
    // Secret comparison: compare the desired secret with the typed hash
    // decoded from state. Desired carries `DeferredValue::Secret(inner)`.
    if let Value::Deferred(DeferredValue::Secret(inner)) = a {
        return secret_matches_state(inner, b);
    }
    if let Value::Deferred(DeferredValue::Secret(inner)) = b {
        return secret_matches_state(inner, a);
    }

    // Retain top-level Ref names across non-consuming Union recursion. Each
    // recursive branch receives its own copy, so sibling probes cannot pollute
    // one another.
    let mut current_path = visited_refs.to_vec();
    let resolved_attr_type = attr_type.map(|attr_type| {
        attr_type
            .resolve_refs_with_defs_on_path(defs, &mut current_path)
            .as_attr()
    });
    // Dispatch through `Shape` so the wildcard `_ => …` arm cannot be reached
    // by a Ref-typed attr_type. The resolver above proves this projection is
    // Ref-free without starting another path.
    let shape = resolved_attr_type.map(|attr_type| {
        attr_type
            .shape_ref_free()
            .expect("resolve_refs_with_defs_on_path must peel every top-level Ref")
    });

    match shape {
        None => {
            // Even without type info, use type_aware_maps_equal / type_aware_lists_equal
            // for Maps/Lists so that nested Secret values are compared via their hashes.
            // semantically_equal uses PartialEq which doesn't handle Secret↔hash comparison.
            match (a, b) {
                (
                    Value::Concrete(ConcreteValue::Map(ma)),
                    Value::Concrete(ConcreteValue::Map(mb)),
                ) => type_aware_maps_equal(ma, mb, |_key| None, defs, context),
                (
                    Value::Concrete(ConcreteValue::List(la)),
                    Value::Concrete(ConcreteValue::List(lb)),
                ) => type_aware_lists_equal(la, lb, None, defs, false, context),
                _ => a.semantically_equal(b),
            }
        }
        Some(at) => match (a, b, at) {
            // Int/Float coercion for numeric types
            (
                Value::Concrete(ConcreteValue::Int(i)),
                Value::Concrete(ConcreteValue::Float(f)),
                crate::schema::Shape::Float { .. } | crate::schema::Shape::Int { .. },
            ) => (*i as f64) == *f && (*i as f64) as i64 == *i,
            (
                Value::Concrete(ConcreteValue::Float(f)),
                Value::Concrete(ConcreteValue::Int(i)),
                crate::schema::Shape::Float { .. } | crate::schema::Shape::Int { .. },
            ) => *f == (*i as f64) && (*i as f64) as i64 == *i,

            // Preserve provider/API-to-DSL transform equality first, then
            // consult the identity's reference form under this initialized
            // provider instance's scope.
            (
                Value::Concrete(ConcreteValue::String(sa)),
                Value::Concrete(ConcreteValue::String(sb)),
                crate::schema::Shape::String {
                    identity, to_dsl, ..
                },
            ) => {
                let a = sa.expose_plaintext();
                let b = sb.expose_plaintext();
                context.canonical_string(a, identity, to_dsl)
                    == context.canonical_string(b, identity, to_dsl)
            }

            (
                Value::Concrete(ConcreteValue::String(a)),
                Value::Concrete(ConcreteValue::String(b)),
                crate::schema::Shape::Reference {
                    identity,
                    pattern: _,
                    length: _,
                },
            ) => context.references.equal(context.path, identity, a, b),

            // Lists: ordered or multiset comparison with inner type awareness
            (
                Value::Concrete(ConcreteValue::List(la)),
                Value::Concrete(ConcreteValue::List(lb)),
                crate::schema::Shape::List {
                    element_type: inner,
                    ordered,
                    ..
                },
            ) => type_aware_lists_equal(la, lb, Some(inner), defs, ordered, context),

            (
                Value::Concrete(ConcreteValue::StringList(la)),
                Value::Concrete(ConcreteValue::StringList(lb)),
                crate::schema::Shape::List {
                    element_type: inner,
                    ordered,
                    ..
                },
            ) => {
                // A StringList pair can reach any List shape; the
                // `Union[String, list(String)]` List member is the main case.
                // Honor this List's ordering and inner element type.
                let as_values = |items: &[_]| {
                    items
                        .iter()
                        .cloned()
                        .map(ConcreteValue::String)
                        .map(Value::Concrete)
                        .collect::<Vec<_>>()
                };
                let la = as_values(la);
                let lb = as_values(lb);
                type_aware_lists_equal(&la, &lb, Some(inner), defs, ordered, context)
            }

            // Maps: recursive comparison with inner value type
            (
                Value::Concrete(ConcreteValue::Map(ma)),
                Value::Concrete(ConcreteValue::Map(mb)),
                crate::schema::Shape::Map { value: inner, .. },
            ) => type_aware_maps_equal(ma, mb, |_key| Some(inner), defs, context),

            // Struct: per-field type-aware comparison with default-value tolerance
            (
                Value::Concrete(ConcreteValue::Map(ma)),
                Value::Concrete(ConcreteValue::Map(mb)),
                crate::schema::Shape::Struct { .. },
            ) => {
                let attr_type =
                    resolved_attr_type.expect("Some(shape) implies Some(resolved attr_type)");
                let fields = crate::schema::struct_fields_with_defs(attr_type, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                type_aware_struct_equal(ma, mb, fields, defs, context)
            }

            // Union: try each member type; if any says equal, they're equal
            (_, _, crate::schema::Shape::Union) => {
                let attr_type =
                    resolved_attr_type.expect("Some(shape) implies Some(resolved attr_type)");
                let types = crate::schema::union_members_with_defs(attr_type, defs)
                    .expect("Shape::Union must expose union members internally");
                // Also check Int/Float coercion for unions containing numeric types
                match (a, b) {
                    (
                        Value::Concrete(ConcreteValue::Int(i)),
                        Value::Concrete(ConcreteValue::Float(f)),
                    )
                    | (
                        Value::Concrete(ConcreteValue::Float(f)),
                        Value::Concrete(ConcreteValue::Int(i)),
                    ) if types.any_on_path(&mut current_path, |member, _| {
                        matches!(
                            &member.as_attr().kind,
                            crate::schema::AttrTypeKind::Float { .. }
                                | crate::schema::AttrTypeKind::Int { .. }
                        )
                    }) =>
                    {
                        (*i as f64) == *f && (*i as f64) as i64 == *i
                    }
                    _ => types.any_on_path(&mut current_path, |member, member_path| {
                        type_aware_equal_on_path(
                            a,
                            b,
                            Some(member.as_attr()),
                            defs,
                            member_path,
                            context,
                        )
                    }),
                }
            }

            // Enum: resolve namespaced text only for typed EnumIdentifier
            // values, then compare. Phase 5 of carina#2986: accept the cross-shape
            // `String × EnumIdentifier` pair so the differ is stable
            // across "state file stores plain string, desired-side parsed
            // as identifier short form" — the steady-state shape for any
            // enum-typed attribute whose value flowed through a provider
            // `read`. Comparison is text-based and case-insensitive on
            // the enum value, matching the existing String × String arm.
            (
                a,
                b,
                crate::schema::Shape::Enum {
                    values,
                    dsl_aliases,
                    to_dsl,
                    ..
                },
            ) if matches!(
                a,
                Value::Concrete(
                    ConcreteValue::String(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_)
                )
            ) && matches!(
                b,
                Value::Concrete(
                    ConcreteValue::String(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_)
                )
            ) =>
            {
                if let (
                    Value::Concrete(ConcreteValue::CanonicalEnum(left)),
                    Value::Concrete(ConcreteValue::CanonicalEnum(right)),
                ) = (a, b)
                    && left == right
                {
                    return true;
                }
                // `CanonicalEnumValue` equality remains identity-strict:
                // `aws.Region.ap-northeast-1` and
                // `awscc.Region.ap-northeast-1` are different typed values.
                // Diffing answers a narrower question: would applying change
                // provider API text? If strict equality fails, fall through to
                // the canonical text comparison below so cross-provider export
                // flows do not produce phantom updates.
                // Provider-read/state paths may still surface API values as
                // String. Treat that text literally; only typed enum variants
                // may strip a namespace or apply a DSL alias.
                let valid_values: Vec<&str> =
                    values.into_iter().flatten().map(String::as_str).collect();
                let dsl_map = crate::schema::DslMap::new(dsl_aliases, to_dsl);
                let canonical = |value: &Value| -> Option<String> {
                    match value {
                        Value::Concrete(ConcreteValue::String(s)) => {
                            Some(s.expose_plaintext().to_owned())
                        }
                        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                            Some(raw.canonical_api_spelling(&valid_values, &dsl_map))
                        }
                        Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
                            Some(canonical.api_value().to_string())
                        }
                        _ => None,
                    }
                };
                let (Some(left), Some(right)) = (canonical(a), canonical(b)) else {
                    return a.semantically_equal(b);
                };
                crate::resource::enum_values_equal(&left, &right)
            }

            // `Shape` has no `Ref` variant by construction (see
            // `crate::schema::Shape`'s type-level docs), so the
            // carina#3340 / carina#3349 invariant — "every walk-site
            // peels Ref before matching" — is enforced by the type
            // system rather than by a hand-written `Ref(_) =>
            // unreachable!()` arm.

            // All other cases: fall back to semantic equality
            _ => a.semantically_equal(b),
        },
    }
}

/// List comparison with type-aware element comparison.
/// When `ordered` is true, elements are compared positionally (sequential).
/// When `ordered` is false, elements are compared as multisets (order-insensitive).
fn type_aware_lists_equal<R: ReferenceEquality>(
    a: &[Value],
    b: &[Value],
    inner: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    ordered: bool,
    context: &ComparisonContext<'_, R>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if ordered {
        // Sequential comparison: element order matters
        a.iter().enumerate().zip(b.iter()).all(|((index, va), vb)| {
            type_aware_equal(
                va,
                vb,
                inner,
                defs,
                &context.at_path(&context.index_path(index)),
            )
        })
    } else {
        // Multiset comparison: order-insensitive
        let (matched_a, mut matched) = crate::resource::match_equal_concrete_items(a, b);
        for (i, item_a) in a.iter().enumerate() {
            if matched_a[i].is_some() {
                continue;
            }
            let mut found = false;
            for (j, item_b) in b.iter().enumerate() {
                if !matched[j]
                    && type_aware_equal(
                        item_a,
                        item_b,
                        inner,
                        defs,
                        &context.at_path(&context.index_path(i)),
                    )
                {
                    matched[j] = true;
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }
        true
    }
}

/// Map comparison with per-key type lookup.
fn type_aware_maps_equal<'a, F, R: ReferenceEquality>(
    a: &IndexMap<String, Value>,
    b: &IndexMap<String, Value>,
    get_type: F,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> bool
where
    F: Fn(&str) -> Option<&'a AttributeType>,
{
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, va)| {
        b.get(k)
            .map(|vb| {
                type_aware_equal(
                    va,
                    vb,
                    get_type(k),
                    defs,
                    &context.at_path(&context.map_key_path(k)),
                )
            })
            .unwrap_or(false)
    })
}

/// Struct comparison that tolerates extra fields with default values.
///
/// When comparing structs, one map may have extra keys that the other doesn't.
/// If the extra key's value is the "zero/default" for its type (e.g., `false`
/// for Bool, `0` for Int), the extra field is ignored. This prevents false diffs
/// when AWS returns default values for fields the user didn't specify.
fn type_aware_struct_equal<R: ReferenceEquality>(
    a: &IndexMap<String, Value>,
    b: &IndexMap<String, Value>,
    fields: &[crate::schema::CompiledStructField],
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> bool {
    let field_types: HashMap<&str, &AttributeType> = fields
        .iter()
        .map(|f| (f.name.as_str(), &f.field_type))
        .collect();

    // Check all keys present in both maps are equal
    for (k, va) in a {
        match b.get(k) {
            Some(vb) => {
                if !type_aware_equal(
                    va,
                    vb,
                    field_types.get(k.as_str()).copied(),
                    defs,
                    &context.at_path(&context.field_path(k)),
                ) {
                    return false;
                }
            }
            None => {
                // Key only in `a` — must be a type default to be tolerated
                let ft = field_types.get(k.as_str()).copied();
                if !is_type_default(va, ft, defs) {
                    return false;
                }
            }
        }
    }

    // Check keys only in `b`
    for (k, vb) in b {
        if a.contains_key(k) {
            continue; // Already checked above
        }
        let ft = field_types.get(k.as_str()).copied();
        if !is_type_default(vb, ft, defs) {
            return false;
        }
    }

    true
}

/// Check if a value is the "zero/default" for its type.
///
/// - Bool: `false`
/// - Int: `0`
/// - Float: `0.0`
/// - String / Enum: `""`
/// - List: empty list
/// - Map: empty map
fn is_type_default(
    value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> bool {
    // Dispatch through `Shape` so the wildcard arm cannot be reached
    // by a `Ref`-typed `attr_type` (carina#3340 / carina#3349). The
    // `Shape` enum has no `Ref` variant by construction, so the type
    // system rather than convention enforces that every default-value
    // classification has seen its target shape.
    let shape = attr_type.map(|attr_type| attr_type.shape_with_defs(defs));
    match (value, shape) {
        (Value::Concrete(ConcreteValue::Bool(false)), Some(crate::schema::Shape::Bool) | None) => {
            true
        }
        (Value::Concrete(ConcreteValue::Int(0)), Some(crate::schema::Shape::Int { .. })) => true,
        (Value::Concrete(ConcreteValue::Float(f)), Some(crate::schema::Shape::Float { .. }))
            if *f == 0.0 =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::Duration(d)), Some(crate::schema::Shape::Duration))
            if d.is_zero() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::String(s)), Some(crate::schema::Shape::String { .. }))
            if s.expose_plaintext().is_empty() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::String(s)), Some(crate::schema::Shape::Enum { .. }))
            if s.expose_plaintext().is_empty() =>
        {
            true
        }
        (
            Value::Concrete(ConcreteValue::EnumIdentifier(s)),
            Some(crate::schema::Shape::Enum { .. }),
        ) if s.as_str().is_empty() => true,
        (Value::Concrete(ConcreteValue::List(l)), Some(crate::schema::Shape::List { .. }))
            if l.is_empty() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::Map(m)), Some(crate::schema::Shape::Map { .. }))
            if m.is_empty() =>
        {
            true
        }
        _ => false,
    }
}

/// Check if a secret's inner value matches a state value.
///
/// Two cases are handled:
/// 1. **Typed persisted hash** (from state file, `--refresh=false`): the state
///    decoder has already lifted the structural `$secret` tag to
///    `RedactedSecret`. We hash the inner value and compare hashes.
/// 2. **Plain-text string** (from provider read, `--refresh=true`): the state
///    value is the raw secret. We compare the inner value directly. This is the
///    fix for issue #1249 where secrets nested in Maps (e.g., tags) caused false
///    diffs because the provider returns plain-text values on read.
///
fn secret_matches_state(inner: &Value, state_value: &Value) -> bool {
    if let Value::Deferred(DeferredValue::RedactedSecret { hash }) = state_value {
        let Ok(material) = canonical_secret_hash_material(inner) else {
            return false;
        };
        return hash.verify(material);
    }

    // Case 2: State has plain-text value (from provider read / --refresh=true).
    // Compare the inner secret value directly against the raw state string.
    match (inner, state_value) {
        (
            Value::Concrete(ConcreteValue::String(inner_str)),
            Value::Concrete(ConcreteValue::String(state_str)),
        ) => inner_str == state_str,
        _ => false,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TypedAttr<'a> {
    pub(crate) attr_type: &'a AttributeType,
    pub(crate) defs: &'a BTreeMap<String, AttributeType>,
}

#[derive(Clone, Copy)]
pub(crate) struct SavedAttr<'a> {
    pub(crate) views: SavedValueViews<'a>,
    pub(crate) authoring: &'a ExplicitFields,
}

pub(crate) struct AttrComparison<'a> {
    pub(crate) from: Option<&'a Value>,
    pub(crate) to: &'a Value,
    pub(crate) saved: Option<SavedAttr<'a>>,
    pub(crate) type_info: Option<TypedAttr<'a>>,
}

/// Authority for the unobservable leaf half of a key comparison.
///
/// Planning compares desired leaves with the saved accepted-request record.
/// Apply-time augmentation and Update display already have a plan verdict, so
/// they must state that verdict explicitly instead of treating an absent
/// record as a request for reconciliation.
#[derive(Clone, Copy)]
pub(crate) enum LeafProvenance<'a> {
    Recorded(Option<&'a [(LeafPath, PersistedValue)]>),
    DecidedByPlan { key_changed: bool },
}

fn contains_pending_reread(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(_))) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_pending_reread),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(contains_pending_reread),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                InterpolationPart::Expr(value) if contains_pending_reread(value)
            )
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args
            .iter()
            .any(|argument| contains_pending_reread(argument.walker_value())),
        Value::Deferred(DeferredValue::Access(access)) => contains_pending_reread(access.base()),
        Value::Deferred(DeferredValue::Secret(inner)) => contains_pending_reread(inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_pending_reread(value) || contains_pending_reread(default)
        }
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(contains_pending_reread)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_pending_reread(expression.lhs()) || contains_pending_reread(expression.rhs())
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => false,
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

/// Return whether a single attribute's values differ semantically.
pub(crate) fn should_patch_attr<R: ReferenceEquality>(
    cmp: AttrComparison<'_>,
    context: &ComparisonContext<'_, R>,
) -> bool {
    // A post-apply read failure can leave a generated desired value unknown.
    // There is no concrete target value to send, so it cannot form a patch
    // (and, in particular, cannot force replacement for a create-only name).
    // Keep an unknown on the current side comparable in the normal way: once
    // the desired value is concrete, the existing partial-read contract is to
    // surface an Update that retries the missing attribute.
    if contains_pending_reread(cmp.to) {
        return false;
    }
    let Some(from_value) = cmp.from else {
        return true;
    };
    let (attr_type, defs) = cmp
        .type_info
        .map(|info| (Some(info.attr_type), info.defs))
        .unwrap_or((None, empty_defs_for_schema_walks()));

    match cmp.saved {
        Some(saved) => {
            let effective_to = merge_with_saved(
                cmp.to,
                saved.views,
                saved.authoring,
                attr_type,
                defs,
                context,
            );
            !type_aware_equal(from_value, &effective_to, attr_type, defs, context)
        }
        None => !type_aware_equal(from_value, cmp.to, attr_type, defs, context),
    }
}

/// Return whether a key is eligible for a provider patch and semantically changed.
pub(crate) fn key_should_enter_patch<R: ReferenceEquality>(
    key: &str,
    schema: Option<&ResourceSchema>,
    leaf_provenance: LeafProvenance<'_>,
    cmp: AttrComparison<'_>,
    context: &ComparisonContext<'_, R>,
) -> bool {
    key_should_enter_patch_with_ignores(
        key,
        schema,
        leaf_provenance,
        cmp,
        &ResolvedIgnoreChanges::default(),
        context,
    )
}

pub(crate) fn key_should_enter_patch_with_ignores<R: ReferenceEquality>(
    key: &str,
    schema: Option<&ResourceSchema>,
    leaf_provenance: LeafProvenance<'_>,
    cmp: AttrComparison<'_>,
    ignore_changes: &ResolvedIgnoreChanges,
    context: &ComparisonContext<'_, R>,
) -> bool {
    if ignore_changes.ignores_whole_attribute(key) {
        return false;
    }
    // CloudControl reads omit write-only values. Ignore that absence only when
    // the key cannot be updated: otherwise the value must enter an Update and
    // exact accepted-request provenance can make subsequent plans converge.
    if schema
        .and_then(|s| s.attributes.get(key))
        .is_some_and(|attr| attr.write_only && attr.create_only && cmp.from.is_none())
    {
        return false;
    }
    let cmp = if cmp.from.is_none()
        && schema
            .and_then(|schema| schema.attributes.get(key))
            .is_some_and(|attribute| attribute.write_only)
    {
        AttrComparison {
            from: cmp.saved.map(|saved| saved.views.projected()),
            ..cmp
        }
    } else {
        cmp
    };
    if let Some(schema) = schema
        && schema.write_only_bearing(key)
    {
        return nested_write_only_should_enter_patch(
            key,
            schema,
            leaf_provenance,
            cmp,
            ignore_changes,
            &context.at_path(key),
        );
    }
    should_patch_attr(cmp, &context.at_path(key))
}

fn nested_write_only_should_enter_patch<R: ReferenceEquality>(
    key: &str,
    schema: &ResourceSchema,
    leaf_provenance: LeafProvenance<'_>,
    cmp: AttrComparison<'_>,
    ignore_changes: &ResolvedIgnoreChanges,
    context: &ComparisonContext<'_, R>,
) -> bool {
    if contains_pending_reread(cmp.to) {
        return false;
    }
    let attr_type = &schema.attributes[key].attr_type;

    let effective_to = cmp.saved.map_or_else(
        || Cow::Borrowed(cmp.to),
        |saved| {
            Cow::Owned(merge_with_saved(
                cmp.to,
                saved.views,
                saved.authoring,
                Some(attr_type),
                &schema.defs,
                context,
            ))
        },
    );
    let observable_equal = write_only_observable_equal(
        schema,
        key,
        cmp.from,
        effective_to.as_ref(),
        attr_type,
        context,
    );

    let desired_leaves = schema
        .write_only_leaf_projection(key, cmp.to)
        .into_iter()
        .filter(|(path, _, _)| !ignore_changes.ignores_leaf(key, path))
        .collect::<Vec<_>>();
    let leaf_equal = match leaf_provenance {
        LeafProvenance::Recorded(saved_leaves) => {
            if schema
                .attributes
                .get(key)
                .is_some_and(|attribute| attribute.create_only)
                && saved_leaves.is_none()
            {
                return !observable_equal;
            }
            saved_leaves.map_or(desired_leaves.is_empty(), |saved_leaves| {
                let saved_leaves = saved_leaves
                    .iter()
                    .filter(|(path, _)| !ignore_changes.ignores_leaf(key, path))
                    .collect::<Vec<_>>();
                saved_leaves.len() == desired_leaves.len()
                    && desired_leaves.iter().zip(saved_leaves).all(
                        |((path, desired, field_type), (saved_path, saved))| {
                            saved_path == path
                                && saved.decode().is_some_and(|saved| {
                                    type_aware_equal(
                                        desired,
                                        &saved,
                                        Some(field_type),
                                        &schema.defs,
                                        &context.at_path(&context.leaf_path(path)),
                                    )
                                })
                        },
                    )
            })
        }
        LeafProvenance::DecidedByPlan { key_changed } => !key_changed,
    };

    !(observable_equal && leaf_equal)
}

fn has_observable_content(value: &Value) -> bool {
    match value {
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(has_observable_content)
        }
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(has_observable_content),
        Value::Concrete(ConcreteValue::StringList(items)) => !items.is_empty(),
        Value::Deferred(DeferredValue::Secret(inner)) => has_observable_content(inner),
        Value::Deferred(DeferredValue::ModuleIf(_)) => true,
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Coalesce { .. })
        | Value::Deferred(DeferredValue::IntegerArithmetic(_))
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        | Value::Deferred(DeferredValue::Access(_))
        | Value::Deferred(DeferredValue::Unknown(_))
        | Value::Deferred(DeferredValue::Unavailable { .. }) => true,
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

/// Compare observable projections for `key_should_enter_patch`'s nested
/// write-only branch.
///
/// A provider may omit the whole attribute when stripping its write-only
/// leaves produces no observable content.
fn write_only_observable_equal<R: ReferenceEquality>(
    schema: &ResourceSchema,
    key: &str,
    from: Option<&Value>,
    to: &Value,
    attr_type: &AttributeType,
    context: &ComparisonContext<'_, R>,
) -> bool {
    let observable_to = schema.strip_write_only_leaves(key, to);
    let Some(from) = from else {
        return !has_observable_content(&observable_to);
    };
    let observable_from = schema.strip_write_only_leaves(key, from);
    type_aware_equal(
        &observable_from,
        &observable_to,
        Some(attr_type),
        &schema.defs,
        context,
    )
}

/// Build a comparison-only view that grafts `Secret` wrappers from source.
///
/// Apply-time resolution unwraps secrets before provider calls. Re-wrapping the
/// comparison view lets the same hash semantics as plan-time diffing run without
/// changing the normalized value sent to the provider patch.
///
/// Matching `(Map, Map)` and equal-length `(List, List)` shapes are walked
/// recursively. Resolved-only map keys are retained as resolved values; source-only
/// keys are ignored unless they contain a secret that cannot be grafted. If a
/// source secret sits behind a divergent shape, there is no unambiguous resolved
/// position to graft it into, so callers get `None` and must fail closed by not
/// patching that key. Callers on normalization paths that change container shape
/// must not assume secret hash comparison remains available.
pub(crate) fn secret_grafted_comparison_view<'a>(
    resolved: &'a Value,
    source: Option<&'a Value>,
) -> Option<Cow<'a, Value>> {
    let Some(source) = source else {
        return Some(Cow::Borrowed(resolved));
    };
    if !contains_secret(source) {
        return Some(Cow::Borrowed(resolved));
    }
    let view = match (resolved, source) {
        (_, Value::Deferred(DeferredValue::Secret(_))) => Cow::Borrowed(source),
        (
            Value::Concrete(ConcreteValue::Map(resolved)),
            Value::Concrete(ConcreteValue::Map(source)),
        ) => {
            let mut out = resolved.clone();
            for (key, resolved_value) in resolved {
                if let Some(source_value) = source.get(key) {
                    out.insert(
                        key.clone(),
                        secret_grafted_comparison_view(resolved_value, Some(source_value))?
                            .into_owned(),
                    );
                }
            }
            if source.iter().any(|(key, source_value)| {
                !resolved.contains_key(key) && contains_secret(source_value)
            }) {
                return None;
            }
            Cow::Owned(Value::Concrete(ConcreteValue::Map(out)))
        }
        (
            Value::Concrete(ConcreteValue::List(resolved)),
            Value::Concrete(ConcreteValue::List(source)),
        ) if resolved.len() == source.len() => Cow::Owned(Value::Concrete(ConcreteValue::List(
            resolved
                .iter()
                .zip(source)
                .map(|(resolved_value, source_value)| {
                    secret_grafted_comparison_view(resolved_value, Some(source_value))
                        .map(Cow::into_owned)
                })
                .collect::<Option<Vec<_>>>()?,
        ))),
        _ => return None,
    };
    Some(view)
}

/// Find changed attributes between desired and current state.
/// If `saved` is provided, each desired value is merged with the saved value
/// before comparison, filling in unmanaged nested fields.
/// If `prev_explicit` is provided, the actual-state side is **projected**
/// through its authoring context before comparison so server-side defaults
/// the user never wrote do not surface as diffs (refs awscc#206). `saved`
/// values anchor per-element list authoring when available; otherwise lists
/// use the legacy union projection. Top-level keys absent from
/// `prev_explicit.children` are treated as unauthored and excluded. Stored
/// list values also anchor the per-element records used to remove uniquely
/// paired server-added list elements (refs carina-provider-awscc#511).
/// `prev_explicit` also drives the explicit-removal detection at the
/// end of this function: the same set of top-level keys is used to
/// flag attributes the user *previously* wrote but no longer mentions.
/// If `schema` is provided, type-aware comparison is used for each attribute.
pub(super) struct AttributeChanges {
    pub changed: Vec<String>,
    pub ignored_readable_differences: usize,
}

pub(super) struct AttributeValueViews<'a> {
    masked: &'a MaskedComparisonAttributes,
    desired: &'a CanonicalComparisonAttributes,
    current: &'a CanonicalComparisonAttributes,
    nested_write_only: &'a NestedWriteOnlyRecord,
}

pub(super) struct AuthoredAttributeViews<'a> {
    masked: &'a MaskedComparisonAttributes,
    current: CanonicalComparisonAttributes,
    saved: Option<CanonicalComparisonAttributes>,
    removal_evidence: AuthoredRemovalEvidence,
    previously_authored_top_level_attributes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthoredRemovalEvidence {
    Recorded,
    Unrecorded,
}

impl AuthoredAttributeViews<'_> {
    pub(super) fn current(&self) -> &CanonicalComparisonAttributes {
        &self.current
    }

    pub(super) fn removal_evidence(&self) -> AuthoredRemovalEvidence {
        self.removal_evidence
    }

    pub(super) fn desired(&self) -> &CanonicalComparisonAttributes {
        self.masked.desired()
    }
}

/// Apply the persisted authoring tree to current and saved values.
///
/// This is the shared removal-evidence seam for ordinary diffing and nested
/// create-only replacement detection. A provider-populated value that the user
/// never authored is absent from both projected views, while a previously
/// authored value remains visible when desired omits it.
pub(super) fn project_authored_attribute_views<'a, R: ReferenceEquality>(
    masked: &'a MaskedComparisonAttributes,
    prev_explicit: Option<&ExplicitFields>,
    schema: Option<&ResourceSchema>,
    context: &ComparisonContext<'_, R>,
) -> AuthoredAttributeViews<'a> {
    let current = masked.current();
    let saved = masked.saved();
    let (removal_evidence, previously_authored_top_level_attributes) = match prev_explicit {
        Some(ExplicitFields::Struct { children }) => (
            AuthoredRemovalEvidence::Recorded,
            children.keys().cloned().collect(),
        ),
        Some(
            ExplicitFields::Leaf
            | ExplicitFields::List { .. }
            | ExplicitFields::ListElements { .. }
            | ExplicitFields::WholeValue { .. }
            | ExplicitFields::Unrecorded,
        )
        | None => (AuthoredRemovalEvidence::Unrecorded, Vec::new()),
    };
    let projection = prev_explicit
        .map(|explicit| ExplicitProjection::for_desired(masked, explicit, schema, context));
    let current = projection.as_ref().map_or_else(
        || current.clone(),
        |projection| projection.current().into_attributes(),
    );
    let saved = projection.as_ref().map_or_else(
        || saved.cloned(),
        |projection| projection.saved().map(|saved| saved.into_attributes()),
    );
    AuthoredAttributeViews {
        masked,
        current,
        saved,
        removal_evidence,
        previously_authored_top_level_attributes,
    }
}

impl<'a> AttributeValueViews<'a> {
    pub(super) fn new(
        masked: &'a MaskedComparisonAttributes,
        desired: &'a CanonicalComparisonAttributes,
        current: &'a CanonicalComparisonAttributes,
        nested_write_only: &'a NestedWriteOnlyRecord,
    ) -> Self {
        Self {
            masked,
            desired,
            current,
            nested_write_only,
        }
    }
}

pub(super) fn find_changed_attributes_with_ignores(
    values: AttributeValueViews<'_>,
    prev_explicit: Option<&ExplicitFields>,
    schema: Option<&ResourceSchema>,
    ignore_changes: &ResolvedIgnoreChanges,
    context: &PlanningComparison<'_>,
    observations: &ObservationComparison<'_>,
) -> AttributeChanges {
    let AttributeValueViews {
        masked,
        desired,
        current,
        nested_write_only,
    } = values;
    let mut changed = Vec::new();

    // Pull the cyclic-struct definition map (`Ref` targets) from the
    // resource schema if available, otherwise an empty map. Walk-sites
    // resolve `Ref` against this map (carina#3340).
    let defs: &BTreeMap<String, AttributeType> = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());

    // Project `current` through `prev_explicit` so server-side defaults
    // the user never authored disappear before any comparison runs.
    // The projection is idempotent and shape-preserving for keys the
    // user did author. Saved values anchor per-element list records when they
    // are available. Without an anchor, projection retains every list item
    // and applies the legacy union restriction to each one.
    //
    // `saved` undergoes the same projection so the saved-merge fallback
    // doesn't smuggle server-only fields back into `effective_desired`
    // (refs awscc#206).
    let raw_saved = masked.saved();
    let projected = project_authored_attribute_views(masked, prev_explicit, schema, context);
    let projected_current = &projected.current;
    let projected_saved = projected.saved.as_ref();

    let mut differing_ignored_paths = std::collections::HashSet::new();
    for path in ignore_changes.iter().filter(|path| !path.is_write_only()) {
        let Some(desired_value) = desired.get(path.attribute()) else {
            continue;
        };
        let desired_leaf = value_at_leaf(desired_value, path.leaf());
        let current_leaf = current
            .get(path.attribute())
            .and_then(|value| value_at_leaf(value, path.leaf()));
        let equal = match (desired_leaf, current_leaf) {
            (Some(desired), Some(current)) => {
                let attr_type = path
                    .leaf()
                    .segments()
                    .is_empty()
                    .then(|| {
                        schema?
                            .attributes
                            .get(path.attribute())
                            .map(|attr| &attr.attr_type)
                    })
                    .flatten();
                // Ignored-change counts describe raw observations; they do
                // not decide a provider effect and must not trigger resolution.
                type_aware_equal(current, desired, attr_type, defs, observations)
            }
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        };
        if !equal {
            differing_ignored_paths.insert(path.authored());
        }
    }

    for (key, desired_value) in masked.desired() {
        if ignore_changes.ignores_whole_attribute(key) {
            continue;
        }
        let type_info = schema.and_then(|s| {
            s.attributes.get(key).map(|attr| TypedAttr {
                attr_type: &attr.attr_type,
                defs,
            })
        });
        // A previously unmanaged attribute is normally absent from the
        // projected current view. If its persisted value is an opaque secret
        // digest and the user now authors a secret at that key, the digest is
        // nevertheless authoritative comparison material: it cannot smuggle
        // a provider default into desired state, and verification proves
        // whether the newly-authored plaintext is the value already stored.
        let current_value = projected_current.get(key).or_else(|| {
            if !desired.get(key).is_some_and(contains_secret) {
                return None;
            }
            raw_saved?
                .get(key)
                .filter(|value| crate::value::contains_redacted_secret(value))
        });

        let comparison = AttrComparison {
            from: current_value,
            to: desired_value,
            saved: projected_saved.and_then(|projected_saved| {
                projected_saved.get(key).map(|projected_value| SavedAttr {
                    views: raw_saved
                        .and_then(|raw_saved| raw_saved.get(key))
                        .map(|raw_value| SavedValueViews::new(projected_value, raw_value))
                        .unwrap_or_else(|| SavedValueViews::same(projected_value)),
                    authoring: match prev_explicit {
                        Some(ExplicitFields::Struct { children }) => {
                            children.get(key).unwrap_or(&ExplicitFields::Unrecorded)
                        }
                        Some(authoring @ ExplicitFields::Unrecorded) => authoring,
                        Some(
                            ExplicitFields::Leaf
                            | ExplicitFields::List { .. }
                            | ExplicitFields::ListElements { .. }
                            | ExplicitFields::WholeValue { .. },
                        )
                        | None => &ExplicitFields::Unrecorded,
                    },
                })
            }),
            type_info,
        };
        let should_patch = key_should_enter_patch_with_ignores(
            key,
            schema,
            LeafProvenance::Recorded(nested_write_only.get(key)),
            comparison,
            ignore_changes,
            context,
        );
        if should_patch {
            changed.push(key.clone());
        }
    }

    // Detect attributes removed from desired but still present in current.
    // Only flag attributes that were previously in the user's desired
    // state (top-level children of `prev_explicit`'s root `Struct`).
    // This prevents false removals for computed/provider-returned
    // attributes the user never specified. With `Unrecorded` (no
    // authoring record) there is nothing to compare against, so
    // attribute removal is not detected for those rows. A later desired-backed
    // state write can seed the record from desired keys (never provider keys),
    // after which removals of those recorded keys are detectable.
    for key in &projected.previously_authored_top_level_attributes {
        if masked.desired().contains_key(key) {
            continue;
        }
        if projected_current.contains_key(key) {
            changed.push(key.clone());
        }
    }

    AttributeChanges {
        changed,
        ignored_readable_differences: differing_ignored_paths.len(),
    }
}

/// Apply cannot widen an Update into a replacement after planning.
#[derive(Debug, thiserror::Error)]
#[error("Create-only attributes changed after planning ({}) for {resource}; create a new plan before applying", .attributes.join(", "))]
pub(crate) struct ApplyTimeCreateOnlyChange {
    pub(crate) resource: Box<crate::resource::ResourceId>,
    pub(crate) attributes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApplyTimeComparisonError {
    #[error(transparent)]
    Reference(#[from] Box<crate::references::ReferencePlanError>),
    #[error("Create-only augmentation failed")] // error-chain: caller-renders-sources
    CreateOnly(#[from] ApplyTimeCreateOnlyChange),
}

/// Reconsider only apply-resolved keys excluded by the original plan. The
/// caller prepares fresh reference evidence against these same masked values.
pub(crate) fn find_apply_time_changed_attributes<S>(
    masked: &MaskedComparisonAttributes,
    authored: &crate::resource::ResolvedResource<S>,
    schema: &ResourceSchema,
    ignores: &ResolvedIgnoreChanges,
    context: &PlanningComparison<'_>,
) -> Result<Vec<String>, ApplyTimeCreateOnlyChange> {
    let changed = masked
        .desired()
        .iter()
        .filter_map(|(key, desired)| {
            let type_info = schema.attributes.get(key).map(|attribute| TypedAttr {
                attr_type: &attribute.attr_type,
                defs: &schema.defs,
            });
            key_should_enter_patch_with_ignores(
                key,
                Some(schema),
                LeafProvenance::DecidedByPlan { key_changed: false },
                AttrComparison {
                    from: masked.current().get(key),
                    to: desired,
                    saved: None,
                    type_info,
                },
                ignores,
                &context.at_path(key),
            )
            .then(|| key.clone())
        })
        .collect::<Vec<_>>();
    let views = project_authored_attribute_views(masked, None, Some(schema), context);
    let create_only =
        super::plan::find_changed_create_only(&changed, authored, &views, schema, context);
    if !create_only.is_empty() {
        return Err(ApplyTimeCreateOnlyChange {
            resource: Box::new(authored.id.clone()),
            attributes: create_only,
        });
    }
    Ok(changed)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
    saved: Option<&HashMap<String, Value>>,
    nested_write_only: &NestedWriteOnlyRecord,
    prev_explicit: Option<&ExplicitFields>,
    schema: Option<&ResourceSchema>,
    context: &PlanningComparison<'_>,
    observations: &ObservationComparison<'_>,
) -> Vec<String> {
    let canonicalize = |attributes| match schema {
        Some(schema) => CanonicalComparisonAttributes::canonicalize(attributes, schema),
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(attributes),
        ),
    };
    let desired = canonicalize(desired.clone());
    let current = canonicalize(current.clone());
    let saved = saved.cloned().map(canonicalize);
    let masked = ResolvedIgnoreChanges::default().mask_comparison_attributes(
        &desired,
        &current,
        saved.as_ref(),
        schema,
    );
    find_changed_attributes_with_ignores(
        AttributeValueViews::new(&masked, &desired, &current, nested_write_only),
        prev_explicit,
        schema,
        &ResolvedIgnoreChanges::default(),
        context,
        observations,
    )
    .changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DslTransform, TypeIdentity};

    fn type_aware_equal(
        a: &Value,
        b: &Value,
        attr_type: Option<&AttributeType>,
        defs: &BTreeMap<String, AttributeType>,
    ) -> bool {
        super::type_aware_equal(
            a,
            b,
            attr_type,
            defs,
            &crate::differ::exact_comparison_context_for_tests(),
        )
    }

    #[test]
    fn saved_plan_display_equality_compares_reference_bytes_without_a_witness() {
        let attr_type = AttributeType::reference(
            TypeIdentity::new(Some("test"), ["keys", "Key"], "Reference"),
            None,
            None,
        );
        let value = Value::Concrete(ConcreteValue::String("alias/live".into()));
        let other = Value::Concrete(ConcreteValue::String("key/a".into()));
        crate::differ::exact_comparison_schema_for_tests().with_recorded_effects(|comparison| {
            assert!(super::type_aware_equal(
                &value,
                &value,
                Some(&attr_type),
                empty_defs_for_schema_walks(),
                &comparison
            ));
            assert!(!super::type_aware_equal(
                &value,
                &other,
                Some(&attr_type),
                empty_defs_for_schema_walks(),
                &comparison
            ));
        });
    }

    #[test]
    fn observation_reference_equality_detects_byte_changes_without_a_witness() {
        let attr_type = AttributeType::reference(
            TypeIdentity::new(Some("test"), ["keys", "Key"], "Reference"),
            None,
            None,
        );
        let alias = Value::Concrete(ConcreteValue::String("alias/live".into()));
        let canonical = Value::Concrete(ConcreteValue::String("key/a".into()));
        let comparison = crate::differ::exact_comparison_context_for_tests();
        assert!(super::type_aware_equal(
            &alias,
            &alias,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
            &comparison
        ));
        assert!(!super::type_aware_equal(
            &alias,
            &canonical,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
            &comparison
        ));
    }

    #[test]
    fn refined_string_to_dsl_transform_is_applied_before_comparison() {
        let attr_type = AttributeType::refined_string(
            None,
            None,
            None,
            Some(DslTransform::StripSuffix(".".to_string())),
        );
        let state = Value::Concrete(ConcreteValue::String("foo.example.com.".into()));
        let desired = Value::Concrete(ConcreteValue::String("foo.example.com".into()));

        assert!(type_aware_equal(
            &state,
            &desired,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
        ));
    }

    #[test]
    fn canonical_enum_cross_identity_same_api_value_is_no_change() {
        let attr_type = AttributeType::enum_(
            TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
            Some(vec!["ap-northeast-1".to_string()]),
            Vec::new(),
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let producer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));
        let consumer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));

        assert!(type_aware_equal(
            &producer,
            &consumer,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
        ));
    }

    #[test]
    fn canonical_enum_cross_identity_different_api_value_is_change() {
        let attr_type = AttributeType::enum_(
            TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
            Some(vec!["ap-northeast-1".to_string(), "us-east-1".to_string()]),
            Vec::new(),
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let producer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));
        let consumer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
                "us-east-1",
            ),
        ));

        assert!(!type_aware_equal(
            &producer,
            &consumer,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
        ));
    }

    #[test]
    fn dotted_string_on_enum_attribute_is_compared_literally() {
        let attr_type = AttributeType::enum_(
            TypeIdentity::bare("Format"),
            Some(vec!["json".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let desired = Value::Concrete(ConcreteValue::String("A.json".into()));
        let state = Value::Concrete(ConcreteValue::String("json".into()));

        assert!(!type_aware_equal(
            &desired,
            &state,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
        ));
    }

    #[test]
    fn dotted_api_enum_strings_remain_literal_values() {
        for api_value in ["TLSv1.1", "ipsec.1"] {
            let attr_type = AttributeType::enum_(
                TypeIdentity::bare("Protocol"),
                Some(vec![api_value.to_string()]),
                Vec::new(),
                None,
                None,
            );
            let string = Value::Concrete(ConcreteValue::String(api_value.into()));
            let canonical = Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    TypeIdentity::bare("Protocol"),
                    api_value,
                ),
            ));

            assert!(type_aware_equal(
                &string,
                &canonical,
                Some(&attr_type),
                empty_defs_for_schema_walks(),
            ));
        }
    }
}
