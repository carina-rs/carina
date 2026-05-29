//! Single source of truth for binding-name lookups (#2231 / #2251).
//!
//! Three sibling views live here, each answering a different question
//! about the same set of names declared in a parsed Carina configuration:
//!
//! - [`BindingNameSet`] — "is this identifier in scope at all?" View
//!   for the parser and LSP scope-check pass. Records all eight
//!   declaration kinds the parser tracks (resource, argument, module
//!   call, upstream state, import alias, user function, variable,
//!   structural binding from let-of-if/for/read).
//! - [`BindingIndex`] — "what schema and which `Resource` does this
//!   binding point at?" Schema-aware view. Used by validation and the
//!   LSP. Each entry has both a `Resource` and a `ResourceSchema`.
//! - [`ResolvedBindings`] — "what attribute values does this binding
//!   actually carry?" Value-aware view. Used by the resolver and
//!   executor. Includes upstream-state bindings that have no `Resource`
//!   or `ResourceSchema`.
//!
//! The three are separate types rather than fields on one because their
//! invariants differ — `BindingIndex` requires both `Resource` and
//! `ResourceSchema`, `ResolvedBindings` requires only attribute values
//! (no schema), `BindingNameSet` requires only the name and an origin
//! tag. Callers that need more than one view hold them side by side.
//!
//! Built once at the parse → validate boundary, then borrowed. The walk
//! is **top-level only** (`parsed.resources`) on purpose: for-body
//! template resources carry a parser-synthesised binding name used for
//! address derivation, but those names are an internal detail and were
//! never visible to validation's `binding_map` pre-#2231. Surfacing them
//! through `BindingIndex` would let ResourceRefs name them, which is a
//! behaviour change neither validation nor the LSP wants. The LSP still
//! walks `iter_all_resources` separately for its own checks (so for-body
//! type/enum diagnostics fire); only the binding-name table is scoped.
//!
//! ```ignore
//! let index = BindingIndex::from_parsed(&parsed, &registry);
//! if let Some(entry) = index.get("vpc") {
//!     // entry.schema is the resolved schema for the binding
//! }
//! ```

use crate::parser::{BindingName, ResourceRef};
use crate::resource::{Composition, Resource, ResourceId, State, Value};
use crate::schema::{ResourceSchema, SchemaRegistry};
use std::collections::HashMap;

/// One entry in the binding index. `schema` is non-`Option` because the
/// builder skips bindings whose schema cannot be resolved — callers never
/// have to defend against half-populated entries.
#[derive(Debug)]
pub struct BindingEntry<'a> {
    pub schema: &'a ResourceSchema,
}

/// Index of `binding_name → (resource, schema)` for every named binding
/// declared in `parsed`. Lifetime `'a` ties the index to its inputs so
/// callers can keep it borrowed without cloning.
///
/// `entries` only contains bindings whose schema resolved successfully.
/// `known_names` records every named binding regardless of schema status,
/// so callers can tell "unknown binding" apart from "binding exists but
/// its schema is missing" — those are separate diagnostics.
#[derive(Debug, Default)]
pub struct BindingIndex<'a> {
    entries: HashMap<String, BindingEntry<'a>>,
    known_names: std::collections::HashSet<String>,
}

impl<'a> BindingIndex<'a> {
    /// Build the index from a parsed file and a [`SchemaRegistry`]. The
    /// registry handles the `(provider, resource_type, kind) -> schema`
    /// lookup, including the kind-aware split between managed resources
    /// and data sources.
    ///
    /// Bindings whose schema is missing from the registry are silently
    /// skipped — callers (validation / LSP) treat unknown resource types
    /// as a separate diagnostic, so reporting them again here would
    /// double-count.
    pub fn from_parsed<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
    ) -> Self {
        let mut entries = HashMap::new();
        let mut known_names = std::collections::HashSet::new();
        // Walk top-level resources only — managed resources, data
        // sources, and compositions (carina#3181: `iter_top_level_resources`
        // chains the typed slices and excludes deferred for-body
        // templates). The parser auto-generates a synthetic `binding` for
        // anonymous for-body templates (used for resource address
        // derivation), but those names are an internal detail — they were
        // never visible to validation's binding map pre-#2231 and
        // surfacing them here would be an unintended behaviour change for
        // ResourceRef lookups. The LSP and validation both still walk
        // `iter_all_resources` *separately* for their own checks; only
        // the binding-name table is scoped to top-level here.
        for rref in parsed.iter_top_level_resources() {
            let Some(binding_name) = rref.binding() else {
                continue;
            };
            known_names.insert(binding_name.to_string());
            // Schema lookup routes by the typestate arm: managed
            // resources via `get_for`, data sources via
            // `get_for_data_source`. A composition resource has no schema,
            // so it stays in `known_names` but gets no `entries` row,
            // matching the prior "known binding, schema absent" surface.
            let schema = match rref {
                ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                    registry.get_for(m)
                }
                ResourceRef::DataSource(d) => registry.get_for_data_source(d),
                ResourceRef::Composition(_) => None,
            };
            let Some(schema) = schema else {
                continue;
            };
            entries.insert(binding_name.to_string(), BindingEntry { schema });
        }
        Self {
            entries,
            known_names,
        }
    }

    pub fn get(&self, name: &str) -> Option<&BindingEntry<'a>> {
        self.entries.get(name)
    }

    /// True iff a binding by this name was declared anywhere in the parsed
    /// file, *even if its schema could not be resolved*. Used to
    /// distinguish "unknown binding" diagnostics from "known binding but
    /// schema missing" (which is a different diagnostic surface).
    pub fn is_declared(&self, name: &str) -> bool {
        self.known_names.contains(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrow-valued projection — `binding_name → &ResourceSchema`. Lets
    /// callers that previously took a `HashMap<String, ResourceSchema>` of
    /// schema clones switch to a borrow-only map without changing their
    /// signatures' shape (just the value type). Saves a per-binding,
    /// per-keystroke `ResourceSchema::clone()` on the LSP path.
    pub fn schemas_by_name(&self) -> HashMap<&str, &'a ResourceSchema> {
        self.entries
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.schema))
            .collect()
    }
}

