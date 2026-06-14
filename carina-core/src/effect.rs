//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when they are executed via a Provider.

use std::{collections::HashSet, ops::Deref};

use serde::{Deserialize, Serialize};

use crate::non_empty::NonEmptyVec;
use crate::resource::{DataSource, Directives, Resource, ResourceId, State};
use crate::wait::predicate::WaitPredicate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanOp {
    Create,
    Read,
    Update,
    Delete,
}

/// Temporary name used during create-before-destroy replacement.
///
/// When a resource with a unique name constraint is replaced with create-before-destroy,
/// the new resource is created with a temporary name to avoid conflicts with the old resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporaryName {
    /// The attribute that holds the name (e.g., "bucket_name")
    pub attribute: String,
    /// The original (desired) name value (e.g., "my-bucket")
    pub original_value: String,
    /// The generated temporary name (e.g., "my-bucket-a1b2c3d4")
    pub temporary_value: String,
    /// Whether the name attribute can be updated after creation (not create-only)
    pub can_rename: bool,
}

/// A dependent resource that must be updated during a create_before_destroy replacement.
///
/// When a resource is replaced with create_before_destroy, dependent resources that
/// reference the replaced resource's computed attributes need to be updated between
/// the create (new) and delete (old) steps. The `to` field retains unresolved
/// `ResourceRef` values so that the apply phase can re-resolve them using the
/// newly created resource's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadingUpdate {
    pub id: ResourceId,
    pub from: Box<State>,
    pub to: Resource,
}

/// Non-empty create-only attribute list for [`Effect::Replace`].
///
/// An empty list would render a destroy-and-recreate plan with no visible
/// reason: the unexplained-replacement bug class (carina#3471) this type makes
/// unrepresentable.
///
/// A replace must name at least one create-only attribute that forced it.
/// Constructing this type from a possibly empty `Vec<String>` requires the
/// fallible [`ChangedCreateOnly::new`] constructor:
///
/// ```compile_fail
/// use carina_core::effect::ChangedCreateOnly;
///
/// let attrs: Vec<String> = Vec::new();
/// let _changed = ChangedCreateOnly(attrs);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ChangedCreateOnly(NonEmptyVec<String>);

impl ChangedCreateOnly {
    pub fn new(attrs: Vec<String>) -> Option<Self> {
        NonEmptyVec::from_vec(attrs).map(Self)
    }

    pub fn push(&mut self, attr: String) {
        self.0.push(attr);
    }

    pub fn contains(&self, attr: &str) -> bool {
        self.0.iter().any(|a| a == attr)
    }
}

