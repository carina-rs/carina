//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::effect::{
    ChangedCreateOnly, Effect, EffectWithSource, MoveAuthority, SavedEffect, TemporaryName,
};
use crate::hint::ProjectCommand;
use crate::module_resolver::ModuleConstraintDiagnostic;
use crate::name_override::NameOverride;
use crate::provider::{AppliedIdentifier, Timestamp};
use crate::resource::{
    DeposedKey, Directives, ResolvedResource, ResolvedResourceId, ResourceId, ResourceIdentity,
    Value,
};
pub use crate::resource::{ModuleSource, SavedModuleSource};

/// Error detected while constructing a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanError {
    /// The resource that triggered the error.
    pub resource_id: ResourceId,
    /// Structured reason for the plan-construction failure.
    pub kind: PlanErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreventDestroyAction {
    Replace,
    Delete,
    DeleteRemovedFromConfiguration,
    CascadeReplace,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error(
    "no schema registered for resource type '{resource_type}'; cannot plan create_before_destroy"
)]
pub struct SchemaNotRegisteredError {
    pub resource_type: String,
    /// The binding name (let-bound) or resolver identity (anonymous)
    /// of the resource that triggered the CBD path. For diagnostics only.
    pub resource_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error(
    "resource type '{resource_type}' does not support create_before_destroy: the replacement cannot coexist with the original"
)]
pub struct ReplacementCannotCoexistError {
    pub resource_type: String,
    /// The binding name (let-bound) or resolver identity (anonymous)
    /// of the resource that triggered the CBD path. For diagnostics only.
    pub resource_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum CreateBeforeDestroyError {
    #[error(transparent)]
    SchemaNotRegistered(#[from] SchemaNotRegisteredError),
    #[error(transparent)]
    ReplacementCannotCoexist(#[from] ReplacementCannotCoexistError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanErrorKind {
    PreventDestroy {
        action: PreventDestroyAction,
    },
    SchemaNotRegistered(SchemaNotRegisteredError),
    ReplacementCannotCoexist(ReplacementCannotCoexistError),
    /// An existing state row cannot safely be mutated because it has no
    /// provider identifier.
    MissingProviderIdentifier,
    /// An unfinished create is no longer present in configuration and cannot
    /// be resumed or safely deleted without its request.
    CreateInFlightOrphan,
    /// Destroy cannot identify an unfinished create yet; apply must resume it.
    DestroyCreateInFlight,
    /// The initial create may have reached the provider, but returned neither
    /// an identifier nor a continuation that could classify its outcome.
    CreateOutcomeUnknown {
        resource: ResourceId,
    },
    /// Desired provider routing no longer matches the route that issued the
    /// persisted create.
    ResumeCreateRoutingMismatch {
        persisted: ResourceId,
    },
    /// An import cannot identify a provider resource with an empty string.
    InvalidImportProviderIdentifier,
    /// A historical replacement generation cannot be destroyed because its
    /// persisted provider identifier is empty.
    MissingDeposedProviderIdentifier {
        key: DeposedKey,
        /// A healthy current generation that will also leave state when the
        /// `removed` block is applied and therefore needs a subsequent import.
        current_identifier: Option<AppliedIdentifier>,
    },
    WaitTargetMissing {
        wait_binding: String,
        target: String,
    },
    WaitTargetIsDataSource {
        wait_binding: String,
        target: String,
    },
    WaitPredicateInvalid {
        wait_binding: String,
        reason: String,
    },
    /// A schema value constraint failed after one or more authored
    /// references became known.
    ResolvedValueConstraint {
        /// Top-level resource attributes implicated by the failure.
        attributes: Vec<String>,
        /// Authored reference paths that supplied the newly known values.
        origins: Vec<String>,
        /// Schema or provider-validator diagnostic.
        message: String,
    },
    /// A module argument constraint failed after its inputs became known.
    ModuleConstraint(ModuleConstraintDiagnostic),
}

/// The sole resolution seam for a resource that is confirmed absent after an
/// outcome-unknown create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutcomeUnknownAbsentResolution {
    StateForget,
}

pub const fn create_outcome_unknown_absent_resolution() -> CreateOutcomeUnknownAbsentResolution {
    CreateOutcomeUnknownAbsentResolution::StateForget
}

impl CreateOutcomeUnknownAbsentResolution {
    fn hint(self, resource: &ResourceId, project_dir: &Path) -> String {
        match self {
            Self::StateForget => {
                let resource_type = resource.display_type().to_string();
                let identity = resource.identity_display().to_string();
                ProjectCommand::new("state forget", project_dir)
                    .with_argument(&resource_type)
                    .with_argument(&identity)
                    .to_string()
            }
        }
    }
}

fn create_outcome_unknown_address(resource: &ResourceId) -> String {
    let identity =
        crate::parser::display_double_quoted_literal_part(&resource.identity_display().to_string());
    format!("{} \"{identity}\"", resource.display_type())
}

impl PlanError {
    pub fn new(resource_id: ResourceId, kind: PlanErrorKind) -> Self {
        Self { resource_id, kind }
    }

    pub fn prevent_destroy(resource_id: ResourceId, action: PreventDestroyAction) -> Self {
        Self::new(resource_id, PlanErrorKind::PreventDestroy { action })
    }

    pub fn create_outcome_unknown(resource_id: ResourceId) -> Self {
        Self::new(
            resource_id.clone(),
            PlanErrorKind::CreateOutcomeUnknown {
                resource: resource_id,
            },
        )
    }

    /// Validate the provider identifier for one deposed generation.
    pub fn require_deposed_provider_identifier(
        resource_id: &ResourceId,
        key: &DeposedKey,
        identifier: &str,
        current_identifier: Option<&str>,
    ) -> Result<AppliedIdentifier, Box<Self>> {
        AppliedIdentifier::new(identifier).map_err(|_error| {
            Box::new(Self::new(
                resource_id.clone(),
                PlanErrorKind::MissingDeposedProviderIdentifier {
                    key: key.clone(),
                    current_identifier: current_identifier
                        .and_then(|identifier| AppliedIdentifier::new(identifier).ok()),
                },
            ))
        })
    }
}

impl From<CreateBeforeDestroyError> for PlanErrorKind {
    fn from(err: CreateBeforeDestroyError) -> Self {
        match err {
            CreateBeforeDestroyError::SchemaNotRegistered(err) => Self::SchemaNotRegistered(err),
            CreateBeforeDestroyError::ReplacementCannotCoexist(err) => {
                Self::ReplacementCannotCoexist(err)
            }
        }
    }
}

impl PlanErrorKind {
    fn prevent_destroy_message(action: PreventDestroyAction) -> &'static str {
        match action {
            PreventDestroyAction::Replace => {
                "resource has prevent_destroy set, but the plan would replace it (which requires destroying the old resource)"
            }
            PreventDestroyAction::Delete => {
                "resource has prevent_destroy set, but the plan would delete it"
            }
            PreventDestroyAction::DeleteRemovedFromConfiguration => {
                "resource has prevent_destroy set, but the plan would delete it (resource removed from configuration)"
            }
            PreventDestroyAction::CascadeReplace => {
                "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
            }
        }
    }
}

impl PlanErrorKind {
    fn fmt_with_project_dir(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        project_dir: &Path,
    ) -> std::fmt::Result {
        match self {
            Self::PreventDestroy { action } => f.write_str(Self::prevent_destroy_message(*action)),
            Self::SchemaNotRegistered(err) => write!(f, "{err}"),
            Self::ReplacementCannotCoexist(err) => write!(f, "{err}"),
            Self::MissingProviderIdentifier => f.write_str(
                "saved state has no provider identifier; add a `removed` block for this resource and apply it, then add an `import` block with the provider identifier to restore management",
            ),
            Self::CreateInFlightOrphan => f.write_str(
                "saved state contains a create still in progress, but the resource is absent from configuration; restore the resource and apply to resume it, or add a `removed` block to forget it (the cloud resource may remain)",
            ),
            Self::DestroyCreateInFlight => f.write_str(
                "saved state contains a create still in progress; run apply first to resume it before destroying",
            ),
            Self::CreateOutcomeUnknown { resource } => {
                let address = create_outcome_unknown_address(resource);
                let absent =
                    create_outcome_unknown_absent_resolution().hint(resource, project_dir);
                write!(
                    f,
                    "the create may have reached the cloud, but its outcome is unknown; if it exists: import {{ to = {address} id = \"…\" }}; if it doesn't: {absent}"
                )
            }
            Self::ResumeCreateRoutingMismatch { persisted } => {
                let route = persisted.provider_instance.as_deref().unwrap_or("default");
                write!(
                    f,
                    "saved create continuation belongs to provider route `{route}` for `{persisted}`, but the desired resource uses a different route; restore the original provider selection or forget the row with a `removed` block"
                )
            }
            Self::InvalidImportProviderIdentifier => f.write_str(
                "import id must be a non-empty provider identifier; set `id` in the `import` block to the identifier used by the provider",
            ),
            Self::MissingDeposedProviderIdentifier {
                key,
                current_identifier,
            } => {
                write!(
                    f,
                    "saved deposed generation `{key}` has no provider identifier and cannot be destroyed; add a `removed` block for this resource and apply it to abandon the unrecoverable generation from state"
                )?;
                if let Some(identifier) = current_identifier {
                    write!(
                        f,
                        ", then add an `import` block with provider identifier `{identifier}` to restore management of the healthy current instance"
                    )?;
                }
                Ok(())
            }
            Self::WaitTargetMissing {
                wait_binding,
                target,
            } => write!(
                f,
                "wait `{wait_binding}`: target binding `{target}` is not a known resource"
            ),
            Self::WaitTargetIsDataSource {
                wait_binding,
                target,
            } => write!(
                f,
                "wait `{wait_binding}`: target binding `{target}` is a data source; wait targets must be managed resources"
            ),
            Self::WaitPredicateInvalid {
                wait_binding,
                reason,
            } => write!(f, "wait `{wait_binding}`: {reason}"),
            Self::ResolvedValueConstraint {
                attributes,
                origins,
                message,
            } => {
                let attributes = attributes
                    .iter()
                    .map(|attribute| format!("`{attribute}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let origins = origins
                    .iter()
                    .map(|origin| format!("`{origin}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                match (attributes.is_empty(), origins.is_empty()) {
                    (false, false) => write!(
                        f,
                        "resolved value constraint failed for attribute(s) {attributes} from reference(s) {origins}: {message}"
                    ),
                    (false, true) => write!(
                        f,
                        "resolved value constraint failed for attribute(s) {attributes}: {message}"
                    ),
                    (true, false) => write!(
                        f,
                        "resolved value constraint failed for reference(s) {origins}: {message}"
                    ),
                    (true, true) => write!(f, "resolved value constraint failed: {message}"),
                }
            }
            Self::ModuleConstraint(diagnostic) => std::fmt::Display::fmt(diagnostic, f),
        }
    }
}

impl std::fmt::Display for PlanErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_with_project_dir(f, Path::new("."))
    }
}

impl PlanError {
    fn fmt_with_project_dir(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        project_dir: &Path,
    ) -> std::fmt::Result {
        match &self.kind {
            // A module constraint already names its module and authored call.
            // The synthetic `_virtual` id is internal correlation metadata,
            // not a second user-facing resource identity.
            PlanErrorKind::ModuleConstraint(diagnostic) => write!(f, "{diagnostic}"),
            kind @ (PlanErrorKind::PreventDestroy { .. }
            | PlanErrorKind::SchemaNotRegistered(_)
            | PlanErrorKind::ReplacementCannotCoexist(_)
            | PlanErrorKind::MissingProviderIdentifier
            | PlanErrorKind::CreateInFlightOrphan
            | PlanErrorKind::DestroyCreateInFlight
            | PlanErrorKind::CreateOutcomeUnknown { .. }
            | PlanErrorKind::ResumeCreateRoutingMismatch { .. }
            | PlanErrorKind::InvalidImportProviderIdentifier
            | PlanErrorKind::MissingDeposedProviderIdentifier { .. }
            | PlanErrorKind::WaitTargetMissing { .. }
            | PlanErrorKind::WaitTargetIsDataSource { .. }
            | PlanErrorKind::WaitPredicateInvalid { .. }
            | PlanErrorKind::ResolvedValueConstraint { .. }) => {
                write!(f, "{}: ", self.resource_id)?;
                kind.fmt_with_project_dir(f, project_dir)
            }
        }
    }