/// Origin of a binding name in [`BindingNameSet`]. Eight kinds, one per
/// declaration form a parsed Carina configuration can introduce.
///
/// Lives on `BindingNameSet` (the scope-only view) rather than on
/// [`BindingValueSource`] (the value-aware view) because not every kind
/// has a value: `Use` (import alias) and `UserFunction` are pure
/// identifiers that the resolver/executor never need to look up
/// attributes on.
///
/// `#[non_exhaustive]` so adding a new declaration form (a future
/// `data` block, etc.) does not break downstream `match` arms.
///
/// Not `Copy`: `Wait` carries an owned `BindingName` (the wait→target
/// edge is a typed value, carina#3085 — not a string convention). The
/// enum is read out by reference (`kind()` / `iter()`); the only
/// in-crate consumers are within this module, so dropping `Copy` is
/// contained.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BindingNameKind {
    /// `let <name> = ...` resource binding.
    Resource,
    /// `argument <name> { ... }` module argument.
    Argument,
    /// `module <name> "..." { ... }` call site.
    ModuleCall,
    /// `upstream_state <name> "..." { ... }` data source.
    UpstreamState,
    /// `use <alias> from "..."` import.
    Use,
    /// `function <name>(...) { ... }` user-defined function.
    UserFunction,
    /// `variable <name> { ... }` config variable.
    Variable,
    /// Iteration / condition binding from `for <name> in ...` or
    /// equivalent structural form. Recorded for scope checks but not
    /// addressable by `ResourceRef` (see the module doc for the
    /// pre-existing invariant).
    Structural,
    /// `let <name> = wait <target> { ... }` wait binding (carina#3085).
    /// Unlike `Structural`, a wait binding **is** addressable by
    /// `ResourceRef`: `<name>.<attr>` is a passthrough to
    /// `<target>.<attr>` (see `notes/specs/2026-05-09-wait-construct-design.md`
    /// value semantics). The `target` is the binding name of the
    /// resource the wait observes; carried here as a typed
    /// [`BindingName`] so the wait→target edge cannot be confused with
    /// an arbitrary string or point at a non-existent name undetected.
    Wait { target: BindingName },
}

/// Name-only scope view: every identifier a parsed Carina configuration
/// has brought into scope, tagged with its declaration kind.
///
/// This is the canonical replacement for the parser's
/// `collect_known_bindings_merged` helper (#2104 / #2301). The two
/// views ([`BindingIndex`] and [`ResolvedBindings`]) carry richer data
/// (schema; resolved values) but are restricted to bindings that
/// actually carry such data. `BindingNameSet` is the broadest of the
/// three — it answers "is this identifier in scope at all?" for
/// parser- and LSP-side identifier-scope diagnostics.
///
/// The set is built once from a [`ParsedFile`] (the merged
/// directory-level view) and borrowed thereafter; names are owned
/// `String`s so the set survives independent of the source.
#[derive(Debug, Default, Clone)]
pub struct BindingNameSet {
    by_name: HashMap<String, BindingNameKind>,
}

impl BindingNameSet {
    /// Build the set from a merged [`ParsedFile`], populating every
    /// declaration form the parser tracks.
    ///
    /// **Kind precedence on collisions**: a single name can appear in
    /// more than one parser-side map by design — for example, a `let
    /// vpc = aws.ec2.Vpc { ... }` registers `vpc` in both
    /// `parsed.resources` (as a resource binding) *and*
    /// `parsed.variables` (as a let-value binding, holding a
    /// placeholder ref). To keep the kind label meaningful, sources are
    /// inserted in **most-specific-first** order and the first
    /// `insert` wins (later sources call `entry().or_insert`).
    ///
    /// The order — `Resource` → `ModuleCall` → `UpstreamState` →
    /// `Argument` → `Use` → `UserFunction` → `Structural` → `Variable`
    /// — keeps `Variable` last because `parsed.variables` is the
    /// catch-all "any `let`-RHS placeholder lives here" map, while the
    /// preceding sources are narrower declaration forms.
    pub fn from_parsed<E>(parsed: &crate::parser::File<E>) -> Self {
        let mut by_name: HashMap<String, BindingNameKind> = HashMap::new();

        // carina#3181: walk the typed top-level slices — managed
        // resources, data sources, and compositions all register a
        // `Resource`-kind binding name (a data source declared as
        // `let x = read ...` is addressable by `ResourceRef`, and a
        // composition binding was a `Resource`-kind name before the
        // typestate split too).
        for rref in parsed.iter_top_level_resources() {
            if let Some(name) = rref.binding() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::Resource);
            }
        }
        // Wait bindings register right after resources: a wait binding
        // is addressable by `ResourceRef` (passthrough to its target),
        // so it belongs with the addressable forms, not with
        // `Structural`. The parser enforces distinct binding names, so
        // a name is a wait binding xor a resource binding — ordering
        // here is documentation, not correctness-critical (carina#3085).
        for wb in &parsed.wait_bindings {
            by_name
                .entry(wb.binding.as_str().to_string())
                .or_insert(BindingNameKind::Wait {
                    target: wb.target.clone(),
                });
        }
        for call in &parsed.module_calls {
            if let Some(name) = call.binding_name.as_deref() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::ModuleCall);
            }
        }
        for upstream in &parsed.upstream_states {
            by_name
                .entry(upstream.binding.clone())
                .or_insert(BindingNameKind::UpstreamState);
        }
        for arg in &parsed.arguments {
            by_name
                .entry(arg.name.clone())
                .or_insert(BindingNameKind::Argument);
        }
        for use_decl in &parsed.uses {
            by_name
                .entry(use_decl.alias.clone())
                .or_insert(BindingNameKind::Use);
        }
        for fn_name in parsed.user_functions.keys() {
            by_name
                .entry(fn_name.clone())
                .or_insert(BindingNameKind::UserFunction);
        }
        for structural in &parsed.structural_bindings {
            by_name
                .entry(structural.clone())
                .or_insert(BindingNameKind::Structural);
        }
        for var_name in parsed.variables.keys() {
            by_name
                .entry(var_name.clone())
                .or_insert(BindingNameKind::Variable);
        }

        Self { by_name }
    }

    /// `true` iff `name` is declared somewhere in the parsed file.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Declaration kind for `name`, or `None` if the name is not
    /// in scope.
    pub fn kind(&self, name: &str) -> Option<&BindingNameKind> {
        self.by_name.get(name)
    }

    /// Iterate every in-scope name. Order is unspecified.
    pub fn iter_names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Iterate every (name, kind) pair. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &BindingNameKind)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// A `wait` binding's value-layer contract: `binding`'s
/// `<binding>.<attr>` is a passthrough to `<target>.<attr>`
/// (carina#3085). Both sides are the typed [`BindingName`] newtype so
/// the wait→target edge cannot be confused with an arbitrary string.
///
/// This is the *only* thing `ResolvedBindings` needs from a wait
/// declaration — deliberately **not** the full parser
/// [`crate::parser::WaitBinding`] (which also carries the `until`
/// predicate, timeout, `depends_on`, source line — all effect-layer /
/// diagnostics concerns irrelevant to value resolution). Decoupling
/// here means: (a) carina-core resolution does not depend on the
/// parser AST shape, and (b) the apply-from-plan-file path — which
/// reconstructs from a serialized `PlanFile` and has no `WaitBinding`
/// — can supply the same typed spec without resurrecting the AST.
/// `WaitBinding` provides a `From` conversion (the plan path); the
/// plan-file path builds it from its serialized `(binding, target)`
/// pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitAliasSpec {
    pub binding: BindingName,
    pub target: BindingName,
}

impl From<&crate::parser::WaitBinding> for WaitAliasSpec {
    fn from(wb: &crate::parser::WaitBinding) -> Self {
        Self {
            binding: wb.binding.clone(),
            target: wb.target.clone(),
        }
    }
}