impl Deref for ChangedCreateOnly {
    type Target = [String];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl TryFrom<Vec<String>> for ChangedCreateOnly {
    type Error = String;

    fn try_from(attrs: Vec<String>) -> Result<Self, Self::Error> {
        Self::new(attrs).ok_or_else(|| {
            "Replace effect requires at least one changed create-only attribute".to_string()
        })
    }
}

impl From<ChangedCreateOnly> for Vec<String> {
    fn from(attrs: ChangedCreateOnly) -> Self {
        attrs.0.into_vec()
    }
}

/// How an [`Effect::Wait`] obtains the cloud provider identifier its
/// polling `read()` needs.
///
/// The differ cannot always know the target's identifier at plan time:
/// when the target is *created in the same apply run* it has no prior
/// state, so its real identifier (e.g. an ACM certificate ARN) only
/// exists after the `Create` effect completes. Splitting "known now"
/// from "resolve at apply" into the type — instead of overloading
/// `Option<String>`'s `None` — forces the executor to handle the
/// apply-time case explicitly via an exhaustive `match`, so future code
/// cannot silently pass a stale plan-time `None` to `provider.read`
/// (carina#3119).
///
/// Guard: the old `Option<String>`-shaped pattern — treating the
/// plan-time value as something you can `.as_deref()` straight into the
/// poll loop — no longer type-checks. `WaitTarget` has no `Option`-like
/// API, so there is no `None` to forward:
///
/// ```compile_fail
/// use carina_core::effect::WaitTarget;
/// let t = WaitTarget::ResolvedAtApply;
/// // Was: `target_identifier.as_deref()` — `WaitTarget` has no
/// // `as_deref`, forcing callers through an exhaustive match that
/// // handles the apply-time resolution explicitly.
/// let _ = t.as_deref();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitTarget {
    /// The target already exists; its identifier was resolved from
    /// `current_states` at plan time.
    Known(String),
    /// The target is created or updated in this same run. The executor
    /// resolves the real identifier from the just-applied state
    /// (`applied_states`) before polling; falls back to no identifier
    /// only when the target was never produced in this plan.
    ResolvedAtApply,
}

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Read the current state of a resource (data source)
    Read { resource: DataSource },

    /// Create a new resource
    Create(Resource),

    /// Update an existing resource
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        /// Attribute names that changed (including removed attributes)
        changed_attributes: Vec<String>,
    },

    /// Replace a resource (delete then create) due to create-only property changes
    Replace {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        #[serde(default)]
        directives: Directives,
        /// Which create-only attributes forced the replacement
        changed_create_only: ChangedCreateOnly,
        /// Dependent resources to update between create and delete (create_before_destroy only)
        #[serde(default)]
        cascading_updates: Vec<CascadingUpdate>,
        /// Temporary name for create-before-destroy when the resource has a unique name constraint
        #[serde(default)]
        temporary_name: Option<TemporaryName>,
        /// Hints mapping attribute names to their original ResourceRef expressions
        /// (e.g., `("vpc_id", "vpc.vpc_id")`). Used by display to show the binding
        /// reference instead of the resolved value for cascade-triggered replacements.
        #[serde(default)]
        cascade_ref_hints: Vec<(String, String)>,
    },

    /// Delete a resource
    Delete {
        id: ResourceId,
        identifier: String,
        #[serde(default)]
        directives: Directives,
        /// The binding name of the deleted resource (for plan tree display)
        #[serde(default)]
        binding: Option<String>,
        /// Binding names this resource depended on (for plan tree display).
        /// Includes both value-reference and explicit `directives.depends_on`
        /// edges — the union the executor needs for ordering.
        #[serde(default)]
        dependencies: HashSet<String>,
        /// Subset of `dependencies` that came from
        /// `directives { depends_on = [...] }` rather than value
        /// references. Captured at Delete construction time because the
        /// originating resource is gone by the time the executor runs
        /// (#2871). Empty for legacy state files.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
    },

    /// Import an existing resource into state (via provider read)
    Import {
        /// Target resource address
        id: ResourceId,
        /// Cloud provider identifier (e.g., `"vpc-0abc123def456"`).
        ///
        /// Carried as a [`Value`] so an interpolation like
        /// `"${upstream.attr}|literal"` whose `upstream.attr` is still
        /// deferred at plan-time remains a
        /// `Value::Deferred(DeferredValue::Interpolation)` for display
        /// rather than being silently substituted to empty (carina#3329).
        /// The executor calls `assert_fully_resolved` before passing the
        /// identifier to the provider, so by apply time this is always
        /// a `Value::Concrete(ConcreteValue::String)`.
        identifier: crate::resource::Value,
    },

    /// Remove a resource from state without destroying it
    Remove {
        /// Resource address to remove from state
        id: ResourceId,
    },

    /// Move/rename a resource in state without destroy/recreate
    Move {
        /// Old resource address
        from: ResourceId,
        /// New resource address
        to: ResourceId,
    },

    /// Wait for `target` to satisfy `until` by polling `read()`.
    ///
    /// Emitted by the differ for a `let <binding> = wait <target> { ... }`
    /// declaration. The executor (not the provider) drives the polling
    /// loop on top of the existing `Provider::read` trait method, so
    /// providers — including WASM plugins — need no contract change.
    ///
    /// Wait effects do **not** persist to `carina.state.json`; they are
    /// re-evaluated on every plan/apply. See
    /// `notes/specs/2026-05-09-wait-construct-design.md` §State file.
    Wait {
        /// The wait's binding name (e.g. `"cert_issued"`).
        binding: String,
        /// Resolved id of the target resource (`wait cert { ... }` →
        /// `cert`'s `ResourceId`).
        target_id: ResourceId,
        /// How the executor obtains the target's cloud provider
        /// identifier for its polling `read()`. See [`WaitTarget`].
        target: WaitTarget,
        /// Typed predicate evaluated against each `read()` snapshot.
        until: WaitPredicate,
        /// Surface form of the `until` expression as the user wrote it
        /// (e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`).
        /// Carried so plan-display never re-stringifies the parsed AST —
        /// same pattern as `Effect::Replace::cascade_ref_hints`.
        until_surface: String,
        /// Hard cap on total wait time. Resolved by the differ from the
        /// user-provided override or the target schema's default.
        #[serde(with = "crate::resource::duration_secs")]
        timeout: std::time::Duration,
        /// Poll cadence between `read()` calls. Resolved from the target
        /// schema's default; not user-visible in MVP.
        #[serde(with = "crate::resource::duration_secs")]
        interval: std::time::Duration,
        /// Additional bindings the wait must wait for before polling,
        /// declared via `depends_on = [...]` in the wait block. The
        /// scheduler treats these like `directives.depends_on` on a
        /// resource — extra ordering edges that aren't expressed via
        /// value references.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
    },
}

