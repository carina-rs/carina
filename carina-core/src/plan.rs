//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

// Admission and Plan own the only conversion between compiled endpoints and
// their private scheduling projection. Executors cannot detach that projection.
#[path = "replacement.rs"]
pub mod replacement;

use crate::resource::ResolvedDependencies;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::effect::{
    ChangedCreateOnly, DeferredReplaceDelete, DeletedInstanceKey, Effect, EffectGeneration,
    EffectWithSource, MoveAuthority, TemporaryName, UnresolvedSavedEffect,
};
use crate::hint::ProjectCommand;
use crate::module_resolver::ModuleConstraintDiagnostic;
use crate::name_override::NameOverride;
use crate::provider::{AppliedIdentifier, Timestamp};
use crate::replacement::{CheckedReplacement, ReplacementInputs, ReplacementScheduling};
#[cfg(test)]
use crate::resource::Directives;
pub use crate::resource::PreventDestroySource;
use crate::resource::{
    DeposedKey, NoPlaceholder, PlanValueKind, ResolvedResource, ResolvedResourceId, ResourceId,
    SnapshotLeaf, SnapshotValue,
};
pub use crate::resource::{ModuleSource, SavedModuleSource};

const SCHEMA_DEFAULT_PREVENT_DESTROY_OPT_OUT: &str =
    "; to allow this, write `directives { prevent_destroy = false }`";

/// Error detected while constructing a plan.
///
/// Rendering requires the project directory because some variants contain a
/// project-scoped recovery command:
///
/// ```compile_fail
/// use carina_core::plan::PlanError;
/// use carina_core::resource::ResourceId;
///
/// let error = PlanError::create_outcome_unknown(ResourceId::with_provider_identity(
///     "awscc",
///     "ec2.Vpc",
///     "main",
///     None,
/// ));
/// let _ = error.to_string();
/// ```
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
#[error("prepared replacement name for '{resource_type}.{attribute}' does not match the target")]
pub struct InvalidPreparedReplacementNameError {
    pub resource_type: String,
    pub attribute: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum CreateBeforeDestroyError {
    #[error(transparent)]
    InvalidPreparedReplacementName(#[from] InvalidPreparedReplacementNameError),
    #[error(transparent)]
    SchemaNotRegistered(#[from] SchemaNotRegisteredError),
    #[error(transparent)]
    ReplacementCannotCoexist(#[from] ReplacementCannotCoexistError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanErrorKind {
    InputRule {
        path: String,
        code: String,
        reason: String,
        context: crate::input_rules::InputWriteContext,
    },
    PreventDestroy {
        action: PreventDestroyAction,
        source: PreventDestroySource,
    },
    SchemaNotRegistered(SchemaNotRegisteredError),
    ReplacementCannotCoexist(ReplacementCannotCoexistError),
    InvalidPreparedReplacementName(InvalidPreparedReplacementNameError),
    /// A referenced producer cannot coexist with its old generation, but a
    /// managed or deferred dependent requires its replacement first.
    CreateBeforeDestroyRequiredByDependent {
        dependent: String,
        cause: CreateBeforeDestroyError,
    },
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
        reason: crate::provider_text::DiagnosticText,
    },
    /// A schema value constraint failed after one or more authored
    /// references became known.
    ResolvedValueConstraint {
        /// Top-level resource attributes implicated by the failure.
        attributes: Vec<String>,
        /// Authored reference paths that supplied the newly known values.
        origins: Vec<String>,
        /// Schema or provider-validator diagnostic.
        message: crate::provider_text::DiagnosticText,
    },
    /// Runtime-only classifier evidence cannot be written into a saved plan.
    #[serde(skip)]
    Knowledge {
        attribute: String,
        error: Box<crate::resource::knowledge::KnowledgeError>,
    },
    /// Resolving a transient constraint candidate must retain typed failures.
    #[serde(skip)]
    ValueResolution {
        attribute: String,
        error: Box<crate::resolver::ResolutionError>,
    },
    /// An update would have to resend an ignored write-only value that the
    /// provider cannot return on read.
    IgnoredWriteOnlyValueUnavailable {
        attribute: String,
        path: String,
    },
    /// A module argument constraint failed after its inputs became known.
    ModuleConstraint(ModuleConstraintDiagnostic),
    UnavailableSecret(crate::resource::UnavailableSecretError),
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

    pub fn prevent_destroy(
        resource_id: ResourceId,
        action: PreventDestroyAction,
        source: PreventDestroySource,
    ) -> Self {
        Self::new(
            resource_id,
            PlanErrorKind::PreventDestroy { action, source },
        )
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
            CreateBeforeDestroyError::InvalidPreparedReplacementName(err) => {
                Self::InvalidPreparedReplacementName(err)
            }
            CreateBeforeDestroyError::SchemaNotRegistered(err) => Self::SchemaNotRegistered(err),
            CreateBeforeDestroyError::ReplacementCannotCoexist(err) => {
                Self::ReplacementCannotCoexist(err)
            }
        }
    }
}

impl From<crate::provider::IgnoredWriteOnlyValueError> for PlanErrorKind {
    fn from(error: crate::provider::IgnoredWriteOnlyValueError) -> Self {
        Self::IgnoredWriteOnlyValueUnavailable {
            attribute: error.attribute().to_string(),
            path: error.path().to_string(),
        }
    }
}

impl From<crate::provider::UpdatePatchError> for PlanErrorKind {
    fn from(error: crate::provider::UpdatePatchError) -> Self {
        match error {
            crate::provider::UpdatePatchError::IgnoredWriteOnly(error) => error.into(),
            crate::provider::UpdatePatchError::UnavailableSecret(error) => {
                Self::UnavailableSecret(error)
            }
        }
    }
}

impl PlanErrorKind {
    fn explicit_prevent_destroy_message(action: PreventDestroyAction) -> &'static str {
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

    fn schema_default_prevent_destroy_message_fragment(
        action: PreventDestroyAction,
    ) -> &'static str {
        match action {
            PreventDestroyAction::Replace => {
                "resource type is protected from destruction by default, but the plan would replace it (which requires destroying the old resource)"
            }
            PreventDestroyAction::Delete => {
                "resource type is protected from destruction by default, but the plan would delete it"
            }
            PreventDestroyAction::DeleteRemovedFromConfiguration => {
                "resource type is protected from destruction by default, but the plan would delete it (resource removed from configuration)"
            }
            PreventDestroyAction::CascadeReplace => {
                "resource type is protected from destruction by default, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
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
        struct Context<'a> {
            kind: &'a PlanErrorKind,
            project_dir: &'a Path,
        }
        impl std::fmt::Display for Context<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.kind.fmt_context(f, self.project_dir)
            }
        }
        std::fmt::Display::fmt(
            &crate::provider_text::DiagnosticText::new(
                Context {
                    kind: self,
                    project_dir,
                }
                .to_string(),
            ),
            f,
        )
    }