/// Where the values for a binding came from. `Local` means a `let` binding
/// in the current configuration; `Upstream` means an `upstream_state` data
/// source bringing values in from another state file.
///
/// Marked `#[non_exhaustive]` because #2301 will introduce structural
/// sources (`for` / `if` / module) — flagging the intent now keeps
/// downstream `match` arms from becoming exhaustive against the current
/// two variants.
///
/// Not `Copy`: `WaitAlias` carries an owned `BindingName` (the
/// wait→target edge is a typed value, carina#3085). `source()` returns
/// a borrow; the only in-crate value-position use is a `matches!`
/// (`Copy`-independent), so dropping `Copy` is contained.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingValueSource {
    Local,
    Upstream,
    /// The binding is a `wait` binding whose `<name>.<attr>` is a
    /// passthrough to `target`'s attributes (carina#3085). The
    /// attributes stored alongside this source are a snapshot of the
    /// target's resolved attributes at construction time (see
    /// `notes/specs/2026-05-09-wait-construct-design.md` value
    /// semantics — "snapshot of the target captured by the read() that
    /// satisfied until"). Retained as a distinct source (not flattened
    /// to `Local`) so "did this value come *through* a wait?" stays
    /// observable — the dependency edge is handled separately by
    /// `Effect::Wait` lowering and is not affected by this value-layer
    /// alias.
    WaitAlias {
        target: BindingName,
    },
}

/// One entry in [`ResolvedBindings`]: the merged attribute map and the
/// source it came from. Stored as a single struct (rather than two
/// parallel maps) so the type system enforces "every name has both
/// attributes and a source".
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub attributes: HashMap<String, Value>,
    pub source: BindingValueSource,
}

/// Value-aware sibling of [`BindingIndex`]. See the module-level doc for
/// why this is a separate type.
///
/// Owned, not borrowed: building the merged map requires combining
/// `Resource.attributes` with `State.attributes`, and there is no single
/// upstream `HashMap<String, HashMap<String, Value>>` already on hand to
/// borrow from. Owning the merged map avoids a self-referential
/// structure.
#[derive(Debug, Default, Clone)]
pub struct ResolvedBindings {
    by_name: HashMap<String, ResolvedBinding>,
}

/// Required inputs for [`ResolvedBindings::pre_apply`] (carina#3248).
///
/// All fields are mandatory; no `Default`, no `Option`. This shape
/// turns "forgot compositions at a new call site" into a compile error —
/// a struct-literal with a missing field fails to compile, so the
/// pre-apply path cannot be silently constructed without the kind
/// of binding sources it needs (carina#3246).
///
/// Slices and maps are borrowed: the constructor reads them once and
/// stores owned copies internally, so the caller retains ownership
/// without forcing a clone at the API boundary.
pub struct PreApplyInputs<'a> {
    pub managed: &'a [Resource],
    pub compositions: &'a [Composition],
    pub data_sources: &'a [crate::resource::DataSource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [WaitAliasSpec],
}

impl ResolvedBindings {
    /// Single typed pre-apply constructor (carina#3248).
    ///
    /// Builds the bindings view from every kind of binding that
    /// reference resolution can name — managed resources, composition
    /// resources (module-call attribute containers), and data
    /// sources — plus the same `current_states`, `remote_bindings`,
    /// and `wait_aliases` inputs the legacy entries took.
    ///
    /// The required-fields `PreApplyInputs` struct turns "forgot to
    /// include compositions at a new call site" into a compile error
    /// rather than a runtime symptom: any missing field on the
    /// struct-literal fails to compile, so a new pre-apply call site
    /// cannot accidentally lose the composition / data-source layer the
    /// way the previous managed-only constructor + opt-in
    /// `add_compositions` shape allowed (carina#3246).
    ///
    /// Layering order: managed first (merged with `current_states`,
    /// DSL-wins-on-collision), then data sources, then compositions,
    /// then wait aliases. The post-apply layering in
    /// `state_writeback.rs` uses the same order, so a same-stack
    /// collision resolves identically on the pre-apply and post-apply
    /// sides. (Same-name collisions are independently rejected by the
    /// parser's `DuplicateBinding` check, so the order is observable
    /// only in test code that constructs colliding inputs by hand.)
    pub fn pre_apply(inputs: PreApplyInputs<'_>) -> Self {
        let mut bindings = Self::build_managed_core(
            inputs.managed,
            inputs.current_states,
            inputs.remote_bindings,
        );
        bindings.layer_data_source_bindings(inputs.data_sources, inputs.current_states);
        bindings
            .layer_compositions_post_apply(inputs.compositions)
            .expect("layer_compositions_post_apply is currently infallible");
        bindings.layer_wait_aliases(inputs.wait_aliases);
        bindings
    }

    /// Shared managed + remote-bindings layering used by both the
    /// pre-apply constructor and the legacy entries during the
    /// migration window. Wait-aliases are layered separately by the
    /// caller after data sources / compositions so a wait whose target is
    /// a data source or composition still snapshots the resolved attrs.
    fn build_managed_core(
        managed: &[Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
    ) -> Self {
        let mut by_name: HashMap<String, ResolvedBinding> = HashMap::new();

        for resource in managed.iter() {
            let Some(binding_name) = resource.binding.as_ref() else {
                continue;
            };
            let mut merged: HashMap<String, Value> = resource.resolved_attributes();
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    merged.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: merged,
                    source: BindingValueSource::Local,
                },
            );
        }

        for (remote_binding, remote_attrs) in remote_bindings {
            by_name.insert(
                remote_binding.clone(),
                ResolvedBinding {
                    attributes: remote_attrs.clone(),
                    source: BindingValueSource::Upstream,
                },
            );
        }