    /// Render this error with project-scoped remediation commands targeting
    /// the project that produced the plan.
    pub fn display_with_project_dir<'a>(
        &'a self,
        project_dir: &'a Path,
    ) -> impl std::fmt::Display + 'a {
        PlanErrorDisplay {
            error: self,
            project_dir,
        }
    }
}

struct PlanErrorDisplay<'a> {
    error: &'a PlanError,
    project_dir: &'a Path,
}

impl std::fmt::Display for PlanErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt_with_project_dir(f, self.project_dir)
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_with_project_dir(f, Path::new("."))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PermanentNameOverride {
    /// Identifies which resource the override targets.
    pub resource_id: ResolvedResourceId,
    /// The schema unique-name attribute whose value is overridden.
    pub attribute: String,
    /// The temporary value used on the cloud side.
    pub temp_value: String,
    /// The DSL value at the time the override was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplaceDisplayMetadata {
    /// Index into `Plan.effects` for the Create half of this replace.
    pub create_idx: usize,
    /// Index into `Plan.effects` for the Delete half of this replace.
    pub delete_idx: usize,
    /// Whether this was a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes whose change drove the replacement.
    pub changed_create_only: ChangedCreateOnly,
    /// Hints mapping attribute names to their original ResourceRef
    /// expressions (e.g. `("vpc_id", "vpc.vpc_id")`). Used by display
    /// to show the binding reference instead of the resolved value
    /// for cascade-triggered replacements.
    pub cascade_ref_hints: Vec<(String, String)>,
    /// Temporary name when CBD used a name swap. `None` for DBC.
    pub temporary_name: Option<TemporaryName>,
    /// Pre-replace state values. Used by display and snapshot tests;
    /// may contain secrets (the redaction walker visits this field
    /// in Phase 7).
    pub previous_attributes: HashMap<String, Value>,
}