    fn fmt_context(&self, f: &mut std::fmt::Formatter<'_>, project_dir: &Path) -> std::fmt::Result {
        match self {
            Self::InputRule { path, code, reason, context } => {
                let operation = match context {
                    crate::input_rules::InputWriteContext::Update => "update",
                    crate::input_rules::InputWriteContext::AuthoredDesired => "write input",
                    crate::input_rules::InputWriteContext::Create
                    | crate::input_rules::InputWriteContext::Replacement { .. }
                    | crate::input_rules::InputWriteContext::ResumeCreate
                    | crate::input_rules::InputWriteContext::DeferredCreate
                    | crate::input_rules::InputWriteContext::DeferredReplacement => "create",
                };
                write!(f, "cannot {operation}: {path} [{code}]: {reason}")
            }
            Self::PreventDestroy { action, source } => match source {
                PreventDestroySource::ExplicitDirective => {
                    f.write_str(Self::explicit_prevent_destroy_message(*action))
                }
                PreventDestroySource::SchemaDefault => {
                    f.write_str(Self::schema_default_prevent_destroy_message_fragment(*action))?;
                    f.write_str(SCHEMA_DEFAULT_PREVENT_DESTROY_OPT_OUT)
                }
            },
            Self::SchemaNotRegistered(err) => write!(f, "{err}"),
            Self::ReplacementCannotCoexist(err) => write!(f, "{err}"),
            Self::InvalidPreparedReplacementName(err) => write!(f, "{err}"),
            Self::CreateBeforeDestroyRequiredByDependent { dependent, cause } => {
                write!(f, "{cause}; create_before_destroy required by dependent `{dependent}`")
            }
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
                        ", then add an `import` block with provider identifier `{}` to restore management of the healthy current instance",
                        crate::provider_text::DiagnosticText::new(identifier.as_str())
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
            Self::Knowledge { attribute, error } => write!(f, "{attribute}: {error}"),
            Self::ValueResolution { attribute, error } => write!(f, "{attribute}: {error}"),
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
            Self::UnavailableSecret(error) => std::fmt::Display::fmt(error, f),
            Self::IgnoredWriteOnlyValueUnavailable { attribute, path } => write!(
                f,
                "cannot send attribute `{attribute}` because ignored write-only path `{path}` must be preserved, but its current value cannot be known"
            ),
        }
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
            PlanErrorKind::Knowledge { attribute, error } => {
                write!(f, "{}.{attribute}: {error}", self.resource_id)
            }
            PlanErrorKind::ValueResolution { attribute, error } => {
                write!(f, "{}.{attribute}: {error}", self.resource_id)
            }
            kind @ (PlanErrorKind::PreventDestroy { .. }
            | PlanErrorKind::InputRule { .. }
            | PlanErrorKind::SchemaNotRegistered(_)
            | PlanErrorKind::ReplacementCannotCoexist(_)
            | PlanErrorKind::InvalidPreparedReplacementName(_)
            | PlanErrorKind::CreateBeforeDestroyRequiredByDependent { .. }
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
            | PlanErrorKind::UnavailableSecret(_)
            | PlanErrorKind::IgnoredWriteOnlyValueUnavailable { .. }
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
    pub previous_attributes: HashMap<String, SnapshotValue>,
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
    pub(crate) previous_attributes: &'a HashMap<String, SnapshotValue>,
}

/// Opaque historical replacement values that may only be converted through
/// the persisted-state writeback encoder.
#[derive(Debug, Clone, Copy)]
pub struct ReplacementPreviousAttributesForStateWriteback<'a> {
    attributes: &'a HashMap<String, SnapshotValue>,
}

impl<'a> ReplacementPreviousAttributesForStateWriteback<'a> {
    /// Borrow plan-time provider/history values for the state layer's sealed
    /// writeback selection step.
    pub fn provider_attributes(self) -> &'a HashMap<String, SnapshotValue> {
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
    pub replacement: CheckedReplacement<ResolvedResource>,
    pub changed_create_only: ChangedCreateOnly,
    pub cascade_ref_hints: Vec<(String, String)>,
    pub consumer_updates: ResolvedDependencies,
}

/// A malformed historical generation explicitly abandoned by a `removed`
/// block because it has no provider identifier and therefore cannot be
/// deleted from the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AbandonedDeposedGeneration {
    id: ResolvedResourceId,
    key: DeposedKey,
}

/// Display-only count of readable drift hidden by `ignore_changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredChangeSummary {
    resource_id: ResourceId,
    count: usize,
}

/// Completeness of the historical ordering attached to one Delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DestroyOrdering {
    Known,
    UnresolvedPersistedDependencies { dependencies: ResolvedDependencies },
}

/// Typed association between a Delete generation and its ordering status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestroyOrderingNotice {
    id: ResolvedResourceId,
    generation: EffectGeneration,
    ordering: DestroyOrdering,
}

impl DestroyOrderingNotice {
    pub fn id(&self) -> &ResolvedResourceId {
        &self.id
    }

    pub fn generation(&self) -> &EffectGeneration {
        &self.generation
    }

    pub fn ordering(&self) -> &DestroyOrdering {
        &self.ordering
    }
}