        Self { by_name }
    }

    /// Layer wait aliases on the view. Extracted from
    /// `pre_apply` so `pre_apply` can sequence the
    /// layering (managed → data sources → compositions → wait aliases)
    /// without duplicating the alias logic.
    fn layer_wait_aliases(&mut self, wait_aliases: &[WaitAliasSpec]) {
        for spec in wait_aliases {
            let Some(target_entry) = self.by_name.get(spec.target.as_str()) else {
                continue;
            };
            let snapshot = target_entry.attributes.clone();
            self.by_name.insert(
                spec.binding.as_str().to_string(),
                ResolvedBinding {
                    attributes: snapshot,
                    source: BindingValueSource::WaitAlias {
                        target: spec.target.clone(),
                    },
                },
            );
        }
    }

    /// Typed post-apply layering (#3176): add composition
    /// bindings onto an existing view.
    ///
    /// **Scope:** post-apply increment layering (carina#3248). The
    /// canonical caller is `state_writeback.rs`, which re-resolves
    /// compositions against post-apply state via
    /// `resolve_virtual_refs_post_apply` and then layers them on top
    /// of the pre-apply binding view for export resolution. Pre-apply
    /// call sites must use [`ResolvedBindings::pre_apply`] instead —
    /// that constructor lays compositions in once at the start and makes
    /// "forgot compositions" a compile error.
    ///
    /// **Ordering contract**: must be called *after* the managed-side
    /// bindings have been constructed, so any composition whose
    /// attributes contain a `ResourceRef` to a managed sibling
    /// resolves against the up-to-date managed view. The caller is
    /// responsible for that ordering; the function itself just
    /// appends.
    ///
    /// Unlike `pre_apply`, this entry does **not** merge
    /// `current_states`: compositions have no provider-side state to
    /// merge. The composition's own attribute map (as authored, after
    /// `resolve_virtual_refs_post_apply` materialised any refs) is
    /// recorded verbatim.
    ///
    /// Same-name collisions favour the composition entry (it is inserted
    /// last) — consistent with the design-doc rule that the
    /// post-apply view is layered *on top of* the pre-apply view.
    ///
    /// Returns `Result<(), String>` to match the design-doc signature;
    /// today the only failure mode (an ordering-precondition check)
    /// is left to the caller, so this currently always returns
    /// `Ok(())`. The Result shape lets future validation (e.g. a
    /// hard error on a same-name collision when the post-apply layer
    /// expects to merge rather than overwrite) be added without
    /// breaking the signature.
    pub fn layer_compositions_post_apply(
        &mut self,
        compositions: &[Composition],
    ) -> Result<(), String> {
        for v in compositions.iter() {
            let Some(binding_name) = v.binding.as_ref() else {
                continue;
            };
            let value_attrs: indexmap::IndexMap<String, crate::resource::Value> = v
                .signature
                .attributes
                .iter()
                .map(|(k, attr)| (k.clone(), attr.to_value()))
                .collect();
            self.by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: crate::resource::attrs_to_hashmap(&value_attrs),
                    source: BindingValueSource::Local,
                },
            );
        }
        Ok(())
    }

    /// Layer data-source bindings onto the view. carina#3181: a
    /// `let x = read aws.iam.user { ... }` binding is addressable by a
    /// `ResourceRef` (`x.user_id`), and an `exports { y = x.user_id }`
    /// must see the data source's resolved attribute map. Data sources
    /// are a distinct typestate from managed resources, so they are
    /// layered in explicitly rather than collapsed into the managed
    /// slice.
    ///
    /// # State merging (carina#3252)
    ///
    /// Each data-source binding carries two layers, in this order:
    ///
    /// 1. `DataSource.attributes` — the parsed DSL attributes (input
    ///    filters like `path_prefix` / `name_regex`, plus the parser's
    ///    `_type` marker).
    /// 2. `current_states[ds.id].attributes` — the **read result** the
    ///    provider returned: `arns`, `user_id`, `account_id`, etc.
    ///    Populated by `read_data_source_with_retry` (apply path) or
    ///    `resolve_data_source_refs_for_refresh` (plan path). Without
    ///    this layer the read attributes are missing from the binding
    ///    entirely; a downstream `ResourceRef` like
    ///    `admin_access_roles.arns` resolves to nothing and the
    ///    executor's `assert_fully_resolved` rejects it with the
    ///    misleading "add a `wait` block" message that does not apply
    ///    to data sources.
    ///
    /// Layer 2 wins on key collision — same direction as
    /// `record_applied` (post-apply, provider value is truth), but
    /// gated on `state.exists` the way [`Self::build_managed_core`]
    /// gates its own state merge (a tombstoned row must not
    /// contribute attributes). `record_applied` itself has no such
    /// gate because its caller only fires it on a successful apply
    /// result. The merge direction deliberately diverges from
    /// [`Self::build_managed_core`] (which is DSL-wins because the
    /// managed-resource DSL attributes *are* the user-authored desired
    /// state): a data source has no desired-state authoring, only a
    /// filter input and a provider-returned result, so the provider's
    /// value is the source of truth. The two layers don't typically
    /// share keys anyway (inputs are filters, outputs are produced
    /// values).
    ///
    /// The shared structural pattern with [`Self::build_managed_core`]
    /// is that *both* merge their `current_states` row into the
    /// binding — the carina#3252 gap was a data-source binding that
    /// dropped state entirely. The merge-direction divergence above is
    /// intentional.
    ///
    /// **Scope:** the only documented caller is
    /// [`ResolvedBindings::pre_apply`]. The function is `pub(crate)`
    /// so a new call site cannot recreate the carina#3252 gap by
    /// forgetting to pass `current_states`.
    pub(crate) fn layer_data_source_bindings(
        &mut self,
        data_sources: &[crate::resource::DataSource],
        current_states: &HashMap<ResourceId, State>,
    ) {
        for d in data_sources.iter() {
            let Some(binding_name) = d.binding.as_ref() else {
                continue;
            };
            let mut merged = crate::resource::attrs_to_hashmap(&d.attributes);
            if let Some(state) = current_states.get(&d.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    merged.insert(k.clone(), v.clone());
                }
            }
            self.by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: merged,
                    source: BindingValueSource::Local,
                },
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<&HashMap<String, Value>> {
        self.by_name.get(name).map(|b| &b.attributes)
    }

    pub fn source(&self, name: &str) -> Option<&BindingValueSource> {
        self.by_name.get(name).map(|b| &b.source)
    }

    /// Record (or refresh) a binding after a `Create` / `Update` effect
    /// returns its post-apply state.
    ///
    /// `resource_attrs` is the resolved DSL attribute map; `state` is
    /// what the provider just reported. **State wins on key collision** —
    /// the provider's freshly-returned values (e.g. an auto-assigned
    /// `id`) are by definition the source of truth for the binding's
    /// downstream consumers. This is the inverse of `pre_apply`'s
    /// "DSL wins" pre-apply rule: pre-apply we trust the DSL, post-apply
    /// we trust the provider.
    ///
    /// Replaces the executor's `update_binding_map` helper (#2300).
    pub fn record_applied(
        &mut self,
        binding: Option<&str>,
        resource_attrs: &HashMap<String, Value>,
        state: &State,
    ) {
        let Some(name) = binding else {
            return;
        };
        let mut merged = resource_attrs.clone();
        for (k, v) in &state.attributes {
            merged.insert(k.clone(), v.clone());
        }
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: merged,
                source: BindingValueSource::Local,
            },
        );
    }

    /// Set a binding directly to the given attribute map and source.
    /// Replaces any existing entry with the same name.
    ///
    /// Used by call sites that already hold a fully-resolved attribute
    /// map and don't need the `record_applied` state-merge path — for
    /// example, post-apply state writeback that hydrates bindings
    /// straight out of `StateFile.resources[].attributes`.
    ///
    /// The name is `set` rather than `insert` to avoid sounding like
    /// `HashMap::insert` (which returns the previous value); this method
    /// is fire-and-forget.
    pub fn set(&mut self, name: &str, attrs: HashMap<String, Value>, source: BindingValueSource) {
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: attrs,
                source,
            },
        );
    }

    /// Project this resolved view into the [`IterableBindings`] a
    /// deferred-for expansion consumes.
    ///
    /// Every entry's merged attribute map (local DSL ⊕ refreshed state,
    /// with upstream and wait-alias entries already folded in by
    /// [`Self::pre_apply`]) is exposed under its binding
    /// name. This is the *only* same-config-aware constructor of
    /// `IterableBindings`: a deferred-for iterable resolves against the
    /// exact post-refresh view every non-loop `ResourceRef` resolves
    /// against (carina#3132 — one resolution timing, no upstream-only
    /// carve-out). The projection is a verbatim clone of the merged
    /// maps; no new join logic.
    pub fn project_iterable_bindings(&self) -> IterableBindings {
        IterableBindings {
            by_binding: self
                .by_name
                .iter()
                .map(|(name, rb)| (name.clone(), rb.attributes.clone()))
                .collect(),
        }
    }
}

