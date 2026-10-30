//! Identifier and prefix handling for resources
//!
//! Functions for generating random suffixes, resolving attribute prefixes,
//! reconciling prefixed names with state, and computing anonymous resource identifiers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::effect::ResolvedMove;
use crate::parser::ProviderConfig;
use crate::parser::{SourceSpan, StateBlock, StateBlockAddress};
use crate::provider::ProviderFactory;
use crate::resource::{
    AnonymousIdentityModuleSource, ConcreteValue, DataSource, DeferredValue, LegacyLoopAddress,
    LoopElementOrigin, PendingReRead, ResolvedResourceId, Resource, ResourceId, ResourceIdentity,
    UnknownReason, Value, ValueString,
};
use crate::schema::{ResourceSchema, SchemaRegistry};
use crate::validation::is_string_compatible_type;
use crate::value::{CanonicalizedProviderConfigs, CanonicalizedResources};

/// Addresses claimed by operator-written state blocks. Heuristic move
/// detectors must not consume a claimed `from` state entry nor synthesize a
/// move onto a claimed `to` name — operator intent always wins over
/// similarity heuristics.
///
/// `from` claims: `moved.from`, `removed.from`.
/// `to` claims: `moved.to`, `import.to`.
#[derive(Debug, Clone, Default)]
pub struct StateBlockClaims {
    from: HashSet<StateBlockAddress>,
    to: HashSet<StateBlockAddress>,
}

/// State entries screened against state-block `from` claims for one provider and
/// resource type, retaining both the complete entry set and the safe heuristic
/// candidate subset.
pub struct ScreenedStateEntries<T> {
    entries: Vec<ScreenedStateEntry<T>>,
}

struct ScreenedStateEntry<T> {
    value: T,
    claimed: bool,
    candidate_eligible: bool,
}

impl<T> ScreenedStateEntries<T> {
    pub fn candidates(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().filter_map(|entry| {
            (!entry.claimed && entry.candidate_eligible).then_some(&entry.value)
        })
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|entry| &entry.value)
    }

    pub(crate) fn claimed(&self) -> impl Iterator<Item = &T> {
        self.entries
            .iter()
            .filter_map(|entry| entry.claimed.then_some(&entry.value))
    }

    /// Retain entries for exact-address existence checks while excluding
    /// entries that do not carry enough state to participate in heuristics.
    pub fn exclude_candidates(mut self, excluded: impl Fn(&T) -> bool) -> Self {
        for entry in &mut self.entries {
            if excluded(&entry.value) {
                entry.candidate_eligible = false;
            }
        }
        self
    }
}

impl StateBlockClaims {
    pub fn new(from: HashSet<StateBlockAddress>, to: HashSet<StateBlockAddress>) -> Self {
        Self { from, to }
    }

    // Test-only convenience. Production call sites must resolve operator
    // claims from parsed state blocks instead of defaulting to no claims.
    #[doc(hidden)]
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn screen_entries<T>(
        &self,
        provider: &str,
        resource_type: &str,
        entries: impl IntoIterator<Item = T>,
        name_of: impl Fn(&T) -> &str,
    ) -> ScreenedStateEntries<T> {
        ScreenedStateEntries {
            entries: entries
                .into_iter()
                .map(|entry| {
                    let claimed = ResourceIdentity::try_from(name_of(&entry).to_owned()).is_ok_and(
                        |identity| {
                            let resource_id = ResourceId::with_provider_identity(
                                provider.to_owned(),
                                resource_type.to_owned(),
                                identity,
                                None,
                            );
                            self.claims_from(&resource_id)
                        },
                    );
                    ScreenedStateEntry {
                        value: entry,
                        claimed,
                        candidate_eligible: true,
                    }
                })
                .collect(),
        }
    }

    /// Whether an operator-written state block owns this persisted source
    /// address. Projected state views use this to keep the source claim after
    /// presenting the row under a moved-to address.
    pub fn claims_from(&self, resource_id: &ResourceId) -> bool {
        self.from
            .iter()
            .any(|address| address.matches_resource_id(resource_id))
    }

    pub fn claims_to(&self, resource_id: &ResourceId) -> bool {
        self.to
            .iter()
            .any(|address| address.matches_resource_id(resource_id))
    }
}

/// Generate a random 8-character lowercase hex suffix using UUID v4.
pub fn generate_random_suffix() -> String {
    let uuid = uuid::Uuid::new_v4();
    let hex = uuid.as_simple().to_string();
    hex[..8].to_string()
}

/// Whether an anonymous resource's generated identity is anchored in schema
/// fields that cannot change in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonymousIdentityBasis {
    /// At least one configured create-only or schema identity attribute is
    /// set, so the normal identifier hash is based on immutable input.
    Stable,
    /// No stable schema attribute is set, so the identifier is derived from
    /// mutable user attributes.
    AttributeDerived,
}

/// Return the immutable schema inputs that participate in an anonymous
/// resource's standard identity hash.
///
/// This is the single definition of both "stable identity basis" and the
/// bytes hashed for that basis. An unresolved `<attribute>_prefix` is treated
/// identically to the same prefix after [`resolve_attr_prefixes`] moves it into
/// [`Resource::prefixes`].
fn stable_anonymous_hash_inputs<S>(
    resource: &Resource<S>,
    schema: &ResourceSchema,
) -> BTreeMap<String, String> {
    let mut inputs = BTreeMap::new();
    for attribute in schema
        .create_only_attributes()
        .into_iter()
        .chain(schema.identity_attributes())
    {
        if inputs.contains_key(attribute) {
            continue;
        }

        let hash_value = if let Some(prefix) = resource.prefixes.get(attribute) {
            Some(format!("Prefix({prefix:?})"))
        } else {
            let unresolved_prefix = format!("{attribute}_prefix");
            resource
                .attributes
                .get(&unresolved_prefix)
                .map(|value| match value {
                    Value::Concrete(ConcreteValue::String(prefix)) => {
                        format!("Prefix({:?})", prefix.expose_plaintext())
                    }
                    value => format!("Prefix({})", deterministic_value_string(value)),
                })
                .or_else(|| resource.get_attr(attribute).map(deterministic_value_string))
        };

        if let Some(hash_value) = hash_value {
            inputs.insert(attribute.to_string(), hash_value);
        }
    }
    inputs
}

/// Classify the identity basis for an anonymous resource.
///
/// Returns `None` when the resource has no schema, so its identity basis is
/// unknown. Otherwise, presence is what matters here, not whether a value is
/// concrete yet. A deferred create-only or identity value will become
/// concrete later but is still an immutable identity input.
pub fn classify_anonymous_identity_basis<S>(
    resource: &Resource<S>,
    registry: &SchemaRegistry,
) -> Option<AnonymousIdentityBasis> {
    let schema = registry.get_for(resource)?;
    let schema = schema.as_schema();

    if stable_anonymous_hash_inputs(resource, schema).is_empty() {
        Some(AnonymousIdentityBasis::AttributeDerived)
    } else {
        Some(AnonymousIdentityBasis::Stable)
    }
}

#[derive(Debug, Clone)]
struct AnonymousResourceKind {
    provider: String,
    resource_type: String,
    provider_instance: Option<String>,
    module_scope_key: Option<String>,
    module_scope_description: Option<String>,
}

impl PartialEq for AnonymousResourceKind {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for AnonymousResourceKind {}

impl PartialOrd for AnonymousResourceKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AnonymousResourceKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            &self.provider,
            &self.resource_type,
            &self.provider_instance,
            &self.module_scope_key,
        )
            .cmp(&(
                &other.provider,
                &other.resource_type,
                &other.provider_instance,
                &other.module_scope_key,
            ))
    }
}

impl AnonymousResourceKind {
    fn of<S: AnonymousIdentityModuleSource>(resource: &Resource<S>) -> Self {
        let module_scope = resource
            .module_source
            .as_ref()
            .and_then(AnonymousIdentityModuleSource::anonymous_scope);
        Self {
            provider: resource.id.provider.clone(),
            resource_type: resource.id.resource_type.clone(),
            provider_instance: resource.id.provider_instance.clone(),
            module_scope_key: module_scope.as_ref().map(|(key, _)| key.clone()),
            module_scope_description: module_scope.map(|(_, description)| description),
        }
    }

    fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }
}

/// A group of anonymous resources whose identities would all be derived from
/// mutable attributes within the same ownership scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeDerivedAnonymousResourceConflict {
    kind: AnonymousResourceKind,
}

impl AttributeDerivedAnonymousResourceConflict {
    /// Return whether `resource` is one of the declarations represented by
    /// this conflict. This lets source-aware callers attach the shared error to
    /// each declaration without reimplementing the grouping rule.
    pub fn includes<S: AnonymousIdentityModuleSource>(
        &self,
        resource: &Resource<S>,
        registry: &SchemaRegistry,
    ) -> bool {
        resource.binding.is_none()
            && classify_anonymous_identity_basis(resource, registry)
                == Some(AnonymousIdentityBasis::AttributeDerived)
            && AnonymousResourceKind::of(resource) == self.kind
    }
}

impl std::fmt::Display for AttributeDerivedAnonymousResourceConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let display_type = self.kind.display_type();
        let provider_instance = self
            .kind
            .provider_instance
            .as_deref()
            .unwrap_or("<default>");
        let scope = self
            .kind
            .module_scope_description
            .as_deref()
            .unwrap_or("the root scope");
        write!(
            f,
            "Anonymous resource identity is derived from mutable attributes for multiple \
             '{display_type}' declarations using provider instance '{provider_instance}' in \
             {scope}. Use `let` bindings to give them distinct stable identities."
        )
    }
}

impl std::error::Error for AttributeDerivedAnonymousResourceConflict {}

/// Why one authored unbound loop cannot receive anonymous identities safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopConflictReason {
    AttributeDerived,
    IdenticalIdentityInputs,
}

#[derive(Debug, Clone)]
struct LoopRewriteMove {
    provider: String,
    resource_type: String,
    from: LegacyLoopAddress,
    to: String,
}

/// A hard anonymous-identity error that can be repaired by binding one loop.
#[derive(Debug, Clone)]
pub struct LoopAnonymousIdentityConflict {
    reason: LoopConflictReason,
    origin: LoopElementOrigin,
    resource_type: String,
    suggested_binding: String,
    moves: Vec<LoopRewriteMove>,
}