impl IgnoredChangeSummary {
    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

/// Mutable plan inputs. Rendering and execution require [`AdmittedPlan`].
/// Decode [`UnresolvedSavedPlan`] and resolve it with schemas to load a plan.
///
/// ```compile_fail,E0277
/// use carina_core::plan::SavedPlan;
/// let unchecked: SavedPlan = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(bound(serialize = "S: Serialize + Clone, V: PlanValueKind"))]
pub struct Plan<S = ModuleSource, V: PlanValueKind = NoPlaceholder> {
    effects: Vec<EffectWithSource<S, V>>,
    #[serde(skip)]
    replacements: ReplacementScheduling,
    #[serde(default, skip_serializing_if = "ReplacementInputs::is_empty")]
    pub(crate) replacement_inputs: Box<ReplacementInputs>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignored_change_summaries: Vec<IgnoredChangeSummary>,
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
    /// Display-only evidence that one legacy orphan/deposed Delete could not
    /// recover every historical edge. Known edges stay on the Delete itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    destroy_ordering_notices: Vec<DestroyOrderingNotice>,
}

impl<S: PartialEq, V: PlanValueKind> PartialEq for Plan<S, V> {
    fn eq(&self, other: &Self) -> bool {
        self.effects == other.effects
            && self.replacement_inputs == other.replacement_inputs
            && self.ignored_change_summaries == other.ignored_change_summaries
            && self.replace_display == other.replace_display
            && self.permanent_name_overrides == other.permanent_name_overrides
            && self.errors == other.errors
            && self.abandoned_deposed == other.abandoned_deposed
            && self.destroy_ordering_notices == other.destroy_ordering_notices
    }
}

impl<S, V: PlanValueKind> Default for Plan<S, V> {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            replacements: ReplacementScheduling::default(),
            replacement_inputs: Box::default(),
            ignored_change_summaries: Vec::new(),
            replace_display: Vec::new(),
            permanent_name_overrides: Vec::new(),
            errors: Vec::new(),
            abandoned_deposed: Vec::new(),
            destroy_ordering_notices: Vec::new(),
        }
    }
}

pub type SavedPlan = Plan<SavedModuleSource, SnapshotLeaf>;
pub type SnapshotPlan<S = ModuleSource> = Plan<S, SnapshotLeaf>;

/// An immutable plan whose final effects have passed replacement admission.
/// Transformations operate on its draft view and return a [`Plan`], so their
/// output must cross [`Plan::admit`] before rendering. Execution additionally
/// requires the core-finalized [`crate::input_rules::InputCheckedPlan`] witness.
///
/// A draft cannot enter the display boundary:
///
/// ```compile_fail,E0308
/// use carina_core::{detail_rows::{PlanDisplay, HistoricalDisplayAttributes, DetailComparisons}, plan::Plan};
/// let draft = Plan::new();
/// let historical = HistoricalDisplayAttributes::for_plan(&draft);
/// PlanDisplay::unprepared(&draft, &historical, DetailComparisons::RecordedEffects);
/// ```
///
/// A draft cannot enter the executor either:
///
/// ```compile_fail,E0308
/// use carina_core::{executor::LiveExecutionInput, plan::Plan};
/// fn bypass<'a>(input: &mut LiveExecutionInput<'a>, draft: &'a Plan) {
///     input.plan = draft;
/// }
/// ```
///
/// Admission cannot survive mutation:
///
/// ```compile_fail,E0596
/// use carina_core::{effect::Effect, plan::AdmittedPlan};
/// fn change(plan: &mut AdmittedPlan, effect: Effect) {
///     plan.add(effect);
/// }
/// ```
///
/// A transformed plan cannot retain the execution capability:
///
/// ```compile_fail,E0308
/// use carina_core::{plan::AdmittedPlan, value::rehydrate_plan_historical_secrets_from_verified_states};
/// use std::collections::HashMap;
/// fn transform(plan: &AdmittedPlan) -> AdmittedPlan {
///     rehydrate_plan_historical_secrets_from_verified_states(plan, &HashMap::new(), &HashMap::new()).unwrap()
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(
    transparent,
    bound(serialize = "S: Serialize + Clone, V: PlanValueKind")
)]
pub struct AdmittedPlan<S = ModuleSource, V: PlanValueKind = NoPlaceholder> {
    plan: Plan<S, V>,
    #[serde(skip)]
    writeback: Vec<crate::replacement::ReplacementWriteback>,
}

impl<S, V: PlanValueKind> std::ops::Deref for AdmittedPlan<S, V> {
    type Target = Plan<S, V>;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S: PartialEq, V: PlanValueKind> PartialEq for AdmittedPlan<S, V> {
    fn eq(&self, other: &Self) -> bool {
        self.plan == other.plan
    }
}

impl<S, V: PlanValueKind> AdmittedPlan<S, V> {
    pub fn into_unadmitted(self) -> Plan<S, V> {
        self.plan
    }
}

impl<S: Clone, V: PlanValueKind> AdmittedPlan<S, V> {
    pub(crate) fn admitted_effects(&self) -> crate::replacement::AdmittedReplacementEffects<S, V> {
        crate::replacement::AdmittedReplacementEffects::from_compiled_parts(
            self.effects.clone(),
            self.replacements.clone(),
            self.replacement_inputs.prepared_names.clone(),
            self.writeback.clone(),
        )
    }
}

/// Effects decoded from a user-supplied saved-plan file, before schema resolution.
/// The CLI checks source authenticity, lineage, serial and schema fingerprints.
/// Deserialization alone does not establish those checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedSavedPlan {
    effects: Vec<UnresolvedSavedEffect>,
    #[serde(default, skip_serializing_if = "ReplacementInputs::is_empty")]
    replacement_inputs: Box<ReplacementInputs>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignored_change_summaries: Vec<IgnoredChangeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    replace_display: Vec<ReplaceDisplayMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permanent_name_overrides: Vec<PermanentNameOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<PlanError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    abandoned_deposed: Vec<AbandonedDeposedGeneration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    destroy_ordering_notices: Vec<DestroyOrderingNotice>,
}

#[cfg(any(test, feature = "test-utils"))]
impl From<SavedPlan> for UnresolvedSavedPlan {
    fn from(plan: SavedPlan) -> Self {
        Self::from_recorded_snapshot(plan)
    }
}