/// A type-level narrowing of [`Effect`] to the variants the basic
/// executor (`execute_basic_effect`) actually handles: `Create`,
/// `Update`, and `Delete`.
///
/// The basic executor was previously typed on `&Effect` and used a
/// `_ => unreachable!("execute_basic_effect called with non-basic
/// effect")` arm to reject `Replace`/`Read`/`Import`/`Remove`/`Move`/
/// `Wait`. Callers' filters were the *only* thing keeping non-basic
/// effects out; a single missed filter (#3164) panicked apply at
/// runtime and left the state lock acquired.
///
/// `BasicEffect` makes that contract live in the type system instead.
/// The only way to obtain one is [`Effect::as_basic`], which returns
/// `None` for every non-basic variant. `execute_basic_effect` takes
/// `BasicEffect<'a>` directly and exhaustively matches its three arms —
/// no `unreachable!()` is needed, and adding a new `Effect` variant
/// won't compile until the call sites decide whether the variant is
/// "basic" or routed elsewhere.
///
/// Variants borrow from the source `&'a Effect` so the basic executor
/// can still forward the original effect into `ExecutionEvent::*`
/// observer calls.
#[derive(Debug)]
pub enum BasicEffect<'a> {
    Create {
        effect: &'a Effect,
        resource: &'a Resource,
    },
    Update {
        effect: &'a Effect,
        id: &'a ResourceId,
        from: &'a State,
        to: &'a Resource,
        changed_attributes: &'a [String],
    },
    Delete {
        effect: &'a Effect,
        id: &'a ResourceId,
        identifier: &'a str,
        directives: &'a Directives,
    },
}

impl<'a> BasicEffect<'a> {
    /// Returns the source `&Effect` this `BasicEffect` was narrowed
    /// from. Used by `execute_basic_effect` to forward the original
    /// effect to `ExecutionEvent::*` observer calls without storing it
    /// twice.
    pub fn as_effect(&self) -> &'a Effect {
        match *self {
            BasicEffect::Create { effect, .. }
            | BasicEffect::Update { effect, .. }
            | BasicEffect::Delete { effect, .. } => effect,
        }
    }
}