/// The set of every binding a deferred-for iterable may legally
/// reference: same-config `let` resources (post-refresh), `upstream_state`
/// data, and `wait` aliases — all merged.
///
/// Exists as a distinct newtype, rather than a bare
/// `HashMap<String, HashMap<String, Value>>`, so the input to
/// [`File::expand_deferred_for_expressions`] *names* its contract. Before
/// carina#3132 the expander took the raw map and the only caller passed
/// the upstream-only `remote_bindings`; a same-config `let cert`'s
/// refreshed `domain_validation_options` was therefore never in scope and
/// the loop stayed deferred forever. Routing construction through
/// [`ResolvedBindings::project_iterable_bindings`] (the merged view) —
/// or the explicitly-named [`Self::from_upstream_only`] for the
/// upstream-only unit tests — makes "iterable resolved against the wrong
/// map" unrepresentable.
#[derive(Debug, Default, Clone)]
pub struct IterableBindings {
    by_binding: HashMap<String, HashMap<String, Value>>,
}

impl IterableBindings {
    /// Construct from an upstream-only binding map.
    ///
    /// Named to make the upstream-only nature explicit at the call site:
    /// the runtime plan/apply paths must use
    /// [`ResolvedBindings::project_iterable_bindings`] so same-config
    /// `let` reads are in scope. This constructor exists for the
    /// carina-core parser tests that exercise the `upstream_state`
    /// iterable path with hand-built bindings.
    pub fn from_upstream_only(remote_bindings: HashMap<String, HashMap<String, Value>>) -> Self {
        Self {
            by_binding: remote_bindings,
        }
    }

    /// Resolve a binding's attribute map (the iterable's source).
    pub fn get(&self, binding: &str) -> Option<&HashMap<String, Value>> {
        self.by_binding.get(binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn vpc_schema() -> ResourceSchema {
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("name", AttributeType::string()))
            .attribute(AttributeSchema::new("cidr_block", AttributeType::string()))
    }

    fn registry_with_vpc() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.insert("aws", vpc_schema());
        r
    }

    #[test]
    fn build_indexes_named_let_binding() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        let entry = index.get("vpc").expect("vpc binding present");
        assert_eq!(entry.schema.resource_type, "ec2.Vpc");
        assert!(index.is_declared("vpc"));
        assert_eq!(index.len(), 1);
    }

    /// carina#3181 PR C: `read` (data-source) bindings must appear in the
    /// `BindingIndex` — `from_parsed` walks the typed `data_sources`
    /// slice, not just managed `resources`.
    #[test]
    fn build_indexes_data_source_binding() {
        let src = r#"
let existing = read aws.ec2.Vpc {
    name = "v"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut registry = SchemaRegistry::new();
        registry.insert("aws", vpc_schema().as_data_source());

        let index = BindingIndex::from_parsed(&parsed, &registry);
        let entry = index
            .get("existing")
            .expect("data-source binding present in index");
        assert_eq!(entry.schema.resource_type, "ec2.Vpc");
        assert!(index.is_declared("existing"));
    }

    #[test]
    fn build_skips_anonymous_resources() {
        // No `let` binding — anonymous resources never appear in the index
        // because they cannot be referenced by name.
        let src = r#"
aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.is_empty());
    }

    #[test]
    fn build_skips_bindings_with_unknown_schema() {
        // The Vpc binding exists but its schema is not registered. Callers
        // (validation / LSP) raise a separate "unknown resource type"
        // diagnostic, so `get` returns None — but `contains_name` still
        // says yes, so a "unknown binding" diagnostic is not double-fired
        // on top of the "unknown resource type" one.
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = SchemaRegistry::new();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("vpc").is_none());
        assert!(
            index.is_declared("vpc"),
            "binding declared in source must show up in `known_names`",
        );
    }

    #[test]
    fn build_includes_only_named_top_level_bindings_not_for_body_templates() {
        // For-body template resources never carry a `binding`, so they
        // must not appear in the index — the iter walks all resources
        // (top-level and for-body) for parity with `iter_all_resources`,
        // but the binding filter weeds out the unnamed ones.
        let src = r#"
let net = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

for _, n in some_iterable {
    aws.ec2.Vpc {
        name = n
        cidr_block = "10.0.0.0/16"
    }
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("net").is_some(), "named let binding indexed");
        assert!(
            !index.is_declared("n"),
            "for-body iteration variable is not a binding the index should know about",
        );
        // For-body templates carry parser-synthesised internal bindings
        // (used for address derivation) — those names never surfaced in
        // the pre-#2231 validation map and `BindingIndex::from_parsed`
        // preserves that contract by walking top-level only.
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn schemas_by_name_returns_borrowed_schemas() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let by_name = index.schemas_by_name();
        let schema = by_name.get("vpc").expect("vpc projection present");
        assert_eq!(schema.resource_type, "ec2.Vpc");
        // The projection borrows from the index — pointer-equal to
        // `index.get(...).schema`, which is the whole point.
        assert!(std::ptr::eq(*schema, index.get("vpc").unwrap().schema));
    }

    #[test]
    fn get_returns_none_for_unknown_name() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("missing").is_none());
    }
}