impl UnresolvedSavedPlan {
    pub(crate) fn from_recorded_snapshot(plan: SavedPlan) -> Self {
        let Plan {
            replacements: _replacements,
            effects,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        } = plan;
        Self {
            effects: effects
                .into_iter()
                .map(UnresolvedSavedEffect::from)
                .collect(),
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReplacementIntentEndpoints {
    pub create_idx: usize,
    pub delete_idx: usize,
}

impl UnresolvedSavedPlan {
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn add_for_test(&mut self, effect: UnresolvedSavedEffect) {
        self.effects.push(effect);
    }

    pub fn effects(&self) -> &[UnresolvedSavedEffect] {
        &self.effects
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn move_authority(&self, from: &ResourceId, to: &ResourceId) -> Option<MoveAuthority<'_>> {
        self.effects.iter().find_map(|effect| {
            let authority = effect.move_authority()?;
            (authority.from() == from && authority.to() == to).then_some(authority)
        })
    }

    pub fn contains_redacted_secret_placeholders(&self) -> bool {
        self.effects
            .iter()
            .any(crate::value::unresolved_effect_contains_redacted_secret)
            || self.replace_display.iter().any(|metadata| {
                metadata
                    .previous_attributes
                    .values()
                    .any(crate::value::contains_redacted_secret)
            })
            || self
                .replacement_inputs
                .contains_redacted_secret_placeholders()
    }

    /// Endpoint indices are wire intents, without admitted display order.
    pub fn replacement_intents(&self) -> impl Iterator<Item = ReplacementIntentEndpoints> + '_ {
        self.replace_display
            .iter()
            .map(|metadata| ReplacementIntentEndpoints {
                create_idx: metadata.create_idx,
                delete_idx: metadata.delete_idx,
            })
    }

    pub fn abandoned_deposed_generations(
        &self,
    ) -> impl Iterator<Item = (&ResolvedResourceId, &DeposedKey)> {
        self.abandoned_deposed
            .iter()
            .map(|generation| (&generation.id, &generation.key))
    }

    pub fn resolve_with_schemas(
        self,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<SavedPlan, crate::resource::ResolvedResourceError> {
        let Self {
            effects,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        } = self;
        let effects = effects
            .into_iter()
            .map(|effect| effect.resolve_with_schemas(schemas))
            .collect::<Result<Vec<_>, _>>()?;
        // Historical secret placeholders are still unverified. Preserve the
        // approved targets until final admission after historical restoration.
        Ok(Plan {
            replacements: ReplacementScheduling::default(),
            effects,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        })
    }
}

impl<V: PlanValueKind> From<Plan<ModuleSource, V>> for Plan<SavedModuleSource, V> {
    fn from(plan: Plan<ModuleSource, V>) -> Self {
        let Plan {
            replacements,
            effects,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        } = plan;
        Self {
            replacements,
            effects: effects.into_iter().map(EffectWithSource::from).collect(),
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
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
        let create_before_destroy = group.replacement.is_create_first();
        let temporary_name = group.replacement.temporary_name().cloned();
        let previous_attributes = group.replacement.old_instances()[0]
            .previous_attributes
            .clone();
        for old in group.replacement.old_instances() {
            self.replacement_inputs.record_old(old);
        }
        if let Some(name) = &temporary_name {
            self.replacement_inputs
                .prepared_names
                .push((group.replacement.target().id.clone(), name.clone()));
        }
        let offset = self.effects.len();
        let (create_idx, delete_idx) = if create_before_destroy {
            (offset, offset + 1)
        } else {
            (offset + 1, offset)
        };
        self.replacements.extend_effects(
            group.replacement,
            group.consumer_updates,
            &mut self.effects,
        );
        self.replace_display.push(ReplaceDisplayMetadata {
            create_idx,
            delete_idx,
            create_before_destroy,
            changed_create_only: group.changed_create_only,
            cascade_ref_hints: group.cascade_ref_hints,
            temporary_name,
            previous_attributes,
        });
    }
}

impl<S: Clone> Plan<S> {
    /// Capture an execution plan as historical/display data without redaction.
    pub fn to_snapshot(&self) -> SnapshotPlan<S> {
        let effects = self
            .effects
            .iter()
            .map(|effect| {
                effect.try_map_value_stage(
                        |_, _, value| Ok::<_, std::convert::Infallible>(value.to_snapshot()),
                        |_, value| Ok(value.to_snapshot()),
                        |template| {
                            crate::effect::value_stage::map_template::<
                                S,
                                NoPlaceholder,
                                SnapshotLeaf,
                                _,
                            >(
                                template,
                                |value| Ok(value.to_snapshot()),
                                |_, value| Ok(value.to_snapshot()),
                            )
                        },
                    )
            })
            .collect::<Result<Vec<_>, std::convert::Infallible>>()
            .expect("snapshot capture is infallible");
        self.clone().with_value_stage(effects)
    }
}

impl<S, V: PlanValueKind> Plan<S, V> {
    pub(crate) fn with_value_stage<Q: PlanValueKind>(
        self,
        effects: Vec<Effect<S, Q>>,
    ) -> Plan<S, Q> {
        let Plan {
            replacements: _replacements,
            effects: _,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        } = self;
        Plan {
            // The replacement vector was mapped by a caller. Execution must
            // re-admit its prepared endpoints rather than inherit a proof.
            replacements: ReplacementScheduling::default(),
            effects,
            ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides,
            errors,
            abandoned_deposed,
            destroy_ordering_notices,
        }
    }

    pub fn add(&mut self, effect: Effect<S, V>) {
        self.effects.push(effect);
    }

    /// Retain the generation's own snapshot alongside its delete endpoint.
    /// The snapshot is replacement input even when no display row exists yet.
    pub fn add_delete(&mut self, old: DeferredReplaceDelete) {
        self.replacement_inputs.record_old(&old);
        self.effects.push(old.to_delete_effect());
    }

    pub(crate) fn previous_delete_attributes(
        &self,
        instance: &DeletedInstanceKey,
    ) -> Option<HashMap<String, SnapshotValue>> {
        self.replacement_inputs.previous(instance)
    }

    /// Re-admit replacement requests after mapping plan values. This refreshes
    /// prepared endpoints, scheduling, and display previews as one operation.
    pub fn prepare_replacements(
        &mut self,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<(), crate::replacement::ReplacementAdmissionError>
    where
        S: Clone,
    {
        let admitted = self.admit_replacement_effects(schemas)?;
        self.install_admission(admitted);
        Ok(())
    }

    /// Admit the final draft and seal it against further mutation.
    pub fn admit(
        mut self,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<AdmittedPlan<S, V>, crate::replacement::ReplacementAdmissionError>
    where
        S: Clone,
    {
        let admitted = self.admit_replacement_effects(schemas)?;
        let writeback = admitted.writeback().to_vec();
        self.install_admission(admitted);
        Ok(AdmittedPlan {
            plan: self,
            writeback,
        })
    }

    fn install_admission(&mut self, admitted: crate::replacement::AdmittedReplacementEffects<S, V>)
    where
        S: Clone,
    {
        let (effects, replacements, temporary_names) = admitted.into_parts();
        self.effects = effects;
        self.replacements = replacements;
        self.replacement_inputs.prepared_names = temporary_names.clone();
        for (id, name) in temporary_names {
            for display in &mut self.replace_display {
                if self
                    .effects
                    .get(display.create_idx)
                    .is_some_and(|effect| effect.resource_id() == &id)
                {
                    display.temporary_name = Some(name.clone());
                }
            }
            self.permanent_name_overrides.retain(|previous| {
                previous.resource_id.as_inner() != &id || previous.attribute != name.attribute
            });
            self.permanent_name_overrides.push(PermanentNameOverride {
                resource_id: ResolvedResourceId::new(id),
                attribute: name.attribute,
                temp_value: name.temporary_value,
                original_value: Some(name.original_value),
            });
        }
        for display in &mut self.replace_display {
            display.create_before_destroy = self
                .effects
                .get(display.create_idx)
                .is_some_and(|effect| self.replacements.is_create_first(effect.resource_id()));
        }
    }

    pub(crate) fn admit_replacement_effects(
        &self,
        registry: &crate::schema::SchemaRegistry,
    ) -> Result<
        crate::replacement::AdmittedReplacementEffects<S, V>,
        crate::replacement::ReplacementAdmissionError,
    >
    where
        S: Clone,
    {
        // These indices associate intent endpoints; their order and serialized
        // display bool grant no admission authority.
        let mut effects = self.effects.clone();
        let mut associations: HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>> =
            HashMap::new();
        for group in &self.replace_display {
            if let (
                Some(Effect::Create(target)),
                Some(Effect::Delete {
                    id,
                    generation: EffectGeneration::Current,
                    ..
                }),
            ) = (
                self.effects.get(group.create_idx),
                self.effects.get(group.delete_idx),
            ) {
                associations
                    .entry(target.resolved_id())
                    .or_default()
                    .insert(id.clone());
                if group.create_before_destroy {
                    // A saved order is a request, never a capability. It must
                    // still be admitted against this target's current schema.
                    if let Some(Effect::Create(target)) = effects.get_mut(group.create_idx) {
                        let mut resource = target.as_resource().clone();
                        resource.directives.create_before_destroy = true;
                        *target = target.replace_inner(resource);
                    }
                }
            }
        }
        let prepared_names = self
            .replacement_inputs
            .prepared_names
            .iter()
            .map(|(id, name)| (ResolvedResourceId::new(id.clone()), name.clone()))
            .collect();
        crate::replacement::AdmittedReplacementEffects::resolve_with_prepared_names(
            &effects,
            registry,
            |instance| {
                self.replacement_inputs
                    .previous(instance)
                    .unwrap_or_default()
            },
            &associations,
            &prepared_names,
        )
    }

    pub fn effects(&self) -> &[Effect<S, V>] {
        &self.effects
    }

    pub fn move_effects(&self) -> crate::effect::SavedMoveEffects<'_, S, V> {
        crate::effect::SavedMoveEffects::from_effects(&self.effects)
    }

    pub fn ignored_change_summaries(&self) -> &[IgnoredChangeSummary] {
        &self.ignored_change_summaries
    }

    pub fn record_unresolvable_destroy_dependencies(
        &mut self,
        id: ResolvedResourceId,
        generation: EffectGeneration,
        dependencies: ResolvedDependencies,
    ) {
        if dependencies.is_empty() {
            return;
        }
        self.destroy_ordering_notices.push(DestroyOrderingNotice {
            id,
            generation,
            ordering: DestroyOrdering::UnresolvedPersistedDependencies { dependencies },
        });
    }

    pub fn record_unresolved_delete_ordering_analysis(
        &mut self,
        analysis: &crate::effect::deps::DependencyAnalysis,
    ) {
        let mut notices: Vec<(ResolvedResourceId, EffectGeneration, ResolvedDependencies)> =
            Vec::new();
        for warning in analysis.unresolved_delete_ordering() {
            if let Some((_, _, dependencies)) =
                notices
                    .iter_mut()
                    .find(|(notice_id, notice_generation, _)| {
                        notice_id == warning.delete_id()
                            && notice_generation == warning.generation()
                    })
            {
                dependencies.insert(warning.dependency().clone());
            } else {
                notices.push((
                    warning.delete_id().clone(),
                    warning.generation().clone(),
                    std::iter::once(warning.dependency().clone()).collect(),
                ));
            }
        }
        for (id, generation, dependencies) in notices {
            self.record_unresolvable_destroy_dependencies(id, generation, dependencies);
        }
    }

    pub fn destroy_ordering_notices(&self) -> &[DestroyOrderingNotice] {
        &self.destroy_ordering_notices
    }

    /// Refresh update inputs for ignored paths from the apply-time read.
    ///
    /// Other updates keep their plan-time source after drift verification;
    /// ignored updates must use the fresh source so a value changed between
    /// plan and apply is never written back accidentally.
    pub fn rebase_ignored_update_sources<P: crate::resource::StateValueLeaf>(
        &mut self,
        actual_states: &HashMap<ResourceId, crate::resource::State<P>>,
    ) {
        for effect in &mut self.effects {
            if let Effect::Update { from, to, .. } = effect
                && !to.ignore_changes().is_empty()
                && let Some(actual) = actual_states.get(&to.id)
            {
                **from = actual.to_snapshot();
            }
        }
    }

    pub(crate) fn record_ignored_readable_differences(
        &mut self,
        resource_id: ResourceId,
        count: usize,
    ) {
        if count == 0 {
            return;
        }
        if let Some(summary) = self
            .ignored_change_summaries
            .iter_mut()
            .find(|summary| summary.resource_id == resource_id)
        {
            summary.count = count;
        } else {
            self.ignored_change_summaries
                .push(IgnoredChangeSummary { resource_id, count });
        }
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
            replacements: _replacements,
            effects,
            ignored_change_summaries: _ignored_change_summaries,
            replace_display,
            replacement_inputs,
            permanent_name_overrides: _permanent_name_overrides,
            errors: _errors,
            abandoned_deposed: _abandoned_deposed,
            destroy_ordering_notices: _destroy_ordering_notices,
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
            || replacement_inputs.contains_redacted_secret_placeholders()
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

    pub(crate) fn effects_mut(&mut self) -> &mut Vec<Effect<S, V>> {
        self.replacements = ReplacementScheduling::default();
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
        F: FnMut(&Effect<S, V>) -> bool,
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
        self.replacements.remap(&old_to_new);
        let old_instances: HashSet<_> = self
            .effects
            .iter()
            .flat_map(Effect::deleted_resource_attribute_keys)
            .collect();
        self.replacement_inputs
            .old_instances
            .retain(|old| old_instances.contains(&old.instance));
        self.replacement_inputs.prepared_names.retain(|(id, _)| {
            self.effects.iter().any(|effect| {
                matches!(effect, Effect::Create(_) | Effect::ResumeCreate { .. })
                    && effect.resource_id() == id
            })
        });
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

    /// Suppress current-instance deletes managed by state-only operations.
    ///
    /// State-block adjustments run in [`crate::differ::PendingCascadePlan::finish`]
    /// before deferred targets absorb surviving deletes. Suppression therefore
    /// sees prior children as standalone deletes and preserves deposed cleanup.
    pub fn suppress_current_deletes(&mut self, ids: &HashSet<ResourceId>) {
        if ids.is_empty() {
            return;
        }
        self.retain(|effect| {
            !matches!(effect,
                Effect::Delete { id, generation: EffectGeneration::Current, .. }
                    if ids.contains(id.as_inner())
            )
        });
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
                Effect::ReleaseListKeys { keys, .. } => summary.release += keys.len(),
                Effect::Wait { .. } => summary.wait += 1,
                Effect::DeferredCreate(_) => {}
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
    /// Number of keyed list entries released from authored state.
    pub release: usize,
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
    Release { count: usize },
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
        if self.release > 0 {
            parts.push(PlanSummaryPart::Release {
                count: self.release,
            });
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
                PlanSummaryPart::Release { count } => format!("{count} to release"),
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

impl<S: Clone, V: PlanValueKind> Plan<S, V> {
    pub(crate) fn scheduling_effects(&self) -> crate::replacement::EffectGraph<'_, S, V> {
        crate::replacement::EffectGraph::new(&self.effects, &self.replacements)
    }
}
impl<S: Clone> Plan<S> {
    pub fn dependency_analysis<'a>(
        &self,
        unresolved: impl IntoIterator<Item = &'a ResolvedResourceId>,
    ) -> Result<
        crate::effect::deps::DependencyAnalysis,
        crate::effect::deps::DependencyAnalysisFailure,
    > {
        self.scheduling_effects().dependency_analysis(unresolved)
    }
}

#[cfg(test)]
impl<S> Plan<S> {
    /// Core executor fixtures retain their whole input plan here.
    pub(crate) fn provider_plan(&self) -> crate::provider::ProviderPlan {
        let mut plan = crate::provider::ProviderPlan::state_only();
        for effect in self.effects() {
            match effect.as_resource_ref() {
                Some(crate::parser::ResourceRef::Resource(resource)) => {
                    plan.include(&resource.id, resource.attributes.values())
                }
                Some(crate::parser::ResourceRef::DataSource(source)) => {
                    plan.include(&source.id, source.attributes.values())
                }
                _ => {}
            }
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::resource::{
        DataSource, RelativeBindingName, ResolvedDataSource, ResolvedResource, Resource, RootScope,
        Value,
    };

    fn render_plan_error_kind(kind: PlanErrorKind) -> String {
        let error = PlanError::new(
            ResourceId::with_provider_identity("test", "Resource", "fixture", None),
            kind,
        );
        let prefix = format!("{}: ", error.resource_id);
        error
            .display_with_project_dir(Path::new("."))
            .to_string()
            .strip_prefix(&prefix)
            .expect("ordinary plan errors include their resource address")
            .to_string()
    }

    #[test]
    fn provider_text_plan_constraint_projection_sanitizes_metadata_labels() {
        let raw = format!("field\x1b[2J\u{9b}{}", "x".repeat(20 * 1024));
        let kind = PlanErrorKind::ResolvedValueConstraint {
            attributes: vec![raw.clone()],
            origins: vec![],
            message: "invalid value".into(),
        };
        let rendered = render_plan_error_kind(kind);
        assert!(!rendered.contains(['\x1b', '\u{9b}']));
        assert!(rendered.len() <= crate::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert!(rendered.contains("truncated,"));
    }

    #[test]
    fn provider_text_recovery_hint_preserves_raw_current_identifier() {
        let raw = "\x1b]52;c;ZXZpbA==\x07\x1b[2J";
        let id = ResourceId::with_identity("test.Resource", "example");
        let error = PlanError::require_deposed_provider_identifier(
            &id,
            &DeposedKey::new_unique(),
            "",
            Some(raw),
        )
        .unwrap_err();
        let rendered = error.display_with_project_dir(Path::new(".")).to_string();
        assert!(!rendered.contains(['\x1b', '\x07']));
        assert!(rendered.contains("provider identifier `\\u{1b}]52;c;ZXZpbA==\\u{7}\\u{1b}[2J`"));
        assert!(
            matches!(&error.kind, PlanErrorKind::MissingDeposedProviderIdentifier { key: _, current_identifier: Some(identifier) } if identifier.as_str() == raw)
        );
    }

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource.with_dependency_phase(()))
    }

    fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
        ResolvedDataSource::new(resource.with_dependency_phase(()))
    }

    fn absolute(binding: &str) -> crate::resource::AbsoluteBindingAddress {
        RootScope::new().resolve(RelativeBindingName::new(binding))
    }

    fn deserialize_plan(json: &str) -> SavedPlan {
        deserialize_plan_value(serde_json::from_str(json).unwrap())
    }

    fn replacement_fixture_registry() -> crate::schema::SchemaRegistry {
        let mut registry = crate::schema::SchemaRegistry::new();
        registry.insert(
            "",
            crate::schema::PatternResourceSchema::new("ec2.Vpc")
                .attribute(
                    crate::schema::PatternAttributeSchema::optional(
                        "name",
                        crate::schema::PatternAttributeType::string(),
                    )
                    .create_only()
                    .build(),
                )
                .with_unique_name_attribute("name")
                .validate()
                .unwrap(),
        );
        registry
    }

    fn deserialize_plan_value(value: serde_json::Value) -> SavedPlan {
        serde_json::from_value::<UnresolvedSavedPlan>(value)
            .unwrap()
            .resolve_with_schemas(&replacement_fixture_registry())
            .unwrap()
    }

    fn changed_create_only() -> ChangedCreateOnly {
        ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap()
    }

    fn cascade_ref_hints() -> Vec<(String, String)> {
        vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())]
    }

    fn temporary_name() -> TemporaryName {
        admitted_fixture().temporary_name().unwrap().clone()
    }

    fn admitted_fixture() -> &'static CheckedReplacement<ResolvedResource> {
        static FIXTURE: std::sync::LazyLock<CheckedReplacement<ResolvedResource>> =
            std::sync::LazyLock::new(|| {
                let create = resolved(
                    Resource::new("ec2.Vpc", "vpc")
                        .with_root_binding("vpc")
                        .with_attribute(
                            "name",
                            Value::Concrete(crate::resource::ConcreteValue::String("main".into())),
                        ),
                );
                let mut target = create.as_resource().clone();
                target.directives.create_before_destroy = true;
                let gateway = ResolvedResourceId::with_provider_identity(
                    "mock",
                    "ec2.InternetGateway",
                    "internet_gateway",
                    None,
                );
                let old = crate::effect::DeferredReplaceDelete {
                    generation: crate::effect::EffectGeneration::Current,
                    id: ResolvedResourceId::with_identity("ec2.Vpc", "vpc-old"),
                    identifier: crate::effect::fixture_applied_identifier("vpc-123"),
                    directives: Directives::default(),
                    binding: Some(absolute("vpc")),
                    dependencies: crate::test_support::persisted([gateway.clone()]),
                    explicit_dependencies: std::iter::once(gateway).collect(),
                    blocked_by_updates: Default::default(),
                    previous_attributes: previous_attributes(),
                };
                crate::replacement::resolve_replacement(
                    crate::replacement::ReplacementRequest::new(
                        create.replace_inner(target),
                        crate::effect::NonEmptyDeletes::try_new(vec![old]).unwrap(),
                    ),
                    &replacement_fixture_registry(),
                )
                .unwrap()
            });
        &FIXTURE
    }

    fn previous_attributes() -> HashMap<String, SnapshotValue> {
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(crate::resource::ConcreteValue::String("10.0.0.0/16".into())),
        )])
    }

    fn permanent_name_override() -> PermanentNameOverride {
        PermanentNameOverride {
            resource_id: ResolvedResourceId::new(ResourceId::with_identity("ec2.Vpc", "vpc")),
            attribute: "name".to_string(),
            temp_value: temporary_name().temporary_value,
            original_value: Some("main".to_string()),
        }
    }

    fn replacement_group(consumer_updates: ResolvedDependencies) -> ReplacementGroup {
        ReplacementGroup {
            replacement: admitted_fixture().clone(),
            changed_create_only: changed_create_only(),
            cascade_ref_hints: cascade_ref_hints(),
            consumer_updates,
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
            id: crate::resource::ResolvedResourceId::with_identity("__wait", "cert_issued"),
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
            explicit_dependencies: Default::default(),
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
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
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
        plan.add_replacement(replacement_group(Default::default()));

        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.replace, 1);
        assert_eq!(summary.delete, 0);
    }

    #[test]
    fn plan_summary_records_deferred_adds() {
        use crate::parser::ForBinding;
        let template_resource = Resource::new("aws.route53.RecordSet", "ignored");
        let deferred = crate::parser::DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(
            Resource::new("acm.Certificate", "cert").with_root_binding("cert"),
        )));
        plan.add(Effect::DeferredCreate(crate::test_support::compiled_loop(
            Box::new(deferred),
        )));

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
        use crate::parser::{ExpandedDeferredForExpression as DeferredForExpression, ForBinding};
        use crate::resource::{Directives, ResourceId};
        let template_resource = Resource::new("aws.route53.RecordSet", "ignored");
        let template = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );
        let deletes = (0..3)
            .map(|idx| DeferredReplaceDelete {
                generation: crate::effect::EffectGeneration::Current,
                previous_attributes: Default::default(),
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.RecordSet",
                    crate::resource::ResourceIdentity::try_from(format!(
                        "validation_records[{idx}]"
                    ))
                    .unwrap(),
                )),
                identifier: crate::effect::fixture_applied_identifier(format!("record-{idx}")),
                directives: Directives::default(),
                binding: Some(absolute(&format!("validation_records[{idx}]"))),
                dependencies: Default::default(),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            })
            .collect();

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace(Box::new(
            crate::test_support::compiled_replace(
                NonEmptyDeletes::try_new(deletes).expect("fixture has deletes"),
                Box::new(template),
            ),
        )));

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
        let consumer_updates: ResolvedDependencies = std::iter::once(
            ResolvedResourceId::with_identity("ec2.Subnet", "subnet_update"),
        )
        .collect();
        let group = replacement_group(consumer_updates.clone());
        let expected_changed_create_only = group.changed_create_only.clone();
        let expected_cascade_ref_hints = group.cascade_ref_hints.clone();
        let expected_temporary_name = group.replacement.temporary_name().cloned();
        let expected_previous_attributes = group.replacement.old_instances()[0]
            .previous_attributes
            .clone();

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
        plan.add_replacement(replacement_group(Default::default()));

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

        plan.add_replacement(replacement_group(Default::default()));

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
        plan.add_replacement(replacement_group(Default::default()));

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
        plan.add_replacement(replacement_group(Default::default()));

        plan.retain(|effect| !matches!(effect, Effect::Delete { .. }));

        assert_eq!(plan.effects().len(), 1);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        assert!(plan.replace_display.is_empty());
    }

    #[test]
    fn plan_replace_display_round_trips_through_serde() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(
            std::iter::once(ResolvedResourceId::with_identity(
                "ec2.Subnet",
                "subnet_update",
            ))
            .collect(),
        ));
        let expected_replace_display = plan.replace_display.clone();

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);

        assert_eq!(deserialized.effects().len(), 2);
        assert_eq!(deserialized.replace_display, expected_replace_display);
    }

    #[test]
    fn plan_admission_builds_permanent_name_override_on_cbd() {
        let override_ = permanent_name_override();
        let group = replacement_group(Default::default());
        let mut plan = Plan::new();
        plan.add_replacement(group);
        plan.prepare_replacements(&replacement_fixture_registry())
            .unwrap();

        assert_eq!(plan.permanent_name_overrides, vec![override_]);
    }

    #[test]
    fn plan_admission_skips_permanent_name_override_on_dbc() {
        let mut group = replacement_group(Default::default());
        let request = group.replacement.into_request();
        let mut target = request.target().as_resource().clone();
        target.directives.create_before_destroy = false;
        group.replacement = crate::replacement::resolve_replacement(
            request.map_target(|resource| resource.replace_inner(target)),
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap();

        let mut plan = Plan::new();
        plan.add_replacement(group);
        plan.prepare_replacements(&replacement_fixture_registry())
            .unwrap();

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
                render_plan_error_kind(PlanErrorKind::PreventDestroy {
                    action,
                    source: PreventDestroySource::ExplicitDirective,
                }),
                expected
            );
        }
    }

    #[test]
    fn plan_error_kind_display_pins_schema_default_prevent_destroy_actions() {
        let opt_out = "to allow this, write `directives { prevent_destroy = false }`";
        let cases = [
            (
                PreventDestroyAction::Replace,
                "resource type is protected from destruction by default, but the plan would replace it (which requires destroying the old resource)",
            ),
            (
                PreventDestroyAction::Delete,
                "resource type is protected from destruction by default, but the plan would delete it",
            ),
            (
                PreventDestroyAction::DeleteRemovedFromConfiguration,
                "resource type is protected from destruction by default, but the plan would delete it (resource removed from configuration)",
            ),
            (
                PreventDestroyAction::CascadeReplace,
                "resource type is protected from destruction by default, but cascade from a replaced dependency would replace it (which requires destroying the old resource)",
            ),
        ];

        for (action, prefix) in cases {
            assert_eq!(
                render_plan_error_kind(PlanErrorKind::PreventDestroy {
                    action,
                    source: PreventDestroySource::SchemaDefault,
                }),
                format!("{prefix}; {opt_out}")
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
            render_plan_error_kind(PlanErrorKind::SchemaNotRegistered(error)),
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
            render_plan_error_kind(PlanErrorKind::ReplacementCannotCoexist(error)),
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
        let rendered = error.display_with_project_dir(Path::new(".")).to_string();

        assert_eq!(
            rendered,
            "awscc.ec2.Vpc.main-vpc: the create may have reached the cloud, but its outcome is unknown; if it exists: import { to = awscc.ec2.Vpc \"main-vpc\" id = \"…\" }; if it doesn't: carina state forget awscc.ec2.Vpc main-vpc"
        );
        let import = rendered
            .split_once("if it exists: ")
            .and_then(|(_prefix, resolutions)| resolutions.split_once("; if it doesn't:"))
            .map(|(import, _forget)| import)
            .expect("the rendered hint must contain both resolution branches");

        crate::parser::parse(
            import,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
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
            render_plan_error_kind(PlanErrorKind::WaitTargetMissing {
                wait_binding: "cert_issued".to_string(),
                target: "cert".to_string(),
            }),
            "wait `cert_issued`: target binding `cert` is not a known resource"
        );
        assert_eq!(
            render_plan_error_kind(PlanErrorKind::WaitTargetIsDataSource {
                wait_binding: "roles_ready".to_string(),
                target: "roles".to_string(),
            }),
            "wait `roles_ready`: target binding `roles` is a data source; wait targets must be managed resources"
        );
        assert_eq!(
            render_plan_error_kind(PlanErrorKind::WaitPredicateInvalid {
                wait_binding: "cert_issued".to_string(),
                reason: "invalid predicate attribute path: empty attribute path".into(),
            }),
            "wait `cert_issued`: invalid predicate attribute path: empty attribute path"
        );
    }

    #[test]
    fn plan_permanent_name_overrides_round_trips_through_serde() {
        let group = replacement_group(Default::default());
        let mut plan = Plan::new();
        plan.add_replacement(group);
        plan.prepare_replacements(&replacement_fixture_registry())
            .unwrap();

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);

        assert_eq!(
            deserialized,
            SavedPlan::from(crate::test_support::snapshot_plan(&plan))
        );
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
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized = deserialize_plan(&json);
        assert_eq!(plan.effects().len(), deserialized.effects().len());
        let saved = SavedPlan::from(crate::test_support::snapshot_plan(&plan));
        assert_eq!(saved.effects(), deserialized.effects());
    }
}