impl Effect {
    /// Narrow this effect to a [`BasicEffect`] if it is one of the
    /// variants the basic executor handles (`Create`, `Update`,
    /// `Delete`). Returns `None` for `Replace`, `Read`, `Import`,
    /// `Remove`, `Move`, and `Wait` — those route through other
    /// executor paths or are state-only (applied by the CLI's
    /// `execute_state_only_effects` step).
    ///
    /// This is the *only* way to construct a `BasicEffect`, so the
    /// basic executor's "this is a Create/Update/Delete" contract
    /// lives in the type system rather than in caller-side filters.
    /// See [`BasicEffect`] for the rationale (#3164).
    ///
    /// Guard: `BasicEffect` has no `From<&Effect>` or other public
    /// constructor — callers must route through `as_basic()` and
    /// handle the `None` case. A bare `&Effect` does not coerce to
    /// `BasicEffect`:
    ///
    /// ```compile_fail
    /// use carina_core::effect::{BasicEffect, Effect};
    /// use carina_core::resource::Resource;
    /// let effect = Effect::Create(Resource::new("test", "x"));
    /// // Was: a missed filter could pass a non-basic `&Effect` straight
    /// // into `execute_basic_effect` and trip `unreachable!()` at apply
    /// // time (carina#3164). The conversion no longer exists.
    /// let _: BasicEffect = (&effect).into();
    /// ```
    pub fn as_basic(&self) -> Option<BasicEffect<'_>> {
        match self {
            Effect::Create(resource) => Some(BasicEffect::Create {
                effect: self,
                resource,
            }),
            Effect::Update {
                id,
                from,
                to,
                changed_attributes,
            } => Some(BasicEffect::Update {
                effect: self,
                id,
                from,
                to,
                changed_attributes,
            }),
            Effect::Delete {
                id,
                identifier,
                directives,
                ..
            } => Some(BasicEffect::Delete {
                effect: self,
                id,
                identifier,
                directives,
            }),
            Effect::Replace { .. }
            | Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. } => None,
        }
    }

    /// Returns true iff this effect polls an external state and
    /// could in principle hang forever — meaning that if it is the
    /// only kind left in flight while no other effect can dispatch,
    /// the executor must intervene rather than wait.
    pub fn is_wait(&self) -> bool {
        matches!(self, Effect::Wait { .. })
    }

    /// Returns the kind of Effect as a string (for display)
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Read { .. } => "read",
            Effect::Create(_) => "create",
            Effect::Update { .. } => "update",
            Effect::Replace { .. } => "replace",
            Effect::Delete { .. } => "delete",
            Effect::Import { .. } => "import",
            Effect::Remove { .. } => "remove",
            Effect::Move { .. } => "move",
            Effect::Wait { .. } => "wait",
        }
    }

    /// Returns whether this Effect causes a mutation
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Effect::Read { .. }) && !self.is_wait()
    }

    /// Returns whether this is a state-only operation (import/remove/move)
    pub fn is_state_operation(&self) -> bool {
        matches!(
            self,
            Effect::Import { .. } | Effect::Remove { .. } | Effect::Move { .. }
        )
    }

    /// Returns the resource ID for this effect
    pub fn resource_id(&self) -> &ResourceId {
        match self {
            Effect::Read { resource } => &resource.id,
            Effect::Create(r) => &r.id,
            Effect::Update { id, .. } => id,
            Effect::Replace { id, .. } => id,
            Effect::Delete { id, .. } => id,
            Effect::Import { id, .. } => id,
            Effect::Remove { id, .. } => id,
            Effect::Move { to, .. } => to,
            Effect::Wait { target_id, .. } => target_id,
        }
    }

    /// Returns a read-only [`ResourceRef`](crate::parser::ResourceRef)
    /// view of the resource for this effect, if it has one. Delete,
    /// Import, Remove, Move, and Wait effects have no resource.
    ///
    /// carina#3181 / #3308: the underlying payloads are typestate
    /// structs — `Create`/`Update`/`Replace` carry a [`Resource`],
    /// `Read` carries a [`DataSource`]. Callers that need a concrete
    /// type match the variant directly; this helper covers the
    /// shared id/attributes/binding/dependency_bindings accessors
    /// through the borrowing `ResourceRef` enum.
    pub fn as_resource_ref(&self) -> Option<crate::parser::ResourceRef<'_>> {
        match self {
            Effect::Create(resource) => Some(crate::parser::ResourceRef::Resource(resource)),
            Effect::Update { to, .. } => Some(crate::parser::ResourceRef::Resource(to)),
            Effect::Replace { to, .. } => Some(crate::parser::ResourceRef::Resource(to)),
            Effect::Read { resource } => Some(crate::parser::ResourceRef::DataSource(resource)),
            Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. } => None,
        }
    }

    /// Returns the `directives` block of this effect's resource, if it
    /// has one. `Read` (data source) and the managed variants both carry
    /// directives; state-only and `Wait` effects do not.
    fn resource_directives(&self) -> Option<&Directives> {
        match self {
            Effect::Create(resource) => Some(&resource.directives),
            Effect::Update { to, .. } => Some(&to.directives),
            Effect::Replace { to, .. } => Some(&to.directives),
            Effect::Read { resource } => Some(&resource.directives),
            Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. } => None,
        }
    }

    /// Returns the binding name for this effect's resource, if it has one.
    pub fn binding_name(&self) -> Option<String> {
        if let Effect::Delete { binding, .. } = self {
            return binding.clone();
        }
        if let Effect::Wait { binding, .. } = self {
            return Some(binding.clone());
        }
        self.as_resource_ref()
            .and_then(|r| r.binding().map(str::to_string))
    }

    /// Returns the binding names this effect depends on **via explicit
    /// `directives { depends_on = [...] }` declarations**, as a snapshot
    /// (cloned).
    ///
    /// For variants carrying a `Resource` (Create, Update, Replace,
    /// Read), the answer is derived live from
    /// `resource.directives.depends_on`. For Delete the answer comes
    /// from a stored `explicit_dependencies` set captured by the differ
    /// at construction time, because the originating resource is gone
    /// by the time the executor runs (#2871).
    ///
    /// State-only effects (Import, Remove, Move) return an empty set —
    /// they are scheduling primitives, not resource-state operations.
    pub fn explicit_dependencies(&self) -> HashSet<String> {
        if let Some(directives) = self.resource_directives() {
            return directives.depends_on.iter().cloned().collect();
        }
        if let Effect::Delete {
            explicit_dependencies,
            ..
        } = self
        {
            return explicit_dependencies.clone();
        }
        if let Effect::Wait {
            explicit_dependencies,
            ..
        } = self
        {
            return explicit_dependencies.clone();
        }
        HashSet::new()
    }
}