impl LoopAnonymousIdentityConflict {
    pub fn file(&self) -> &str {
        self.origin.file()
    }

    pub fn line(&self) -> usize {
        self.origin.line()
    }

    pub fn source_span(&self) -> Option<crate::parser::SourceSpan> {
        self.origin.source().source_span()
    }
}

impl std::fmt::Display for LoopAnonymousIdentityConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.reason {
            LoopConflictReason::AttributeDerived => "derive identity from mutable attributes",
            LoopConflictReason::IdenticalIdentityInputs => "have identical stable identity inputs",
        };
        write!(
            formatter,
            "Anonymous resources produced by the unbound `{}` at {}:{} cannot be identified \
             safely because multiple '{}' elements {reason}.",
            self.origin.kind().loop_kind(),
            self.file(),
            self.line(),
            self.resource_type,
        )?;

        let Some(source) = self.origin.source().source_text() else {
            return Ok(());
        };
        write!(
            formatter,
            "\nRewrite the loop with a binding and migrate its existing state addresses:\n\n\
             let {} = {}",
            self.suggested_binding, source,
        )?;
        for migration in &self.moves {
            let display_type = if migration.provider.is_empty() {
                migration.resource_type.clone()
            } else {
                format!("{}.{}", migration.provider, migration.resource_type)
            };
            write!(
                formatter,
                "\n\nmoved {{\n  from = {} {}\n  to = {} {}\n}}",
                display_type,
                crate::parser::display_string_literal(migration.from.exact_state_name()),
                display_type,
                crate::parser::display_string_literal(&migration.to),
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for LoopAnonymousIdentityConflict {}

/// A generated anonymous identifier shared by declarations that cannot be
/// repaired as one authored loop rewrite.
#[derive(Debug, Clone)]
pub struct AnonymousResourceIdentifierCollision {
    first_type: String,
    second_type: String,
    identifier: String,
}

impl AnonymousResourceIdentifierCollision {
    /// Generated identity shared by the conflicting declarations.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }
}

impl std::fmt::Display for AnonymousResourceIdentifierCollision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Anonymous resource identifier collision: '{}' and '{}' produce the same identifier \
             '{}'. Use `let` bindings to give them distinct names.",
            self.first_type, self.second_type, self.identifier,
        )
    }
}

impl std::error::Error for AnonymousResourceIdentifierCollision {}

/// Typed failures from anonymous identity assignment.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AnonymousIdentityError {
    #[error(transparent)]
    AttributeDerived(Box<AttributeDerivedAnonymousResourceConflict>),
    #[error(transparent)]
    Collision(#[from] AnonymousResourceIdentifierCollision),
    #[error(transparent)]
    Loop(Box<LoopAnonymousIdentityConflict>),
    #[error(transparent)]
    LegacyMoveTarget(Box<LegacyLoopMoveTargetError>),
    #[error(transparent)]
    Aggregate(AnonymousIdentityErrors),
}

/// Deterministically ordered anonymous-identity failures discovered in one
/// assignment pass.
#[derive(Debug, Clone)]
pub struct AnonymousIdentityErrors(Vec<AnonymousIdentityError>);