/// Borrowed public view of replacement display metadata.
///
/// The plan still owns the serializable metadata internally, but display
/// frontends in other crates need enough information to render decomposed
/// replacement Create/Delete pairs as a single replacement row.
#[derive(Debug, Clone, Copy)]
pub struct ReplaceDisplayInfo<'a> {
    /// Index into `Plan.effects` for the Create half of this replace.
    pub create_idx: usize,
    /// Index into `Plan.effects` for the Delete half of this replace.
    pub delete_idx: usize,
    /// Whether this was a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes whose change drove the replacement.
    pub changed_create_only: &'a [String],
    /// Hints mapping attribute names to their original ResourceRef expressions.
    pub cascade_ref_hints: &'a [(String, String)],
    /// Temporary name when CBD used a name swap. `None` for DBC.
    pub temporary_name: Option<&'a TemporaryName>,
    /// Pre-replace state values used as the "from" side of display diffs.
    pub(crate) previous_attributes: &'a HashMap<String, Value>,
}

/// Opaque historical replacement values that may only be converted through
/// the persisted-state writeback encoder.
#[derive(Debug, Clone, Copy)]
pub struct ReplacementPreviousAttributesForStateWriteback<'a> {
    attributes: &'a HashMap<String, Value>,
}

impl<'a> ReplacementPreviousAttributesForStateWriteback<'a> {
    /// Borrow plan-time provider/history values for the state layer's sealed
    /// writeback selection step.
    pub fn provider_attributes(self) -> &'a HashMap<String, Value> {
        self.attributes
    }
}

impl<'a> ReplaceDisplayInfo<'a> {
    fn from_metadata(metadata: &'a ReplaceDisplayMetadata) -> Self {
        Self {
            create_idx: metadata.create_idx,
            delete_idx: metadata.delete_idx,
            create_before_destroy: metadata.create_before_destroy,
            changed_create_only: &metadata.changed_create_only,
            cascade_ref_hints: &metadata.cascade_ref_hints,
            temporary_name: metadata.temporary_name.as_ref(),
            previous_attributes: &metadata.previous_attributes,
        }
    }
}

pub(crate) struct ReplacementGroup {
    /// Create-side payload (identity-resolved).
    pub create: ResolvedResource,
    /// Delete-side payload.
    pub delete: ReplacementDelete,
    /// Whether this is a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes that drove the replacement.
    pub changed_create_only: ChangedCreateOnly,
    /// Cascade ref hints for display.
    pub cascade_ref_hints: Vec<(String, String)>,
    /// Temporary name when CBD swaps the name attribute. `None` for DBC.
    pub temporary_name: Option<TemporaryName>,
    /// Permanent name override recorded when CBD swaps the name attribute.
    /// `None` for DBC and CBD paths that did not need a temporary name.
    pub permanent_name_override: Option<PermanentNameOverride>,
    /// Identities of consumer effects (typically Updates) the Delete
    /// must wait for during apply. Populated into
    /// `Effect::Delete.blocked_by_updates`.
    pub consumer_updates: HashSet<ResourceIdentity>,
    /// Pre-replace attribute values, captured for display + snapshot.
    /// The redaction walker visits this in Phase 7.
    pub previous_attributes: HashMap<String, Value>,
}

pub(crate) struct ReplacementDelete {
    pub id: ResolvedResourceId,
    pub identifier: AppliedIdentifier,
    pub directives: Directives,
    pub binding: Option<String>,
    pub dependencies: HashSet<String>,
    pub explicit_dependencies: HashSet<String>,
}

/// A malformed historical generation explicitly abandoned by a `removed`
/// block because it has no provider identifier and therefore cannot be
/// deleted from the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AbandonedDeposedGeneration {
    id: ResolvedResourceId,
    key: DeposedKey,
}

/// Plan containing Effects to be executed
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize + Clone",
    deserialize = "S: Deserialize<'de>"
))]
pub struct Plan<S = ModuleSource> {
    effects: Vec<EffectWithSource<S>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) replace_display: Vec<ReplaceDisplayMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) permanent_name_overrides: Vec<PermanentNameOverride>,
    /// Errors detected during plan generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<PlanError>,
    /// Deposed generations a `removed` block must forget without a provider
    /// call because their persisted identifiers are unusable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    abandoned_deposed: Vec<AbandonedDeposedGeneration>,
}

impl<S> Default for Plan<S> {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            replace_display: Vec::new(),
            permanent_name_overrides: Vec::new(),
            errors: Vec::new(),
            abandoned_deposed: Vec::new(),
        }
    }
}

