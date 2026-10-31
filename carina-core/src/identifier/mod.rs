//! Identifier and prefix handling for resources
//!
//! Functions for generating random suffixes, resolving attribute prefixes,
//! reconciling prefixed names with state, and computing anonymous resource identifiers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::differ::ComparisonContext;
use crate::effect::ResolvedMove;
use crate::parser::ProviderConfig;
use crate::parser::{SourceSpan, StateBlock, StateBlockAddress};
use crate::provider::{ComparisonResolutionError, ProviderFactory, ProviderRouter};
use crate::resource::{
    AnonymousIdentityModuleSource, ConcreteValue, DataSource, DeferredValue, LegacyLoopAddress,
    LoopElementOrigin, PendingReRead, ResolvedResourceId, Resource, ResourceId, ResourceIdentity,
    UnknownReason, Value, ValueString,
};
use crate::schema::{
    AttributeType, ResourceSchema, SchemaRegistry, Shape, ValidatedResourceSchema,
};
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
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => format!(
            "ModuleIf({}, {}, {})",
            deterministic_value_string(deferred.condition().value()),
            deterministic_value_string(deferred.then_value()),
            deterministic_value_string(deferred.else_value())
        ),
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
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => format!(
            "IntegerArithmetic({:?}, {}, {})",
            expression.op(),
            deterministic_value_string(expression.lhs()),
            deterministic_value_string(expression.rhs())
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

/// Stable concrete-value spelling used to decide whether a create-only value
/// can participate in reconciliation. This deliberately has no provider
/// context: anonymous identity hashing remains source-spelling based through
/// [`deterministic_value_string`], while comparison canonicalization happens
/// only at the matching seam below.
fn comparable_create_only_value_string(value: &Value) -> Option<String> {
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
                .map(comparable_create_only_feature_string)
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
                    format!("{:?}: {}", key, comparable_create_only_feature_string(item))
                })
                .collect();
            Some(format!("Map({{{}}})", parts.join(", ")))
        }
        _ => None,
    }
}

fn comparable_create_only_feature_string(value: &Value) -> String {
    comparable_create_only_value_string(value).unwrap_or_else(|| deterministic_value_string(value))
}

/// Canonical comparison spelling for one create-only value under the owning
/// resource's schema and initialized provider scope.
///
/// In particular, refined reference strings use the same
/// [`ComparisonContext::canonical_string`] projection as `type_aware_equal`,
/// so a bare reference and its long form cannot compare equal while producing
/// different prefix-match keys. Resolved candidates do not use this key; they
/// compare only through `type_aware_equal`.
fn canonical_create_only_value_string(
    value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_>,
) -> Option<String> {
    match (
        value,
        attr_type.map(|attr_type| attr_type.shape_with_defs(defs)),
    ) {
        (
            Value::Concrete(ConcreteValue::String(value)),
            Some(Shape::String {
                identity, to_dsl, ..
            }),
        ) => Some(
            context
                .canonical_string(value.expose_plaintext(), identity, to_dsl)
                .into_owned(),
        ),
        _ => comparable_create_only_value_string(value),
    }
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
    projected_id: ResolvedResourceId,
    origin_id: ResolvedResourceId,
    create_only_values: StateCreateOnlyValues,
    identity_values: StateIdentityValues,
}

impl AnonymousIdStateInfo {
    /// Build one reconciliation row from persisted values and an optional
    /// already-applied projection of that same row.
    ///
    /// The full typed ids keep the effective name, persisted origin, and
    /// provider route together. A projection across provider or resource type
    /// is rejected instead of allowing callers to assemble inconsistent
    /// identity fields.
    pub fn try_from_state_attributes(
        origin_id: ResolvedResourceId,
        projected_id: Option<ResolvedResourceId>,
        schema: &ValidatedResourceSchema,
        attributes: &HashMap<String, Value>,
    ) -> Option<Self> {
        Self::try_from_comparable_values(
            origin_id,
            projected_id,
            StateCreateOnlyValues::from_state_attributes(schema, attributes),
            StateIdentityValues::from_state_attributes(schema, attributes),
        )
    }

    /// Build an address-only row when no resource schema is available.
    ///
    /// Exact legacy-address migration can still use such a row when there is
    /// only one possible owner. It cannot participate in stable-value
    /// disambiguation or create-only reconciliation.
    pub fn try_without_comparable_attributes(
        origin_id: ResolvedResourceId,
        projected_id: Option<ResolvedResourceId>,
    ) -> Option<Self> {
        Self::try_from_comparable_values(
            origin_id,
            projected_id,
            StateCreateOnlyValues::default(),
            StateIdentityValues::default(),
        )
    }