impl std::fmt::Display for AnonymousIdentityErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n\n")?;
            }
            std::fmt::Display::fmt(error, formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for AnonymousIdentityErrors {}

impl AnonymousIdentityError {
    fn from_errors(mut errors: Vec<Self>) -> Self {
        debug_assert!(!errors.is_empty());
        if errors.len() == 1 {
            errors.pop().expect("one anonymous identity error")
        } else {
            Self::Aggregate(AnonymousIdentityErrors(errors))
        }
    }

    /// Flatten an aggregate into the typed failures it contains. Single
    /// failures are returned unchanged so adapters can report every authored
    /// conflict without parsing display text.
    pub fn into_individual_errors(self) -> Vec<Self> {
        match self {
            Self::Aggregate(errors) => errors
                .0
                .into_iter()
                .flat_map(Self::into_individual_errors)
                .collect(),
            error => vec![error],
        }
    }
}

/// Failure at either typed-input boundary of anonymous identity assignment.
#[derive(Debug, thiserror::Error)]
pub enum AnonymousIdentityComputationError {
    #[error(transparent)]
    ProviderConfig(#[from] crate::value::ProviderConfigCanonicalizationError),
    #[error(transparent)]
    Identity(#[from] AnonymousIdentityError),
}

/// Which anonymous-identity conflicts are policy errors at this call site.
///
/// Generated identifier collisions are representational failures and remain
/// errors under both policies. Recovery commands may permit the broader
/// mutable-attribute ambiguity so broken configuration cannot block destroy
/// or state repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonymousIdentityConflictPolicy {
    RejectAttributeDerived,
    RepresentabilityOnly,
}

impl AnonymousIdentityConflictPolicy {
    const fn rejects_attribute_derived(self) -> bool {
        matches!(self, Self::RejectAttributeDerived)
    }
}

/// Canonicalize provider identity inputs and assign anonymous resource IDs
/// through the same factory lookup on every plan, editor, and apply path.
pub fn compute_anonymous_identifiers_with_factories<S: AnonymousIdentityModuleSource>(
    resources: CanonicalizedResources<'_, S>,
    providers: &[ProviderConfig],
    factories: &[Box<dyn ProviderFactory>],
    registry: &SchemaRegistry,
    conflict_policy: AnonymousIdentityConflictPolicy,
) -> Result<(), AnonymousIdentityComputationError> {
    let providers =
        crate::value::canonicalize_provider_configs_with_schemas(providers, &|provider_name| {
            crate::provider::find_factory(factories, provider_name)
                .map(ProviderFactory::provider_config_attribute_types)
        })?;
    compute_anonymous_identifiers_with_provider_configs(
        resources,
        &providers,
        registry,
        conflict_policy,
        &|provider_name| {
            crate::provider::find_factory(factories, provider_name)
                .map(|factory| {
                    factory
                        .identity_attributes()
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        },
    )?;
    Ok(())
}

impl From<AttributeDerivedAnonymousResourceConflict> for AnonymousIdentityError {
    fn from(error: AttributeDerivedAnonymousResourceConflict) -> Self {
        Self::AttributeDerived(Box::new(error))
    }
}

impl From<LoopAnonymousIdentityConflict> for AnonymousIdentityError {
    fn from(error: LoopAnonymousIdentityConflict) -> Self {
        Self::Loop(Box::new(error))
    }
}

impl From<LegacyLoopMoveTargetError> for AnonymousIdentityError {
    fn from(error: LegacyLoopMoveTargetError) -> Self {
        Self::LegacyMoveTarget(Box::new(error))
    }
}

/// An operator-written move whose destination is one of Carina's historical
/// synthetic loop identities.
#[derive(Debug, Clone)]
pub struct LegacyLoopMoveTargetError {
    target: StateBlockAddress,
    replacement: Option<StateBlockAddress>,
    destination: StateBlockDestination,
    source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Copy)]
enum StateBlockDestination {
    Import,
    Moved,
}

impl StateBlockDestination {
    const fn field_name(self) -> &'static str {
        match self {
            Self::Import => "import.to",
            Self::Moved => "moved.to",
        }
    }
}

impl LegacyLoopMoveTargetError {
    pub fn source_span(&self) -> Option<SourceSpan> {
        self.source_span
    }
}

fn display_state_block_address(address: &StateBlockAddress) -> String {
    format!(
        "{} {}",
        address.display_type(),
        crate::parser::display_string_literal(address.name_str()),
    )
}

impl std::fmt::Display for LegacyLoopMoveTargetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "The `{}` address {} is a legacy synthetic loop address and cannot be a \
             destination.",
            self.destination.field_name(),
            display_state_block_address(&self.target),
        )?;
        if let Some(replacement) = &self.replacement {
            write!(
                formatter,
                " Use the current anonymous address {} instead.",
                display_state_block_address(replacement),
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for LegacyLoopMoveTargetError {}

fn has_legacy_synthetic_identity_segment(identity: &str) -> bool {
    let mut characters = identity.chars().peekable();
    let mut at_segment_start = true;
    let mut bracket_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }
        if bracket_depth > 0 {
            match character {
                '\'' | '"' => quote = Some(character),
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                _ => {}
            }
            continue;
        }
        match character {
            '[' => {
                bracket_depth = 1;
                at_segment_start = false;
            }
            '.' => at_segment_start = true,
            '_' if at_segment_start => {
                if !matches!(characters.peek().copied(), None | Some('[' | '.')) {
                    return true;
                }
                at_segment_start = false;
            }
            _ => at_segment_start = false,
        }
    }

    false
}

/// Reject historical synthetic loop identities as state-block destinations.
///
/// A legacy identity remains valid as `moved.from`, which is how users can
/// explicitly migrate old state to a newly bound loop. It is never a valid
/// `moved.to` or `import.to` address. When provenance points to exactly one
/// current resource, include that resource's resolved address in the repair
/// text.
pub fn check_legacy_loop_move_targets<S>(
    state_blocks: &[StateBlock],
    resources: &[Resource<S>],
) -> Vec<LegacyLoopMoveTargetError> {
    state_blocks
        .iter()
        .filter_map(|block| {
            let (target, destination, source_span) = match block {
                StateBlock::Import { to, to_span, id: _ } => {
                    (to.clone(), StateBlockDestination::Import, *to_span)
                }
                StateBlock::Moved { addresses } => (
                    addresses.to(),
                    StateBlockDestination::Moved,
                    addresses.to_span(),
                ),
                StateBlock::Removed { from: _ } => return None,
            };
            let is_desired_address = resources.iter().any(|resource| {
                resource.id.provider == target.provider
                    && resource.id.resource_type == target.resource_type
                    && resource.id.identity_str() == Some(target.name_str())
            });
            if is_desired_address {
                return None;
            }
            let has_reserved_segment = has_legacy_synthetic_identity_segment(target.name_str());

            let mut replacements = Vec::new();
            for resource in resources {
                if resource.id.provider != target.provider
                    || resource.id.resource_type != target.resource_type
                {
                    continue;
                }
                let Some(identity) = resource.id.identity() else {
                    continue;
                };
                if !resource
                    .loop_provenance
                    .legacy_candidates(identity)
                    .iter()
                    .any(|legacy| legacy.exact_state_name() == target.name_str())
                {
                    continue;
                }
                let replacement = StateBlockAddress::new(
                    resource.id.provider.clone(),
                    resource.id.resource_type.clone(),
                    identity.clone(),
                );
                if !replacements.contains(&replacement) {
                    replacements.push(replacement);
                }
            }

            if !has_reserved_segment && replacements.is_empty() {
                return None;
            }

            Some(LegacyLoopMoveTargetError {
                target,
                replacement: match replacements.as_slice() {
                    [replacement] => Some(replacement.clone()),
                    _ => None,
                },
                destination,
                source_span,
            })
        })
        .collect()
}

/// Find ambiguous anonymous-resource groups before any generated identity is
/// assigned. A mutable-attribute identity is safe to reconcile only when it is
/// the sole anonymous declaration of its kind and ownership scope.
pub fn check_attribute_derived_anonymous_resource_conflicts<S: AnonymousIdentityModuleSource>(
    resources: &[Resource<S>],
    registry: &SchemaRegistry,
) -> Vec<AttributeDerivedAnonymousResourceConflict> {
    let mut counts = BTreeMap::<AnonymousResourceKind, usize>::new();
    for resource in resources {
        if resource.binding.is_some()
            || classify_anonymous_identity_basis(resource, registry)
                != Some(AnonymousIdentityBasis::AttributeDerived)
        {
            continue;
        }
        *counts
            .entry(AnonymousResourceKind::of(resource))
            .or_default() += 1;
    }

    counts
        .into_iter()
        .filter_map(|(kind, declaration_count)| {
            (declaration_count >= 2).then_some(AttributeDerivedAnonymousResourceConflict { kind })
        })
        .collect()
}

/// Return bindings that must be retained because anonymizing the resource
/// could make its identity ambiguous or impossible to derive.
///
/// Schema-backed attribute-derived groups need bindings when at least two
/// resources of the same kind share an ownership scope. Schema-less resources
/// are always retained conservatively: plan cannot derive an anonymous
/// identity for them, so removing the binding would turn valid lint advice
/// into a later hard error. Stable and singleton attribute-derived resources
/// remain safe to anonymize.
pub(crate) fn identity_required_bindings(
    resources: &[Resource],
    registry: &SchemaRegistry,
) -> HashSet<String> {
    let mut required = HashSet::new();
    let mut resources_by_kind = BTreeMap::<AnonymousResourceKind, Vec<&Resource>>::new();
    for resource in resources {
        match classify_anonymous_identity_basis(resource, registry) {
            None => {
                if let Some(binding) = &resource.binding {
                    required.insert(binding.clone());
                }
            }
            Some(AnonymousIdentityBasis::AttributeDerived) => {
                resources_by_kind
                    .entry(AnonymousResourceKind::of(resource))
                    .or_default()
                    .push(resource);
            }
            Some(AnonymousIdentityBasis::Stable) => {}
        }
    }

    required.extend(
        resources_by_kind
            .into_values()
            .filter(|resources| resources.len() >= 2)
            .flatten()
            .filter_map(|resource| resource.binding.clone()),
    );
    required
}

/// Resolve `<attr>_prefix` meta-attributes in resources.
///
/// For each resource attribute ending in `_prefix`, checks if the base attribute
/// (without `_prefix`) exists in the schema as a string-compatible type. If so:
/// - Removes the `_prefix` attribute
/// - Stores the prefix in `resource.prefixes`
/// - Generates a temporary name: `prefix + random_suffix`
///
/// Errors if both `<attr>_prefix` and `<attr>` are specified, or if prefix is empty.
pub fn resolve_attr_prefixes(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources.iter_mut() {
        let schema = match registry.get_for(resource) {
            Some(schema) => schema.schema().as_schema(),
            None => continue, // Unknown resource type; validate_resources will catch this
        };

        // Collect prefix attributes to process
        let prefix_attrs: Vec<(String, ValueString)> = resource
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if let Some(base_attr) = key.strip_suffix("_prefix")
                    && let Value::Concrete(ConcreteValue::String(prefix_value)) = value
                    && let Some(attr_schema) = schema.attributes.get(base_attr)
                    && is_string_compatible_type(&attr_schema.attr_type, &schema.defs)
                {
                    return Some((base_attr.to_string(), prefix_value.clone()));
                }
                None
            })
            .collect();

        for (base_attr, prefix_value) in prefix_attrs {
            let prefix_key = format!("{}_prefix", base_attr);

            // Error if prefix is empty
            if prefix_value.expose_plaintext().is_empty() {
                all_errors.push(format!("{}: '{}' cannot be empty", resource.id, prefix_key));
                continue;
            }

            // Error if both prefix and base attribute are specified
            if resource.attributes.contains_key(&base_attr) {
                all_errors.push(format!(
                    "{}: cannot specify both '{}' and '{}'",
                    resource.id, prefix_key, base_attr
                ));
                continue;
            }

            // Remove the _prefix attribute. `shift_remove` (not
            // `swap_remove`) preserves the source-order of the rest of
            // the attributes, which is the whole point of #2222.
            resource.attributes.shift_remove(&prefix_key);

            // Store prefix
            resource.prefixes.insert(
                base_attr.clone(),
                prefix_value.expose_plaintext().to_owned(),
            );

            // Generate temporary name
            let generated_name = ValueString::derived_from(
                format!(
                    "{}{}",
                    prefix_value.expose_plaintext(),
                    generate_random_suffix()
                ),
                [&prefix_value],
            );
            resource.set_attr(
                base_attr,
                Value::Concrete(ConcreteValue::String(generated_name)),
            );
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// State information needed for prefix reconciliation.
#[derive(Clone)]
pub struct PrefixStateInfo {
    /// Attribute prefixes stored in state (e.g., {"bucket_name": "my-app-"})
    pub prefixes: HashMap<String, String>,
    /// Attribute values available for prefix reconciliation.
    pub attribute_values: HashMap<String, PrefixStateValue>,
}

/// Persisted prefix value, including the typed case where a completed create
/// could not read the generated name back yet.
#[derive(Clone)]
pub enum PrefixStateValue {
    Known(String),
    PendingReRead(PendingReRead),
}

/// Reconcile prefixed names with existing state.
///
/// For resources that have prefixes and a matching entry in state (same prefix),
/// reuses the existing name from state instead of the temporarily generated one.
/// If the prefix has changed or there's no state match, keeps the new generated name.
pub fn reconcile_prefixed_names(
    resources: &mut [Resource],
    find_state: &dyn Fn(&str, &str, &str) -> Option<PrefixStateInfo>,
) {
    for resource in resources.iter_mut() {
        if resource.prefixes.is_empty() {
            continue;
        }
        let Some(identity) = resource.id.identity_str() else {
            continue;
        };

        // Find matching resource in state
        let state_info = find_state(&resource.id.provider, &resource.id.resource_type, identity);
        let state_info = match state_info {
            Some(si) => si,
            None => continue,
        };

        let updates: Vec<_> = resource
            .prefixes
            .iter()
            .filter_map(|(attr_name, prefix)| {
                if let Some(state_prefix) = state_info.prefixes.get(attr_name)
                    && state_prefix == prefix
                {
                    state_info.attribute_values.get(attr_name).map(|value| {
                        let value = match value {
                            PrefixStateValue::Known(name) => {
                                Value::Concrete(ConcreteValue::String(name.clone().into()))
                            }
                            PrefixStateValue::PendingReRead(pending) => {
                                Value::Deferred(DeferredValue::Unknown(
                                    UnknownReason::PendingReRead(pending.clone()),
                                ))
                            }
                        };
                        (attr_name.clone(), value)
                    })
                } else {
                    None
                }
            })
            .collect();
        for (attr_name, value) in updates {
            resource.set_attr(attr_name, value);
        }
    }
}

/// Produce a deterministic string representation of a Value for hashing.
///
/// Unlike `format!("{:?}", value)`, this ensures Map entries are sorted by key,
/// so the output is consistent across runs (HashMap iteration order is random).
fn deterministic_value_string(value: &Value) -> String {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => {
            format!("String({:?})", s.expose_plaintext())
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            format!("EnumIdentifier({:?})", s.as_str())
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            format!("EnumApiValue({:?})", c.api_value())
        }
        Value::Concrete(ConcreteValue::Int(i)) => format!("Int({})", i),
        Value::Concrete(ConcreteValue::Float(f)) => format!("Float({})", f),
        Value::Concrete(ConcreteValue::Bool(b)) => format!("Bool({})", b),
        Value::Concrete(ConcreteValue::Duration(d)) => format!("Duration({})", d.as_secs()),
        Value::Concrete(ConcreteValue::List(items)) => {
            let parts: Vec<String> = items.iter().map(deterministic_value_string).collect();
            format!("List([{}])", parts.join(", "))
        }
        Value::Concrete(ConcreteValue::StringList(items)) => {
            let parts: Vec<String> = items
                .iter()
                .map(|s| format!("{:?}", s.expose_plaintext()))
                .collect();
            format!("StringList([{}])", parts.join(", "))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{:?}: {}", k, deterministic_value_string(v)))
                .collect();
            format!("Map({{{}}})", parts.join(", "))
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            format!("ResourceRef({})", path.to_plaintext_dot_string())
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            format!("BindingRef({})", binding)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => format!(
            "Coalesce({}, {})",
            deterministic_value_string(value),
            deterministic_value_string(default)
        ),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let strs: Vec<String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => {
                        format!("Literal({:?})", s.expose_plaintext())
                    }
                    InterpolationPart::Expr(v) => {
                        format!("Expr({})", deterministic_value_string(v))
                    }
                })
                .collect();
            format!("Interpolation([{}])", strs.join(", "))
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, args, .. }
            | DeferredValue::Unavailable { name, args, .. },
        ) => {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|argument| deterministic_value_string(argument.walker_value()))
                .collect();
            format!("FunctionCall({}({}))", name, arg_strs.join(", "))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|argument| deterministic_value_string(argument.walker_value()))
                .collect();
            format!("FunctionCall({}({}))", function.name, arg_strs.join(", "))
        }
        Value::Deferred(DeferredValue::Access(access)) => format!(
            "Access({}{})",
            deterministic_value_string(access.base()),
            access.step().to_plaintext_dot_string()
        ),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            format!("Secret({})", deterministic_value_string(inner))
        }
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
            format!(
                "RedactedSecret({})",
                crate::resource::legacy_secret_identity_material(hash)
            )
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            use crate::resource::UnknownReason;
            // Variant labels feed identity hashes; keep them stable across Rust renames.
            match reason {
                UnknownReason::UpstreamExport { path } => {
                    format!("Unknown(UpstreamRef({}))", path.to_plaintext_dot_string())
                }
                UnknownReason::UpstreamAllExports { binding } => {
                    format!("Unknown(UpstreamBareRef({}))", binding)
                }
                UnknownReason::ForKey => "Unknown(ForKey)".to_string(),
                UnknownReason::ForIndex => "Unknown(ForIndex)".to_string(),
                UnknownReason::ForValue => "Unknown(ForValue)".to_string(),
                UnknownReason::FnParam { name } => {
                    format!("Unknown(FnParam({name}))")
                }
                UnknownReason::ModuleArgument { name, .. } => {
                    format!("Unknown(ModuleArgument({name}))")
                }
                UnknownReason::FnLocal { name } => {
                    format!("Unknown(FnLocal({name}))")
                }
                UnknownReason::EmptyInterpolation => "Unknown(EmptyInterpolation)".to_string(),
                UnknownReason::PendingReRead(pending) => match pending.reason() {
                    crate::resource::ReReadReason::Incomplete { detail } => {
                        format!("Unknown(PendingReRead(Incomplete({detail:?})))")
                    }
                    crate::resource::ReReadReason::ReadFailed { detail } => {
                        format!("Unknown(PendingReRead(ReadFailed({detail:?})))")
                    }
                    crate::resource::ReReadReason::NotFound => {
                        "Unknown(PendingReRead(NotFound))".to_string()
                    }
                },
            }
        }
    }
}