/// Render an [`Effect::Import`] identifier for plan display.
///
/// Three shapes matter:
///
/// 1. **Concrete `String` / `EnumIdentifier`**: print the bare
///    identifier text (no DSL string quoting — operators read this as
///    the cloud-API identifier, not as a DSL literal).
/// 2. **`Value::Deferred(Interpolation)`**: walk the parts and emit
///    each one inline — `Literal` segments verbatim, `Expr` segments
///    through [`crate::value::format_value_with_key`]. Concrete `Expr`
///    parts therefore render as bare text and a deferred upstream ref
///    renders as `(known after upstream apply: …)` *without* the
///    surrounding `${…}` syntax, so the operator sees the full
///    composite identifier exactly as it will look after apply with
///    the deferred slot called out.
/// 3. **Other deferred shapes**: fall through to
///    [`crate::value::format_value_with_key`].
///
/// Carina#3329.
pub fn format_import_identifier(identifier: &crate::resource::Value) -> String {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};
    match identifier {
        Value::Concrete(ConcreteValue::String(s)) => s.clone(),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => s.to_string(),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    InterpolationPart::Literal(s) => out.push_str(s),
                    // Recurse so a nested `Value::Deferred(Interpolation)`
                    // produced by canonicalization stays unquoted and
                    // un-`${…}`-wrapped at every level. Falling through
                    // to `format_value_with_key` here would re-introduce
                    // the wrapping the outer level was designed to
                    // strip.
                    InterpolationPart::Expr(v) => out.push_str(&format_import_identifier(v)),
                }
            }
            out
        }
        other => crate::value::format_value_with_key(other, Some("id")),
    }
}