    fn try_from_comparable_values(
        origin_id: ResolvedResourceId,
        projected_id: Option<ResolvedResourceId>,
        create_only_values: StateCreateOnlyValues,
        identity_values: StateIdentityValues,
    ) -> Option<Self> {
        let projected_id = projected_id.unwrap_or_else(|| origin_id.clone());
        if origin_id.provider() != projected_id.provider()
            || origin_id.resource_type() != projected_id.resource_type()
        {
            return None;
        }
        Some(Self {
            projected_id,
            origin_id,
            create_only_values,
            identity_values,
        })
    }

    pub fn name(&self) -> &str {
        self.projected_id.identity_str()
    }

    pub fn origin_id(&self) -> &ResolvedResourceId {
        &self.origin_id
    }

    pub fn projected_id(&self) -> &ResolvedResourceId {
        &self.projected_id
    }

    pub fn provider_instance(&self) -> Option<&str> {
        self.projected_id.provider_instance()
    }

    pub fn create_only_values(&self) -> &StateCreateOnlyValues {
        &self.create_only_values
    }

    /// Build a detected move from this validated persisted/projected state row.
    pub fn move_to(&self, to: ResolvedResourceId) -> ResolvedMove {
        ResolvedMove::from_detected_rename(self.origin_id.clone(), self.projected_id.clone(), to)
    }
}

/// Canonical state-side values for schema identity attributes that are not
/// also create-only. Attributes in both sets are owned exclusively by the
/// create-only seam.
#[derive(Clone, Default)]
struct StateIdentityValues {
    values: HashMap<String, ResolvedCreateOnlyValue>,
}

impl StateIdentityValues {
    fn from_state_attributes(
        schema: &ValidatedResourceSchema,
        observed_attributes: &HashMap<String, Value>,
    ) -> Self {
        let mut canonical = observed_attributes.clone();
        crate::value::canonicalize_state_attribute_map_with_schema(&mut canonical, schema);
        let resource_schema = schema.as_schema();
        let create_only = resource_schema
            .create_only_attributes()
            .into_iter()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        let values = resource_schema
            .identity_attributes()
            .into_iter()
            .filter(|attribute| !create_only.contains(*attribute))
            .filter_map(|attribute| {
                canonical
                    .get(attribute)
                    .cloned()
                    .and_then(ResolvedCreateOnlyValue::from_value)
                    .map(|value| (attribute.to_string(), value))
            })
            .collect();
        Self { values }
    }
}

/// Desired schema identity values used only to disambiguate legacy loop
/// owners. Create-only attributes, including overlaps, are represented by
/// [`DesiredCreateOnlyValues`] instead.
struct DesiredIdentityValues {
    values: HashMap<String, DesiredCreateOnlyValue>,
}

impl DesiredIdentityValues {
    fn from_resource(
        resource: &Resource,
        schema: &ResourceSchema,
        find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    ) -> Self {
        let create_only = schema
            .create_only_attributes()
            .into_iter()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        let values = desired_comparable_values(
            resource,
            schema,
            schema
                .identity_attributes()
                .into_iter()
                .filter(|attribute| !create_only.contains(*attribute)),
            find_state_by_binding,
            true,
        );
        Self { values }
    }

    fn resolved_subset_match(
        &self,
        state_values: &StateIdentityValues,
        schema: &ResourceSchema,
        context: &ComparisonContext<'_>,
    ) -> (usize, bool) {
        resolved_subset_match(&self.values, &state_values.values, schema, context)
    }
}