pub fn canonical_create_only_value_string(value: &Value) -> Option<String> {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => Some(s.expose_plaintext().to_owned()),
        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => Some(raw.as_str().to_string()),
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Some(format!("EnumApiValue({:?})", c.api_value()))
        }
        Value::Concrete(ConcreteValue::Int(i)) => Some(format!("Int({})", i)),
        Value::Concrete(ConcreteValue::Float(f)) => Some(format!("Float({})", f)),
        Value::Concrete(ConcreteValue::Bool(b)) => Some(format!("Bool({})", b)),
        // State JSON stores durations as integer seconds, so the comparison
        // channel must use the same form as the state-side Int round-trip.
        Value::Concrete(ConcreteValue::Duration(d)) => Some(format!("Int({})", d.as_secs())),
        Value::Concrete(ConcreteValue::List(items)) => {
            let parts: Vec<String> = items
                .iter()
                .map(canonical_create_only_feature_string)
                .collect();
            Some(format!("List([{}])", parts.join(", ")))
        }
        // State JSON cannot distinguish StringList from List, so the
        // canonical comparison channel uses the List form.
        Value::Concrete(ConcreteValue::StringList(items)) => Some(format!(
            "List([{}])",
            items
                .iter()
                .map(|item| item.expose_plaintext())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let parts: Vec<String> = entries
                .iter()
                .map(|(key, item)| {
                    format!("{:?}: {}", key, canonical_create_only_feature_string(item))
                })
                .collect();
            Some(format!("Map({{{}}})", parts.join(", ")))
        }
        _ => None,
    }
}

fn canonical_create_only_feature_string(value: &Value) -> String {
    canonical_create_only_value_string(value).unwrap_or_else(|| deterministic_value_string(value))
}

/// Maximum Hamming distance (out of 64 bits) for SimHash-based reconciliation.
/// Two identifiers with distance below this threshold are considered the "same resource"
/// with modified attributes.
pub(crate) const SIMHASH_HAMMING_THRESHOLD: u32 = 20;

/// A 64-bit SimHash value. The field is private so standard create-only
/// hashes cannot be smuggled into Hamming-distance comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SimHash(u64);

impl SimHash {
    pub(crate) fn parse_16_hex(identifier_suffix: &str) -> Option<Self> {
        if identifier_suffix.len() != 16 {
            return None;
        }
        u64::from_str_radix(identifier_suffix, 16).ok().map(Self)
    }

    pub(crate) fn distance(self, other: SimHash) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    #[cfg(test)]
    pub(crate) fn with_flipped_mask_for_test(self, mask: u64) -> Self {
        Self(self.0 ^ mask)
    }