/// Resolve an [`Effect::Import`] identifier to the concrete string the
/// provider's `read()` needs, or return a structured error describing
/// which deferred segment prevented resolution.
///
/// Centralizing the check means a future apply-side caller cannot
/// silently ship a `Value::Deferred(…)` to the provider by rolling its
/// own incomplete `match`: the only public path from a `Value` import
/// identifier to a provider-ready `&str` goes through this helper.
/// Plan-time display still calls
/// [`crate::value::format_value_with_key`] on the same field, so a
/// deferred upstream-state ref renders as
/// `(known after upstream apply: …)` rather than being silently
/// substituted to empty — see carina#3329.
pub fn resolve_import_identifier(identifier: &crate::resource::Value) -> Result<&str, String> {
    use crate::resource::{ConcreteValue, Value};
    match identifier {
        Value::Concrete(ConcreteValue::String(s)) => Ok(s.as_str()),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Ok(s.as_str()),
        other => Err(format!(
            "import identifier did not resolve to a concrete string at apply time \
             (got {}). A referenced upstream value has not been published yet — \
             apply the upstream stack first, then re-run apply here.",
            crate::value::format_value_with_key(other, Some("id"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn plan_op_supports_debug_eq_and_hash() {
        let op = PlanOp::Create;
        assert_eq!(op, PlanOp::Create);
        assert_eq!(format!("{op:?}"), "Create");

        let mut ops = HashSet::new();
        ops.insert(op);
        assert!(ops.contains(&PlanOp::Create));
    }

    #[test]
    fn format_import_identifier_recurses_into_nested_interpolation() {
        // Carina#3329 (round-2): if canonicalization or a future
        // resolver pass produces a `Value::Deferred(Interpolation)`
        // *inside* an `Expr` part of the outer interpolation, the
        // helper must stay in its inline-string mode at every level
        // rather than falling back to `format_value_with_key` (which
        // would wrap the nested value as a DSL string literal with
        // surrounding `"…"` quotes and `${…}` syntax).
        use crate::resource::{
            ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
        };
        let inner = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamBareRef {
                    binding: "u".into(),
                },
            ))),
            InterpolationPart::Literal("-suffix".into()),
        ]));
        let outer = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(inner),
            InterpolationPart::Literal("-tail".into()),
        ]));
        let rendered = format_import_identifier(&outer);
        assert_eq!(
            rendered, "prefix-(known after upstream apply: u)-suffix-tail",
            "nested interpolation must render inline with no `${{…}}` wrapping or quoting"
        );
        // Sanity: a bare concrete still renders bare.
        assert_eq!(
            format_import_identifier(&Value::Concrete(ConcreteValue::String("plain".into()))),
            "plain"
        );
    }

    #[test]
    fn resolve_import_identifier_accepts_concrete_string() {
        use crate::resource::{ConcreteValue, Value};
        let v = Value::Concrete(ConcreteValue::String("vpc-0abc".into()));
        assert_eq!(resolve_import_identifier(&v).unwrap(), "vpc-0abc");
    }

    #[test]
    fn resolve_import_identifier_rejects_deferred_interpolation() {
        // carina#3329: an apply-time `Effect::Import.identifier` carrying
        // an unresolved interpolation must surface a structured error,
        // not be silently shipped to the provider as the
        // partially-substituted literal. Pre-#3329 the field was a
        // `String` so the same shape would land at the provider as
        // `|literal|literal` with no way to detect the dropped
        // interpolation; the helper now gates that path through the
        // `Value` type.
        use crate::resource::{
            ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
        };
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamBareRef {
                    binding: "upstream".into(),
                },
            ))),
            InterpolationPart::Literal("|tail".into()),
        ]));
        let err = resolve_import_identifier(&v).unwrap_err();
        assert!(
            err.contains("did not resolve to a concrete string"),
            "unexpected error message: {err}"
        );
        // Sanity: still passes for a concrete String/EnumIdentifier.
        let s = Value::Concrete(ConcreteValue::enum_identifier("ENUM_X"));
        assert_eq!(resolve_import_identifier(&s).unwrap(), "ENUM_X");
    }

    #[test]
    fn read_is_not_mutating() {
        let resource = DataSource::new("test", "example");
        let effect = Effect::Read { resource };
        assert!(!effect.is_mutating());
    }

    #[test]
    fn create_is_mutating() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource);
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource = DataSource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Read {
            resource: resource.clone(),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn resource_returns_some_for_create() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource.clone());
        assert_eq!(effect.as_resource_ref().unwrap().id(), &resource.id);
    }

    #[test]
    fn resource_returns_none_for_delete() {
        let effect = Effect::Delete {
            id: ResourceId::new("test", "a"),
            identifier: "id-123".to_string(),
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert!(effect.as_resource_ref().is_none());
    }

    #[test]
    fn binding_name_returns_binding() {
        let resource = Resource::new("test", "my_binding").with_binding("my_binding");
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), Some("my_binding".to_string()));
    }

    #[test]
    fn binding_name_returns_none_without_binding() {
        use crate::resource::{ConcreteValue, Value};
        let resource = Resource::new("test", "no_binding").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), None);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use std::collections::HashMap;

        let effects = vec![
            Effect::Create(Resource::new("s3.Bucket", "my-bucket")),
            Effect::Read {
                resource: DataSource::new("s3.Bucket", "existing"),
            },
            Effect::Update {
                id: ResourceId::new("s3.Bucket", "my-bucket"),
                from: Box::new(State::existing(
                    ResourceId::new("s3.Bucket", "my-bucket"),
                    HashMap::from([(
                        "versioning".to_string(),
                        Value::Concrete(ConcreteValue::String("Disabled".to_string())),
                    )]),
                )),
                to: Resource::new("s3.Bucket", "my-bucket").with_attribute(
                    "versioning",
                    Value::Concrete(ConcreteValue::String("Enabled".to_string())),
                ),
                changed_attributes: vec!["versioning".to_string()],
            },
            Effect::Replace {
                id: ResourceId::new("ec2.Vpc", "my-vpc"),
                from: Box::new(State::existing(
                    ResourceId::new("ec2.Vpc", "my-vpc"),
                    HashMap::from([(
                        "cidr_block".to_string(),
                        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                    )]),
                )),
                to: Resource::new("ec2.Vpc", "my-vpc").with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
                ),
                directives: Directives::default(),
                changed_create_only: crate::effect::ChangedCreateOnly::new(vec![
                    "cidr_block".to_string(),
                ])
                .unwrap(),
                // carina#3181 PR D: cover `CascadingUpdate.to:
                // Resource` in the serde round-trip.
                cascading_updates: vec![CascadingUpdate {
                    id: ResourceId::new("ec2.Subnet", "my-subnet"),
                    from: Box::new(State::not_found(ResourceId::new("ec2.Subnet", "my-subnet"))),
                    to: Resource::new("ec2.Subnet", "my-subnet").with_attribute(
                        "vpc_id",
                        Value::Concrete(ConcreteValue::String("vpc.id".to_string())),
                    ),
                }],
                temporary_name: None,
                cascade_ref_hints: vec![],
            },
            Effect::Delete {
                id: ResourceId::new("s3.Bucket", "old-bucket"),
                identifier: "old-bucket".to_string(),
                directives: Directives::default(),
                binding: None,
                dependencies: HashSet::new(),
                explicit_dependencies: std::collections::HashSet::new(),
            },
        ];

        for effect in effects {
            let json = serde_json::to_string(&effect).unwrap();
            let deserialized: Effect = serde_json::from_str(&json).unwrap();
            assert_eq!(effect, deserialized, "Round-trip failed for {:?}", effect);
        }
    }

    #[test]
    fn changed_create_only_constructor_rejects_empty() {
        assert!(ChangedCreateOnly::new(Vec::new()).is_none());
        assert_eq!(
            &ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap()[..],
            ["cidr_block".to_string()]
        );
    }

    #[test]
    fn changed_create_only_push_preserves_non_empty() {
        let mut changed = ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap();
        changed.push("vpc_id".to_string());
        assert_eq!(
            &changed[..],
            ["cidr_block".to_string(), "vpc_id".to_string()]
        );
    }

    #[test]
    fn replace_changed_create_only_serializes_as_plain_array() {
        let effect = Effect::Replace {
            id: ResourceId::new("test", "x"),
            from: Box::new(State::not_found(ResourceId::new("test", "x"))),
            to: Resource::new("test", "x"),
            directives: Directives::default(),
            changed_create_only: ChangedCreateOnly::new(vec!["x".to_string()]).unwrap(),
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };

        let json = serde_json::to_value(&effect).unwrap();
        assert_eq!(
            json["Replace"]["changed_create_only"],
            serde_json::json!(["x"])
        );
        let decoded: Effect = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, effect);
    }

    #[test]
    fn replace_deserialize_rejects_empty_changed_create_only() {
        let json = serde_json::json!({
            "Replace": {
                "id": {"provider": "", "resource_type": "test", "name": "x"},
                "from": {
                    "id": {"provider": "", "resource_type": "test", "name": "x"},
                    "identifier": "x-1",
                    "attributes": {},
                    "exists": true,
                    "dependency_bindings": []
                },
                "to": {
                    "id": {"provider": "", "resource_type": "test", "name": "x"},
                    "attributes": {}
                },
                "directives": {},
                "changed_create_only": [],
                "cascading_updates": [],
                "temporary_name": null,
                "cascade_ref_hints": []
            }
        });

        let err = serde_json::from_value::<Effect>(json)
            .expect_err("empty changed_create_only must not deserialize as Replace");
        assert!(
            err.to_string()
                .contains("Replace effect requires at least one changed create-only attribute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn explicit_dependencies_derived_from_resource_directives() {
        use crate::resource::Directives;
        let mut bucket = Resource::new("s3.Bucket", "b");
        bucket.directives = Directives {
            depends_on: vec!["role".to_string(), "kms".to_string()],
            ..Directives::default()
        };
        let create = Effect::Create(bucket.clone());
        let got = create.explicit_dependencies();
        assert!(got.contains("role") && got.contains("kms"), "got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_for_delete_uses_stored_set() {
        let effect = Effect::Delete {
            id: ResourceId::new("s3.Bucket", "b"),
            identifier: "x".to_string(),
            directives: Directives::default(),
            binding: Some("bucket".to_string()),
            dependencies: HashSet::from(["role".to_string(), "kms".to_string()]),
            explicit_dependencies: HashSet::from(["role".to_string()]),
        };
        let got = effect.explicit_dependencies();
        assert!(got.contains("role"));
        assert!(!got.contains("kms"), "kms is value-ref-only; got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_empty_for_state_only_effects() {
        let imp = Effect::Import {
            id: ResourceId::new("s3.Bucket", "b"),
            identifier: crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                "x".to_string(),
            )),
        };
        let rem = Effect::Remove {
            id: ResourceId::new("s3.Bucket", "b"),
        };
        let mov = Effect::Move {
            from: ResourceId::new("s3.Bucket", "old"),
            to: ResourceId::new("s3.Bucket", "new"),
        };
        for e in [imp, rem, mov] {
            assert!(
                e.explicit_dependencies().is_empty(),
                "expected empty for state-only, got {:?}",
                e.explicit_dependencies()
            );
        }
    }

    #[test]
    fn delete_legacy_state_without_explicit_deps_deserialises_to_empty() {
        // Pre-#2871 state files have no `explicit_dependencies` field.
        // `#[serde(default)]` must populate it as an empty HashSet so
        // round-tripping legacy state never fails.
        let legacy = serde_json::json!({
            "Delete": {
                "id": {"provider": "aws", "resource_type": "s3.Bucket", "name": "b"},
                "identifier": "x",
                "directives": {},
                "binding": null,
                "dependencies": ["role"],
            }
        });
        let effect: Effect = serde_json::from_value(legacy).unwrap();
        if let Effect::Delete {
            explicit_dependencies,
            ..
        } = &effect
        {
            assert!(explicit_dependencies.is_empty());
        } else {
            panic!("expected Delete, got {:?}", effect);
        }
    }

    #[test]
    fn wait_variant_constructs() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let _ = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(75 * 60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
    }

    #[test]
    fn wait_binding_name_returns_wait_binding() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert_eq!(e.binding_name(), Some("cert_issued".to_string()));
    }

    #[test]
    fn wait_is_not_mutating() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert!(!e.is_mutating());
        assert_eq!(e.kind(), "wait");
    }

    #[test]
    fn wait_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let original = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(4500),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        // Duration must round-trip as plain integer seconds (matches the
        // project's "no { secs, nanos }" rule from #2824).
        assert!(
            json.contains("\"timeout\":4500"),
            "expected `\"timeout\":4500` in JSON, got: {}",
            json
        );
        let decoded: Effect = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, original);
    }

    /// `as_basic()` must return `Some` for the three variants the
    /// basic executor handles, and `None` for every other variant.
    /// This is the carina#3164 type-level contract: filters used to
    /// be caller-side, and a missed filter (Move slipping into Phase
    /// 1 of the phased executor) panicked apply.
    #[test]
    fn as_basic_narrows_to_create_update_and_delete_only() {
        use crate::resource::State as ResState;

        let rid = ResourceId::new("test", "x");

        // Basic variants must narrow.
        let create = Effect::Create(Resource::new("test", "x"));
        assert!(matches!(
            create.as_basic(),
            Some(BasicEffect::Create { .. })
        ));

        let update = Effect::Update {
            id: rid.clone(),
            from: Box::new(ResState::not_found(rid.clone())),
            to: Resource::new("test", "x"),
            changed_attributes: vec![],
        };
        assert!(matches!(
            update.as_basic(),
            Some(BasicEffect::Update { .. })
        ));

        let delete = Effect::Delete {
            id: rid.clone(),
            identifier: "x-1".to_string(),
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
        };
        assert!(matches!(
            delete.as_basic(),
            Some(BasicEffect::Delete { .. })
        ));

        // Non-basic variants must not. If a new variant is added and
        // someone forgets to extend `as_basic`, the exhaustive match
        // inside `as_basic` is what catches it at compile time; this
        // test catches misclassification of an existing variant.
        let read = Effect::Read {
            resource: DataSource::new("test", "x"),
        };
        let replace = Effect::Replace {
            id: rid.clone(),
            from: Box::new(ResState::not_found(rid.clone())),
            to: Resource::new("test", "x"),
            directives: Directives::default(),
            changed_create_only: ChangedCreateOnly::new(vec!["attr".to_string()]).unwrap(),
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };
        let import = Effect::Import {
            id: rid.clone(),
            identifier: crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                "x-1".to_string(),
            )),
        };
        let remove = Effect::Remove { id: rid.clone() };
        let mov = Effect::Move {
            from: rid.clone(),
            to: ResourceId::new("test", "y"),
        };
        let wait = Effect::Wait {
            binding: "w".to_string(),
            target_id: rid.clone(),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: crate::wait::predicate::AttrPath {
                    segments: vec!["status".to_string()],
                },
                value: crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                    "ready".to_string(),
                )),
            },
            until_surface: "status == 'ready'".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        };
        for (label, e) in [
            ("Read", read),
            ("Replace", replace),
            ("Import", import),
            ("Remove", remove),
            ("Move", mov),
            ("Wait", wait),
        ] {
            assert!(
                e.as_basic().is_none(),
                "{label} must not narrow to BasicEffect"
            );
        }
    }
}