pub type SavedPlan = Plan<SavedModuleSource>;

impl From<Plan> for SavedPlan {
    fn from(plan: Plan) -> Self {
        let Plan {
            effects,
            replace_display,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
        } = plan;
        Self {
            effects: effects.into_iter().map(SavedEffect::from).collect(),
            replace_display,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
        }
    }
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a token-free resume effect and retain only its non-secret display
    /// timestamp. The provider token is joined from locked state at dispatch.
    pub fn add_resume_create(&mut self, resource: ResolvedResource, started_at: Timestamp) {
        self.effects.push(Effect::ResumeCreate {
            resource,
            started_at,
        });
    }

    pub(crate) fn add_replacement(&mut self, group: ReplacementGroup) {
        let delete = group.delete;
        let delete_effect = Effect::Delete {
            id: delete.id,
            identifier: delete.identifier,
            generation: crate::effect::EffectGeneration::Current,
            directives: delete.directives,
            binding: delete.binding,
            dependencies: delete.dependencies,
            explicit_dependencies: delete.explicit_dependencies,
            blocked_by_updates: group.consumer_updates,
        };

        let (create_idx, delete_idx) = if group.create_before_destroy {
            let create_idx = self.effects.len();
            self.effects.push(Effect::Create(group.create));
            let delete_idx = self.effects.len();
            self.effects.push(delete_effect);
            (create_idx, delete_idx)
        } else {
            let delete_idx = self.effects.len();
            self.effects.push(delete_effect);
            let create_idx = self.effects.len();
            self.effects.push(Effect::Create(group.create));
            (create_idx, delete_idx)
        };

        self.replace_display.push(ReplaceDisplayMetadata {
            create_idx,
            delete_idx,
            create_before_destroy: group.create_before_destroy,
            changed_create_only: group.changed_create_only,
            cascade_ref_hints: group.cascade_ref_hints,
            temporary_name: group.temporary_name,
            previous_attributes: group.previous_attributes,
        });

        if let Some(override_) = group.permanent_name_override {
            self.permanent_name_overrides.push(override_);
        }
    }
}

impl<S> Plan<S> {
    pub fn add(&mut self, effect: Effect<S>) {
        self.effects.push(effect);
    }

    pub fn effects(&self) -> &[Effect<S>] {
        &self.effects
    }