    #[cfg(test)]
    pub(crate) fn is_zero_for_test(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::LowerHex for SimHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnonymousHashSuffix {
    Standard(u32),
    SimHash(SimHash),
}

impl AnonymousHashSuffix {
    fn discriminator(self) -> String {
        match self {
            Self::Standard(hash) => format!("{hash:08x}"),
            Self::SimHash(hash) => format!("{hash:016x}"),
        }
    }
}

/// Find the unique candidate closest (by Hamming distance) to `target` SimHash
/// among `candidates`, below `SIMHASH_HAMMING_THRESHOLD`. Returns `None` when
/// no candidate qualifies, or when two or more candidates tie at the minimum
/// distance — the latter is an ambiguous match the caller should refuse to
/// commit to (rebinding to the wrong state entry would silently corrupt
/// addresses).
pub(crate) fn closest_unique_simhash_match<C: Copy>(
    target: SimHash,
    candidates: impl IntoIterator<Item = C>,
    hash_of: impl Fn(C) -> SimHash,
) -> Option<C> {
    let mut best: Option<(C, u32)> = None;
    let mut unique = true;
    for c in candidates {
        let distance = target.distance(hash_of(c));
        if distance >= SIMHASH_HAMMING_THRESHOLD {
            continue;
        }
        match best {
            None => best = Some((c, distance)),
            Some((_, prev)) => {
                if distance < prev {
                    best = Some((c, distance));
                    unique = true;
                } else if distance == prev {
                    unique = false;
                }
            }
        }
    }
    best.and_then(|(c, _)| if unique { Some(c) } else { None })
}

/// Flatten a Value into individual SimHash features.
///
/// Map values are expanded so each entry becomes a separate feature (e.g., `tags.Environment`),
/// allowing SimHash to produce close hashes when only one map entry changes.
/// Non-map values use `deterministic_value_string` as the feature value.
pub(crate) fn flatten_value_for_simhash(
    prefix: &str,
    value: &Value,
    out: &mut std::collections::BTreeMap<String, String>,
) {
    match value {
        Value::Concrete(ConcreteValue::Map(map)) => {
            for (k, v) in map {
                let key = format!("{}.{}", prefix, k);
                flatten_value_for_simhash(&key, v, out);
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let key = format!("{}[{}]", prefix, i);
                flatten_value_for_simhash(&key, item, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), deterministic_value_string(value));
        }
    }
}

/// Compute SimHash of a set of key-value attributes.
///
/// SimHash is a locality-sensitive hash: changing one attribute flips only a few bits,
/// so similar inputs produce similar hashes. This enables similarity-based reconciliation
/// using Hamming distance on the identifier alone.
pub(crate) fn compute_simhash<K: std::fmt::Display>(
    attributes: &std::collections::BTreeMap<K, String>,
) -> SimHash {
    use std::hash::{Hash, Hasher};

    let mut v = [0i32; 64];
    for (key, value) in attributes {
        let feature = format!("{}={}", key, value);
        let mut hasher = std::hash::DefaultHasher::new();
        feature.hash(&mut hasher);
        let hash = hasher.finish();
        for (i, count) in v.iter_mut().enumerate() {
            if (hash >> i) & 1 == 1 {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
    }
    let mut result: u64 = 0;
    for (i, count) in v.iter().enumerate() {
        if *count > 0 {
            result |= 1 << i;
        }
    }
    SimHash(result)
}

/// Extract the hash portion from an anonymous resource identifier.
///
/// Supports both 8 hex chars (standard hash, u32) and 16 hex chars (SimHash, u64).
/// Identifier format: `{resource_type}_{hex}` (e.g., `ec2_eip_a3f2b1c8d79f1524`).
pub(crate) fn extract_hash_from_identifier(identifier: &str) -> Option<AnonymousHashSuffix> {
    let hex_part = identifier.rsplit('_').next()?;
    match hex_part.len() {
        16 => SimHash::parse_16_hex(hex_part).map(AnonymousHashSuffix::SimHash),
        8 => u32::from_str_radix(hex_part, 16)
            .ok()
            .map(AnonymousHashSuffix::Standard),
        _ => None,
    }
}

/// Return the canonical discriminator carried by a generated anonymous identity.
///
/// This is a value-free display projection of the same 8- or 16-hex suffix
/// parsed by anonymous-resource reconciliation. Callers must still use typed
/// resource metadata (such as `Resource::binding`) to decide whether a
/// resource is anonymous; a matching suffix alone does not establish that.
pub fn anonymous_identity_discriminator(identity: &ResourceIdentity) -> Option<String> {
    extract_hash_from_identifier(identity.as_str()).map(AnonymousHashSuffix::discriminator)
}

/// Build a SimHash over the combined set of provider identity values plus
/// a resource's user-specified attributes (flattened).
///
/// This is the shared feature set used by both anonymous identifier computation
/// (when the schema has no create-only attributes) and `compute_resource_simhash`
/// so the two always agree on what a resource's anonymous ID would be.
fn simhash_from_identity_and_resource<S>(
    identity_values: &BTreeMap<String, String>,
    resource: &Resource<S>,
) -> SimHash {
    let mut simhash_values = identity_values.clone();
    for (key, value) in &resource.attributes {
        flatten_value_for_simhash(key, value, &mut simhash_values);
    }
    compute_simhash(&simhash_values)
}

/// Compute the SimHash anonymous identifier generation would produce for a
/// single resource. Used by `detect_anonymous_to_named_renames` to recover the
/// anonymous ID of a resource that has since been wrapped in a `let` binding.
fn compute_resource_simhash(
    resource: &Resource,
    providers: &CanonicalizedProviderConfigs,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> SimHash {
    let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
    if !resource.id.provider.is_empty() {
        let identity_attrs = identity_attributes_fn(&resource.id.provider);
        if let Some(pc) = providers
            .as_slice()
            .iter()
            .find(|p| p.name == resource.id.provider)
        {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    identity_values.insert(attr_name.clone(), deterministic_value_string(value));
                }
            }
        }
    }

    simhash_from_identity_and_resource(&identity_values, resource)
}

#[derive(Default)]
struct LoopBindingReservations {
    by_scope: BTreeMap<String, BTreeSet<String>>,
}

impl LoopBindingReservations {
    fn suggest(&mut self, origin: &LoopElementOrigin, base: &str) -> String {
        let occupied = self
            .by_scope
            .entry(origin.source().binding_scope_key())
            .or_insert_with(|| origin.source().binding_scope().cloned().unwrap_or_default());
        let mut suggested = base.to_string();
        let mut suffix = 2usize;
        while occupied.contains(&suggested) {
            suggested = format!("{base}_{suffix}");
            suffix += 1;
        }
        occupied.insert(suggested.clone());
        suggested
    }
}

fn record_direct_loop_conflict(
    conflicts: &mut Vec<(LoopConflictReason, LoopElementOrigin)>,
    reason: LoopConflictReason,
    origin: &LoopElementOrigin,
) {
    if conflicts.iter().any(|(seen_reason, seen)| {
        *seen_reason == reason && seen.source().same_authored_source(origin.source())
    }) {
        return;
    }
    conflicts.push((reason, origin.clone()));
}

fn direct_loop_conflicts<S>(
    sources: &[(LoopConflictReason, LoopElementOrigin)],
    resources: &[Resource<S>],
    reservations: &mut LoopBindingReservations,
) -> Vec<LoopAnonymousIdentityConflict> {
    let mut conflicts = Vec::new();
    for (reason, seed_origin) in sources {
        let matching: Vec<_> = resources
            .iter()
            .filter_map(|resource| {
                let origin = resource.loop_provenance.direct_origin()?;
                seed_origin
                    .source()
                    .same_authored_source(origin.source())
                    .then_some((resource, origin))
            })
            .collect();
        let Some((seed_resource, _)) = matching.first() else {
            continue;
        };
        let base = seed_resource
            .id
            .resource_type
            .rsplit('.')
            .next()
            .map(crate::parser::pascal_to_snake)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "resource".to_string());
        let resource_type = seed_resource.id.display_type();
        let suggested_binding = reservations.suggest(seed_origin, &base);
        let moves = matching
            .into_iter()
            .map(|(resource, origin)| LoopRewriteMove {
                provider: resource.id.provider.clone(),
                resource_type: resource.id.resource_type.clone(),
                from: origin.legacy_address().clone(),
                to: origin.rewritten_address(&suggested_binding),
            })
            .collect();

        conflicts.push(LoopAnonymousIdentityConflict {
            reason: *reason,
            origin: seed_origin.clone(),
            resource_type,
            suggested_binding,
            moves,
        });
    }
    conflicts
}

#[derive(Debug, Clone)]
struct LoopModuleConflictSource {
    origin: LoopElementOrigin,
    module_name: String,
}

struct LoopModuleCollisionDiscovery {
    sources: Vec<LoopModuleConflictSource>,
    colliding_current_prefixes: BTreeSet<String>,
    plain_errors: Vec<AnonymousIdentityError>,
}

fn module_prefix_collision_fully_explains_members<S>(
    member_indices: &[usize],
    resources: &[Resource<S>],
    discovery: &LoopModuleCollisionDiscovery,
) -> bool {
    if member_indices.len() < 2 {
        return false;
    }

    discovery.colliding_current_prefixes.iter().any(|prefix| {
        let mut origins = Vec::with_capacity(member_indices.len());
        for &index in member_indices {
            let Some(origin) = resources[index]
                .loop_provenance
                .module_prefixes()
                .iter()
                .find(|candidate| candidate.current_prefix().as_str() == prefix)
                .map(|candidate| candidate.origin())
            else {
                return false;
            };
            if origins.contains(&origin) {
                // Two declarations from one module call still collide after
                // the outer call receives a distinct bound prefix. That is an
                // independent module-internal error, not part of this rewrite.
                return false;
            }
            origins.push(origin);
        }
        true
    })
}

fn module_name_from_instance_prefix(prefix: &str) -> String {
    let tail = prefix.rsplit('.').next().unwrap_or(prefix);
    tail.rsplit_once('_')
        .filter(|(_, hash)| hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|(module, _)| module.to_string())
        .filter(|module| !module.is_empty())
        .unwrap_or_else(|| "module".to_string())
}

fn discover_loop_module_prefix_collisions<S>(
    resources: &[Resource<S>],
) -> LoopModuleCollisionDiscovery {
    let mut by_current_prefix = BTreeMap::<String, Vec<(usize, usize)>>::new();
    for (resource_index, resource) in resources.iter().enumerate() {
        for (prefix_index, prefix) in resource
            .loop_provenance
            .module_prefixes()
            .iter()
            .enumerate()
        {
            by_current_prefix
                .entry(prefix.current_prefix().as_str().to_string())
                .or_default()
                .push((resource_index, prefix_index));
        }
    }

    let mut sources = Vec::<LoopModuleConflictSource>::new();
    let mut colliding_current_prefixes = BTreeSet::new();
    let mut plain_errors = Vec::new();
    for (current_prefix, entries) in by_current_prefix {
        let mut distinct_origins = Vec::<(usize, usize)>::new();
        for entry in entries.iter().copied() {
            let origin = resources[entry.0].loop_provenance.module_prefixes()[entry.1].origin();
            if distinct_origins.iter().any(|seen| {
                resources[seen.0].loop_provenance.module_prefixes()[seen.1].origin() == origin
            }) {
                continue;
            }
            distinct_origins.push(entry);
        }
        if distinct_origins.len() < 2 {
            continue;
        }

        let seed = resources[distinct_origins[0].0]
            .loop_provenance
            .module_prefixes()[distinct_origins[0].1]
            .origin();
        let one_authored_source = distinct_origins.iter().all(|entry| {
            let origin = resources[entry.0].loop_provenance.module_prefixes()[entry.1].origin();
            seed.source().same_authored_source(origin.source())
        });
        if one_authored_source {
            if !sources
                .iter()
                .any(|source| source.origin.source().same_authored_source(seed.source()))
            {
                sources.push(LoopModuleConflictSource {
                    origin: seed.clone(),
                    module_name: module_name_from_instance_prefix(&current_prefix),
                });
            }
        } else {
            let first = &resources[distinct_origins[0].0];
            let second = &resources[distinct_origins[1].0];
            plain_errors.push(
                AnonymousResourceIdentifierCollision {
                    first_type: first.id.display_type(),
                    second_type: second.id.display_type(),
                    identifier: current_prefix.clone(),
                }
                .into(),
            );
        }
        colliding_current_prefixes.insert(current_prefix);
    }

    LoopModuleCollisionDiscovery {
        sources,
        colliding_current_prefixes,
        plain_errors,
    }
}

fn loop_module_conflicts<S>(
    sources: &[LoopModuleConflictSource],
    resources: &[Resource<S>],
    computed_identities: &HashMap<usize, String>,
    reservations: &mut LoopBindingReservations,
) -> Vec<LoopAnonymousIdentityConflict> {
    let mut conflicts = Vec::new();
    for source in sources {
        let base = crate::parser::pascal_to_snake(&source.module_name);
        let base = if base.is_empty() { "module" } else { &base };
        let suggested_binding = reservations.suggest(&source.origin, base);
        let mut seen_moves = BTreeSet::new();
        let mut moves = Vec::new();
        for (index, resource) in resources.iter().enumerate() {
            let Some(current_identity) = resource.id.identity().cloned().or_else(|| {
                computed_identities
                    .get(&index)
                    .cloned()
                    .map(ResourceIdentity::new)
            }) else {
                continue;
            };
            for prefix in resource.loop_provenance.module_prefixes() {
                if !source
                    .origin
                    .source()
                    .same_authored_source(prefix.origin().source())
                {
                    continue;
                }
                let Some((from, to)) =
                    prefix.rewritten_resource_addresses(&current_identity, &suggested_binding)
                else {
                    continue;
                };
                let key = (
                    resource.id.provider.clone(),
                    resource.id.resource_type.clone(),
                    from.exact_state_name().to_string(),
                    to.clone(),
                );
                if !seen_moves.insert(key) {
                    continue;
                }
                moves.push(LoopRewriteMove {
                    provider: resource.id.provider.clone(),
                    resource_type: resource.id.resource_type.clone(),
                    from,
                    to,
                });
            }
        }
        conflicts.push(LoopAnonymousIdentityConflict {
            reason: LoopConflictReason::IdenticalIdentityInputs,
            origin: source.origin.clone(),
            resource_type: format!("module {}", source.module_name),
            suggested_binding,
            moves,
        });
    }
    conflicts
}

#[cfg(test)]
pub fn compute_anonymous_identifiers_for_test(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
    registry: &SchemaRegistry,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Result<(), AnonymousIdentityError> {
    let providers = CanonicalizedProviderConfigs::from_configs_for_test(providers.to_vec());
    let resources = crate::value::canonicalize_resources_with_schemas(resources, registry)
        .expect("anonymous-identity test resources must canonicalize");
    compute_anonymous_identifiers_with_provider_configs(
        resources,
        &providers,
        registry,
        AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        identity_attributes_fn,
    )
}

/// Compute stable identifiers for anonymous resources (those without ResourceId identity).
/// Uses create-only properties and provider identity attributes to generate a deterministic hash.
///
/// `identity_attributes_fn` takes a provider name and returns the list of identity attribute names
/// for that provider (e.g., `["region"]`).
pub fn compute_anonymous_identifiers_with_provider_configs<S: AnonymousIdentityModuleSource>(
    resources: CanonicalizedResources<'_, S>,
    providers: &CanonicalizedProviderConfigs,
    registry: &SchemaRegistry,
    conflict_policy: AnonymousIdentityConflictPolicy,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Result<(), AnonymousIdentityError> {
    use std::hash::{Hash, Hasher};

    let resources = resources.into_mut_slice();

    let mut module_collision_discovery = discover_loop_module_prefix_collisions(resources);
    let mut identity_errors = std::mem::take(&mut module_collision_discovery.plain_errors);
    let mut direct_conflict_sources = Vec::new();

    if conflict_policy.rejects_attribute_derived() {
        for conflict in check_attribute_derived_anonymous_resource_conflicts(resources, registry) {
            let members: Vec<usize> = resources
                .iter()
                .enumerate()
                .filter_map(|(index, resource)| {
                    conflict.includes(resource, registry).then_some(index)
                })
                .collect();
            let explained_by_module_prefix = module_prefix_collision_fully_explains_members(
                &members,
                resources,
                &module_collision_discovery,
            );
            if !explained_by_module_prefix
                && members
                    .iter()
                    .any(|&index| resources[index].loop_provenance.direct_origin().is_none())
            {
                identity_errors.push(conflict.into());
            }
            if !explained_by_module_prefix {
                for origin in members
                    .iter()
                    .filter_map(|&index| resources[index].loop_provenance.direct_origin().cloned())
                {
                    record_direct_loop_conflict(
                        &mut direct_conflict_sources,
                        LoopConflictReason::AttributeDerived,
                        &origin,
                    );
                }
            }
        }
    }

    // First pass: compute identifiers and detect collisions
    let mut computed: Vec<(usize, String, BTreeMap<String, String>)> = Vec::new();

    for (idx, resource) in resources.iter().enumerate() {
        if resource.id.identity_str().is_some() {
            continue;
        }

        // Look up schema for this resource's provider (skip if no provider set)
        if resource.id.provider.is_empty() {
            continue;
        }
        let provider_name = &resource.id.provider;

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();

        // Collect identity attribute values (e.g., region) from provider config
        let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
        let identity_attrs = identity_attributes_fn(provider_name);
        if let Some(pc) = providers
            .as_slice()
            .iter()
            .find(|p| p.name == *provider_name)
        {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    identity_values.insert(attr_name.clone(), deterministic_value_string(value));
                }
            }
        }

        let hash_values = stable_anonymous_hash_inputs(resource, schema);
        let use_simhash = hash_values.is_empty();

        let hash_suffix = if use_simhash {
            // Use SimHash for locality-sensitive hashing: similar inputs produce
            // similar hashes, enabling Hamming distance reconciliation.
            let simhash = simhash_from_identity_and_resource(&identity_values, resource);
            AnonymousHashSuffix::SimHash(simhash)
        } else {
            // Use standard hash for create-only properties
            let mut hasher = std::hash::DefaultHasher::new();
            for (k, v) in &identity_values {
                k.hash(&mut hasher);
                v.hash(&mut hasher);
            }
            for (k, v) in &hash_values {
                k.hash(&mut hasher);
                v.hash(&mut hasher);
            }
            AnonymousHashSuffix::Standard((hasher.finish() & 0xFFFFFFFF) as u32)
        };
        let hash_str = hash_suffix.discriminator();

        // Build identifier: provider_resource_type_hash (e.g., awscc_ec2_vpc_a3f2b1c8).
        // The resource_type segments carry PascalCase for the final type name
        // (e.g., "ec2.Vpc"); identifier names are snake_case values per the
        // naming-conventions rule, so lower each segment before joining.
        // The provider segment makes anonymous identifiers self-describe their
        // provider so plan output and state files distinguish e.g.
        // `aws.iam.RolePolicy` from `awscc.iam.RolePolicy` at a glance.
        let provider_snake = resource
            .id
            .provider
            .split('.')
            .map(crate::parser::pascal_to_snake)
            .collect::<Vec<_>>()
            .join("_");
        let type_snake = resource
            .id
            .resource_type
            .split('.')
            .map(crate::parser::pascal_to_snake)
            .collect::<Vec<_>>()
            .join("_");
        let bare_identifier = format!("{}_{}_{}", provider_snake, type_snake, hash_str);
        // If this anonymous resource lives inside a module instantiation,
        // surface the owning instance in the identifier (`<instance>.<bare>`).
        // The expander leaves anonymous resources `Pending` so this pass
        // still sees them; without the prefix, a `Pending` module-internal
        // resource would land at the same address as a top-level
        // `Pending` resource with matching create-only attrs (#2516).
        let identifier = resource
            .module_source
            .as_ref()
            .and_then(AnonymousIdentityModuleSource::anonymous_outer_instance_identity)
            .map_or_else(
                || bare_identifier.clone(),
                |instance| format!("{}.{}", instance, bare_identifier),
            );

        computed.push((idx, identifier, hash_values));
    }

    let computed_identities = computed
        .iter()
        .map(|(index, identity, _)| (*index, identity.clone()))
        .collect::<HashMap<_, _>>();

    // Detect collisions among identities generated in this pass.
    let mut collisions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (computed_index, (_, identifier, _)) in computed.iter().enumerate() {
        collisions
            .entry(identifier.clone())
            .or_default()
            .push(computed_index);
    }
    for (identifier, collision_members) in collisions {
        if collision_members.len() < 2 {
            continue;
        }
        let resource_indices: Vec<usize> = collision_members
            .iter()
            .map(|&computed_index| computed[computed_index].0)
            .collect();
        if module_prefix_collision_fully_explains_members(
            &resource_indices,
            resources,
            &module_collision_discovery,
        ) {
            continue;
        }
        let first_inputs = &computed[collision_members[0]].2;
        let identical_stable_inputs = !first_inputs.is_empty()
            && collision_members
                .iter()
                .all(|&computed_index| &computed[computed_index].2 == first_inputs);
        if identical_stable_inputs {
            let mut has_direct_member = false;
            for origin in resource_indices
                .iter()
                .filter_map(|&index| resources[index].loop_provenance.direct_origin().cloned())
            {
                has_direct_member = true;
                record_direct_loop_conflict(
                    &mut direct_conflict_sources,
                    LoopConflictReason::IdenticalIdentityInputs,
                    &origin,
                );
            }
            let has_plain_member = resource_indices
                .iter()
                .any(|&index| resources[index].loop_provenance.direct_origin().is_none());
            if has_direct_member && !has_plain_member {
                continue;
            }
        }
        identity_errors.push(
            AnonymousResourceIdentifierCollision {
                first_type: resources[resource_indices[0]].id.display_type(),
                second_type: resources[resource_indices[1]].id.display_type(),
                identifier,
            }
            .into(),
        );
    }

    // Module-internal resources with assigned names never enter the generated
    // identity pass above. Still reject two anonymous module calls that
    // resolve to the same complete resource address.
    let mut resolved_identities =
        BTreeMap::<(String, String, Option<String>, String), Vec<usize>>::new();
    for (index, resource) in resources.iter().enumerate() {
        let Some(identity) = resource.id.identity_str() else {
            continue;
        };
        resolved_identities
            .entry((
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
                resource.id.provider_instance.clone(),
                identity.to_string(),
            ))
            .or_default()
            .push(index);
    }
    for ((_, _, _, identity), resource_indices) in resolved_identities {
        if resource_indices.len() < 2
            || module_prefix_collision_fully_explains_members(
                &resource_indices,
                resources,
                &module_collision_discovery,
            )
        {
            continue;
        }
        identity_errors.push(
            AnonymousResourceIdentifierCollision {
                first_type: resources[resource_indices[0]].id.display_type(),
                second_type: resources[resource_indices[1]].id.display_type(),
                identifier: identity,
            }
            .into(),
        );
    }

    let mut reservations = LoopBindingReservations::default();
    identity_errors.extend(
        direct_loop_conflicts(&direct_conflict_sources, resources, &mut reservations)
            .into_iter()
            .map(AnonymousIdentityError::from),
    );
    identity_errors.extend(
        loop_module_conflicts(
            &module_collision_discovery.sources,
            resources,
            &computed_identities,
            &mut reservations,
        )
        .into_iter()
        .map(AnonymousIdentityError::from),
    );

    if !identity_errors.is_empty() {
        return Err(AnonymousIdentityError::from_errors(identity_errors));
    }

    // Second pass: apply identifiers
    for (idx, identifier, _) in computed {
        let provider = resources[idx].id.provider.clone();
        let resource_type = resources[idx].id.resource_type.clone();
        let provider_instance = resources[idx].id.provider_instance.clone();
        resources[idx].id = ResourceId::with_provider_identity(
            &provider,
            &resource_type,
            ResourceIdentity::new(identifier),
            provider_instance,
        );
    }

    Ok(())
}

#[cfg(test)]
pub fn compute_anonymous_identifiers_with_provider_configs_for_test(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
    registry: &SchemaRegistry,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Result<(), AnonymousIdentityError> {
    let providers = CanonicalizedProviderConfigs::from_configs_for_test(providers.to_vec());
    let resources = crate::value::canonicalize_resources_with_schemas(resources, registry)
        .expect("anonymous-identity test resources must canonicalize");
    compute_anonymous_identifiers_with_provider_configs(
        resources,
        &providers,
        registry,
        AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        identity_attributes_fn,
    )
}

/// State information needed for anonymous identifier reconciliation.
#[derive(Clone)]
pub struct AnonymousIdStateInfo {
    /// The resource name (anonymous identifier) stored in state
    pub name: String,
    /// Provider-instance routing carried by the persisted state row.
    pub provider_instance: Option<String>,
    /// Stable create-only and identity attribute values from state, keyed by
    /// DSL attribute name. Existing reconciliation consumes the create-only
    /// subset; exact legacy-loop disambiguation also consumes identity fields.
    pub create_only_values: HashMap<String, String>,
}

fn resolved_anonymous_state_id(
    provider: &str,
    resource_type: &str,
    entry: &AnonymousIdStateInfo,
) -> ResolvedResourceId {
    ResolvedResourceId::with_provider_identity(
        provider,
        resource_type,
        ResourceIdentity::new(entry.name.clone()),
        entry.provider_instance.clone(),
    )
}

enum StableIdentityMatchValue {
    Exact(String),
    Prefix(String),
}

fn stable_identity_match_values(
    resource: &Resource,
    schema: &ResourceSchema,
) -> Option<BTreeMap<String, StableIdentityMatchValue>> {
    let mut values = BTreeMap::new();
    for attribute in schema
        .create_only_attributes()
        .into_iter()
        .chain(schema.identity_attributes())
    {
        if values.contains_key(attribute) {
            continue;
        }
        let value = if let Some(prefix) = resource.prefixes.get(attribute) {
            StableIdentityMatchValue::Prefix(prefix.clone())
        } else if let Some(value) = resource.get_attr(attribute) {
            let Some(value) = canonical_create_only_value_string(value) else {
                continue;
            };
            StableIdentityMatchValue::Exact(value)
        } else {
            continue;
        };
        values.insert(attribute.to_string(), value);
    }
    (!values.is_empty()).then_some(values)
}

fn legacy_row_stable_identity_matches(
    candidate: &LegacyLoopMoveCandidate<'_>,
    state_entry: &AnonymousIdStateInfo,
    registry: &SchemaRegistry,
) -> Option<bool> {
    let schema = registry.get_for(candidate.resource)?;
    let desired_values = stable_identity_match_values(candidate.resource, schema.as_schema())?;
    if state_entry.create_only_values.is_empty() {
        return None;
    }
    Some(desired_values.iter().all(|(attribute, desired)| {
        let Some(persisted) = state_entry.create_only_values.get(attribute) else {
            return false;
        };
        match desired {
            StableIdentityMatchValue::Exact(value) => persisted == value,
            StableIdentityMatchValue::Prefix(prefix) => persisted.starts_with(prefix),
        }
    }))
}

type LegacyLoopAddressKey = (String, String, String, Option<String>);

fn loop_address_key(id: &ResolvedResourceId) -> LegacyLoopAddressKey {
    (
        id.provider().to_string(),
        id.resource_type().to_string(),
        id.identity_str().to_string(),
        id.provider_instance().map(str::to_string),
    )
}

struct LegacyLoopMoveCandidate<'a> {
    resource: &'a Resource,
    destination: ResolvedResourceId,
    legacy: LegacyLoopAddress,
}

/// Detect exact state moves from historical synthetic loop addresses to the
/// anonymous identities now assigned to the same authored elements.
///
/// This is deliberately an exact-address migration oracle. It never falls
/// back to SimHash or distance matching. When multiple desired elements claim
/// the same historical address, the persisted stable identity fields must
/// select exactly one owner.
pub fn detect_legacy_loop_address_moves(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    claims: &StateBlockClaims,
    reserved_moves: &[ResolvedMove],
) -> Vec<ResolvedMove> {
    let desired_ids: HashSet<LegacyLoopAddressKey> = resources
        .iter()
        .filter_map(|resource| ResolvedResourceId::try_new(resource.id.clone()))
        .map(|id| loop_address_key(&id))
        .collect();
    let reserved_sources: HashSet<LegacyLoopAddressKey> = reserved_moves
        .iter()
        .map(|migration| loop_address_key(migration.from()))
        .collect();

    let mut candidates = Vec::new();
    for resource in resources {
        let Some(destination) = ResolvedResourceId::try_new(resource.id.clone()) else {
            continue;
        };
        let Some(identity) = resource.id.identity() else {
            continue;
        };
        for legacy in resource.loop_provenance.legacy_candidates(identity) {
            if candidates
                .iter()
                .any(|candidate: &LegacyLoopMoveCandidate<'_>| {
                    candidate.destination == destination && candidate.legacy == legacy
                })
            {
                continue;
            }
            candidates.push(LegacyLoopMoveCandidate {
                resource,
                destination: destination.clone(),
                legacy,
            });
        }
    }

    let mut visited_groups = HashSet::new();
    let mut used_sources = HashSet::new();
    let mut migrations = Vec::new();
    for candidate in &candidates {
        let group_key = (
            candidate.destination.provider().to_string(),
            candidate.destination.resource_type().to_string(),
            candidate.legacy.exact_state_name().to_string(),
            candidate
                .destination
                .provider_instance()
                .map(str::to_string),
        );
        if !visited_groups.insert(group_key.clone()) {
            continue;
        }

        let state_entries = find_state_by_type(&group_key.0, &group_key.1);
        let exact_rows: Vec<&AnonymousIdStateInfo> = state_entries
            .all()
            .filter(|entry| entry.name == group_key.2 && entry.provider_instance == group_key.3)
            .collect();
        let [state_entry] = exact_rows.as_slice() else {
            continue;
        };
        let source = resolved_anonymous_state_id(&group_key.0, &group_key.1, state_entry);
        let source_key = loop_address_key(&source);
        if claims.claims_from(source.as_inner())
            || reserved_sources.contains(&source_key)
            || desired_ids.contains(&source_key)
            || used_sources.contains(&source_key)
        {
            continue;
        }

        let eligible: Vec<&LegacyLoopMoveCandidate<'_>> = candidates
            .iter()
            .filter(|other| {
                other.destination.provider() == group_key.0
                    && other.destination.resource_type() == group_key.1
                    && other.legacy.exact_state_name() == group_key.2
                    && other.destination.provider_instance() == group_key.3.as_deref()
            })
            .filter(|other| {
                !claims.claims_to(other.destination.as_inner())
                    && !state_entries.all().any(|entry| {
                        entry.name == other.destination.identity_str()
                            && entry.provider_instance.as_deref()
                                == other.destination.provider_instance()
                    })
            })
            .collect();
        let selected = match eligible.as_slice() {
            [only] => match legacy_row_stable_identity_matches(only, state_entry, registry) {
                Some(true) | None => Some(*only),
                Some(false) => None,
            },
            [] => None,
            multiple => {
                let stable_matches: Vec<_> = multiple
                    .iter()
                    .copied()
                    .filter(|candidate| {
                        legacy_row_stable_identity_matches(candidate, state_entry, registry)
                            == Some(true)
                    })
                    .collect();
                match stable_matches.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                }
            }
        };

        if let Some(selected) = selected {
            migrations.push(ResolvedMove::from_detected_rename(
                source,
                selected.destination.clone(),
            ));
            used_sources.insert(source_key);
        }
    }
    migrations
}