fn legacy_row_stable_identity_matches(
    candidate: &LegacyLoopMoveCandidate<'_>,
    state_entry: &AnonymousIdStateInfo,
    registry: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Result<Option<bool>, ComparisonResolutionError> {
    let Some(schema) = registry.get_for(candidate.resource) else {
        return Ok(None);
    };
    let comparison = comparison_provider.comparison_for(registry, &candidate.resource.id)?;
    let schema = schema.as_schema();
    let desired_create_only =
        DesiredCreateOnlyValues::from_resource(candidate.resource, schema, find_state_by_binding);
    let desired_identity =
        DesiredIdentityValues::from_resource(candidate.resource, schema, find_state_by_binding);
    if state_entry.create_only_values.is_empty() && state_entry.identity_values.values.is_empty() {
        return Ok(None);
    }
    let (create_only_count, create_only_matches) = desired_create_only.resolved_subset_match(
        state_entry.create_only_values(),
        schema,
        comparison.context(),
    );
    let (identity_count, identity_matches) = desired_identity.resolved_subset_match(
        &state_entry.identity_values,
        schema,
        comparison.context(),
    );
    Ok(((create_only_count + identity_count) > 0)
        .then_some(create_only_matches && identity_matches))
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
    comparison_provider: &ProviderRouter,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    claims: &StateBlockClaims,
    reserved_moves: &[ResolvedMove],
) -> Result<Vec<ResolvedMove>, ComparisonResolutionError> {
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
            .filter(|entry| {
                entry.name() == group_key.2 && entry.provider_instance() == group_key.3.as_deref()
            })
            .collect();
        let [state_entry] = exact_rows.as_slice() else {
            continue;
        };
        let source_key = loop_address_key(state_entry.origin_id());
        let projected_source_key = loop_address_key(state_entry.projected_id());
        if claims.claims_from(state_entry.origin_id().as_inner())
            || reserved_sources.contains(&source_key)
            || desired_ids.contains(&projected_source_key)
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
                        entry.name() == other.destination.identity_str()
                            && entry.provider_instance() == other.destination.provider_instance()
                    })
            })
            .collect();
        let selected = match eligible.as_slice() {
            [only] => match legacy_row_stable_identity_matches(
                only,
                state_entry,
                registry,
                comparison_provider,
                find_state_by_binding,
            )? {
                Some(true) | None => Some(*only),
                Some(false) => None,
            },
            [] => None,
            multiple => {
                let mut stable_matches = Vec::new();
                for candidate in multiple.iter().copied() {
                    if legacy_row_stable_identity_matches(
                        candidate,
                        state_entry,
                        registry,
                        comparison_provider,
                        find_state_by_binding,
                    )? == Some(true)
                    {
                        stable_matches.push(candidate);
                    }
                }
                match stable_matches.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                }
            }
        };

        if let Some(selected) = selected {
            migrations.push(state_entry.move_to(selected.destination.clone()));
            used_sources.insert(source_key);
        }
    }
    Ok(migrations)
}

/// Cross-resource state information used to resolve simple deferred
/// create-only references like `route_table_id = private_rtb.id`.
#[derive(Clone)]
pub struct AnonymousIdBindingStateInfo {
    /// The resource name stored in state. Let-bound resources are usually
    /// named by their binding (`private_rtb`, `inst.x`).
    pub name: String,
    /// Persisted attribute values keyed by DSL attribute name (the state JSON
    /// attribute key). The consuming create-only schema canonicalizes them.
    pub attribute_values: HashMap<String, Value>,
}

fn state_entry_matches_binding_scope(binding: &str, state_name: &str) -> bool {
    binding.contains('.') || !state_name.contains('.')
}