    /// Return the move capability for an exact route pair present in this
    /// plan.
    pub fn move_authority(&self, from: &ResourceId, to: &ResourceId) -> Option<MoveAuthority<'_>> {
        self.effects.iter().find_map(|effect| {
            let authority = effect.move_authority()?;
            (authority.from() == from && authority.to() == to).then_some(authority)
        })
    }

    /// Whether any typed value-bearing field contains a saved-plan secret
    /// placeholder.
    ///
    /// The owned plan and its replacement metadata are destructured
    /// exhaustively so a newly added field cannot silently escape the saved
    /// plan's verification gate.
    pub fn contains_redacted_secret_placeholders(&self) -> bool {
        let Plan {
            effects,
            replace_display,
            permanent_name_overrides: _permanent_name_overrides,
            errors: _errors,
            abandoned_deposed: _abandoned_deposed,
        } = self;

        effects
            .iter()
            .any(crate::value::effect_contains_redacted_secret)
            || replace_display.iter().any(|metadata| {
                let ReplaceDisplayMetadata {
                    create_idx: _create_idx,
                    delete_idx: _delete_idx,
                    create_before_destroy: _create_before_destroy,
                    changed_create_only: _changed_create_only,
                    cascade_ref_hints: _cascade_ref_hints,
                    temporary_name: _temporary_name,
                    previous_attributes,
                } = metadata;
                previous_attributes
                    .values()
                    .any(crate::value::contains_redacted_secret)
            })
    }

    /// Replacement display metadata for decomposed Create/Delete replacement pairs.
    pub fn replace_display_info(&self) -> impl Iterator<Item = ReplaceDisplayInfo<'_>> {
        self.replace_display
            .iter()
            .map(ReplaceDisplayInfo::from_metadata)
    }

    /// Historical replacement values used when state writeback must preserve
    /// an old generation after the provider result no longer carries it.
    pub fn replacement_previous_attributes_for_state_writeback(
        &self,
        delete_idx: usize,
    ) -> Option<ReplacementPreviousAttributesForStateWriteback<'_>> {
        self.replace_display
            .iter()
            .find(|metadata| metadata.delete_idx == delete_idx)
            .map(|metadata| ReplacementPreviousAttributesForStateWriteback {
                attributes: &metadata.previous_attributes,
            })
    }

    pub(crate) fn replace_display_mut(&mut self) -> &mut Vec<ReplaceDisplayMetadata> {
        &mut self.replace_display
    }

    pub fn permanent_name_overrides_for_state(
        &self,
    ) -> HashMap<ResourceId, HashMap<String, NameOverride>> {
        let mut overrides: HashMap<ResourceId, HashMap<String, NameOverride>> = HashMap::new();
        for override_ in &self.permanent_name_overrides {
            overrides
                .entry(override_.resource_id.as_inner().clone())
                .or_default()
                .insert(
                    override_.attribute.clone(),
                    NameOverride {
                        temp_value: override_.temp_value.clone(),
                        original_value: override_.original_value.clone(),
                    },
                );
        }
        overrides
    }

    /// Add persisted name-override metadata to a fixture without requiring
    /// the persisted plan representation to deserialize as a live plan.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn fixture_add_permanent_name_override(
        &mut self,
        resource_id: ResolvedResourceId,
        attribute: impl Into<String>,
        temp_value: impl Into<String>,
        original_value: Option<String>,
    ) {
        self.permanent_name_overrides.push(PermanentNameOverride {
            resource_id,
            attribute: attribute.into(),
            temp_value: temp_value.into(),
            original_value,
        });
    }

    pub(crate) fn effects_mut(&mut self) -> &mut Vec<Effect<S>> {
        &mut self.effects
    }

    // No `Plan::is_empty()`: it ambiguously straddled "no effects at
    // all" (display semantics) and "nothing to apply" (routing
    // semantics). Read/Wait effects are non-mutating but non-empty,
    // and routing on `is_empty()` mis-routed export-only configs
    // through the resource-apply pipeline (carina#3270, carina#3275).
    // Call sites must say what they mean:
    //   - display "no effects at all" → `plan.effects().is_empty()`
    //   - routing "nothing to apply"   → `!plan.has_mutations()`

    /// Add a plan-construction error.
    pub fn add_error(&mut self, error: PlanError) {
        self.errors.push(error);
    }

    /// Returns plan-construction errors.
    pub fn errors(&self) -> &[PlanError] {
        &self.errors
    }

    /// Returns true if there are plan-construction errors.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Remove plan-construction errors that don't satisfy the predicate.
    pub fn retain_errors<F>(&mut self, f: F)
    where
        F: FnMut(&PlanError) -> bool,
    {
        self.errors.retain(f);
    }

    /// Record one unrecoverable deposed generation that a matching `removed`
    /// block explicitly abandons from state.
    pub fn abandon_deposed_generation(&mut self, id: ResolvedResourceId, key: DeposedKey) {
        self.abandoned_deposed
            .push(AbandonedDeposedGeneration { id, key });
    }

    /// Deposed generations that state writeback must forget without treating
    /// them as successful provider deletes.
    pub fn abandoned_deposed_generations(
        &self,
    ) -> impl Iterator<Item = (&ResolvedResourceId, &DeposedKey)> {
        self.abandoned_deposed
            .iter()
            .map(|generation| (&generation.id, &generation.key))
    }

    /// Remove effects that don't satisfy the predicate
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&Effect<S>) -> bool,
    {
        let mut keep = f;
        let mut old_to_new = vec![None; self.effects.len()];
        let mut retained = Vec::with_capacity(self.effects.len());

        for (old_idx, effect) in self.effects.drain(..).enumerate() {
            if keep(&effect) {
                old_to_new[old_idx] = Some(retained.len());
                retained.push(effect);
            }
        }

        self.effects = retained;
        self.replace_display = self
            .replace_display
            .drain(..)
            .filter_map(|mut metadata| {
                let create_idx = old_to_new.get(metadata.create_idx).copied().flatten()?;
                let delete_idx = old_to_new.get(metadata.delete_idx).copied().flatten()?;
                metadata.create_idx = create_idx;
                metadata.delete_idx = delete_idx;
                Some(metadata)
            })
            .collect();
    }

    /// Number of mutating Effects
    pub fn mutation_count(&self) -> usize {
        self.effects.iter().filter(|e| e.is_mutating()).count()
    }

    /// True iff the plan contains at least one mutating effect.
    ///
    /// This is the **only** correct routing predicate for "does this
    /// plan need the resource-apply pipeline?". `Read`/`Wait` effects
    /// do not mutate infrastructure, so a plan that holds only those
    /// must take the export-only fast path (`persist_exports_only`)
    /// or short-circuit with "no changes". A predicate built on
    /// `effects().is_empty()` mis-routes any plan that carries a
    /// data-source read — every config with `let x = read aws.*`
    /// produces one (carina#3270 source-driven apply, carina#3275
    /// saved-plan apply).
    pub fn has_mutations(&self) -> bool {
        self.effects.iter().any(|e| e.is_mutating())
    }

    /// Generate a summary of the Plan for display
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        let deferred_summary = crate::plan_tree::deferred_summary_for_plan(self);
        let replacement_create_indices: HashSet<usize> = self
            .replace_display
            .iter()
            .map(|metadata| metadata.create_idx)
            .collect();
        let replacement_delete_indices: HashSet<usize> = self
            .replace_display
            .iter()
            .map(|metadata| metadata.delete_idx)
            .collect();

        summary.replace += self.replace_display.len();

        for (idx, effect) in self.effects.iter().enumerate() {
            match effect {
                Effect::Read { .. } => summary.read += 1,
                Effect::Create(_) => {
                    if !replacement_create_indices.contains(&idx) {
                        summary.create += 1;
                    }
                }
                Effect::ResumeCreate { .. } => summary.create += 1,
                Effect::Update { .. } => summary.update += 1,
                Effect::Delete { .. } => {
                    if !replacement_delete_indices.contains(&idx) {
                        summary.delete += 1;
                    }
                }
                Effect::DeferredReplace(_) => {}
                Effect::Import { .. } => summary.import += 1,
                Effect::Remove { .. } => summary.remove += 1,
                Effect::Move { .. } => summary.moved += 1,
                Effect::Wait { .. } => summary.wait += 1,
                Effect::DeferredCreate { .. } => {}
            }
        }
        summary.deferred = deferred_summary.entries;
        summary
    }
}

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub read: usize,
    pub create: usize,
    pub update: usize,
    pub replace: usize,
    pub delete: usize,
    pub deferred: Vec<DeferredSummaryEntry>,
    pub import: usize,
    pub remove: usize,
    pub moved: usize,
    pub wait: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredSummaryEntry {
    pub upstream_binding: String,
    pub verb: String,
    pub action: DeferredSummaryAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredSummaryAction {
    Add,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSummaryPart {
    Read { count: usize },
    Import { count: usize },
    Create { count: usize },
    Update { count: usize },
    Replace { count: usize },
    Delete { count: usize },
    Remove { count: usize },
    Move { count: usize },
    Wait { count: usize },
}

impl PlanSummary {
    pub fn parts(&self) -> Vec<PlanSummaryPart> {
        let mut parts = Vec::new();
        if self.read > 0 {
            parts.push(PlanSummaryPart::Read { count: self.read });
        }
        if self.import > 0 {
            parts.push(PlanSummaryPart::Import { count: self.import });
        }
        parts.push(PlanSummaryPart::Create { count: self.create });
        parts.push(PlanSummaryPart::Update { count: self.update });
        if self.replace > 0 {
            parts.push(PlanSummaryPart::Replace {
                count: self.replace,
            });
        }
        parts.push(PlanSummaryPart::Delete { count: self.delete });
        if self.remove > 0 {
            parts.push(PlanSummaryPart::Remove { count: self.remove });
        }
        if self.moved > 0 {
            parts.push(PlanSummaryPart::Move { count: self.moved });
        }
        if self.wait > 0 {
            parts.push(PlanSummaryPart::Wait { count: self.wait });
        }
        parts
    }

    pub fn render_line(&self) -> String {
        format!("Plan: {}", self.render_body())
    }

    pub fn render_body(&self) -> String {
        self.parts()
            .into_iter()
            .map(|part| match part {
                PlanSummaryPart::Read { count } => format!("{count} to read"),
                PlanSummaryPart::Import { count } => format!("{count} to import"),
                PlanSummaryPart::Create { count } => format!("{count} to create"),
                PlanSummaryPart::Update { count } => format!("{count} to update"),
                PlanSummaryPart::Replace { count } => format!("{count} to replace"),
                PlanSummaryPart::Delete { count } => format!("{count} to delete"),
                PlanSummaryPart::Remove { count } => format!("{count} to remove from state"),
                PlanSummaryPart::Move { count } => format!("{count} to move"),
                PlanSummaryPart::Wait { count } => format!("{count} to wait"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn deferred_lines(&self) -> Vec<String> {
        self.deferred
            .iter()
            .map(|entry| {
                let action = match entry.action {
                    DeferredSummaryAction::Add => "add",
                    DeferredSummaryAction::Replace => "replace",
                };
                format!(
                    "N to {action} after {} {}.",
                    entry.upstream_binding, entry.verb
                )
            })
            .collect()
    }
}

impl std::fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.render_line())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::DeferredReplacePayload;
    use crate::resource::{
        DataSource, ResolvedDataSource, ResolvedResource, Resource, ResourceIdentity,
    };

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
        ResolvedDataSource::new(resource)
    }

    fn deserialize_plan(json: &str) -> SavedPlan {
        serde_json::from_str::<SavedPlan>(json).unwrap()
    }

    fn deserialize_plan_value(value: serde_json::Value) -> SavedPlan {
        serde_json::from_value::<SavedPlan>(value).unwrap()
    }

    fn changed_create_only() -> ChangedCreateOnly {
        ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap()
    }

    fn cascade_ref_hints() -> Vec<(String, String)> {
        vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())]
    }

    fn temporary_name() -> TemporaryName {
        TemporaryName {
            attribute: "name".to_string(),
            original_value: "main".to_string(),
            temporary_value: "main-cbd".to_string(),
            can_rename: false,
        }
    }

    fn previous_attributes() -> HashMap<String, Value> {
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(crate::resource::ConcreteValue::String("10.0.0.0/16".into())),
        )])
    }

    fn permanent_name_override() -> PermanentNameOverride {
        PermanentNameOverride {
            resource_id: ResolvedResourceId::new(ResourceId::with_identity("ec2.Vpc", "vpc")),
            attribute: "name".to_string(),
            temp_value: "main-cbd".to_string(),
            original_value: Some("main".to_string()),
        }
    }

    fn replacement_group(consumer_updates: HashSet<ResourceIdentity>) -> ReplacementGroup {
        ReplacementGroup {
            create: resolved(Resource::new("ec2.Vpc", "vpc").with_binding("vpc")),
            delete: ReplacementDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "ec2.Vpc", "vpc-old",
                )),
                identifier: crate::effect::fixture_applied_identifier("vpc-123"),
                directives: Directives::default(),
                binding: Some("vpc".to_string()),
                dependencies: HashSet::from(["internet_gateway".to_string()]),
                explicit_dependencies: HashSet::from(["internet_gateway".to_string()]),
            },
            create_before_destroy: true,
            changed_create_only: changed_create_only(),
            cascade_ref_hints: cascade_ref_hints(),
            temporary_name: Some(temporary_name()),
            permanent_name_override: None,
            consumer_updates,
            previous_attributes: previous_attributes(),
        }
    }

    #[test]
    fn empty_plan() {
        let plan = Plan::new();
        assert!(plan.effects().is_empty());
        assert_eq!(plan.mutation_count(), 0);
        assert!(!plan.has_mutations());
    }

    /// carina#3270: a plan that holds only `Read` effects (data-source
    /// reads, with no managed-resource mutation) must report
    /// `has_mutations() == false`. The export-only apply path
    /// (`persist_exports_only`) gates on this; the old `is_empty()`
    /// check returned false for the same plan and mis-routed the
    /// run through the full resource-apply pipeline.
    #[test]
    fn read_only_plan_has_no_mutations() {
        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: resolved_data_source(DataSource::with_provider(
                "aws",
                "iam.Roles",
                "admin_access_roles",
                None,
            )),
        });
        // `effects().is_empty()` is false — Read counts as a present effect.
        assert!(!plan.effects().is_empty());
        // But there is no mutation, so the export-only fast path
        // must take this plan.
        assert!(!plan.has_mutations());
        assert_eq!(plan.mutation_count(), 0);
    }

    #[test]
    fn plan_with_create_has_mutations() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "acm.Certificate",
            "cert",
        ))));
        assert!(plan.has_mutations());
        assert_eq!(plan.mutation_count(), 1);
    }

    #[test]
    fn plan_summary_counts_wait() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "acm.Certificate",
            "cert",
        ))));
        plan.add(Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: crate::wait::ManagedWaitTarget::new(&resolved(Resource::new(
                "acm.Certificate",
                "cert",
            ))),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.wait, 1);
    }

    #[test]
    fn plan_summary() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "a"))));
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "b"))));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(
                crate::resource::ResourceId::with_identity("s3.Bucket", "c"),
            ),
            identifier: crate::effect::fixture_applied_identifier("c"),
            generation: crate::effect::EffectGeneration::Current,
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        });

        let summary = plan.summary();
        assert_eq!(summary.create, 2);
        assert_eq!(summary.delete, 1);
    }

    #[test]
    fn plan_summary_counts_decomposed_replacement_as_replace() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet",
        ))));
        plan.add_replacement(replacement_group(HashSet::new()));

        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.replace, 1);
        assert_eq!(summary.delete, 0);
    }

    #[test]
    fn plan_summary_records_deferred_adds() {
        use crate::parser::ForBinding;
        use crate::resource::ResourceId;

        let template_resource = Resource::new("route53.RecordSet", "validation_records[?]");
        let deferred = crate::parser::DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.RecordSet".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(
            Resource::new("acm.Certificate", "cert").with_binding("cert"),
        )));
        plan.add(Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.RecordSet",
                "validation_records",
            )),
            template: Box::new(deferred),
        });

        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.deferred.len(), 1);
        assert_eq!(summary.deferred[0].upstream_binding, "cert");
        assert_eq!(summary.deferred[0].action, DeferredSummaryAction::Add);
        assert!(summary.to_string().contains("1 to create"));
        assert_eq!(
            summary.deferred_lines(),
            vec!["N to add after cert applies."]
        );
    }

    #[test]
    fn plan_summary_excludes_deferred_replace_from_totals() {
        use crate::effect::{DeferredReplaceDelete, NonEmptyDeletes};
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::resource::{Directives, ResourceId};
        use std::collections::HashSet;

        let template_resource = Resource::new("route53.RecordSet", "validation_records[?]");
        let template = DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.RecordSet".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };
        let deletes = (0..3)
            .map(|idx| DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.RecordSet",
                    crate::resource::ResourceIdentity::try_from(format!(
                        "validation_records[{idx}]"
                    ))
                    .unwrap(),
                )),
                identifier: crate::effect::fixture_applied_identifier(format!("record-{idx}")),
                directives: Directives::default(),
                binding: Some(format!("validation_records[{idx}]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            })
            .collect();

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(deletes).expect("fixture has deletes"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.RecordSet",
                "validation_records",
            )),
            template: Box::new(template),
        })));

        let summary = plan.summary();
        assert_eq!(summary.replace, 0);
        assert_eq!(summary.delete, 0);
        assert_eq!(summary.deferred.len(), 1);
        assert_eq!(summary.deferred[0].upstream_binding, "cert");
        assert_eq!(summary.deferred[0].action, DeferredSummaryAction::Replace);
        assert_eq!(
            summary.deferred_lines(),
            vec!["N to replace after cert resolves."]
        );
    }

    #[test]
    fn plan_add_replacement_registers_create_delete_and_display_atomically() {
        let consumer_updates = HashSet::from([ResourceIdentity::new("subnet_update")]);
        let group = replacement_group(consumer_updates.clone());
        let expected_changed_create_only = group.changed_create_only.clone();
        let expected_cascade_ref_hints = group.cascade_ref_hints.clone();
        let expected_temporary_name = group.temporary_name.clone();
        let expected_previous_attributes = group.previous_attributes.clone();

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert_eq!(plan.effects().len(), 2);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        match &plan.effects()[1] {
            Effect::Delete {
                blocked_by_updates, ..
            } => assert_eq!(blocked_by_updates, &consumer_updates),
            other => panic!("expected Delete effect, got {other:?}"),
        }

        assert_eq!(plan.replace_display.len(), 1);
        let metadata = &plan.replace_display[0];
        assert_eq!(metadata.create_idx, 0);
        assert_eq!(metadata.delete_idx, 1);
        assert!(metadata.create_before_destroy);
        assert_eq!(metadata.changed_create_only, expected_changed_create_only);
        assert_eq!(metadata.cascade_ref_hints, expected_cascade_ref_hints);
        assert_eq!(metadata.temporary_name, expected_temporary_name);
        assert_eq!(metadata.previous_attributes, expected_previous_attributes);
    }

    #[test]
    fn replace_display_info_exposes_display_fields_without_metadata_visibility() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(HashSet::new()));

        let info = plan
            .replace_display_info()
            .next()
            .expect("replacement display info");
        let expected_changed = changed_create_only();
        let expected_hints = cascade_ref_hints();
        let expected_temporary_name = temporary_name();
        let expected_previous_attributes = previous_attributes();

        assert_eq!(info.create_idx, 0);
        assert_eq!(info.delete_idx, 1);
        assert!(info.create_before_destroy);
        assert_eq!(info.changed_create_only, &*expected_changed);
        assert_eq!(info.cascade_ref_hints, expected_hints.as_slice());
        assert_eq!(info.temporary_name, Some(&expected_temporary_name));
        assert_eq!(info.previous_attributes, &expected_previous_attributes);
    }

    #[test]
    fn plan_add_replacement_preserves_existing_effects_indices() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet-a",
        ))));
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet-b",
        ))));

        plan.add_replacement(replacement_group(HashSet::new()));

        assert_eq!(plan.effects().len(), 4);
        assert_eq!(plan.replace_display.len(), 1);
        assert_eq!(plan.replace_display[0].create_idx, 2);
        assert_eq!(plan.replace_display[0].delete_idx, 3);
    }

    #[test]
    fn plan_retain_remaps_replace_display_after_preceding_effect_removed() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet-a",
        ))));
        plan.add_replacement(replacement_group(HashSet::new()));

        plan.retain(|effect| {
            !matches!(
                effect,
                Effect::Create(resource) if resource.id.resource_type == "ec2.Subnet"
            )
        });

        assert_eq!(plan.effects().len(), 2);
        assert_eq!(plan.replace_display.len(), 1);
        assert_eq!(plan.replace_display[0].create_idx, 0);
        assert_eq!(plan.replace_display[0].delete_idx, 1);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        assert!(matches!(plan.effects()[1], Effect::Delete { .. }));
    }

    #[test]
    fn plan_retain_drops_replace_display_when_one_half_removed() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(HashSet::new()));

        plan.retain(|effect| !matches!(effect, Effect::Delete { .. }));

        assert_eq!(plan.effects().len(), 1);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        assert!(plan.replace_display.is_empty());
    }

    #[test]
    fn plan_replace_display_round_trips_through_serde() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(HashSet::from([ResourceIdentity::new(
            "subnet_update",
        )])));
        let expected_replace_display = plan.replace_display.clone();

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);

        assert_eq!(deserialized.effects().len(), 2);
        assert_eq!(deserialized.replace_display, expected_replace_display);
    }

    #[test]
    fn plan_add_replacement_pushes_permanent_name_override_on_cbd() {
        let override_ = permanent_name_override();
        let mut group = replacement_group(HashSet::new());
        group.permanent_name_override = Some(override_.clone());

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert_eq!(plan.permanent_name_overrides, vec![override_]);
    }

    #[test]
    fn plan_add_replacement_skips_permanent_name_override_on_dbc() {
        let mut group = replacement_group(HashSet::new());
        group.create_before_destroy = false;
        group.temporary_name = None;
        group.permanent_name_override = None;

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert!(plan.permanent_name_overrides.is_empty());
    }

    #[test]
    fn plan_error_kind_display_pins_prevent_destroy_actions() {
        let cases = [
            (
                PreventDestroyAction::Replace,
                "resource has prevent_destroy set, but the plan would replace it (which requires destroying the old resource)",
            ),
            (
                PreventDestroyAction::Delete,
                "resource has prevent_destroy set, but the plan would delete it",
            ),
            (
                PreventDestroyAction::DeleteRemovedFromConfiguration,
                "resource has prevent_destroy set, but the plan would delete it (resource removed from configuration)",
            ),
            (
                PreventDestroyAction::CascadeReplace,
                "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)",
            ),
        ];

        for (action, expected) in cases {
            assert_eq!(
                PlanErrorKind::PreventDestroy { action }.to_string(),
                expected
            );
        }
    }

    #[test]
    fn plan_error_kind_display_delegates_schema_not_registered_error() {
        let error = SchemaNotRegisteredError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "vpc".to_string(),
        };
        let expected = error.to_string();

        assert_eq!(
            PlanErrorKind::SchemaNotRegistered(error).to_string(),
            expected
        );
        assert_eq!(
            expected,
            "no schema registered for resource type 'ec2.Vpc'; cannot plan create_before_destroy"
        );
    }

    #[test]
    fn plan_error_kind_display_delegates_replacement_cannot_coexist_error() {
        let error = ReplacementCannotCoexistError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "vpc".to_string(),
        };
        let expected = error.to_string();

        assert_eq!(
            PlanErrorKind::ReplacementCannotCoexist(error).to_string(),
            expected
        );
        assert_eq!(
            expected,
            "resource type 'ec2.Vpc' does not support create_before_destroy: the replacement cannot coexist with the original"
        );
    }

    #[test]
    fn create_outcome_unknown_error_has_parseable_import_and_forget_hint() {
        let resource = ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "main-vpc",
            Some("management".to_string()),
        );
        let error = PlanError::create_outcome_unknown(resource);
        let rendered = error.to_string();

        assert_eq!(
            rendered,
            "awscc.ec2.Vpc.main-vpc: the create may have reached the cloud, but its outcome is unknown; if it exists: import { to = awscc.ec2.Vpc \"main-vpc\" id = \"…\" }; if it doesn't: carina state forget awscc.ec2.Vpc main-vpc"
        );
        let import = rendered
            .split_once("if it exists: ")
            .and_then(|(_prefix, resolutions)| resolutions.split_once("; if it doesn't:"))
            .map(|(import, _forget)| import)
            .expect("the rendered hint must contain both resolution branches");

        crate::parser::parse(import, &crate::parser::ProviderContext::default())
            .expect("the rendered single-line import form must parse");
    }

    #[test]
    fn create_outcome_unknown_forget_hint_keeps_the_project_dir() {
        let resource = ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "main vpc",
            Some("management".to_string()),
        );
        let error = PlanError::create_outcome_unknown(resource);

        assert!(
            error
                .display_with_project_dir(std::path::Path::new("infra/production project"))
                .to_string()
                .ends_with(
                    "if it doesn't: carina state forget awscc.ec2.Vpc 'main vpc' 'infra/production project'"
                )
        );
    }

    #[test]
    fn plan_error_kind_display_pins_wait_errors() {
        assert_eq!(
            PlanErrorKind::WaitTargetMissing {
                wait_binding: "cert_issued".to_string(),
                target: "cert".to_string(),
            }
            .to_string(),
            "wait `cert_issued`: target binding `cert` is not a known resource"
        );
        assert_eq!(
            PlanErrorKind::WaitTargetIsDataSource {
                wait_binding: "roles_ready".to_string(),
                target: "roles".to_string(),
            }
            .to_string(),
            "wait `roles_ready`: target binding `roles` is a data source; wait targets must be managed resources"
        );
        assert_eq!(
            PlanErrorKind::WaitPredicateInvalid {
                wait_binding: "cert_issued".to_string(),
                reason: "invalid predicate attribute path: empty attribute path".to_string(),
            }
            .to_string(),
            "wait `cert_issued`: invalid predicate attribute path: empty attribute path"
        );
    }

    #[test]
    fn plan_permanent_name_overrides_round_trips_through_serde() {
        let mut group = replacement_group(HashSet::new());
        group.permanent_name_override = Some(permanent_name_override());
        let mut plan = Plan::new();
        plan.add_replacement(group);

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);

        assert_eq!(deserialized, SavedPlan::from(plan));
    }

    #[test]
    fn plan_without_permanent_name_overrides_field_deserializes_to_empty() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "s3.Bucket",
            "bucket",
        ))));
        plan.add_error(PlanError::new(
            ResourceId::with_identity("s3.Bucket", "bucket"),
            PlanErrorKind::WaitTargetMissing {
                wait_binding: "wait_bucket".to_string(),
                target: "bucket".to_string(),
            },
        ));

        let mut json = serde_json::to_value(&plan).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("permanent_name_overrides");
        assert!(!object.contains_key("permanent_name_overrides"));

        let deserialized = deserialize_plan_value(json);
        assert!(deserialized.permanent_name_overrides.is_empty());
        assert_eq!(deserialized.effects().len(), 1);
        assert_eq!(deserialized.errors().len(), 1);
    }

    #[test]
    fn plan_without_replace_display_field_deserializes_to_empty() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "s3.Bucket",
            "bucket",
        ))));
        plan.add_error(PlanError::new(
            ResourceId::with_identity("s3.Bucket", "bucket"),
            PlanErrorKind::WaitTargetMissing {
                wait_binding: "wait_bucket".to_string(),
                target: "bucket".to_string(),
            },
        ));

        let mut json = serde_json::to_value(&plan).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("replace_display");
        assert!(!object.contains_key("replace_display"));

        let deserialized = deserialize_plan_value(json);
        assert!(deserialized.replace_display.is_empty());
        assert_eq!(deserialized.effects().len(), 1);
        assert_eq!(deserialized.errors().len(), 1);
    }

    #[test]
    fn plan_serde_round_trip() {
        use crate::resource::ResourceId;

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "a"))));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
            identifier: crate::effect::fixture_applied_identifier("b-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);
        assert_eq!(plan.effects().len(), deserialized.effects().len());
        let saved = SavedPlan::from(plan);
        assert_eq!(saved.effects(), deserialized.effects());
    }
}