/// Cross-resource state information used to resolve simple deferred
/// create-only references like `route_table_id = private_rtb.id`.
#[derive(Clone)]
pub struct AnonymousIdBindingStateInfo {
    /// The resource name stored in state. Let-bound resources are usually
    /// named by their binding (`private_rtb`, `inst.x`).
    pub name: String,
    /// Canonical attribute values from state, keyed by DSL attribute name
    /// (the state JSON attribute key).
    pub attribute_values: HashMap<String, String>,
}

fn state_entry_matches_binding_scope(binding: &str, state_name: &str) -> bool {
    binding.contains('.') || !state_name.contains('.')
}

fn resolve_deferred_create_only_value_string(
    value: &Value,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Option<String> {
    let Value::Deferred(DeferredValue::ResourceRef { path }) = value else {
        return None;
    };
    if !path.segments().is_empty() {
        return None;
    }

    let candidates: Vec<AnonymousIdBindingStateInfo> = find_state_by_binding(path.binding())
        .into_iter()
        .filter(|entry| state_entry_matches_binding_scope(path.binding(), &entry.name))
        .collect();
    let [entry] = candidates.as_slice() else {
        return None;
    };
    entry.attribute_values.get(path.attribute()).cloned()
}

fn canonical_or_resolved_create_only_value_string(
    value: &Value,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Option<String> {
    canonical_create_only_value_string(value)
        .or_else(|| resolve_deferred_create_only_value_string(value, find_state_by_binding))
}

/// Reconcile anonymous resource identifiers with existing state.
///
/// When a create-only property changes on an anonymous resource, the computed
/// hash-based identifier changes. This function detects such cases by comparing
/// create-only property values and returns a resolved move from the old state
/// row to the resource's freshly computed identifier when at least one
/// create-only property matches (partial match). The desired resource identity
/// is immutable at this boundary, so every match preserves the invariant that
/// an anonymous identity is derived from its current identity inputs.
///
/// `find_state_by_type` takes (provider, resource_type) and returns screened
/// state entries for that resource type with their create-only attribute
/// values. Existence checks inspect every entry, while heuristic matching only
/// inspects unclaimed candidates.
/// `find_state_by_binding` takes a binding name and returns state entries
/// carrying that binding across resource types, for resolving single-hop
/// deferred create-only refs of the exact form `binding.attr`. Binding
/// resolution is unresolvable when zero state entries match and ambiguous when
/// multiple state entries with the same binding are visible in scope; both
/// cases yield no value and are not errors. Dotted bindings are already
/// module-qualified by their binding string, while bare bindings only resolve
/// against dot-free root state entry names.
/// Binding lookup reads an exact address's attributes; it does not select an
/// identity to adopt or re-key, so it is not an identity-candidate source.
/// `data_sources` reserves resolved identities because active managed and
/// data-source rows later meet in wait-identifier and execution-result maps
/// keyed by `ResourceId`; reconciliation must not make those entries alias.
/// The reservation uses the state-row uniqueness key
/// `(provider, resource_type, identity)`. It deliberately excludes
/// `provider_instance`, matching reconciliation candidate lookup and
/// `StateFile::find_resource`.
///
/// Every matching branch returns a [`ResolvedMove`] from the matched state row
/// to the resource's resolved current address. This includes SimHash matches
/// for older identifier formats and unique full or partial create-only
/// matches. The move source carries the matched state row's provider-instance
/// route, while the destination carries the desired resource's route.
pub fn reconcile_anonymous_identifiers(
    resources: &[Resource],
    data_sources: &[DataSource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    claims: &StateBlockClaims,
) -> Vec<ResolvedMove> {
    let mut moves = Vec::new();
    let mut used_names: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for id in resources
        .iter()
        .map(|resource| &resource.id)
        .chain(data_sources.iter().map(|data_source| &data_source.id))
    {
        let Some(identity) = id.identity_str() else {
            continue;
        };
        let key = (id.provider.clone(), id.resource_type.clone());
        used_names
            .entry(key)
            .or_default()
            .insert(identity.to_string());
    }

    let mut claimed_names: HashMap<(String, String), HashSet<String>> = HashMap::new();

    for resource in resources {
        let Some(resolved_resource_id) = ResolvedResourceId::try_new(resource.id.clone()) else {
            continue;
        };
        let resource_identity = resolved_resource_id.identity_str().to_string();

        // Skip let-bound (named) resources entirely. Reconciliation is only
        // meaningful for anonymous hash-derived identifiers. Named resources
        // should never be rebound to a different state entry.
        if resource.binding.is_some() {
            continue;
        }

        if claims.claims_to(&resource.id) {
            continue;
        }

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();

        let create_only_attrs = schema.create_only_attributes();
        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);
        let key = (
            resource.id.provider.clone(),
            resource.id.resource_type.clone(),
        );

        // If the resource's name already exists in state, no reconciliation is needed.
        if state_entries.all().any(|e| e.name == resource_identity) {
            continue;
        }

        // Collect this resource's create-only values
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(value) = resource.get_attr(attr_name)
                && let Some(value) =
                    canonical_or_resolved_create_only_value_string(value, find_state_by_binding)
            {
                resource_co_values.insert(attr_name, value);
            }
        }

        if create_only_attrs.is_empty() || resource_co_values.is_empty() {
            // No create-only properties or none set: use SimHash-based Hamming distance
            // matching to find the closest state entry.
            let Some(AnonymousHashSuffix::SimHash(resource_hash)) =
                extract_hash_from_identifier(&resource_identity)
            else {
                continue;
            };

            let candidates = state_entries
                .candidates()
                .filter(|entry| entry.name != resource_identity)
                .filter(|entry| {
                    !used_names
                        .get(&key)
                        .is_some_and(|names| names.contains(&entry.name))
                        && !claimed_names
                            .get(&key)
                            .is_some_and(|names| names.contains(&entry.name))
                })
                .filter_map(|entry| match extract_hash_from_identifier(&entry.name) {
                    Some(AnonymousHashSuffix::SimHash(hash)) => Some((entry, hash)),
                    _ => None,
                });

            if let Some((state_entry, _)) =
                closest_unique_simhash_match(resource_hash, candidates, |(_, hash)| hash)
            {
                // The state entry may use an older identifier format (e.g.
                // pre-provider-prefix). Keep our freshly-computed new-format
                // name on the resource and record a move so the wiring
                // layer can re-key the state entry.
                let from = resolved_anonymous_state_id(
                    resolved_resource_id.provider(),
                    resolved_resource_id.resource_type(),
                    state_entry,
                );
                moves.push(ResolvedMove::from_detected_rename(
                    from,
                    resolved_resource_id.clone(),
                ));
                claimed_names
                    .entry(key)
                    .or_default()
                    .insert(state_entry.name.clone());
            }
            continue;
        }

        // Full matches handle no-edit upgrades where the hash input changed
        // across Carina versions but every create-only value is unchanged.
        // Prefer a unique full match over partial matches; if full matches are
        // ambiguous, skip rather than guessing. Partial matches keep the
        // existing "some create-only values changed" reconciliation behavior.
        // Entries already used by another current resource are live and must
        // not be stolen; entries claimed earlier in this pass are also excluded
        // so two resources cannot collapse onto the same state row.
        let mut full_matches: Vec<&AnonymousIdStateInfo> = Vec::new();
        let mut partial_matches: Vec<&AnonymousIdStateInfo> = Vec::new();
        for entry in state_entries.candidates() {
            if entry.name == resource_identity {
                // Same identifier, no reconciliation needed
                continue;
            }
            if used_names
                .get(&key)
                .is_some_and(|names| names.contains(&entry.name))
                || claimed_names
                    .get(&key)
                    .is_some_and(|names| names.contains(&entry.name))
            {
                continue;
            }

            // Compare create-only values: count matches and mismatches
            let mut matched = 0;
            let mut mismatched = 0;
            for (attr, value) in &resource_co_values {
                if let Some(state_value) = entry.create_only_values.get(*attr) {
                    let state_value = state_value.to_string();
                    if &state_value == value {
                        matched += 1;
                    } else {
                        mismatched += 1;
                    }
                }
            }

            if matched > 0 && mismatched == 0 {
                full_matches.push(entry);
            } else if matched > 0 && mismatched > 0 {
                partial_matches.push(entry);
            }
        }

        let matched_name = if full_matches.len() == 1 {
            Some(full_matches[0])
        } else if full_matches.is_empty() && partial_matches.len() == 1 {
            Some(partial_matches[0])
        } else {
            None
        };

        if let Some(matched_entry) = matched_name {
            let from = resolved_anonymous_state_id(
                resolved_resource_id.provider(),
                resolved_resource_id.resource_type(),
                matched_entry,
            );
            moves.push(ResolvedMove::from_detected_rename(
                from,
                resolved_resource_id,
            ));
            claimed_names
                .entry(key)
                .or_default()
                .insert(matched_entry.name.clone());
        }
    }
    moves
}

/// Detect let-bound (named) resources that were previously anonymous.
///
/// When a user converts an anonymous resource to a `let`-bound resource while
/// preserving the same create-only attributes, the old state entry (with a
/// hash-derived name) doesn't match the new binding name. Without this
/// detection the differ treats the change as delete + create, which for
/// destructive resources (e.g., `awscc.sso.Instance`) can wipe out live data.
///
/// Returns resolved moves from each old anonymous state row to the matched
/// named resource. The wiring layer applies them before running the differ.
///
/// Matching rules:
/// 1. Only `let`-bound resources are candidates (those with `binding.is_some()`)
/// 2. The resource's binding name must not already exist in state
/// 3. For resources whose schema has create-only attributes: there must be
///    exactly one orphaned anonymous state entry whose create-only attribute
///    values all match the new resource (ambiguous matches are skipped)
/// 4. For resources with no create-only attributes (e.g., `awscc.sso.Instance`):
///    fall back to SimHash Hamming-distance matching, using the same SimHash
///    that `compute_anonymous_identifiers` would have produced. This requires
///    `providers` and `identity_attributes_fn` so identity values (e.g.,
///    region) contribute to the hash just like they did at creation time.
///
/// An "orphaned" state entry is one whose name is not used by any current DSL
/// resource (so it would otherwise appear as a Delete in the plan).
pub fn detect_anonymous_to_named_renames(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    providers: &CanonicalizedProviderConfigs,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
    claims: &StateBlockClaims,
) -> Vec<ResolvedMove> {
    // Collect the set of resource names currently used in the DSL per
    // (provider, resource_type). Any state entry not in this set is an orphan.
    let mut used_names: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for resource in resources {
        let Some(identity) = resource.id.identity_str() else {
            continue;
        };
        let key = (
            resource.id.provider.clone(),
            resource.id.resource_type.clone(),
        );
        used_names
            .entry(key)
            .or_default()
            .insert(identity.to_string());
    }

    let mut renames = Vec::new();

    for resource in resources {
        // Only rename let-bound resources whose binding was previously anonymous.
        if resource.binding.is_none() {
            continue;
        }
        let Some(resource_identity) = resource.id.identity_str() else {
            continue;
        };
        if claims.claims_to(&resource.id) {
            continue;
        }

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();

        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);

        // Skip if the binding name already exists in state — nothing to rename.
        if state_entries.all().any(|e| e.name == resource_identity) {
            continue;
        }

        let used_in_dsl = used_names
            .get(&(
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            ))
            .cloned()
            .unwrap_or_default();

        // Collect this resource's create-only values (may be empty if the
        // schema has no create-only attributes or none are set).
        let create_only_attrs = schema.create_only_attributes();
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(Value::Concrete(ConcreteValue::String(v))) = resource.get_attr(attr_name) {
                resource_co_values.insert(attr_name, v.expose_plaintext().to_owned());
            }
        }

        let matched_entry: Option<&AnonymousIdStateInfo> = if !resource_co_values.is_empty() {
            // Create-only path: find orphaned entries whose create-only values all match.
            let mut matches: Vec<&AnonymousIdStateInfo> = Vec::new();
            for entry in state_entries.candidates() {
                if used_in_dsl.contains(&entry.name) {
                    continue;
                }
                if extract_hash_from_identifier(&entry.name).is_none() {
                    continue;
                }
                let all_match = resource_co_values.iter().all(|(attr, value)| {
                    entry
                        .create_only_values
                        .get(*attr)
                        .is_some_and(|v| v == value)
                });
                if all_match {
                    matches.push(entry);
                }
            }
            // Only rename on a unique match to avoid rebinding the wrong entry.
            if matches.len() == 1 {
                Some(matches[0])
            } else {
                None
            }
        } else {
            // SimHash fallback (rule 4 in the function doc). Pick the orphan
            // entry closest to the computed SimHash; tie → ambiguous, skip.
            let resource_hash =
                compute_resource_simhash(resource, providers, identity_attributes_fn);
            let candidates = state_entries
                .candidates()
                .filter(|e| !used_in_dsl.contains(&e.name))
                .filter_map(|e| match extract_hash_from_identifier(&e.name) {
                    Some(AnonymousHashSuffix::SimHash(hash)) => Some((e, hash)),
                    _ => None,
                });
            closest_unique_simhash_match(resource_hash, candidates, |(_, h)| h)
                .map(|(entry, _)| entry)
        };

        if let Some(entry) = matched_entry {
            let from = resolved_anonymous_state_id(
                &resource.id.provider,
                &resource.id.resource_type,
                entry,
            );
            let Some(to) = ResolvedResourceId::try_new(resource.id.clone()) else {
                continue;
            };
            renames.push(ResolvedMove::from_detected_rename(from, to));
        }
    }

    renames
}

#[cfg(test)]
pub fn detect_anonymous_to_named_renames_for_test(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    providers: &[ProviderConfig],
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
    claims: &StateBlockClaims,
) -> Vec<(ResolvedResourceId, ResolvedResourceId)> {
    let providers = CanonicalizedProviderConfigs::from_configs_for_test(providers.to_vec());
    detect_anonymous_to_named_renames(
        resources,
        registry,
        find_state_by_type,
        &providers,
        identity_attributes_fn,
        claims,
    )
    .into_iter()
    .map(ResolvedMove::into_parts)
    .collect()
}

#[cfg(test)]
mod tests;