fn resolve_deferred_create_only_value(
    value: &Value,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Option<Value> {
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

fn canonical_or_resolved_create_only_value(
    value: &Value,
    schema: &ResourceSchema,
    attribute: &str,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Result<ResolvedCreateOnlyValue, UnresolvedReason> {
    let canonical = if let Some(resolved) =
        resolve_deferred_create_only_value(value, find_state_by_binding)
    {
        crate::value::canonicalize_state_attribute_value_with_schema(resolved, schema, attribute)
    } else {
        crate::value::canonicalize_desired_attribute_value_with_schema(
            value.clone(),
            schema,
            attribute,
        )
        .map_err(UnresolvedReason::Type)?
    };
    ResolvedCreateOnlyValue::from_value(canonical).ok_or(UnresolvedReason::NotComparable)
}

#[derive(Clone)]
struct ResolvedCreateOnlyValue {
    value: Value,
}

impl ResolvedCreateOnlyValue {
    fn from_value(value: Value) -> Option<Self> {
        comparable_create_only_value_string(&value)?;
        Some(Self { value })
    }

    fn semantically_equal(
        &self,
        other: &Self,
        schema: &ResourceSchema,
        attribute: &str,
        context: &ComparisonContext<'_>,
    ) -> bool {
        schema
            .attributes
            .get(attribute)
            .is_some_and(|attribute_schema| {
                crate::differ::type_aware_equal(
                    &self.value,
                    &other.value,
                    Some(&attribute_schema.attr_type),
                    &schema.defs,
                    context,
                )
            })
    }

    fn starts_with(
        &self,
        prefix: &str,
        schema: &ResourceSchema,
        attribute: &str,
        context: &ComparisonContext<'_>,
    ) -> bool {
        let Some(attribute_schema) = schema.attributes.get(attribute) else {
            return false;
        };
        let state_key = canonical_create_only_value_string(
            &self.value,
            Some(&attribute_schema.attr_type),
            &schema.defs,
            context,
        );
        let prefix_key = canonical_create_only_value_string(
            &Value::Concrete(ConcreteValue::String(prefix.into())),
            Some(&attribute_schema.attr_type),
            &schema.defs,
            context,
        );
        state_key
            .zip(prefix_key)
            .is_some_and(|(state, prefix)| state.starts_with(&prefix))
    }
}

/// Whether one configured desired create-only value can be compared yet.
enum DesiredCreateOnlyValue {
    Resolved(ResolvedCreateOnlyValue),
    Prefix(String),
    Unresolved,
}

impl DesiredCreateOnlyValue {
    fn matches_state(
        &self,
        state: &ResolvedCreateOnlyValue,
        schema: &ResourceSchema,
        attribute: &str,
        context: &ComparisonContext<'_>,
    ) -> Option<bool> {
        match self {
            Self::Resolved(desired) => {
                Some(desired.semantically_equal(state, schema, attribute, context))
            }
            Self::Prefix(prefix) => Some(state.starts_with(prefix, schema, attribute, context)),
            Self::Unresolved => None,
        }
    }
}

#[derive(Debug)]
enum UnresolvedReason {
    NotComparable,
    Type(crate::schema::TypeError),
}

/// Desired create-only values selected by a resource schema.
///
/// Every configured desired attribute is retained as either a resolved value
/// or an unresolved marker. Persisted values use the separate
/// [`StateCreateOnlyValues`] type so unresolved desired values cannot enter the
/// state side of a comparison.
pub struct DesiredCreateOnlyValues {
    values: HashMap<String, DesiredCreateOnlyValue>,
}

fn desired_comparable_values<'attribute, S>(
    resource: &Resource<S>,
    schema: &ResourceSchema,
    attributes: impl IntoIterator<Item = &'attribute str>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    prefer_prefixes: bool,
) -> HashMap<String, DesiredCreateOnlyValue> {
    attributes
        .into_iter()
        .filter_map(|attribute| {
            if prefer_prefixes && let Some(prefix) = resource.prefixes.get(attribute) {
                return Some((
                    attribute.to_string(),
                    DesiredCreateOnlyValue::Prefix(prefix.clone()),
                ));
            }
            resource.get_attr(attribute).map(|value| {
                let value = match canonical_or_resolved_create_only_value(
                    value,
                    schema,
                    attribute,
                    find_state_by_binding,
                ) {
                    Ok(value) => DesiredCreateOnlyValue::Resolved(value),
                    Err(error) => {
                        match error {
                            UnresolvedReason::NotComparable => tracing::debug!(
                                attribute,
                                reason = "value has no stable comparison form",
                                "stable value is not available for identity comparison"
                            ),
                            UnresolvedReason::Type(error) => tracing::debug!(
                                attribute,
                                error = ?error,
                                "stable value is not available for identity comparison"
                            ),
                        }
                        DesiredCreateOnlyValue::Unresolved
                    }
                };
                (attribute.to_string(), value)
            })
        })
        .collect()
}

fn resolved_subset_match(
    desired_values: &HashMap<String, DesiredCreateOnlyValue>,
    state_values: &HashMap<String, ResolvedCreateOnlyValue>,
    schema: &ResourceSchema,
    context: &ComparisonContext<'_>,
) -> (usize, bool) {
    let mut resolved = 0;
    let mut all_match = true;
    for (attribute, desired) in desired_values {
        let Some(matches) = state_values
            .get(attribute)
            .and_then(|state| desired.matches_state(state, schema, attribute, context))
        else {
            if !matches!(desired, DesiredCreateOnlyValue::Unresolved) {
                resolved += 1;
                all_match = false;
            }
            continue;
        };
        resolved += 1;
        all_match &= matches;
    }
    (resolved, all_match)
}

impl DesiredCreateOnlyValues {
    /// Extract create-only values configured on a resource, resolving simple
    /// state-backed binding references through `find_state_by_binding`.
    pub fn from_resource<S>(
        resource: &Resource<S>,
        schema: &ResourceSchema,
        find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    ) -> Self {
        let values = desired_comparable_values(
            resource,
            schema,
            schema.create_only_attributes(),
            find_state_by_binding,
            true,
        );
        Self { values }
    }

    /// Extract desired values when binding resolution is intentionally not
    /// available, such as post-diff moved-block hinting.
    pub fn without_binding_resolution<S>(resource: &Resource<S>, schema: &ResourceSchema) -> Self {
        Self::from_resource(resource, schema, &|_binding| Vec::new())
    }

    /// Whether at least one create-only attribute was configured.
    pub fn has_configured_values(&self) -> bool {
        !self.values.is_empty()
    }

    fn has_resolved_values(&self) -> bool {
        self.values.values().any(|value| {
            matches!(
                value,
                DesiredCreateOnlyValue::Resolved(_) | DesiredCreateOnlyValue::Prefix(_)
            )
        })
    }

    /// Whether every configured create-only attribute has a comparable value.
    pub fn all_configured_values_resolved(&self) -> bool {
        self.values.values().all(|value| {
            matches!(
                value,
                DesiredCreateOnlyValue::Resolved(_) | DesiredCreateOnlyValue::Prefix(_)
            )
        })
    }

    fn resolved_subset_match(
        &self,
        state_values: &StateCreateOnlyValues,
        schema: &ResourceSchema,
        context: &ComparisonContext<'_>,
    ) -> (usize, bool) {
        resolved_subset_match(&self.values, &state_values.values, schema, context)
    }

    fn match_counts(
        &self,
        state_values: &StateCreateOnlyValues,
        schema: &ResourceSchema,
        context: &ComparisonContext<'_>,
    ) -> (usize, usize) {
        let mut matched = 0;
        let mut mismatched = 0;
        for (attribute, desired_value) in &self.values {
            let Some(state_value) = state_values.values.get(attribute) else {
                continue;
            };
            let Some(matches) =
                desired_value.matches_state(state_value, schema, attribute, context)
            else {
                continue;
            };
            if matches {
                matched += 1;
            } else {
                mismatched += 1;
            }
        }
        (matched, mismatched)
    }

    /// Compare every configured desired value with canonical persisted state
    /// using the resource schema's type-aware equality rules.
    pub fn all_match(
        &self,
        state_values: &StateCreateOnlyValues,
        schema: &ResourceSchema,
        context: &ComparisonContext<'_>,
    ) -> bool {
        self.values.iter().all(|(attribute, desired)| {
            let Some(state) = state_values.values.get(attribute) else {
                return false;
            };
            desired
                .matches_state(state, schema, attribute, context)
                .unwrap_or(false)
        })
    }
}

/// Comparable create-only values selected from state-side observations.
///
/// Inputs may come from a persisted state row or from provider-refreshed
/// [`crate::resource::Current`] attributes. Construction applies each schema attribute's
/// `canonicalize_state` conversion before comparison. Unlike
/// [`DesiredCreateOnlyValues`], this type cannot represent an unresolved
/// configured value.
#[derive(Clone, Default)]
pub struct StateCreateOnlyValues {
    values: HashMap<String, ResolvedCreateOnlyValue>,
}

impl StateCreateOnlyValues {
    /// Canonicalize and select create-only values from observed state-side
    /// attributes. Missing or non-comparable attributes remain absent.
    pub fn from_state_attributes(
        schema: &ValidatedResourceSchema,
        observed_attributes: &HashMap<String, Value>,
    ) -> Self {
        let mut canonical = observed_attributes.clone();
        crate::value::canonicalize_state_attribute_map_with_schema(&mut canonical, schema);
        let values = schema
            .as_schema()
            .create_only_attributes()
            .into_iter()
            .filter_map(|attribute| {
                canonical
                    .get(attribute)
                    .cloned()
                    .and_then(ResolvedCreateOnlyValue::from_value)
                    .map(|value| (attribute.to_string(), value))
            })
            .collect();
        Self { values }
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
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
/// A configured create-only attribute whose desired value cannot be resolved
/// is represented as `Unresolved`. Reconciliation deliberately excludes it
/// from both the matched and mismatched counts, preserving the existing
/// partial-match behavior. If none of the configured values resolve, the
/// existing SimHash fallback remains in effect.
/// `data_sources` reserves resolved identities because active managed and
/// data-source rows later meet in wait-identifier and execution-result maps
/// keyed by `ResourceId`; reconciliation must not make those entries alias.
/// The reservation uses the state-row uniqueness key
/// `(provider, resource_type, identity)`. It deliberately excludes
/// `provider_instance`, matching reconciliation candidate lookup and
/// `StateFile::find_resource`.
///
/// Every matching branch returns a [`ResolvedMove`] from the matched persisted
/// row to the resource's resolved current address. This includes SimHash
/// matches for older identifier formats and unique full or partial create-only
/// matches. When an earlier move has already projected that row, the move also
/// retains the projected source used for in-memory transfer. The persisted
/// origin and projected source each keep their own provider-instance route;
/// the destination carries the desired resource's route.
pub fn reconcile_anonymous_identifiers(
    resources: &[Resource],
    data_sources: &[DataSource],
    registry: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    claims: &StateBlockClaims,
) -> Result<Vec<ResolvedMove>, ComparisonResolutionError> {
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
        let comparison = comparison_provider.comparison_for(registry, &resource.id)?;
        let schema = schema.as_schema();

        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);
        let key = (
            resource.id.provider.clone(),
            resource.id.resource_type.clone(),
        );

        // If the resource's name already exists in state, no reconciliation is needed.
        if state_entries
            .all()
            .any(|entry| entry.name() == resource_identity)
        {
            continue;
        }

        let resource_co_values =
            DesiredCreateOnlyValues::from_resource(resource, schema, find_state_by_binding);

        if !resource_co_values.has_resolved_values() {
            // No create-only properties or none set: use SimHash-based Hamming distance
            // matching to find the closest state entry.
            let Some(AnonymousHashSuffix::SimHash(resource_hash)) =
                extract_hash_from_identifier(&resource_identity)
            else {
                continue;
            };

            let candidates = state_entries
                .candidates()
                .filter(|entry| entry.name() != resource_identity)
                .filter(|entry| {
                    !used_names
                        .get(&key)
                        .is_some_and(|names| names.contains(entry.name()))
                        && !claimed_names
                            .get(&key)
                            .is_some_and(|names| names.contains(entry.name()))
                })
                .filter_map(|entry| match extract_hash_from_identifier(entry.name()) {
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
                moves.push(state_entry.move_to(resolved_resource_id.clone()));
                claimed_names
                    .entry(key)
                    .or_default()
                    .insert(state_entry.name().to_string());
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
            if entry.name() == resource_identity {
                // Same identifier, no reconciliation needed
                continue;
            }
            if used_names
                .get(&key)
                .is_some_and(|names| names.contains(entry.name()))
                || claimed_names
                    .get(&key)
                    .is_some_and(|names| names.contains(entry.name()))
            {
                continue;
            }

            let (matched, mismatched) = resource_co_values.match_counts(
                entry.create_only_values(),
                schema,
                comparison.context(),
            );

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
            moves.push(matched_entry.move_to(resolved_resource_id));
            claimed_names
                .entry(key)
                .or_default()
                .insert(matched_entry.name().to_string());
        }
    }
    Ok(moves)
}

/// Full desired-resource context plus the subset still eligible and the
/// origins already claimed by earlier anonymous-to-named fixed-point passes.
pub struct AnonymousToNamedCandidates<'a> {
    resources: &'a [Resource],
    eligible_ids: &'a HashSet<ResourceId>,
    claimed_origins: &'a AnonymousToNamedClaimedOrigins,
}

impl<'a> AnonymousToNamedCandidates<'a> {
    /// Keep all desired addresses visible for orphan screening while limiting
    /// move detection to `eligible_ids` and retaining earlier-pass claims.
    pub fn new(
        resources: &'a [Resource],
        eligible_ids: &'a HashSet<ResourceId>,
        claimed_origins: &'a AnonymousToNamedClaimedOrigins,
    ) -> Self {
        Self {
            resources,
            eligible_ids,
            claimed_origins,
        }
    }

    fn eligible(&self) -> impl Iterator<Item = &Resource> {
        self.resources
            .iter()
            .filter(|resource| self.eligible_ids.contains(&resource.id))
    }
}

/// Persisted origins already assigned by earlier anonymous-to-named passes.
///
/// Keeping the incumbent desired address with each origin lets a later pass
/// report both sides of a cross-pass contest without inferring ownership from
/// the projected state name.
#[derive(Default)]
pub struct AnonymousToNamedClaimedOrigins {
    claimant_by_origin: HashMap<ResourceId, ResourceId>,
}

impl AnonymousToNamedClaimedOrigins {
    /// Index accepted detected moves by their persisted origins.
    pub fn from_moves(moves: &[ResolvedMove]) -> Self {
        let claimant_by_origin = moves
            .iter()
            .map(|resolved_move| {
                (
                    resolved_move.from().as_inner().clone(),
                    resolved_move.to().as_inner().clone(),
                )
            })
            .collect();
        Self { claimant_by_origin }
    }

    fn claimant_for(&self, origin: &ResolvedResourceId) -> Option<&ResourceId> {
        self.claimant_by_origin.get(origin.as_inner())
    }

    fn contains(&self, origin: &ResolvedResourceId) -> bool {
        self.claimant_for(origin).is_some()
    }
}

/// A desired resource whose full create-only match is an origin assigned in
/// an earlier fixed-point pass.
pub struct AnonymousToNamedCrossPassContest {
    origin: ResolvedResourceId,
    incumbent: ResourceId,
    contender: ResourceId,
}

impl AnonymousToNamedCrossPassContest {
    /// Return the contested origin, incumbent destination, and later
    /// contender destination.
    pub fn into_parts(self) -> (ResolvedResourceId, ResourceId, ResourceId) {
        (self.origin, self.incumbent, self.contender)
    }
}

/// Moves accepted in one detection pass plus contests against earlier passes.
pub struct AnonymousToNamedRenameDetection {
    moves: Vec<ResolvedMove>,
    cross_pass_contests: Vec<AnonymousToNamedCrossPassContest>,
}

impl AnonymousToNamedRenameDetection {
    /// Separate accepted moves from contests against earlier passes.
    pub fn into_parts(self) -> (Vec<ResolvedMove>, Vec<AnonymousToNamedCrossPassContest>) {
        (self.moves, self.cross_pass_contests)
    }
}

/// State indexes used while matching named resources to anonymous state rows.
pub struct AnonymousToNamedStateLookup<'a> {
    find_by_type: &'a dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    find_by_binding: &'a dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
}

impl<'a> AnonymousToNamedStateLookup<'a> {
    pub fn new(
        find_by_type: &'a dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
        find_by_binding: &'a dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    ) -> Self {
        Self {
            find_by_type,
            find_by_binding,
        }
    }
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
/// `find_state_by_binding` resolves the same single-hop `binding.attr`
/// references as [`reconcile_anonymous_identifiers`]; missing or ambiguous
/// binding rows leave that create-only value unresolved. Unlike anonymous
/// reconciliation's documented partial matching, rename detection never uses
/// a create-only subset: every configured value must be resolved before the
/// create-only path is eligible.
///
/// Matching rules:
/// 1. Only `let`-bound resources are candidates (those with `binding.is_some()`)
/// 2. The resource's binding name must not already exist in state
/// 3. For resources with fully resolved configured create-only attributes:
///    exactly one orphaned anonymous state entry whose values all match wins.
///    Ambiguous matches are skipped without a fallback. If otherwise-unique
///    matches from multiple desired resources select the same orphan in one
///    pass, every claim on that orphan is also skipped. A full match on an
///    origin claimed in an earlier pass is reported as a cross-pass contest,
///    also without a SimHash fallback.
/// 4. For resources with no configured create-only attributes, or with a
///    fully resolved create-only set that matches zero state rows, fall back
///    to SimHash Hamming-distance matching among rows that do not explicitly
///    contradict any desired create-only value. This uses the same SimHash
///    that `compute_anonymous_identifiers` would have produced. A partially
///    unresolved desired set does not fall back because its identity inputs
///    are not known well enough to select a prior row safely.
///
/// An "orphaned" state entry is one whose name is not used by any current DSL
/// resource (so it would otherwise appear as a Delete in the plan).
pub fn detect_anonymous_to_named_renames(
    candidates: AnonymousToNamedCandidates<'_>,
    registry: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
    state_lookup: AnonymousToNamedStateLookup<'_>,
    providers: &CanonicalizedProviderConfigs,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
    claims: &StateBlockClaims,
) -> Result<AnonymousToNamedRenameDetection, ComparisonResolutionError> {
    // Collect the set of resource names currently used in the DSL per
    // (provider, resource_type). Any state entry not in this set is an orphan.
    let mut used_names: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for resource in candidates.resources {
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
    let mut cross_pass_contests = Vec::new();

    for resource in candidates.eligible() {
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
        let comparison = comparison_provider.comparison_for(registry, &resource.id)?;
        let schema = schema.as_schema();

        let state_entries =
            (state_lookup.find_by_type)(&resource.id.provider, &resource.id.resource_type);

        // Skip if the binding name already exists in state — nothing to rename.
        if state_entries
            .all()
            .any(|entry| entry.name() == resource_identity)
        {
            continue;
        }

        let used_in_dsl = used_names
            .get(&(
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            ))
            .cloned()
            .unwrap_or_default();

        // This is empty only when the resource configures no create-only
        // attributes. Configured-but-unresolved values remain explicit.
        let resource_co_values =
            DesiredCreateOnlyValues::from_resource(resource, schema, state_lookup.find_by_binding);

        let simhash_match = || {
            let resource_hash =
                compute_resource_simhash(resource, providers, identity_attributes_fn);
            let candidates = state_entries
                .candidates()
                .filter(|entry| !used_in_dsl.contains(entry.name()))
                .filter(|entry| !candidates.claimed_origins.contains(entry.origin_id()))
                .filter(|entry| {
                    let (_matched, mismatched) = resource_co_values.match_counts(
                        entry.create_only_values(),
                        schema,
                        comparison.context(),
                    );
                    mismatched == 0
                })
                .filter_map(|entry| match extract_hash_from_identifier(entry.name()) {
                    Some(AnonymousHashSuffix::SimHash(hash)) => Some((entry, hash)),
                    _ => None,
                });
            closest_unique_simhash_match(resource_hash, candidates, |(_, hash)| hash)
                .map(|(entry, _)| entry)
        };

        let matched_entry: Option<&AnonymousIdStateInfo> = if resource_co_values
            .has_configured_values()
            && resource_co_values.all_configured_values_resolved()
        {
            // Create-only path: find orphaned entries whose create-only values all match.
            let mut matches: Vec<&AnonymousIdStateInfo> = Vec::new();
            let mut prior_claimed_matches = Vec::new();
            for entry in state_entries.candidates() {
                let prior_claimant = candidates.claimed_origins.claimant_for(entry.origin_id());
                let anonymous_identity = prior_claimant
                    .map_or_else(|| entry.name(), |_| entry.origin_id().identity_str());
                if extract_hash_from_identifier(anonymous_identity).is_none() {
                    continue;
                }
                let all_match = resource_co_values.all_match(
                    entry.create_only_values(),
                    schema,
                    comparison.context(),
                );
                if !all_match {
                    continue;
                }
                if let Some(incumbent) = prior_claimant {
                    prior_claimed_matches.push((entry, incumbent));
                } else if !used_in_dsl.contains(entry.name()) {
                    matches.push(entry);
                }
            }
            if prior_claimed_matches.is_empty() {
                match matches.as_slice() {
                    [entry] => Some(*entry),
                    [] => simhash_match(),
                    _ => None,
                }
            } else {
                cross_pass_contests.extend(prior_claimed_matches.into_iter().map(
                    |(entry, incumbent)| AnonymousToNamedCrossPassContest {
                        origin: entry.origin_id().clone(),
                        incumbent: incumbent.clone(),
                        contender: resource.id.clone(),
                    },
                ));
                None
            }
        } else if resource_co_values.has_configured_values() {
            // At least one configured value is unresolved. Neither a subset
            // comparison nor SimHash is safe enough to select a prior row.
            None
        } else {
            simhash_match()
        };

        if let Some(entry) = matched_entry {
            let Some(to) = ResolvedResourceId::try_new(resource.id.clone()) else {
                continue;
            };
            renames.push(entry.move_to(to));
        }
    }

    let mut claims_per_origin = HashMap::<ResourceId, usize>::new();
    for resolved_move in &renames {
        *claims_per_origin
            .entry(resolved_move.from().as_inner().clone())
            .or_default() += 1;
    }
    renames.retain(|resolved_move| {
        claims_per_origin
            .get(resolved_move.from().as_inner())
            .is_some_and(|count| *count == 1)
    });

    Ok(AnonymousToNamedRenameDetection {
        moves: renames,
        cross_pass_contests,
    })
}

#[cfg(test)]
pub fn detect_anonymous_to_named_renames_for_test(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> ScreenedStateEntries<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    providers: &[ProviderConfig],
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
    claims: &StateBlockClaims,
) -> Vec<(ResolvedResourceId, ResolvedResourceId)> {
    let providers = CanonicalizedProviderConfigs::from_configs_for_test(providers.to_vec());
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        registry,
        resources.iter().map(|resource| &resource.id),
    );
    let candidates = resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect();
    let claimed_origins = AnonymousToNamedClaimedOrigins::default();
    let (moves, contests) = detect_anonymous_to_named_renames(
        AnonymousToNamedCandidates::new(resources, &candidates, &claimed_origins),
        registry,
        &comparison_provider,
        AnonymousToNamedStateLookup::new(find_state_by_type, find_state_by_binding),
        &providers,
        identity_attributes_fn,
        claims,
    )
    .expect("anonymous-to-named comparison fixture must resolve")
    .into_parts();
    debug_assert!(contests.is_empty());
    moves.into_iter().map(ResolvedMove::into_parts).collect()
}

#[cfg(test)]
mod tests;