#[cfg(test)]
mod resolved_bindings_tests {
    use super::*;
    use crate::resource::{ConcreteValue, DataSource, Resource, ResourceId, State, Value};
    use std::collections::BTreeSet;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut r = Resource::new("test.resource", name);
        r.attributes = attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        r.binding = binding.map(|b| b.to_string());
        r
    }

    #[test]
    fn local_binding_carries_dsl_attributes() {
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".to_string()
            )))
        );
        assert_eq!(resolved.source("vpc"), Some(&BindingValueSource::Local));
    }

    #[test]
    fn local_binding_merges_state_attributes_when_dsl_missing_them() {
        let rid = ResourceId::new("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: true,
                attributes: vec![
                    (
                        "id".to_string(),
                        Value::Concrete(ConcreteValue::String("vpc-abc".to_string())),
                    ),
                    // conflicting key — DSL value should win
                    (
                        "cidr_block".to_string(),
                        Value::Concrete(ConcreteValue::String("WRONG".to_string())),
                    ),
                ]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-abc".to_string()
            ))),
            "state-only attribute must be merged in",
        );
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".to_string()
            ))),
            "DSL value must win when both sides define a key",
        );
    }

    #[test]
    fn upstream_state_binding_is_first_class() {
        // Upstream-state bindings have no `Resource` and no `ResourceSchema`,
        // which is the case `BindingIndex` cannot represent.
        let resources: Vec<Resource> = Vec::new();
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut network_attrs = HashMap::new();
        network_attrs.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
        );
        remote.insert("network".to_string(), network_attrs);

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("network").expect("upstream binding present");
        assert_eq!(
            attrs.get("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-123".to_string()
            )))
        );
        assert_eq!(
            resolved.source("network"),
            Some(&BindingValueSource::Upstream)
        );
    }

    #[test]
    fn unbound_resources_are_excluded() {
        let resources = vec![make_resource(
            "anonymous",
            None,
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        assert!(resolved.get("anonymous").is_none());
    }

    #[test]
    fn upstream_overrides_local_on_name_collision() {
        // The pre-#2299 resolver inserted remote bindings *after* local
        // ones, so upstream wins when both define the same name. This is
        // load-bearing behaviour — preserving it makes #2299 a no-op for
        // configurations that already rely on it.
        let resources = vec![make_resource(
            "my-local",
            Some("shared"),
            vec![(
                "kind",
                Value::Concrete(ConcreteValue::String("local".to_string())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote.insert(
            "shared".to_string(),
            vec![(
                "kind".to_string(),
                Value::Concrete(ConcreteValue::String("upstream".to_string())),
            )]
            .into_iter()
            .collect(),
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        let attrs = resolved.get("shared").expect("shared binding present");
        assert_eq!(
            attrs.get("kind"),
            Some(&Value::Concrete(ConcreteValue::String(
                "upstream".to_string()
            ))),
            "upstream binding must override local one with the same name",
        );
        assert_eq!(
            resolved.source("shared"),
            Some(&BindingValueSource::Upstream),
        );
    }

    #[test]
    fn destroyed_state_does_not_contribute_attributes() {
        // `State.exists = false` represents a tombstone (resource was
        // destroyed since the last apply). The resolver must not merge
        // its attributes — they describe a no-longer-real resource.
        let rid = ResourceId::new("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: false,
                attributes: vec![(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-stale".to_string())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert!(
            attrs.get("id").is_none(),
            "tombstoned state must not contribute attributes",
        );
    }

    #[test]
    fn record_applied_inserts_state_winning_merge() {
        // Post-apply semantics: the freshly returned `State.attributes`
        // must override the DSL `resource_attrs` on conflict — the
        // provider just told us the real value (e.g. an auto-assigned
        // `id` differs from the placeholder, or `arn` materialises). The
        // executor relied on this in `update_binding_map`, so the
        // `ResolvedBindings` API has to preserve it byte-for-byte.
        let mut resolved = ResolvedBindings::default();
        let resource_attrs: HashMap<String, Value> = vec![
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-dsl".to_string())),
            ),
            (
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            ),
        ]
        .into_iter()
        .collect();
        let state = State {
            id: ResourceId::new("test.resource", "my-vpc"),
            identifier: None,
            exists: true,
            attributes: vec![
                // overrides DSL value
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-applied".to_string())),
                ),
                // adds a state-only key
                (
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-abc".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
            dependency_bindings: BTreeSet::new(),
        };

        resolved.record_applied(Some("vpc"), &resource_attrs, &state);

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-applied".to_string()
            ))),
            "state must win over resource_attrs on conflict",
        );
        assert_eq!(
            attrs.get("id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-abc".to_string()
            )))
        );
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".to_string()
            ))),
        );
        assert_eq!(resolved.source("vpc"), Some(&BindingValueSource::Local));
    }

    #[test]
    fn record_applied_with_no_binding_is_a_noop() {
        let mut resolved = ResolvedBindings::default();
        let attrs: HashMap<String, Value> = HashMap::new();
        let state = State {
            id: ResourceId::new("test.resource", "anon"),
            identifier: None,
            exists: true,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        };

        resolved.record_applied(None, &attrs, &state);
        assert!(resolved.get("anon").is_none());
    }

    #[test]
    fn cloned_view_does_not_share_storage_with_original() {
        // The phased / replace executor paths snapshot the binding map
        // for cascade frames: `local_binding_map = binding_snapshot.clone()`.
        // After #2300 the same pattern relies on `ResolvedBindings: Clone`,
        // and the clone must be independent so frame-local mutations
        // don't leak back into the parent snapshot.
        let resources = vec![make_resource(
            "vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let parent = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            data_sources: &[],
            current_states: &states,
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let mut child = parent.clone();
        let extra_state = State {
            id: ResourceId::new("test.resource", "subnet"),
            identifier: None,
            exists: true,
            attributes: vec![(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("subnet-1".to_string())),
            )]
            .into_iter()
            .collect(),
            dependency_bindings: BTreeSet::new(),
        };
        child.record_applied(Some("subnet"), &HashMap::new(), &extra_state);

        assert!(child.get("subnet").is_some());
        assert!(
            parent.get("subnet").is_none(),
            "child mutation must not leak into the parent snapshot",
        );
    }

    #[test]
    fn set_replaces_entry_and_records_source() {
        // `set` is the no-merge counterpart to `record_applied` — used
        // when the caller already holds a fully-resolved attribute map
        // (e.g. post-apply state-writeback hydrating exports). Verify
        // it overwrites any existing entry and records the given source.
        let mut resolved = ResolvedBindings::default();
        let initial: HashMap<String, Value> = vec![(
            "kind".to_string(),
            Value::Concrete(ConcreteValue::String("first".to_string())),
        )]
        .into_iter()
        .collect();
        resolved.set("registry", initial, BindingValueSource::Upstream);
        assert_eq!(
            resolved.source("registry"),
            Some(&BindingValueSource::Upstream)
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::Concrete(ConcreteValue::String("first".to_string())))
        );

        let replacement: HashMap<String, Value> = vec![(
            "kind".to_string(),
            Value::Concrete(ConcreteValue::String("second".to_string())),
        )]
        .into_iter()
        .collect();
        resolved.set("registry", replacement, BindingValueSource::Local);
        assert_eq!(
            resolved.source("registry"),
            Some(&BindingValueSource::Local),
            "set must replace the source as well as the attributes",
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::Concrete(ConcreteValue::String(
                "second".to_string()
            )))
        );
    }

    // ---- carina#3085: wait-binding passthrough alias ----

    fn wait_spec(binding: &str, target: &str) -> WaitAliasSpec {
        WaitAliasSpec {
            binding: BindingName::new(binding),
            target: BindingName::new(target),
        }
    }

    /// Test plan item 1: a wait binding resolves to its target's
    /// attribute map, sourced as `WaitAlias { target }`.
    #[test]
    fn wait_alias_resolves_to_target_attributes() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:1:certificate/abc".to_string(),
                )),
            )],
        );
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        assert_eq!(
            resolved
                .get("cert_issued")
                .and_then(|a| a.get("certificate_arn")),
            Some(&Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:us-east-1:1:certificate/abc".to_string()
            ))),
            "cert_issued.certificate_arn must passthrough to cert's value"
        );
        assert_eq!(
            resolved.source("cert_issued"),
            Some(&BindingValueSource::WaitAlias {
                target: BindingName::new("cert")
            }),
            "source must record the wait→target edge, not flatten to Local"
        );
    }

    /// Test plan item 1 (negative): a wait whose target has no entry
    /// (typo / scoped-out) creates no alias — the ref stays unresolved
    /// and the existing PlanError path (not a panic) surfaces it.
    #[test]
    fn wait_alias_absent_target_creates_no_entry() {
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "nonexistent")],
        });
        assert!(
            resolved.get("cert_issued").is_none(),
            "no alias when target is absent (ref stays unresolved → existing PlanError)"
        );
    }

    /// The target may be an upstream binding, not just a resource —
    /// the alias must still mirror it (aliases materialise after both
    /// local and upstream entries exist).
    #[test]
    fn wait_alias_target_can_be_upstream() {
        let mut remote = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "id".to_string(),
            Value::Concrete(ConcreteValue::String("up-1".to_string())),
        );
        remote.insert("up".to_string(), attrs);
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &remote,
            wait_aliases: &[wait_spec("waited", "up")],
        });
        assert_eq!(
            resolved.get("waited").and_then(|a| a.get("id")),
            Some(&Value::Concrete(ConcreteValue::String("up-1".to_string())))
        );
    }

    /// Test plan item 5: the alias is an independent snapshot — a
    /// later `set("cert", …)` write-back must not mutate the alias,
    /// and vice versa (design: read-time snapshot).
    #[test]
    fn wait_alias_is_independent_snapshot() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String("arn:old".to_string())),
            )],
        );
        let mut resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        let mut new_attrs = HashMap::new();
        new_attrs.insert(
            "certificate_arn".to_string(),
            Value::Concrete(ConcreteValue::String("arn:new".to_string())),
        );
        resolved.set("cert", new_attrs, BindingValueSource::Local);
        assert_eq!(
            resolved
                .get("cert_issued")
                .and_then(|a| a.get("certificate_arn")),
            Some(&Value::Concrete(ConcreteValue::String(
                "arn:old".to_string()
            ))),
            "wait alias is a snapshot: a later set('cert', …) must not mutate it"
        );
    }

    /// Design Risks (multiple downstream consumers): the single wait
    /// alias entry resolves consistently for every consumer that reads
    /// it — there is one `cert_issued` entry, so N downstream resources
    /// referencing `cert_issued.*` all see the same value. (The single
    /// `Effect::Wait` fan-out to all consumers is the dependency-graph
    /// machinery's job, unchanged by this value-layer alias and
    /// covered by the wait_downstream_apply E2E.)
    #[test]
    fn wait_alias_resolves_consistently_for_repeated_lookups() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String("arn:shared".to_string())),
            )],
        );
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        // Two independent consumers both resolve the same value.
        let a = resolved
            .get("cert_issued")
            .and_then(|m| m.get("certificate_arn"))
            .cloned();
        let b = resolved
            .get("cert_issued")
            .and_then(|m| m.get("certificate_arn"))
            .cloned();
        assert_eq!(a, b);
        assert_eq!(
            a,
            Some(Value::Concrete(ConcreteValue::String(
                "arn:shared".to_string()
            )))
        );
    }

    /// carina#3252: a `read aws.X` data-source binding must expose the
    /// attributes that `read_data_source_with_retry` wrote into
    /// `current_states[ds.id]`, not just the DSL-side input filters.
    ///
    /// Pre-fix: `layer_data_sources_post_apply` recorded only
    /// `DataSource.attributes` (the `path_prefix` / `name_regex` filters),
    /// so a downstream managed resource referencing
    /// `admin_access_roles.arns` saw nothing in the binding map and the
    /// executor's `assert_fully_resolved` rejected the unresolved
    /// `ResourceRef` with the misleading "add a `wait` block" message.
    ///
    /// The fix mirrors `build_managed_core`: pre-apply layering for a
    /// data-source binding must merge the read-result `State.attributes`
    /// (the actual output: `arns`, `user_id`, ...) on top of the DSL
    /// input map. Read-result wins on key collision — the input map is
    /// just the filter set the provider was *given*; the state map is
    /// what the provider *returned*.
    #[test]
    fn data_source_binding_merges_state_attributes() {
        let ds_id = ResourceId::new("aws.iam.Roles", "admin_access_roles");
        let mut ds = DataSource::new("aws.iam.Roles", "admin_access_roles");
        ds.id = ds_id.clone();
        ds.binding = Some("admin_access_roles".to_string());
        ds.attributes.insert(
            "path_prefix".to_string(),
            Value::Concrete(ConcreteValue::String(
                "/aws-reserved/sso.amazonaws.com/".to_string(),
            )),
        );
        ds.attributes.insert(
            "name_regex".to_string(),
            Value::Concrete(ConcreteValue::String(
                "^AWSReservedSSO_AdministratorAccess_[0-9a-f]{16}$".to_string(),
            )),
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State {
                id: ds_id,
                identifier: None,
                exists: true,
                attributes: vec![(
                    "arns".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::String(
                            "arn:aws:iam::111111111111:role/aws-reserved/sso.amazonaws.com/AWSReservedSSO_AdministratorAccess_abcdef0123456789".to_string(),
                        )),
                    ])),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[ds],
            current_states: &states,
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });

        let attrs = resolved
            .get("admin_access_roles")
            .expect("data-source binding present");
        // The DSL-input filter survives — useful for plan-display refs
        // that name the input field.
        assert!(
            attrs.get("path_prefix").is_some(),
            "DSL input filter must remain visible in the binding",
        );
        // The read result must be visible so downstream resource refs
        // (e.g. `role.assume_role_policy_document = admin_access_roles.arns`)
        // resolve to a concrete `List<String>` at apply time.
        let arns = attrs
            .get("arns")
            .expect("read-state attribute `arns` must be visible on the binding");
        match arns {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items.len(), 1, "one role arn returned");
            }
            other => panic!("expected concrete List, got {:?}", other),
        }
        assert_eq!(
            resolved.source("admin_access_roles"),
            Some(&BindingValueSource::Local)
        );
    }

    /// carina#3252 follow-up: when the data-source DSL input map and the
    /// read state share a key, the read state must win — it is the value
    /// the provider returned. Mirrors `record_applied`'s "state wins"
    /// rule for managed resources. The two maps don't normally collide
    /// (inputs are filters, outputs are produced values), but a provider
    /// is allowed to echo an input back into its state and the binding
    /// view must surface the post-read value, not the pre-read filter.
    #[test]
    fn data_source_binding_state_wins_on_key_collision() {
        let ds_id = ResourceId::new("aws.example.Echo", "echo");
        let mut ds = DataSource::new("aws.example.Echo", "echo");
        ds.id = ds_id.clone();
        ds.binding = Some("echo".to_string());
        ds.attributes.insert(
            "filter".to_string(),
            Value::Concrete(ConcreteValue::String("dsl-input".to_string())),
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State {
                id: ds_id,
                identifier: None,
                exists: true,
                attributes: vec![(
                    "filter".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-result".to_string())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[ds],
            current_states: &states,
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        let attrs = resolved.get("echo").expect("echo binding present");
        assert_eq!(
            attrs.get("filter"),
            Some(&Value::Concrete(ConcreteValue::String(
                "provider-result".to_string()
            ))),
            "read-state attribute must override DSL input on collision",
        );
    }

    /// carina#3252: cover the no-DSL-input shape (e.g.
    /// `read aws.sts.CallerIdentity {}` — failure path (b)/(c) in the
    /// issue comment). The DSL attribute map is empty; the binding's
    /// entire visible content comes from `current_states[ds.id]`. A
    /// regression that re-broke `current_states` merging would still
    /// pass [`data_source_binding_merges_state_attributes`] because that
    /// test also asserts the DSL filter survives — this test does not,
    /// so it pins the state-only variant.
    #[test]
    fn data_source_binding_with_no_dsl_inputs_exposes_state_only() {
        let ds_id = ResourceId::with_provider("aws", "sts.CallerIdentity", "caller", None);
        let mut ds = DataSource::with_provider("aws", "sts.CallerIdentity", "caller", None);
        ds.id = ds_id.clone();
        ds.binding = Some("caller".to_string());
        assert!(
            ds.attributes.is_empty(),
            "this test covers the no-DSL-input shape",
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State::existing(
                ds_id,
                vec![(
                    "account_id".to_string(),
                    Value::Concrete(ConcreteValue::String("111111111111".to_string())),
                )]
                .into_iter()
                .collect(),
            ),
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            data_sources: &[ds],
            current_states: &states,
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        let attrs = resolved.get("caller").expect("caller binding present");
        assert_eq!(
            attrs.get("account_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "111111111111".to_string()
            ))),
            "with no DSL inputs, the binding's content is entirely the \
             read state's attributes",
        );
    }
}

#[cfg(test)]
mod binding_name_set_tests {
    use super::*;
    use crate::parser::{ParsedFile, parse};

    fn parsed_with(src: &str) -> ParsedFile {
        parse(src, &Default::default()).expect("parse")
    }

    #[test]
    fn records_resource_let_bindings_as_resource_kind() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("vpc"));
        assert_eq!(names.kind("vpc"), Some(&BindingNameKind::Resource));
    }

    #[test]
    fn records_upstream_state_with_upstream_kind() {
        let src = r#"
let network = upstream_state {
    source = "../network"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("network"));
        assert_eq!(names.kind("network"), Some(&BindingNameKind::UpstreamState));
    }

    #[test]
    fn records_top_level_let_value_with_variable_kind() {
        // `let <name> = <Value>` (non-resource RHS) is captured under
        // `parsed.variables` and surfaces as `Variable` in the name set.
        let src = r#"
let region = "ap-northeast-1"
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("region"));
        assert_eq!(names.kind("region"), Some(&BindingNameKind::Variable));
    }

    #[test]
    fn records_arguments_block_entries_with_argument_kind() {
        let src = r#"
arguments {
    env: String
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("env"));
        assert_eq!(names.kind("env"), Some(&BindingNameKind::Argument));
    }

    #[test]
    fn records_fn_def_with_user_function_kind() {
        let src = r#"
fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("double"));
        assert_eq!(names.kind("double"), Some(&BindingNameKind::UserFunction));
    }

    #[test]
    fn records_module_call_with_module_call_kind() {
        let src = r#"
let cluster = module {
    source = "../cluster"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(
            names.contains("cluster"),
            "module-call binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("cluster"), Some(&BindingNameKind::ModuleCall));
    }

    #[test]
    fn matches_collect_known_bindings_merged_byte_for_byte() {
        // BindingNameSet is the canonical replacement for
        // `collect_known_bindings_merged`. The set of names must be
        // identical so the replacement is a pure refactor.
        let src = r#"
arguments {
    env: String
}

let region = "ap-northeast-1"

let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

let network = upstream_state {
    source = "../network"
}

fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);
        let legacy = crate::parser::collect_known_bindings_merged(&parsed);

        let from_set: std::collections::HashSet<&str> = names.iter_names().collect();
        let legacy_set: std::collections::HashSet<&str> = legacy.into_iter().collect();
        assert_eq!(from_set, legacy_set);
    }

    #[test]
    fn structural_binding_is_in_scope_but_not_addressable_by_resource_ref() {
        // The pre-#2301 invariant: when a `let` binds the result of an
        // `if` / `for` / `read` expression, the parser flags the
        // binding `is_structural` and records the name in
        // `parsed.structural_bindings` (so unused-binding warnings
        // don't fire). Such names are visible to scope checks but not
        // surfaced through `ResolvedBindings` — the executor /
        // resolver address bindings by their attribute map, and a
        // structural binding's "value" is the entire if/for branch,
        // not an attribute set.
        let src = r#"
let chosen = if true { "primary" } else { "fallback" }
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        // Scope-side: `chosen` is in scope as a Structural kind.
        assert!(
            names.contains("chosen"),
            "structural binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("chosen"), Some(&BindingNameKind::Structural));

        // Value-side: `ResolvedBindings` does NOT carry an entry for
        // `chosen`, so a `ResourceRef` to `chosen.foo` cannot resolve.
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &parsed.resources,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        assert!(
            resolved.get("chosen").is_none(),
            "structural bindings must stay invisible to ResourceRef resolution",
        );
    }

    /// carina#3085 Test plan item 2: a `wait` binding registers as
    /// `BindingNameKind::Wait { target }` and — unlike `Structural` —
    /// **is** addressable (`contains` is true), because
    /// `<wait-binding>.<attr>` is a passthrough to its target.
    #[test]
    fn wait_binding_registers_as_wait_kind_and_is_addressable() {
        let src = r#"
let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}

let cert_issued = wait cert {
    until = cert.status == aws.acm.Certificate.Status.Issued
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(
            names.contains("cert_issued"),
            "a wait binding is addressable (passthrough), unlike Structural; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(
            names.kind("cert_issued"),
            Some(&BindingNameKind::Wait {
                target: BindingName::new("cert")
            }),
            "wait binding must carry its typed target edge"
        );
        // The target itself is still a plain resource binding.
        assert_eq!(names.kind("cert"), Some(&BindingNameKind::Resource));
    }
}
