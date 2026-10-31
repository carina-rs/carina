//! Parser AST types and the public `ParsedFile` result.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).
//!
//! Static-type states that cannot be represented by a sound [`TypeExpr`] live
//! in [`crate::static_type`] rather than as a sentinel variant here.

use super::display_string_literal;
use super::error::{
    DeferredForBodyKind, DeferredForBodyUnsupportedError, DeferredForIterableResolution,
    ParseError, ParseWarning, WarningKind,
};
use super::expressions::for_expr::ForBinding;
use super::expressions::validate_expr::CompareOp;
use super::util::snake_to_pascal;
use crate::binding_index::IterableBindings;
use crate::eval_value::{StructuralHandle, StructuralHandleKind, StructuralHandleMetadata};
use crate::provider_source::ProviderSource;
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, Directives, ForLoopElement, FunctionArgument,
    GraphNode, LeafNode, LiveComposition, ModuleSource, PathSegment, Resource, ResourceId,
    ResourceIdentity, SavedModuleSource, StructuralOperand, StructuralOperandRef,
    UnboundLoopTemplate, UnknownReason, Value,
};
use crate::static_type::{ConsumerStaticType, ExportStaticType};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Whether a deferred `for` writes user-addressable bound resources or
/// anonymous resources carrying migration-only loop provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeferredForTarget {
    Bound { binding: String },
    Unbound { template: UnboundLoopTemplate },
}

impl DeferredForTarget {
    pub fn bound_binding(&self) -> Option<&str> {
        match self {
            Self::Bound { binding } => Some(binding),
            Self::Unbound { .. } => None,
        }
    }

    pub fn scheduler_key(&self) -> &str {
        match self {
            Self::Bound { binding } => binding,
            Self::Unbound { template } => template.scheduler_key(),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Bound { binding } => binding.clone(),
            Self::Unbound { template } => template.display_name(),
        }
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bound { binding: left }, Self::Bound { binding: right }) => left == right,
            (Self::Unbound { template: left }, Self::Unbound { template: right }) => {
                left.same_system_target(right)
            }
            (Self::Bound { .. }, Self::Unbound { .. })
            | (Self::Unbound { .. }, Self::Bound { .. }) => false,
        }
    }

    pub(crate) fn matches_legacy_binding(&self, binding: &str) -> bool {
        match self {
            Self::Bound {
                binding: target_binding,
            } => target_binding == binding,
            Self::Unbound { template } => template.matches_legacy_binding(binding),
        }
    }

    pub(crate) fn set_loop_display_file(&mut self, file: &str) {
        if let Self::Unbound { template } = self {
            template.set_display_file(file);
        }
    }

    pub(crate) fn set_loop_binding_scope(
        &mut self,
        binding_scope: std::sync::Arc<std::collections::BTreeSet<String>>,
    ) {
        if let Self::Unbound { template } = self {
            template.set_binding_scope(binding_scope);
        }
    }
}

/// A for-expression whose iterable is unresolved (e.g., upstream_state not yet available).
///
/// Parser-side deferred loops use [`ParsedDeferredForExpression`] instead.
/// This resource-only shape is the resolved typestate consumed by planning,
/// execution, display, and state persistence.
/// Captures the structural shape of the loop body so the plan can show what
/// resources *would* be created once the iterable becomes available.
/// Also stores enough information to expand the loop later when the iterable
/// is loaded from upstream_state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "S: serde::Serialize",
    deserialize = "S: serde::Deserialize<'de>"
))]
pub struct DeferredForExpression<S = ModuleSource> {
    /// Full source path of the file this deferred expression originated
    /// from (stamped by `config_loader` after parsing).
    #[serde(skip)]
    pub file: Option<String>,
    /// Source line number of the `for` keyword.
    #[serde(skip)]
    pub line: usize,
    /// The for-expression header, e.g., `for account_id in orgs.accounts`.
    pub header: String,
    /// The provider-qualified resource type the loop body would produce (e.g., `awscc.sso.Assignment`).
    pub resource_type: String,
    /// Attribute template: key → value (concrete values are resolved;
    /// loop-bound values remain as deferred postfix access expressions).
    pub attributes: Vec<(String, Value)>,
    /// Typed destination for the generated resources.
    pub target: DeferredForTarget,
    /// Complete iterable expression. This can remain a deferred coalesce until
    /// its referenced binding reaches a complete resolution boundary.
    pub iterable: Value,
    /// Binding pattern — records the kind (Simple/Indexed/Map) so the expansion
    /// can verify the resolved iterable's shape matches and substitute the
    /// correct variable(s).
    pub binding: ForBinding,
    /// Template resource for expansion (the for body parsed with placeholders).
    #[serde(with = "deferred_template_resource_serde")]
    pub template_resource: Resource<S>,
}

impl<S> DeferredForExpression<S> {
    pub fn site(&self) -> DeferredForSite<'_> {
        DeferredForSite {
            file: self.file.as_deref(),
            line: self.line,
            header: &self.header,
            binding: &self.binding,
            iterable: &self.iterable,
        }
    }
}

/// Typed parser-side body of a deferred `for` expression.
///
/// Every grammar-supported body kind is retained until module expansion has
/// substituted caller arguments. Only the [`Self::Resource`] arm can cross
/// the final resolution seam into [`DeferredForExpression`].
#[derive(Debug, Clone)]
pub enum DeferredForBody {
    Resource(Box<Resource>),
    Read(Box<DataSource>),
    ModuleCall(ModuleCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedDeferredForIterableState {
    Deferred,
    UndefinedIdentifier { name: String },
}

#[derive(Debug)]
pub(crate) enum DeferredForResourceExpressionError {
    UndefinedIterable(ParseError),
    Unsupported {
        file: Option<String>,
        error: Box<DeferredForBodyUnsupportedError>,
    },
}

/// Parser-side typestate for a deferred `for` expression.
///
/// Unlike [`DeferredForExpression`], this representation is deliberately not
/// serializable and may contain a read or module-call template. The module
/// resolver consumes it after all module argument substitution and produces
/// the resource-only resolved representation.
#[derive(Debug, Clone)]
pub struct ParsedDeferredForExpression {
    /// Full source path, stamped by the directory loader.
    pub file: Option<String>,
    /// Source line of the `for` keyword.
    pub line: usize,
    /// Exact authored span of the complete `for` expression.
    pub span: SourceSpan,
    /// Canonically rendered loop header, for example `for item in names`.
    pub header: String,
    pub target: DeferredForTarget,
    pub iterable: Value,
    pub(crate) iterable_state: ParsedDeferredForIterableState,
    pub binding: ForBinding,
    pub body: DeferredForBody,
}

impl ParsedDeferredForExpression {
    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        self.site().root_bindings()
    }

    pub fn site(&self) -> DeferredForSite<'_> {
        DeferredForSite {
            file: self.file.as_deref(),
            line: self.line,
            header: &self.header,
            binding: &self.binding,
            iterable: &self.iterable,
        }
    }

    /// The one typed construction point for a bare undefined deferred
    /// iterable diagnostic. Static identifier validation and the final
    /// module-resolution conversion both consume this value, so they cannot
    /// drift in variant, suggestion, source path, or authored span.
    pub(crate) fn undefined_iterable_error(&self, known_bindings: &[String]) -> Option<ParseError> {
        let ParsedDeferredForIterableState::UndefinedIdentifier { name } = &self.iterable_state
        else {
            return None;
        };
        let error = ParseError::undefined_deferred_iterable(
            name.clone(),
            self.line,
            known_bindings.to_vec(),
            self.span,
        );
        Some(match &self.file {
            Some(path) => error.in_file(path),
            None => error,
        })
    }

    pub(crate) fn into_resource_expression(
        self,
        known_bindings: &[String],
    ) -> Result<DeferredForExpression, DeferredForResourceExpressionError> {
        if let Some(error) = self.undefined_iterable_error(known_bindings) {
            return Err(DeferredForResourceExpressionError::UndefinedIterable(error));
        }
        let Self {
            file,
            line,
            span,
            header,
            target,
            iterable,
            iterable_state,
            binding,
            body,
        } = self;
        debug_assert!(matches!(
            iterable_state,
            ParsedDeferredForIterableState::Deferred
        ));
        let iterable_resolution = if super::functions::contains_module_argument(&iterable) {
            DeferredForIterableResolution::DependsOnEnclosingModuleArgument
        } else {
            match &iterable {
                Value::Concrete(_) => {
                    DeferredForIterableResolution::ConcreteAfterModuleArgumentSubstitution
                }
                Value::Deferred(_) => DeferredForIterableResolution::StillDeferred,
            }
        };
        let template_resource = match body {
            DeferredForBody::Resource(resource) => *resource,
            DeferredForBody::Read(data_source) => {
                return Err(DeferredForResourceExpressionError::Unsupported {
                    file,
                    error: Box::new(DeferredForBodyUnsupportedError {
                        line,
                        header,
                        body: DeferredForBodyKind::Read {
                            resource_type: data_source.id.display_type(),
                        },
                        span,
                        iterable_resolution,
                    }),
                });
            }
            DeferredForBody::ModuleCall(call) => {
                return Err(DeferredForResourceExpressionError::Unsupported {
                    file,
                    error: Box::new(DeferredForBodyUnsupportedError {
                        line,
                        header,
                        body: DeferredForBodyKind::ModuleCall {
                            name: call.module_name,
                        },
                        span,
                        iterable_resolution,
                    }),
                });
            }
        };
        let resource_type = if template_resource.id.provider.is_empty() {
            template_resource.id.resource_type.clone()
        } else {
            format!(
                "{}.{}",
                template_resource.id.provider, template_resource.id.resource_type
            )
        };
        let attributes = template_resource.attributes.clone().into_iter().collect();
        Ok(DeferredForExpression {
            file,
            line,
            header,
            resource_type,
            attributes,
            target,
            iterable,
            binding,
            template_resource,
        })
    }
}

/// Phase-neutral, read-only metadata for a deferred `for` expression.
///
/// Before module resolution this borrows the parser-side typed entry; after
/// the resolution seam it borrows the resource-only entry. The loop body is
/// intentionally absent so downstream crates cannot bypass the resolved body
/// typestate.
#[derive(Debug, Clone, Copy)]
pub struct DeferredForSite<'a> {
    pub file: Option<&'a str>,
    pub line: usize,
    pub header: &'a str,
    pub binding: &'a ForBinding,
    pub iterable: &'a Value,
}

impl<'a> DeferredForSite<'a> {
    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        self.iterable.root_bindings()
    }
}

pub type SavedDeferredForExpression = DeferredForExpression<SavedModuleSource>;

impl From<DeferredForExpression> for SavedDeferredForExpression {
    fn from(expression: DeferredForExpression) -> Self {
        let DeferredForExpression {
            file,
            line,
            header,
            resource_type,
            attributes,
            target,
            iterable,
            binding,
            template_resource,
        } = expression;
        Self {
            file,
            line,
            header,
            resource_type,
            attributes,
            target,
            iterable,
            binding,
            template_resource: template_resource.into(),
        }
    }
}

/// Saved deferred templates are the one resource-shaped payload allowed to
/// retain a pending identity: the identity is assigned only after the iterable
/// materializes. Keep that exception local to this field so ordinary resource
/// and state serialization continues to reject process-local pending tokens.
mod deferred_template_resource_serde {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::{BTreeSet, HashMap, HashSet};

    #[derive(Serialize)]
    struct ResourceIdRef<'a> {
        provider: &'a str,
        resource_type: &'a str,
        identity: Option<&'a ResourceIdentity>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_instance: &'a Option<String>,
    }

    #[derive(Serialize)]
    #[serde(bound(serialize = "S: Serialize"))]
    struct ResourceRef<'a, S> {
        id: ResourceIdRef<'a>,
        attributes: &'a IndexMap<String, Value>,
        directives: &'a Directives,
        prefixes: &'a HashMap<String, String>,
        binding: &'a Option<String>,
        dependency_bindings: &'a BTreeSet<String>,
        module_source: &'a Option<S>,
        #[serde(skip_serializing_if = "crate::resource::DefaultTagKeys::is_empty")]
        default_tag_keys: &'a crate::resource::DefaultTagKeys,
        #[serde(skip_serializing_if = "crate::resource::LoopProvenance::is_empty")]
        loop_provenance: &'a crate::resource::LoopProvenance,
    }

    #[derive(Deserialize)]
    struct ResourceIdWire {
        provider: String,
        resource_type: String,
        #[serde(default, alias = "name")]
        identity: Option<ResourceIdentity>,
        #[serde(default)]
        provider_instance: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(bound(deserialize = "S: Deserialize<'de>"))]
    struct ResourceWire<S> {
        id: ResourceIdWire,
        attributes: IndexMap<String, Value>,
        #[serde(default)]
        directives: Directives,
        #[serde(default)]
        prefixes: HashMap<String, String>,
        #[serde(default)]
        binding: Option<String>,
        #[serde(default)]
        dependency_bindings: BTreeSet<String>,
        #[serde(default)]
        module_source: Option<S>,
        #[serde(default)]
        default_tag_keys: crate::resource::DefaultTagKeys,
        #[serde(default)]
        loop_provenance: crate::resource::LoopProvenance,
    }

    pub(super) fn serialize<S, Ser>(
        resource: &Resource<S>,
        serializer: Ser,
    ) -> Result<Ser::Ok, Ser::Error>
    where
        S: Serialize,
        Ser: Serializer,
    {
        ResourceRef {
            id: ResourceIdRef {
                provider: &resource.id.provider,
                resource_type: &resource.id.resource_type,
                identity: resource.id.identity(),
                provider_instance: &resource.id.provider_instance,
            },
            attributes: &resource.attributes,
            directives: &resource.directives,
            prefixes: &resource.prefixes,
            binding: &resource.binding,
            dependency_bindings: &resource.dependency_bindings,
            module_source: &resource.module_source,
            default_tag_keys: &resource.default_tag_keys,
            loop_provenance: &resource.loop_provenance,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, S, De>(deserializer: De) -> Result<Resource<S>, De::Error>
    where
        S: Deserialize<'de>,
        De: Deserializer<'de>,
    {
        let ResourceWire {
            id:
                ResourceIdWire {
                    provider,
                    resource_type,
                    identity,
                    provider_instance,
                },
            attributes,
            directives,
            prefixes,
            binding,
            dependency_bindings,
            module_source,
            default_tag_keys,
            loop_provenance,
        } = ResourceWire::deserialize(deserializer)?;
        let id = match identity {
            Some(identity) => ResourceId::with_provider_identity(
                provider,
                resource_type,
                identity,
                provider_instance,
            ),
            None => ResourceId::pending_with_provider(provider, resource_type, provider_instance),
        };
        Ok(Resource {
            id,
            attributes,
            directives,
            resolved_ignore_changes: Default::default(),
            prefixes,
            binding,
            dependency_bindings,
            module_source,
            default_tag_keys,
            loop_provenance,
            quoted_string_attrs: HashSet::new(),
        })
    }
}

impl<S> DeferredForExpression<S> {
    /// Every binding that can affect the iterable, including references in a
    /// coalesce fallback.
    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        self.site().root_bindings()
    }

    /// Every direct reference spelling that may contribute the iterable.
    pub fn reference_candidates(&self) -> std::collections::BTreeSet<String> {
        let mut candidates = std::collections::BTreeSet::new();
        self.iterable.visit_resource_refs(&mut |path| {
            candidates.insert(path.to_string());
        });
        self.iterable.visit_binding_refs(&mut |binding| {
            candidates.insert(binding.to_string());
        });
        candidates
    }

    /// Compact description of the binding roots that must become available.
    /// The full iterable spelling remains available through `format_value` for
    /// unresolved expressions whose display should name the accessed value.
    pub fn binding_description(&self) -> String {
        let roots = self.root_bindings();
        if roots.is_empty() {
            crate::value::format_value(&self.iterable)
        } else {
            roots.into_iter().collect::<Vec<_>>().join(", ")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected {expected_kind} iterable, got {got_kind}")]
pub struct ShapeMismatch {
    expected_kind: &'static str,
    got_kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeferredForExpansionError {
    #[error(transparent)]
    ShapeMismatch(#[from] ShapeMismatch),
    #[error("deferred-for binding `{binding}` at line {line}, attribute `{attribute}`: {message}")]
    ValueAccess {
        binding: String,
        line: usize,
        attribute: String,
        message: String,
    },
    #[error("deferred-for iterable `{expression}` at line {line}: {message}")]
    Resolution {
        line: usize,
        expression: String,
        message: String,
    },
}

#[derive(Debug)]
struct PlaceholderAccessError {
    line: usize,
    message: String,
    /// A missing field/key reached directly through an access chain. A
    /// surrounding coalesce may consume this as `Absent`; errors nested in a
    /// collection, interpolation, or function argument are not the absence of
    /// that enclosing expression.
    coalesce_absence: bool,
}

fn nested_placeholder_error(mut error: PlaceholderAccessError) -> PlaceholderAccessError {
    error.coalesce_absence = false;
    error
}

impl ShapeMismatch {
    fn new(expected_kind: &'static str, got_kind: &'static str) -> Self {
        Self {
            expected_kind,
            got_kind,
        }
    }

    pub fn expected_kind(&self) -> &'static str {
        self.expected_kind
    }

    pub fn got_kind(&self) -> &'static str {
        self.got_kind
    }
}

/// Origin of a resource yielded by [`File::iter_all_resources`].
///
/// `Direct` means the resource was declared at top-level and its iterable
/// (if any) resolved at parse time. `Deferred` carries the phase-specific
/// loop expression: parser files expose [`ParsedDeferredForExpression`], and
/// module-resolved files expose [`DeferredForExpression`].
#[derive(Debug)]
pub enum ResourceContext<
    'a,
    S: 'a = ModuleSource,
    D: DeferredForPhaseTraversal = ResolvedDeferredForPhase,
> {
    Direct,
    Deferred(D::DeferredRef<'a, S>),
}

/// A resource yielded by [`File::iter_all_resources`], discriminated by
/// its typestate arm.
///
/// Part of the carina#3181 resource typestate split. The top-level arms borrow
/// from [`File::resources`], [`File::compositions`], and
/// [`File::data_sources`]. The deferred arm borrows the managed-resource
/// template and the phase-specific loop expression.
///
/// Read-only consumers use the `id`, `attributes`, and `binding` passthroughs;
/// callers that branch on the kind match the enum arm directly.
#[derive(Debug)]
pub enum ResourceRef<
    'a,
    S: 'a = ModuleSource,
    D: DeferredForPhaseTraversal = ResolvedDeferredForPhase,
> {
    /// A top-level resource.
    Resource(&'a Resource<S>),
    /// A top-level composition (module-expansion synthetic node).
    LiveComposition(&'a LiveComposition),
    /// A top-level data source (`read`-keyword resource).
    DataSource(&'a DataSource<S>),
    /// The managed-resource body of a deferred `for` expression. The
    /// `deferred` field's type is selected by `D`, so a resolved file cannot
    /// name or yield a parser-side loop body.
    Deferred {
        resource: &'a Resource<S>,
        deferred: D::DeferredRef<'a, S>,
    },
}

/// Mutable counterpart of [`ResourceRef`] used by whole-file resource
/// transformations. The resolved deferred arm also exposes its denormalized
/// attribute projection so callers can keep it synchronized after mutation.
#[derive(Debug)]
pub enum ResourceMut<'a, D: DeferredForPhaseTraversal = ResolvedDeferredForPhase> {
    Resource(&'a mut Resource),
    LiveComposition(&'a mut LiveComposition),
    DataSource(&'a mut DataSource),
    Deferred {
        resource: &'a mut Resource,
        attributes: D::DeferredAttributesMut<'a>,
    },
}

impl<'a, S: 'a, D: DeferredForPhaseTraversal> Copy for ResourceContext<'a, S, D> {}

impl<'a, S: 'a, D: DeferredForPhaseTraversal> Clone for ResourceContext<'a, S, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, S: 'a, D: DeferredForPhaseTraversal> Copy for ResourceRef<'a, S, D> {}

impl<'a, S: 'a, D: DeferredForPhaseTraversal> Clone for ResourceRef<'a, S, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, S: 'a, D: DeferredForPhaseTraversal> ResourceRef<'a, S, D> {
    /// Stable identifier of this resource.
    pub fn id(&self) -> &'a ResourceId {
        match self {
            ResourceRef::Resource(r) => &r.id,
            ResourceRef::LiveComposition(v) => &v.id,
            ResourceRef::DataSource(d) => &d.id,
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => &resource.id,
        }
    }

    /// Source-order preserving attribute map as `Value`s.
    ///
    /// Returned as a [`Cow`](std::borrow::Cow) because
    /// [`LiveComposition`] stores its attributes typed as
    /// [`LiveCompositionAttribute`](crate::resource::LiveCompositionAttribute)
    /// since #3294, and must materialize a `Value`-typed view on
    /// demand. Every other variant returns a borrowed reference. Callers
    /// `.iter()` / `.contains_key()` / `.get()` through the `Cow` directly
    /// via `Deref`.
    pub fn attributes(&self) -> std::borrow::Cow<'a, IndexMap<String, Value>> {
        match self {
            ResourceRef::Resource(r) => std::borrow::Cow::Borrowed(&r.attributes),
            ResourceRef::LiveComposition(v) => std::borrow::Cow::Owned(
                v.signature
                    .attributes
                    .iter()
                    .map(|(k, attr)| (k.clone(), attr.to_value()))
                    .collect(),
            ),
            ResourceRef::DataSource(d) => std::borrow::Cow::Borrowed(&d.attributes),
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => std::borrow::Cow::Borrowed(&resource.attributes),
        }
    }

    /// `let` binding name if any.
    pub fn binding(&self) -> Option<&'a str> {
        match self {
            ResourceRef::Resource(r) => r.binding.as_deref(),
            ResourceRef::LiveComposition(v) => v.binding.as_deref(),
            ResourceRef::DataSource(d) => d.binding.as_deref(),
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => resource.binding.as_deref(),
        }
    }

    /// Binding names this resource depends on (via `ResourceRef` /
    /// `BindingRef` values in its attribute tree).
    ///
    /// Mirrors the field formerly exposed by
    /// `ResourceLike::dependency_bindings`; #3308 lifts this accessor
    /// onto `ResourceRef` so the trait can retire.
    pub fn dependency_bindings(&self) -> &'a std::collections::BTreeSet<String> {
        match self {
            ResourceRef::Resource(r) => &r.dependency_bindings,
            ResourceRef::LiveComposition(v) => &v.dependency_bindings,
            ResourceRef::DataSource(d) => &d.dependency_bindings,
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => &resource.dependency_bindings,
        }
    }

    /// `directives` meta-argument block. [`LiveComposition`] drops the
    /// field (no `prevent_destroy` on a synthetic node) — returns `None`
    /// for the `Virtual` arm.
    pub fn directives(&self) -> Option<&'a Directives> {
        match self {
            ResourceRef::Resource(r) => Some(&r.directives),
            ResourceRef::LiveComposition(_) => None,
            ResourceRef::DataSource(d) => Some(&d.directives),
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => Some(&resource.directives),
        }
    }

    /// Parser-level set of attributes written as quoted string literals.
    pub fn quoted_string_attrs(&self) -> &'a HashSet<String> {
        match self {
            ResourceRef::Resource(r) => &r.quoted_string_attrs,
            ResourceRef::LiveComposition(v) => &v.quoted_string_attrs,
            ResourceRef::DataSource(d) => &d.quoted_string_attrs,
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => &resource.quoted_string_attrs,
        }
    }

    /// Source context for this resource: top-level, resolved deferred, or
    /// parser-side deferred.
    pub fn context(&self) -> ResourceContext<'a, S, D> {
        match self {
            ResourceRef::Deferred {
                resource: _resource,
                deferred,
            } => ResourceContext::Deferred(*deferred),
            ResourceRef::Resource(_)
            | ResourceRef::LiveComposition(_)
            | ResourceRef::DataSource(_) => ResourceContext::Direct,
        }
    }

    /// Attributes projected to a `HashMap<String, Value>` — the
    /// lookup-shaped view consumed by schema validation.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        crate::resource::attrs_to_hashmap(&self.attributes())
    }
}

/// Resource type path for typed references (e.g., aws.vpc, aws.security_group)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceTypePath {
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// Resource type (e.g., "vpc", "security_group")
    pub resource_type: String,
}

impl ResourceTypePath {
    pub fn new(provider: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
        }
    }

    /// Parse from a dot-separated string (e.g., "aws.vpc" or "aws.security_group")
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() >= 2 {
            Some(Self {
                provider: parts[0].to_string(),
                resource_type: parts[1..].join("."),
            })
        } else {
            None
        }
    }
}

impl std::fmt::Display for ResourceTypePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.provider, self.resource_type)
    }
}

/// Type expression for arguments/attributes parameters
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TypeExpr {
    String,
    Bool,
    Int,
    Float,
    /// Time duration. Surface form: `<integer><unit>` literal (`75min`,
    /// `1h`, `30s`); internal form: `Value::Concrete(ConcreteValue::Duration(std::time::Duration))`.
    Duration,
    /// Schema type identified by name (e.g., "ipv4_cidr", "ipv4_address", "arn")
    Simple(std::string::String),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>),
    /// Reference to a resource type (e.g., aws.vpc)
    Ref(ResourceTypePath),
    /// Dotted type path produced by the parser before provider schemas
    /// have been loaded enough to classify it as a resource ref or
    /// provider custom type.
    DottedUnresolved(ResourceTypePath),
    /// Provider-defined schema type (e.g., awscc.ec2.VpcId, awscc.ec2.SubnetId).
    ///
    /// The owner and semantic identity travel as one key because they are not
    /// interchangeable: AWSCC can own a type in the shared `aws` identity
    /// namespace.
    SchemaType {
        owned: super::OwnedTypeIdentity,
    },
    /// Structural record type: `struct { name: type, ... }`.
    ///
    /// Field order matches source order and participates in `PartialEq` —
    /// two struct types with the same fields in different order are not
    /// equal. A `Value::Concrete(ConcreteValue::Map)` satisfies a struct type when every field
    /// name appears as a key with a value that matches the field's type,
    /// with no extra keys.
    Struct {
        fields: Vec<(String, TypeExpr)>,
    },
    /// Singleton string literal type: `'dev'` accepts only the value
    /// `Value::Concrete(ConcreteValue::String("dev"))` (carina-rs/carina#2611). Composes with
    /// [`TypeExpr::Union`] to produce closed-set string types like
    /// `'dev' | 'prod'`, and with [`TypeExpr::List`] / `Map` to nest
    /// (`list('dev' | 'prod')`).
    StringLiteral(String),
    /// Union of two or more types: `T1 | T2 | ...`. A value matches
    /// the union if it matches at least one member type. Today the
    /// only grammar-reachable shape is unions of [`TypeExpr::StringLiteral`]
    /// — `'dev' | 'prod'` — but the AST shape stays general so future
    /// additions (`String | Int`, nullable types via `T | none`) drop
    /// in without another structural change. See carina-rs/carina#2611.
    Union(Vec<TypeExpr>),
}

impl TypeExpr {
    /// True when this `TypeExpr` represents a string-shaped value at
    /// runtime: bare `String`, a `Simple` named identity (typically a
    /// string-base custom like `AwsAccountId`), or a `SchemaType`
    /// (provider-defined string-typed identifier like
    /// `awscc.ec2.VpcId`). Callers use this to accept these in any
    /// string-compatible receiver position; the symmetric strictness
    /// in the opposite direction (`String → Custom{Specific}`) is
    /// enforced by `attr_type_demands_specific_custom` in the
    /// validation crate.
    pub fn is_string_shaped(&self) -> bool {
        matches!(
            self,
            TypeExpr::String
                | TypeExpr::Simple(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
        )
    }
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::String => write!(f, "String"),
            TypeExpr::Bool => write!(f, "Bool"),
            TypeExpr::Int => write!(f, "Int"),
            TypeExpr::Float => write!(f, "Float"),
            TypeExpr::Duration => write!(f, "Duration"),
            TypeExpr::Simple(name) => write!(f, "{}", snake_to_pascal(name)),
            TypeExpr::List(inner) => write!(f, "list({})", inner),
            TypeExpr::Map(inner) => write!(f, "map({})", inner),
            TypeExpr::Ref(path) => write!(f, "{}", path),
            TypeExpr::DottedUnresolved(path) => write!(f, "{}", path),
            TypeExpr::SchemaType { owned } => f.write_str(&owned.authored_name()),
            TypeExpr::Struct { fields } => {
                if fields.is_empty() {
                    write!(f, "struct {{}}")
                } else {
                    write!(f, "struct {{ ")?;
                    for (i, (name, ty)) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", name, ty)?;
                    }
                    write!(f, " }}")
                }
            }
            TypeExpr::StringLiteral(s) => f.write_str(&display_string_literal(s)),
            TypeExpr::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{}", m)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod type_expr_display_tests {
    use super::TypeExpr;

    #[test]
    fn string_literal_with_apostrophe_uses_canonical_quotes() {
        assert_eq!(
            TypeExpr::StringLiteral("it's".to_string()).to_string(),
            "\"it's\""
        );
    }
}

/// Argument parameter definition (in `arguments { ... }` block)
#[derive(Debug, Clone)]
pub struct ArgumentParameter<T = TypeExpr> {
    pub name: String,
    pub static_type: T,
    pub default: Option<Value>,
    /// Optional description (from block form)
    pub description: Option<String>,
    /// Optional validation blocks (from block form). Multiple blocks are allowed.
    pub validations: Vec<ValidationBlock>,
}

pub type ConsumerArgumentParameter = ArgumentParameter<ConsumerStaticType>;

/// A validate block: `validation { condition = <expr> error_message = "..." }`
#[derive(Debug, Clone)]
pub struct ValidationBlock {
    pub condition: ValidateExpr,
    pub error_message: Option<String>,
    source: DeclarationSource,
}

impl ValidationBlock {
    pub(crate) fn new_in_source(
        condition: ValidateExpr,
        error_message: Option<String>,
        source_span: SourceSpan,
        source_file: PathBuf,
    ) -> Self {
        Self {
            condition,
            error_message,
            source: DeclarationSource::authored(source_span, source_file),
        }
    }

    #[cfg(test)]
    pub(crate) fn dummy(condition: ValidateExpr, error_message: Option<String>) -> Self {
        Self::new_in_source(
            condition,
            error_message,
            SourceSpan::dummy(),
            PathBuf::from("test.crn"),
        )
    }

    pub fn source_span(&self) -> SourceSpan {
        self.source.span
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source.file
    }
}

// `CompareOp` lives in `expressions::validate_expr` (re-exported above).

/// Validate expression AST node
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ValidateExpr {
    /// Boolean literal
    Bool(bool),
    /// Integer literal
    Int(i64),
    /// Float literal
    Float(f64),
    /// Duration literal (`75min`, `1h`, `30s`).
    Duration(std::time::Duration),
    /// String literal
    String(String),
    /// Variable reference (argument name)
    Var(String),
    /// Attribute/index access rooted at a module argument. The path remains
    /// explicit until module expansion knows whether the caller supplied a
    /// value or a structural handle.
    Access {
        argument: String,
        path: Vec<PathSegment>,
    },
    /// Module-expansion result for one argument operand. Retaining the
    /// authored argument name keeps diagnostics stable while the typed value
    /// (often a caller-side `ResourceRef`) resolves during plan/apply.
    BoundArgument {
        argument: String,
        /// Authored operand, including any postfix path, used for diagnostics.
        #[serde(default)]
        operand: String,
        value: Value,
    },
    /// Binary Int arithmetic used by `require` and argument validation.
    IntegerArithmetic {
        lhs: Box<ValidateExpr>,
        op: crate::integer_arithmetic::IntegerOp,
        rhs: Box<ValidateExpr>,
        #[serde(
            skip,
            default = "crate::integer_arithmetic::IntegerOperatorSpan::synthetic"
        )]
        operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
    },
    /// Comparison: lhs op rhs
    Compare {
        lhs: Box<ValidateExpr>,
        op: CompareOp,
        rhs: Box<ValidateExpr>,
    },
    /// Logical AND
    And(Box<ValidateExpr>, Box<ValidateExpr>),
    /// Logical OR
    Or(Box<ValidateExpr>, Box<ValidateExpr>),
    /// Logical NOT
    Not(Box<ValidateExpr>),
    /// Function call (e.g., len(x))
    FunctionCall {
        name: String,
        args: Vec<ValidateExpr>,
    },
    /// Null literal
    Null,
}

/// A require block: `require <condition>, "error message"`
/// Used for cross-argument constraints at the module top level.
#[derive(Debug, Clone)]
pub struct RequireBlock {
    pub condition: ValidateExpr,
    pub error_message: String,
    source: DeclarationSource,
}

impl RequireBlock {
    pub(crate) fn new_in_source(
        condition: ValidateExpr,
        error_message: String,
        source_span: SourceSpan,
        source_file: PathBuf,
    ) -> Self {
        Self {
            condition,
            error_message,
            source: DeclarationSource::authored(source_span, source_file),
        }
    }

    #[cfg(test)]
    pub(crate) fn dummy(condition: ValidateExpr, error_message: String) -> Self {
        Self::new_in_source(
            condition,
            error_message,
            SourceSpan::dummy(),
            PathBuf::from("test.crn"),
        )
    }

    pub fn source_span(&self) -> SourceSpan {
        self.source.span
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source.file
    }
}

/// Attribute parameter definition (in `attributes { ... }` block)
#[derive(Debug, Clone)]
pub struct AttributeParameter<T = TypeExpr> {
    pub name: String,
    pub static_type: Option<T>,
    pub value: Option<Value>,
    source: DeclarationSource,
}

impl AttributeParameter {
    pub(crate) fn new_in_source(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value>,
        source_span: SourceSpan,
        source_file: PathBuf,
    ) -> Self {
        Self {
            name,
            static_type: type_expr,
            value,
            source: DeclarationSource::authored(source_span, source_file),
        }
    }

    #[cfg(test)]
    pub(crate) fn dummy(name: String, type_expr: Option<TypeExpr>, value: Option<Value>) -> Self {
        Self::new_in_source(
            name,
            type_expr,
            value,
            SourceSpan::dummy(),
            PathBuf::from("test.crn"),
        )
    }

    pub fn source_span(&self) -> SourceSpan {
        self.source.span
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source.file
    }
}

impl<T> AttributeParameter<T> {
    pub(crate) fn map_type_expr<U>(self, map: impl FnOnce(T) -> U) -> AttributeParameter<U> {
        AttributeParameter {
            name: self.name,
            static_type: self.static_type.map(map),
            value: self.value,
            source: self.source,
        }
    }
}

pub type ConsumerAttributeParameter = AttributeParameter<ConsumerStaticType>;

/// An export parameter in an `exports { }` block, as produced by the
/// parser before any inference runs.
///
/// `static_type` is optional here because the user may have
/// omitted the annotation. The loader then runs `apply_inference`
/// (#2360 stage 2) to resolve the effective type and emits
/// [`InferredExportParam`] downstream.
#[derive(Debug, Clone)]
pub struct ParsedExportParam {
    pub name: String,
    pub static_type: Option<TypeExpr>,
    pub value: Option<Value>,
    source: DeclarationSource,
}

impl ParsedExportParam {
    pub(crate) fn new_in_source(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value>,
        source_span: SourceSpan,
        source_file: PathBuf,
    ) -> Self {
        Self {
            name,
            static_type: type_expr,
            value,
            source: DeclarationSource::authored(source_span, source_file),
        }
    }

    #[cfg(test)]
    pub(crate) fn dummy(name: String, type_expr: Option<TypeExpr>, value: Option<Value>) -> Self {
        Self::new_in_source(
            name,
            type_expr,
            value,
            SourceSpan::dummy(),
            PathBuf::from("test.crn"),
        )
    }

    pub fn source_span(&self) -> SourceSpan {
        self.source.span
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source.file
    }
}

impl PartialEq for ParsedExportParam {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            name,
            static_type,
            value,
            source: _,
        } = self;
        let Self {
            name: other_name,
            static_type: other_static_type,
            value: other_value,
            source: _,
        } = other;
        name == other_name && static_type == other_static_type && value == other_value
    }
}

/// Alias kept so the parser's own construct sites (which always
/// produce the parser-phase shape) read naturally as
/// `ExportParameter`; downstream consumers should use the explicit
/// [`ParsedExportParam`] / [`InferredExportParam`] names.
pub type ExportParameter = ParsedExportParam;

/// Phase-agnostic accessor over an export parameter. Implemented by both
/// [`ParsedExportParam`] (parser phase) and [`InferredExportParam`]
/// (post-loader phase) so reference resolution, type validation, module
/// resolution, and upstream-state checks work uniformly across both shapes.
pub trait ExportParamLike {
    fn name(&self) -> &str;
    fn value(&self) -> Option<&Value>;
    /// Replace the value after binding-aware export resolution.
    ///
    /// This mutator is intentionally part of the phase-agnostic trait so the
    /// export boundary can return the same parser/inference shape wrapped in
    /// a checked type instead of maintaining parallel representations.
    fn set_value(&mut self, value: Option<Value>);
    fn type_expr_opt(&self) -> Option<&TypeExpr>;
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr>;
    fn inference_failed(&self) -> bool;
}

impl ExportParamLike for ParsedExportParam {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }
    fn set_value(&mut self, value: Option<Value>) {
        self.value = value;
    }
    fn type_expr_opt(&self) -> Option<&TypeExpr> {
        self.static_type.as_ref()
    }
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr> {
        self.static_type.as_mut()
    }
    fn inference_failed(&self) -> bool {
        false
    }
}

/// An address as written in a state block (`import { to = X 'addr' }`,
/// `removed { from = X 'addr' }`, `moved { from = X 'a', to = X 'b' }`).
///
/// The DSL surface form for these blocks has **no syntax for routing**
/// — there is no slot to specify which `provider_instance` the address
/// targets. The `provider_instance` of the matched resource (or state
/// row) is decided downstream by looking at the let-bound resource's
/// `directives { provider = ... }`.
///
/// This type is the **routing-agnostic** counterpart to
/// [`crate::resource::ResourceId`]:
///
/// - Its `Eq`/`Hash` includes only `(provider, resource_type, name)`.
///   Comparing two state-block addresses cannot silently mismatch on
///   a routing field that the DSL never specified.
/// - It does **not** implement `PartialEq<ResourceId>` and there is no
///   `From<&StateBlockAddress>` / `From<&ResourceId>` shortcut.
///   [`matches_resource_id`](Self::matches_resource_id) is the single
///   definition of whether an address identifies a configured resource or
///   persisted state row; plan and state lookups route through it. Only when
///   no match exists may a caller use the explicit
///   [`to_unrouted_resource_id`](Self::to_unrouted_resource_id)
///   escape hatch documented on the method itself.
///
/// This shape makes the carina#3324 bug class — comparing a
/// None-routed parsed address against a routed `ResourceId` and
/// silently missing — unrepresentable: the type signature forces every
/// consumer to perform the resolution step explicitly. A new consumer
/// added tomorrow cannot reach the buggy path; the compiler rejects
/// the comparison.
///
/// The cross-type equality guard is enforced by the type system:
///
/// ```compile_fail
/// use carina_core::parser::StateBlockAddress;
/// use carina_core::resource::ResourceId;
///
/// let addr = StateBlockAddress::new("aws", "route53.RecordSet", "r");
/// let routed = ResourceId::with_provider_identity(
///     "aws",
///     "route53.RecordSet",
///     "r",
///     Some("management".to_string()),
/// );
///
/// // The compiler rejects this — `StateBlockAddress` does not
/// // implement `PartialEq<ResourceId>`. A new consumer cannot
/// // silently miss a routed-vs-unrouted match the way carina#3324
/// // did before the newtype existed.
/// let _ = addr == routed;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StateBlockAddress {
    pub provider: String,
    pub resource_type: String,
    pub name: crate::resource::ResourceIdentity,
}

impl StateBlockAddress {
    /// Construct a `StateBlockAddress`. The `name` is canonicalized
    /// through [`crate::utils::canonicalize_map_key_address`] so all
    /// three DSL surface forms (`binding.key`, `binding['key']`,
    /// `binding["key"]`) collapse to the same value — the parser's
    /// `parse_state_block_address` does the same, and the type's
    /// `Eq`/`Hash` only match when the canonical form does. Without
    /// canonicalization at the constructor, a programmatic caller
    /// could build an address that silently fails to match a parsed
    /// one (the same class of bug the newtype exists to prevent).
    pub fn new(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<crate::resource::ResourceIdentity>,
    ) -> Self {
        let raw_name = name.into();
        let canonical = crate::utils::canonicalize_map_key_address(raw_name.as_str());
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            name: crate::resource::ResourceIdentity::new(canonical),
        }
    }

    /// Borrow the address's name as `&str`.
    pub fn name_str(&self) -> &str {
        self.name.as_str()
    }

    /// Returns the display type including provider prefix, matching
    /// [`ResourceId::display_type`] so plan/error output reads the
    /// same for both routing-agnostic and routed addresses.
    pub fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }

    /// Whether this routing-agnostic address identifies `resource_id`.
    ///
    /// State-block syntax has no provider-instance slot, so matching uses the
    /// authored `(provider, resource_type, identity)` tuple and deliberately
    /// ignores [`ResourceId::provider_instance`](crate::resource::ResourceId::provider_instance).
    /// This is the single cross-type matching rule. Import/move planning,
    /// state-file lookup, state-block claims, and unused-binding validation
    /// all share this primitive.
    pub fn matches_resource_id(&self, resource_id: &crate::resource::ResourceId) -> bool {
        resource_id.provider == self.provider
            && resource_id.resource_type == self.resource_type
            && resource_id.identity() == Some(&self.name)
    }

    /// Find the configured resource identified by this address.
    pub fn find_resource<'a>(
        &self,
        resources: impl IntoIterator<Item = &'a crate::resource::Resource>,
    ) -> Option<&'a crate::resource::Resource> {
        resources
            .into_iter()
            .find(|resource| self.matches_resource_id(&resource.id))
    }

    /// Last-resort lift to a [`ResourceId`] with no routing.
    ///
    /// Use this **only** when the address resolved against no plan or
    /// state entry (e.g. the user wrote `import { to = X 'addr' }` for
    /// a resource that does not exist anywhere). The resulting
    /// `ResourceId` carries `provider_instance = None`, which is the
    /// best the type system can do when there is no row to inherit
    /// routing from.
    ///
    /// Normal resolution paths must instead inherit routing from the
    /// matched plan Create or state row — they should never call this
    /// method as a shortcut. Doing so re-introduces the carina#3324
    /// bug class.
    pub fn to_unrouted_resource_id(&self) -> crate::resource::ResolvedResourceId {
        crate::resource::ResolvedResourceId::with_provider_identity(
            self.provider.clone(),
            self.resource_type.clone(),
            self.name.clone(),
            None,
        )
    }
}

impl std::fmt::Display for StateBlockAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}.{}", self.resource_type, self.name)
        } else {
            write!(f, "{}.{}.{}", self.provider, self.resource_type, self.name)
        }
    }
}

/// The two routing-agnostic addresses in a `moved` block.
///
/// carina#3749: a move changes only a resource's identity. The provider and
/// resource type are stored once so a parsed move cannot represent a transfer
/// between different resource schemas.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MovedAddresses {
    provider: String,
    resource_type: String,
    from_name: crate::resource::ResourceIdentity,
    to_name: crate::resource::ResourceIdentity,
    #[serde(skip, default)]
    to_span: Option<SourceSpan>,
}

impl MovedAddresses {
    /// Construct same-type source and destination addresses for a move.
    pub fn new(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        from_name: impl Into<crate::resource::ResourceIdentity>,
        to_name: impl Into<crate::resource::ResourceIdentity>,
    ) -> Self {
        let from_name = from_name.into();
        let to_name = to_name.into();
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            from_name: crate::resource::ResourceIdentity::new(
                crate::utils::canonicalize_map_key_address(from_name.as_str()),
            ),
            to_name: crate::resource::ResourceIdentity::new(
                crate::utils::canonicalize_map_key_address(to_name.as_str()),
            ),
            to_span: None,
        }
    }

    pub(in crate::parser) fn with_to_span(mut self, to_span: SourceSpan) -> Self {
        self.to_span = Some(to_span);
        self
    }

    /// Build the full source address.
    pub fn from(&self) -> StateBlockAddress {
        StateBlockAddress {
            provider: self.provider.clone(),
            resource_type: self.resource_type.clone(),
            name: self.from_name.clone(),
        }
    }

    /// Build the full destination address.
    pub fn to(&self) -> StateBlockAddress {
        StateBlockAddress {
            provider: self.provider.clone(),
            resource_type: self.resource_type.clone(),
            name: self.to_name.clone(),
        }
    }

    /// Parser span of the authored destination address, when available.
    pub fn to_span(&self) -> Option<SourceSpan> {
        self.to_span
    }
}

/// State manipulation block (import, removed, moved)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StateBlock {
    /// Import existing infrastructure into Carina management
    Import {
        /// Target resource address (routing-agnostic — see
        /// [`StateBlockAddress`]).
        to: StateBlockAddress,
        /// Parser span of the authored destination address. Diagnostic-only;
        /// saved representations retain the typed address but not source bytes.
        #[serde(skip, default)]
        to_span: Option<SourceSpan>,
        /// Cloud provider identifier (e.g., `"vpc-0abc123def456"`).
        ///
        /// Carried as a [`Value`] (not `String`) so a `"${X.attr}|..."`
        /// interpolation referencing a deferred upstream-state value
        /// stays a `Value::Deferred(DeferredValue::Interpolation)` from
        /// parse through plan-time resolution and display. The pre-#3329
        /// `String` shape silently dropped `${...}` segments at parse
        /// time and presented a partially-substituted literal as if it
        /// were a real cloud identifier. See carina#3329.
        id: Value,
    },
    /// Remove a resource from state without destroying it
    Removed {
        /// Resource address to remove from state (routing-agnostic).
        from: StateBlockAddress,
    },
    /// Rename/move a resource in state without destroy/recreate
    Moved {
        /// Same-type source and destination addresses.
        addresses: MovedAddresses,
    },
}

/// Module `use` statement (previously `import`).
#[derive(Debug, Clone)]
pub struct UseStatement {
    pub path: String,
    pub alias: String,
    source: DeclarationSource,
}

impl UseStatement {
    pub(crate) fn new_in_source(
        path: String,
        alias: String,
        source_span: SourceSpan,
        source_file: PathBuf,
    ) -> Self {
        Self {
            path,
            alias,
            source: DeclarationSource::authored(source_span, source_file),
        }
    }

    #[cfg(test)]
    pub(crate) fn dummy(path: String, alias: String) -> Self {
        Self::new_in_source(path, alias, SourceSpan::dummy(), PathBuf::from("test.crn"))
    }

    pub fn source_span(&self) -> SourceSpan {
        self.source.span
    }

    pub fn source_file(&self) -> &Path {
        &self.source.file
    }
}

/// Parameter for a user-defined function
#[derive(Debug, Clone)]
pub struct FnParam {
    pub name: String,
    pub param_type: Option<TypeExpr>,
    pub default: Option<Value>,
}

/// The body of a user-defined function: a value expression.
/// Functions are pure value transformations only.
#[derive(Debug, Clone)]
pub struct UserFunctionBody {
    pub value: Value,
    pub source_line: usize,
    /// Owning `.crn` file, stamped by the directory loader after parsing.
    pub source_file: Option<std::path::PathBuf>,
}

/// User-defined pure function
#[derive(Debug, Clone)]
pub struct UserFunction {
    pub name: String,
    pub params: Vec<FnParam>,
    /// Optional return type annotation
    pub return_type: Option<TypeExpr>,
    /// Local let bindings inside the function body (name, expression)
    pub local_lets: Vec<(String, Value, usize)>,
    /// The body of the function
    pub body: UserFunctionBody,
}

/// Module call (instantiation)
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleArgument {
    operand: StructuralOperand,
}

impl ModuleArgument {
    pub(crate) fn value_ref(&self) -> StructuralOperandRef<'_> {
        self.operand.value_ref()
    }

    pub(crate) fn access_ref(&self) -> StructuralOperandRef<'_> {
        self.operand.access_ref()
    }

    pub(crate) fn from_parts(value: Value, access_base: Option<Value>) -> Self {
        Self {
            operand: StructuralOperand::from_value_with_access_base(value, access_base),
        }
    }

    pub fn new(value: Value) -> Self {
        Self::from_parts(value, None)
    }

    pub(crate) fn with_access_base(value: Value, access_base: Value) -> Self {
        Self::from_parts(value, Some(access_base))
    }

    pub(crate) fn from_handle(handle: StructuralHandle) -> Self {
        Self {
            operand: StructuralOperand::from_handle(handle),
        }
    }

    pub fn value(&self) -> Option<&Value> {
        self.operand.value()
    }

    /// Return the authored bare-handle diagnostic without exposing the
    /// parser-internal handle representation to downstream consumers.
    pub fn bare_handle_error(&self) -> Option<String> {
        self.handle().map(|handle| handle.bare_error().to_string())
    }

    pub(crate) fn handle(&self) -> Option<&StructuralHandle> {
        self.operand.handle()
    }

    pub(crate) fn pending_parameter(&self) -> Option<&str> {
        self.operand.parameter()
    }

    pub(crate) fn access_base(&self) -> Option<&Value> {
        self.operand.explicit_access_base()
    }

    /// Value-shaped projection reserved for generic reference/dependency
    /// walkers. Semantic consumers must use the typed operand accessors.
    pub(crate) fn walker_value(&self) -> &Value {
        self.operand.traversal_value()
    }

    pub(crate) fn try_rewrite<E>(
        &mut self,
        rewrite: impl FnMut(&Value) -> Result<Value, E>,
    ) -> Result<(), E> {
        self.operand = self.operand.try_map(rewrite)?;
        Ok(())
    }
}

impl From<Value> for ModuleArgument {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

/// Parser-owned identity of a source file containing a module call.
///
/// The inner path is intentionally opaque: callers may inspect it for
/// diagnostics and saved-plan normalization, but only the parser can mint an
/// identity-bearing module call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceFileId(Arc<Path>);

impl SourceFileId {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(Arc::from(path.into_boxed_path()))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Parser-owned identity of one lexical binding scope.
///
/// The directory path separates root and imported-module scopes. A lexical
/// child additionally carries its source file and parser-minted ordinal so
/// sibling files cannot mint the same child identity. Authored names are
/// deliberately not used as scope identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BindingScopeId {
    directory: Arc<Path>,
    lexical_source_file: Option<Arc<Path>>,
    ordinal: u64,
}

impl BindingScopeId {
    pub(crate) fn root(source_file: &Path) -> Self {
        let directory = source_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self {
            directory: Arc::from(directory.into_boxed_path()),
            lexical_source_file: None,
            ordinal: 0,
        }
    }

    pub(in crate::parser) fn child(&self, source_file: &Path, ordinal: u64) -> Self {
        Self {
            directory: self.directory.clone(),
            lexical_source_file: Some(Arc::from(source_file.to_path_buf().into_boxed_path())),
            ordinal,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

/// Stable parser address of a resource that owns an authored value sink.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceAddress(ResourceId);

impl ResourceAddress {
    pub(in crate::parser) fn new(id: ResourceId) -> Self {
        Self(id)
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.0
    }
}

/// Owner of one successfully resolved bare binding use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BindingUseOwner {
    Resource(ResourceAddress),
    ModuleArgument {
        call: ModuleCallId,
        argument: String,
        value_span: SourceSpan,
    },
}

/// One structural step below a resource attribute or module argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AttributePathSegment {
    Attribute(String),
    ListIndex(usize),
    Key(String),
    StructField(String),
}

/// Parser-authored path from an owning sink to one bare binding leaf.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttributePath(Arc<[AttributePathSegment]>);

impl AttributePath {
    pub(in crate::parser) fn root(name: impl Into<String>) -> Self {
        Self(Arc::from([AttributePathSegment::Attribute(name.into())]))
    }

    pub(in crate::parser) fn with_segment(&self, segment: AttributePathSegment) -> Self {
        let mut path = self.0.to_vec();
        path.push(segment);
        Self(path.into())
    }

    pub fn segments(&self) -> &[AttributePathSegment] {
        &self.0
    }

    pub fn root_name(&self) -> &str {
        match self.0.first() {
            Some(AttributePathSegment::Attribute(name)) => name,
            Some(
                AttributePathSegment::ListIndex(_)
                | AttributePathSegment::Key(_)
                | AttributePathSegment::StructField(_),
            )
            | None => unreachable!("AttributePath is constructed with an attribute root"),
        }
    }
}

/// Parser-side classification of the lexical binding selected for a bare use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BareBindingKind {
    Value {
        declaration: crate::schema::BindingKind,
    },
    Structural {
        kind: StructuralHandleKind,
        resource_type: Option<ResourceTypePath>,
    },
}

/// Authored provenance for one bare identifier resolved by normal lexical
/// lookup. This data is parser-only and never enters [`Value`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareBindingUse {
    pub source_file: SourceFileId,
    pub scope: BindingScopeId,
    pub owner: BindingUseOwner,
    pub path: AttributePath,
    pub name: String,
    pub span: SourceSpan,
    pub binding: BareBindingKind,
}

/// A value sink reserved because at least one structural leaf cannot be
/// represented as runtime [`Value`] data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingValueSlot {
    pub owner: BindingUseOwner,
    pub path: AttributePath,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BareBindingSource {
    Value {
        value: Arc<Value>,
        declared_type: Option<Arc<TypeExpr>>,
    },
    Structural(StructuralHandle),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedBareBindingUse {
    pub(crate) authored: BareBindingUse,
    pub(crate) source: BareBindingSource,
    pub(crate) pending_slot: Option<PendingValueSlot>,
}

/// Opaque parsed-file side data for bare binding provenance and pending sinks.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedBindingUses {
    records: Vec<ParsedBareBindingUse>,
    record_indices_by_owner: HashMap<BindingUseOwnerKey, Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BindingUseOwnerKey {
    Resource(ResourceId),
    ModuleCall(ModuleCallId),
}

impl From<&BindingUseOwner> for BindingUseOwnerKey {
    fn from(owner: &BindingUseOwner) -> Self {
        match owner {
            BindingUseOwner::Resource(address) => Self::Resource(address.resource_id().clone()),
            BindingUseOwner::ModuleArgument { call, .. } => Self::ModuleCall(call.clone()),
        }
    }
}

impl ParsedBindingUses {
    pub(crate) fn record(
        &mut self,
        authored: BareBindingUse,
        source: BareBindingSource,
        pending_slot: Option<PendingValueSlot>,
    ) {
        let owner = BindingUseOwnerKey::from(&authored.owner);
        let pending_slot = pending_slot.filter(|candidate| {
            !self
                .records
                .iter()
                .filter_map(|record| record.pending_slot.as_ref())
                .any(|slot| slot == candidate)
        });
        let index = self.records.len();
        self.records.push(ParsedBareBindingUse {
            authored,
            source,
            pending_slot,
        });
        self.record_indices_by_owner
            .entry(owner)
            .or_default()
            .push(index);
    }

    pub(crate) fn records(&self) -> &[ParsedBareBindingUse] {
        &self.records
    }

    pub fn uses(&self) -> impl ExactSizeIterator<Item = &BareBindingUse> {
        self.records.iter().map(|record| &record.authored)
    }

    pub fn pending_slots(&self) -> impl Iterator<Item = &PendingValueSlot> {
        self.records
            .iter()
            .filter_map(|record| record.pending_slot.as_ref())
    }

    pub(crate) fn has_pending_slot_for(&self, use_site: &BareBindingUse) -> bool {
        self.record_indices_by_owner
            .get(&BindingUseOwnerKey::from(&use_site.owner))
            .is_some_and(|indices| {
                indices.iter().any(|index| {
                    self.records[*index]
                        .pending_slot
                        .as_ref()
                        .is_some_and(|slot| slot.path.root_name() == use_site.path.root_name())
                })
            })
    }

    pub(crate) fn remap_resource_owner(&mut self, from: &ResourceId, to: &ResourceId) {
        let from = BindingUseOwnerKey::Resource(from.clone());
        let to_key = BindingUseOwnerKey::Resource(to.clone());
        if let Some(indices) = self.record_indices_by_owner.remove(&from) {
            for index in &indices {
                let BindingUseOwner::Resource(address) = &mut self.records[*index].authored.owner
                else {
                    unreachable!("resource owner index points to a module argument")
                };
                *address = ResourceAddress::new(to.clone());
                if let Some(slot) = &mut self.records[*index].pending_slot {
                    let BindingUseOwner::Resource(address) = &mut slot.owner else {
                        unreachable!("resource record has a module-argument pending slot")
                    };
                    *address = ResourceAddress::new(to.clone());
                }
            }
            self.record_indices_by_owner
                .entry(to_key)
                .or_default()
                .extend(indices);
        }
    }

    pub(crate) fn remap_module_call_owner(&mut self, from: &ModuleCallId, to: &ModuleCallId) {
        let from = BindingUseOwnerKey::ModuleCall(from.clone());
        let to_key = BindingUseOwnerKey::ModuleCall(to.clone());
        if let Some(indices) = self.record_indices_by_owner.remove(&from) {
            for index in &indices {
                let BindingUseOwner::ModuleArgument { call, .. } =
                    &mut self.records[*index].authored.owner
                else {
                    unreachable!("module-call owner index points to a resource")
                };
                *call = to.clone();
                if let Some(slot) = &mut self.records[*index].pending_slot {
                    let BindingUseOwner::ModuleArgument { call, .. } = &mut slot.owner else {
                        unreachable!("module-call record has a resource pending slot")
                    };
                    *call = to.clone();
                }
            }
            self.record_indices_by_owner
                .entry(to_key)
                .or_default()
                .extend(indices);
        }
    }

    pub(crate) fn extend(&mut self, other: Self) {
        for record in other.records {
            self.record(record.authored, record.source, record.pending_slot);
        }
    }
}

/// Parser-owned discriminator for one concrete `for` expansion.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpansionKey(String);

impl ExpansionKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a module call is direct or one concrete expansion of a `for` body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CallExpansion {
    Direct,
    Expanded(ExpansionKey),
}

/// Deterministic parser-minted identity of one concrete module call.
///
/// User-authored names and argument values deliberately do not participate in
/// equality. The source file and authored span identify the call site, while
/// [`CallExpansion`] distinguishes concrete iterations sharing that span.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleCallId {
    source_file: SourceFileId,
    call_span: SourceSpan,
    expansion: CallExpansion,
}

impl ModuleCallId {
    pub(in crate::parser) fn direct(source_file: PathBuf, call_span: SourceSpan) -> Self {
        Self {
            source_file: SourceFileId::new(source_file),
            call_span,
            expansion: CallExpansion::Direct,
        }
    }

    fn expand(&mut self, expansion_key: String) {
        self.expansion = CallExpansion::Expanded(ExpansionKey(expansion_key));
    }

    pub fn source_file(&self) -> &Path {
        self.source_file.path()
    }

    pub fn source_file_id(&self) -> &SourceFileId {
        &self.source_file
    }

    pub fn call_span(&self) -> SourceSpan {
        self.call_span
    }

    pub fn expansion(&self) -> &CallExpansion {
        &self.expansion
    }

    #[cfg(test)]
    pub(crate) fn test_dummy_in(source_file: impl Into<PathBuf>) -> Self {
        Self::direct(source_file.into(), SourceSpan::dummy())
    }
}

/// Module call (instantiation)
#[derive(Debug, Clone)]
pub struct ModuleCall {
    pub module_name: String,
    pub binding_name: Option<String>,
    arguments: HashMap<String, AuthoredModuleArgument>,
    source: Box<ModuleCallSite>,
    loop_origin: Option<Box<crate::resource::LoopElementOrigin>>,
}

impl ModuleCall {
    pub(super) fn from_authored_arguments_in_source(
        module_name: impl Into<String>,
        binding_name: Option<String>,
        call_span: SourceSpan,
        arguments: HashMap<String, AuthoredModuleArgument>,
        source_file: PathBuf,
    ) -> Self {
        Self {
            module_name: module_name.into(),
            binding_name,
            arguments,
            source: Box::new(ModuleCallSite {
                id: ModuleCallId::direct(source_file, call_span),
            }),
            loop_origin: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test(
        module_name: impl Into<String>,
        binding_name: Option<String>,
        arguments: HashMap<String, ModuleArgument>,
    ) -> Self {
        let span = SourceSpan::dummy();
        Self::from_authored_arguments_in_source(
            module_name,
            binding_name,
            span,
            arguments
                .into_iter()
                .map(|(name, argument)| (name, AuthoredModuleArgument::new(argument, span, span)))
                .collect(),
            PathBuf::from("test.crn"),
        )
    }

    pub(super) fn set_expansion_key(&mut self, expansion_key: String) {
        self.source.id.expand(expansion_key);
    }

    pub(in crate::parser) fn set_loop_origin(
        &mut self,
        origin: Option<crate::resource::LoopElementOrigin>,
    ) {
        self.loop_origin = origin.map(Box::new);
    }

    pub(crate) fn loop_origin(&self) -> Option<&crate::resource::LoopElementOrigin> {
        self.loop_origin.as_deref()
    }

    pub(crate) fn loop_origin_mut(&mut self) -> Option<&mut crate::resource::LoopElementOrigin> {
        self.loop_origin.as_deref_mut()
    }

    pub fn id(&self) -> &ModuleCallId {
        &self.source.id
    }

    /// Source file assigned before this call was parsed.
    pub fn source_file(&self) -> &std::path::Path {
        self.source.id.source_file()
    }

    /// Return this call's parser span; every call carries one by construction.
    pub fn source_span(&self) -> SourceSpan {
        self.source.id.call_span()
    }

    /// Iterate authored arguments, each of which carries parser spans by construction.
    pub fn arguments(&self) -> impl Iterator<Item = (&str, &ModuleArgument)> {
        self.arguments
            .iter()
            .map(|(name, argument)| (name.as_str(), &argument.argument))
    }

    /// Iterate borrowed authored arguments with their parser key and value spans.
    ///
    /// Every authored argument carries both spans by construction.
    pub fn arguments_with_source_spans(&self) -> impl Iterator<Item = AuthoredArgumentRef<'_>> {
        self.arguments
            .iter()
            .map(|(name, argument)| AuthoredArgumentRef {
                name,
                argument: &argument.argument,
                key_span: argument.key_span,
                value_span: argument.value_span,
            })
    }

    pub fn argument(&self, name: &str) -> Option<&ModuleArgument> {
        self.arguments.get(name).map(|argument| &argument.argument)
    }

    pub fn authored_argument(&self, name: &str) -> Option<AuthoredArgumentRef<'_>> {
        self.arguments
            .get_key_value(name)
            .map(|(name, argument)| AuthoredArgumentRef {
                name,
                argument: &argument.argument,
                key_span: argument.key_span,
                value_span: argument.value_span,
            })
    }

    pub fn contains_argument(&self, name: &str) -> bool {
        self.arguments.contains_key(name)
    }

    pub(crate) fn arguments_mut(&mut self) -> impl Iterator<Item = &mut ModuleArgument> {
        self.arguments
            .values_mut()
            .map(|argument| &mut argument.argument)
    }

    /// Source identity retained by a directory-loaded module call.
    pub fn diagnostic_source(&self) -> ModuleCallDiagnosticSource {
        ModuleCallDiagnosticSource {
            id: self.source.id.clone(),
        }
    }
}

/// Borrowed view of one parser-authored module-call argument.
#[derive(Debug, Clone, Copy)]
pub struct AuthoredArgumentRef<'a> {
    name: &'a str,
    argument: &'a ModuleArgument,
    key_span: SourceSpan,
    value_span: SourceSpan,
}

impl<'a> AuthoredArgumentRef<'a> {
    pub fn name(self) -> &'a str {
        self.name
    }

    pub fn argument(self) -> &'a ModuleArgument {
        self.argument
    }

    pub fn key_span(self) -> SourceSpan {
        self.key_span
    }

    pub fn value_span(self) -> SourceSpan {
        self.value_span
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AuthoredModuleArgument {
    argument: ModuleArgument,
    key_span: SourceSpan,
    value_span: SourceSpan,
}

impl AuthoredModuleArgument {
    pub(crate) fn new(
        argument: ModuleArgument,
        key_span: SourceSpan,
        value_span: SourceSpan,
    ) -> Self {
        Self {
            argument,
            key_span,
            value_span,
        }
    }
}

#[derive(Debug, Clone)]
struct ModuleCallSite {
    id: ModuleCallId,
}

#[derive(Debug, Clone)]
struct DeclarationSource {
    file: PathBuf,
    span: SourceSpan,
}

impl DeclarationSource {
    fn authored(span: SourceSpan, file: PathBuf) -> Self {
        Self { file, span }
    }
}

/// A parser-authored source span used to retain declaration identity.
///
/// Module calls, their arguments, and declaration parameters share this
/// representation. Lines and columns are one-indexed, matching pest positions.
/// Calls expanded from one `for` body intentionally share the authored span
/// and are distinguished by the [`CallExpansion`] carried by
/// [`ModuleCall::id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
}

/// Source provenance for a resolved module argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentOrigin {
    Authored {
        key_span: SourceSpan,
        value_span: SourceSpan,
    },
    Defaulted,
}

impl SourceSpan {
    /// Build a parser-authored span and restore heredoc-collapsed positions.
    ///
    /// Pest's implicit skip may extend wrapper pairs through trailing trivia.
    /// The flattened inner tree still exposes leaf tokens and delimiter-ended
    /// compound tokens (including strings); their maximum end is the authored
    /// source end without lexing comments out of the raw source again.
    pub(in crate::parser) fn from_pest(
        pair: &pest::iterators::Pair<'_, crate::parser::Rule>,
        file: &crate::parser::PreprocessedFile,
    ) -> Self {
        let range = Self::preprocessed_range(pair);
        Self::from_preprocessed_range(range.start, range.end, file)
    }

    pub(in crate::parser) fn preprocessed_range(
        pair: &pest::iterators::Pair<'_, crate::parser::Rule>,
    ) -> std::ops::Range<usize> {
        let span = pair.as_span();
        let start = span.start();
        let token_end = pair
            .clone()
            .into_inner()
            .flatten()
            .filter(|inner| {
                inner.clone().into_inner().next().is_none()
                    || matches!(
                        inner.as_str().as_bytes().last(),
                        Some(b'\'' | b'"' | b')' | b']' | b'}')
                    )
            })
            .map(|inner| inner.as_span().end())
            .max()
            .unwrap_or_else(|| span.end());
        // A module call's closing brace is grammar punctuation rather than an
        // inner pair. The module-call pair itself ends exactly at that brace;
        // unlike declaration/value pairs, it cannot absorb trailing trivia
        // outside the block.
        let end = if matches!(
            pair.as_rule(),
            crate::parser::Rule::module_call
                | crate::parser::Rule::for_expr
                | crate::parser::Rule::if_expr
        ) {
            span.end()
        } else {
            token_end
        };
        start..end
    }

    /// Build a span for a standalone parser fragment that has no heredoc
    /// preprocessing map. Production file parsing uses [`Self::from_pest`].
    pub(in crate::parser) fn from_unmapped_pest(
        pair: &pest::iterators::Pair<'_, crate::parser::Rule>,
    ) -> Self {
        let span = pair.as_span();
        let (start_line, start_column) = span.start_pos().line_col();
        let (end_line, end_column) = span.end_pos().line_col();
        Self {
            start_byte: span.start(),
            end_byte: span.end(),
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    pub(crate) fn contains(self, other: Self) -> bool {
        self.start_byte <= other.start_byte && self.end_byte >= other.end_byte
    }

    pub(in crate::parser) fn from_preprocessed_range(
        start_byte: usize,
        end_byte: usize,
        file: &crate::parser::PreprocessedFile,
    ) -> Self {
        let start = file.original_position(start_byte);
        let end = file.original_position(end_byte);
        Self {
            start_byte: start.byte(),
            end_byte: end.byte(),
            start_line: start.line(),
            start_column: start.column(),
            end_line: end.line(),
            end_column: end.column(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn dummy() -> Self {
        Self {
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 2,
        }
    }

    pub const fn start_byte(self) -> usize {
        self.start_byte
    }

    pub const fn end_byte(self) -> usize {
        self.end_byte
    }

    pub const fn start_line(self) -> usize {
        self.start_line
    }

    pub const fn start_column(self) -> usize {
        self.start_column
    }

    pub const fn end_line(self) -> usize {
        self.end_line
    }

    pub const fn end_column(self) -> usize {
        self.end_column
    }
}

/// Source identity carried from parsing through module expansion.
///
/// Argument anchors travel with typed resolved arguments, not in a parallel
/// map keyed by an authored name.
///
/// ```compile_fail
/// use carina_core::parser::ModuleCallDiagnosticSource;
///
/// fn name_keyed_anchor(source: &ModuleCallDiagnosticSource) {
///     let _ = &source.argument_spans;
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallDiagnosticSource {
    pub id: ModuleCallId,
}

impl ModuleCallDiagnosticSource {
    pub fn file(&self) -> &Path {
        self.id.source_file()
    }

    pub fn call_span(&self) -> SourceSpan {
        self.id.call_span()
    }
}

/// Parser-only binding metadata that must survive directory fixed-point passes.
///
/// This type is public only because [`File`] is publicly constructible. Its
/// contents stay crate-private so evaluator-only handles cannot escape into the
/// runtime [`Value`] model.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParserBindingMetadata {
    structural_handles: IndexMap<String, StructuralHandle>,
    structural_aliases: IndexMap<String, StructuralHandle>,
    value_structural_handles: IndexMap<String, StructuralHandleMetadata>,
    resource_source_files: HashMap<ResourceId, PathBuf>,
    argument_type_sources: IndexMap<String, (SourceFileId, SourceSpan)>,
    pending_function_type_checks: Vec<PendingFunctionTypeCheck>,
}

/// A call whose body was evaluable during bootstrap parsing but whose dotted
/// parameter annotation needs provider schemas before it can be checked.
/// Kept parser-only so the computed result can drive structural parsing (for
/// example a `for` iterable) without silently dropping the later type check.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingFunctionTypeCheck {
    pub(crate) function_name: String,
    pub(crate) arguments: Vec<FunctionArgument>,
    pub(crate) source_line: usize,
    pub(crate) source_file: Option<PathBuf>,
}

impl ParserBindingMetadata {
    pub(crate) fn structural_handles(&self) -> &IndexMap<String, StructuralHandle> {
        &self.structural_handles
    }

    pub(crate) fn insert_structural_handle(&mut self, name: String, handle: StructuralHandle) {
        self.structural_handles.insert(name, handle);
    }

    pub(crate) fn structural_aliases(&self) -> &IndexMap<String, StructuralHandle> {
        &self.structural_aliases
    }

    pub(crate) fn insert_structural_alias(&mut self, name: String, handle: StructuralHandle) {
        self.structural_aliases.insert(name, handle);
    }

    pub(crate) fn value_structural_handles(&self, name: &str) -> Option<&StructuralHandleMetadata> {
        self.value_structural_handles.get(name)
    }

    pub(crate) fn insert_value_structural_handles(
        &mut self,
        name: String,
        metadata: StructuralHandleMetadata,
    ) {
        self.value_structural_handles.insert(name, metadata);
    }

    pub(crate) fn insert_resource_source_file(&mut self, id: ResourceId, path: PathBuf) {
        self.resource_source_files.insert(id, path);
    }

    pub(crate) fn resource_source_file(&self, id: &ResourceId) -> Option<&Path> {
        self.resource_source_files.get(id).map(PathBuf::as_path)
    }

    pub(crate) fn insert_argument_type_source(
        &mut self,
        name: String,
        source_file: PathBuf,
        span: SourceSpan,
    ) {
        self.argument_type_sources
            .insert(name, (SourceFileId::new(source_file), span));
    }

    pub(crate) fn argument_type_source(&self, name: &str) -> Option<(&SourceFileId, SourceSpan)> {
        self.argument_type_sources
            .get(name)
            .map(|(source_file, span)| (source_file, *span))
    }

    pub(crate) fn insert_pending_function_type_check(&mut self, check: PendingFunctionTypeCheck) {
        if !self.pending_function_type_checks.contains(&check) {
            self.pending_function_type_checks.push(check);
        }
    }

    pub(crate) fn pending_function_type_checks(&self) -> &[PendingFunctionTypeCheck] {
        &self.pending_function_type_checks
    }

    pub(crate) fn set_pending_function_source_file(&mut self, path: &Path) {
        for check in &mut self.pending_function_type_checks {
            check.source_file = Some(path.to_path_buf());
        }
    }

    pub(crate) fn clear_pending_function_type_checks(&mut self) {
        self.pending_function_type_checks.clear();
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.structural_handles.retain(|name, _| keep(name));
        self.structural_aliases.retain(|name, _| keep(name));
        self.value_structural_handles.retain(|name, _| keep(name));
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.structural_handles.extend(other.structural_handles);
        self.structural_aliases.extend(other.structural_aliases);
        self.value_structural_handles
            .extend(other.value_structural_handles);
        self.resource_source_files
            .extend(other.resource_source_files);
        self.argument_type_sources
            .extend(other.argument_type_sources);
        for check in other.pending_function_type_checks {
            self.insert_pending_function_type_check(check);
        }
    }
}

/// Provider configuration
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub attributes: IndexMap<String, Value>,
    /// Default tags to apply to all resources that support tags.
    /// Extracted from `default_tags = { ... }` in the provider block.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub default_tags: IndexMap<String, Value>,
    /// Provider source (e.g., "carina-rs/awscc", "github.com/carina-rs/carina-provider-awscc",
    /// or "file:///path/to/binary").
    /// Extracted from the provider block and not passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ProviderSource>,
    /// Provider version constraint (e.g., "~0.5.0", "^1.2.0").
    /// Extracted from the provider block and not passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionConstraint>,
    /// Git revision (branch, tag, or commit SHA) to resolve the provider from CI artifacts.
    /// Mutually exclusive with `version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Non-literal well-known attributes (e.g. `default_tags = some_let.field`)
    /// drained and validated by the post-resolver finalize step. Invariant:
    /// empty after finalization. In-memory transit only — never serialized.
    #[serde(skip)]
    pub unresolved_attributes: IndexMap<String, Value>,
    /// The sole source of truth for whether this entry is default or named.
    /// `Some(name)` identifies a named instance declared via
    /// `let <name> = provider <kind> { ... }`; `None` identifies the default
    /// instance produced by a top-level `provider <kind> { ... }` block.
    /// [`ProviderConfig::is_default`] derives its result from this field, and
    /// resources without an explicit provider directive route to the `None`
    /// (default) instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
}

impl ProviderConfig {
    /// Whether this entry is the kind's default instance.
    ///
    /// Default instances come from top-level `provider <kind> { ... }`
    /// blocks and have no binding. Named instances always carry the `let`
    /// binding that declared them.
    pub fn is_default(&self) -> bool {
        self.binding.is_none()
    }
}

/// Backend configuration for state storage
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendConfig {
    /// Backend type (e.g., "s3", "gcs", "local")
    pub backend_type: String,
    /// Backend-specific attributes
    pub attributes: HashMap<String, Value>,
    /// Parser-authored source spans for attribute keys. Runtime and saved-plan
    /// representations deliberately omit this diagnostic-only metadata.
    #[serde(skip)]
    pub attribute_spans: HashMap<String, SourceSpan>,
}

/// Upstream state reference: `let <binding> = upstream_state { source = "<dir>" }`.
///
/// Declares a read-only reference to another Carina configuration's state.
///
/// `source` is a directory path. Carina itself is directory-scoped, so every
/// sibling `.crn` file in a project (or module) shares the same base
/// directory; the resolution rule is simply:
///
/// - Absolute paths are used as-is.
/// - Relative paths are resolved against the **enclosing project or module
///   directory** — the one passed to `carina validate` / `plan` / `apply`,
///   or the module directory containing the `upstream_state` declaration.
///
/// Which specific `.crn` file inside that directory declares the
/// `upstream_state` does not affect resolution. The upstream's backend and
/// state file are derived from the upstream configuration itself.
#[derive(Debug, Clone)]
pub struct UpstreamState {
    /// The binding name (e.g., "orgs")
    pub binding: String,
    /// Source directory (raw, unresolved path)
    pub source: std::path::PathBuf,
}

/// Typed `until` predicate captured at parse time. MVP supports the
/// `<binding>.<attr-path> == <value>` shape; future operators
/// (`!=`, `&&`/`||`, comparisons, `in`) will grow new variants here
/// without breaking existing fields.
///
/// `lhs_segments` is the dotted path under the target binding —
/// `[target, attr]` for `cert.status`, `[target, parent, attr]` for
/// `cert.renewal_summary.renewal_status`. Always non-empty and the
/// first segment is the target binding name (enforced by the parser).
///
/// `rhs` is the literal value to compare against, captured as a
/// `Value` so namespaced enums (`aws.acm.Certificate.Status.Issued`),
/// string literals, integers, booleans, and durations all flow into
/// the same predicate type.
#[derive(Debug, Clone)]
pub struct UntilPredicateAst {
    pub lhs_segments: Vec<String>,
    pub rhs: crate::resource::Value,
}

/// A binding identifier (a `let`/`wait`/`depends_on` name such as
/// `cert_issued` or its instance-prefixed form `r.cert_issued`).
///
/// This newtype exists so a binding name cannot be confused with an
/// arbitrary `String` at a binding-expecting position, and so the
/// instance-prefix operation is typed as `BindingName -> BindingName`
/// (see `module_resolver::apply_instance_prefix`). It deliberately does
/// **not** encode prefix state (Raw vs Prefixed): the wait-binding list
/// mixes top-level (never-prefixed) and module-derived (prefixed)
/// entries in one `Vec`, so a state-in-the-type distinction is not
/// expressible here — that larger data-flow reshape is tracked
/// separately (carina#3066).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingName(String);

impl BindingName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::ops::Deref for BindingName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BindingName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BindingName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BindingName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Compare a `BindingName` directly against a string slice so existing
/// `wb.target == some_str` / lookup sites don't need `.as_str()`
/// ceremony. The newtype's value is preventing *accidental* String
/// substitution at construction/parameter boundaries, not forbidding
/// equality checks.
impl PartialEq<str> for BindingName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for BindingName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<BindingName> for str {
    fn eq(&self, other: &BindingName) -> bool {
        self == other.0
    }
}

impl PartialEq<BindingName> for &str {
    fn eq(&self, other: &BindingName) -> bool {
        *self == other.0
    }
}

impl PartialEq<String> for BindingName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<BindingName> for String {
    fn eq(&self, other: &BindingName) -> bool {
        self == &other.0
    }
}

/// A `wait <target> { ... }` declaration captured during parse.
///
/// Carries the parsed surface form of the `until` predicate so plan
/// display can echo the user-authored expression verbatim, plus the
/// structured `until_predicate` for the differ / executor to lower
/// into `WaitPredicate`. `timeout_secs` is normalised to seconds
/// because `Duration` from carina#2824 already canonicalises that way.
///
/// See `notes/specs/2026-05-09-wait-construct-design.md`.
#[derive(Debug, Clone)]
pub struct WaitBinding {
    /// The wait's binding name (e.g. `cert_issued`).
    pub binding: BindingName,
    /// Identifier of the target resource binding (e.g. `cert`).
    pub target: BindingName,
    /// Surface form of the `until` expression as the user wrote it
    /// (e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`).
    pub until_raw: String,
    /// Structured predicate for the differ / executor to consume.
    pub until_predicate: UntilPredicateAst,
    /// Optional user override of the wait timeout, in whole seconds. When
    /// `None` the differ falls back to the target schema's default.
    pub timeout_secs: Option<u64>,
    /// Additional ordering edges declared via `depends_on = [...]`. The
    /// ordering machinery itself is shared with the per-resource
    /// `directives.depends_on` (carina#2823).
    pub depends_on: Vec<BindingName>,
    /// Source line of the `wait` keyword. Used by diagnostics.
    pub line: usize,
}

/// An export parameter as seen post-inference (#2360 stage 2).
///
/// `static_type` retains whether the effective type came from an annotation or
/// rhs inference, plus any typed reason why checking is unavailable.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredExportParam {
    pub name: String,
    pub static_type: ExportStaticType,
    pub value: Option<Value>,
}

impl ExportParamLike for InferredExportParam {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }
    fn set_value(&mut self, value: Option<Value>) {
        self.value = value;
    }
    fn type_expr_opt(&self) -> Option<&TypeExpr> {
        self.static_type.known()
    }
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr> {
        match &mut self.static_type {
            ExportStaticType::Declared(type_expr) => Some(type_expr),
            ExportStaticType::Inferred(crate::static_type::StaticType::Known(type_expr)) => {
                Some(type_expr)
            }
            ExportStaticType::Inferred(crate::static_type::StaticType::Unchecked(_reason)) => None,
        }
    }
    fn inference_failed(&self) -> bool {
        matches!(
            self.static_type,
            ExportStaticType::Inferred(crate::static_type::StaticType::Unchecked(
                crate::static_type::InferredUncheckedReason::InferenceFailed(_)
            ))
        )
    }
}

/// Deferred-loop bodies before the module-resolution seam.
#[derive(Debug, Clone, Default)]
pub struct ParsedDeferredForPhase {
    pub(crate) expressions: Vec<ParsedDeferredForExpression>,
}

/// Resource-only deferred loops carried by a finalized file.
///
/// This collection can contain only [`DeferredForExpression`] values, so its
/// public transformations cannot reintroduce parser-side module-call, data
/// source, or invalid bodies. Module resolution creates it while finalizing a
/// [`File`] into [`ResolvedDeferredForPhase`]; later phases may move, filter,
/// or append already-finalized expressions.
///
/// Production callers outside `carina-core` cannot append an arbitrary loop
/// and thereby bypass the finalization seam:
///
/// ```compile_fail,E0599
/// use carina_core::parser::{DeferredForExpression, ResolvedDeferredForExpressions};
///
/// fn bypass_finalization(
///     expressions: &mut ResolvedDeferredForExpressions,
///     unchecked: DeferredForExpression,
/// ) {
///     expressions.push(unchecked);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedDeferredForExpressions(Vec<DeferredForExpression>);

impl ResolvedDeferredForExpressions {
    pub fn as_slice(&self) -> &[DeferredForExpression] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, DeferredForExpression> {
        self.0.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, DeferredForExpression> {
        self.0.iter_mut()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn empty_like(&self) -> Self {
        Self(Vec::new())
    }

    pub fn partition(
        self,
        mut predicate: impl FnMut(&DeferredForExpression) -> bool,
    ) -> (Self, Self) {
        let (selected, retained) = self.0.into_iter().partition(|item| predicate(item));
        (Self(selected), Self(retained))
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_expressions_for_test(
        expressions: impl IntoIterator<Item = DeferredForExpression>,
    ) -> Self {
        Self(expressions.into_iter().collect())
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn pop(&mut self) -> Option<DeferredForExpression> {
        self.0.pop()
    }

    pub(crate) fn remove(&mut self, index: usize) -> DeferredForExpression {
        self.0.remove(index)
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// Move an already-finalized collection into this finalized collection.
    ///
    /// External crates cannot construct either collection or append an
    /// individual expression, so this operation preserves the module-
    /// resolution finalization proof carried by both values.
    pub fn append_finalized(&mut self, other: Self) {
        self.extend(other);
    }
}

impl std::ops::Index<usize> for ResolvedDeferredForExpressions {
    type Output = DeferredForExpression;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl std::ops::Deref for ResolvedDeferredForExpressions {
    type Target = [DeferredForExpression];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoIterator for ResolvedDeferredForExpressions {
    type Item = DeferredForExpression;
    type IntoIter = std::vec::IntoIter<DeferredForExpression>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ResolvedDeferredForExpressions {
    type Item = &'a DeferredForExpression;
    type IntoIter = std::slice::Iter<'a, DeferredForExpression>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Deferred-loop phase reached only by module-resolution finalization.
///
/// A [`File`] can change from [`ParsedDeferredForPhase`] to
/// `ResolvedDeferredForPhase` only through the finalization seam, which
/// converts every supported resource body and rejects every unsupported or
/// invalid body. The resulting collection remains mutable, but its element
/// type permits only resource deferred loops.
#[derive(Debug, Clone)]
pub struct ResolvedDeferredForPhase {
    pub(crate) expressions: ResolvedDeferredForExpressions,
}

mod deferred_for_phase_private {
    pub trait Sealed {}
}

/// Sealed traversal implemented by the two deferred-loop phases.
///
/// The explicit phase implementations keep whole-file resource walks
/// exhaustive while allowing phase-agnostic validators to preserve the
/// caller's typestate.
#[doc(hidden)]
pub trait DeferredForPhaseTraversal: deferred_for_phase_private::Sealed {
    type DeferredRef<'a, S: 'a>: Copy;
    type DeferredAttributesMut<'a>;

    fn resource_refs(&self) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, Self>> + '_>
    where
        Self: Sized;
    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, Self>> + '_>
    where
        Self: Sized;
    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_>> + '_>;

    fn deferred_header<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> &'a str;

    fn deferred_line<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> usize;

    fn sync_deferred_attributes(attributes: Self::DeferredAttributesMut<'_>, resource: &Resource);
}

impl deferred_for_phase_private::Sealed for ParsedDeferredForPhase {}

impl DeferredForPhaseTraversal for ParsedDeferredForPhase {
    type DeferredRef<'a, S: 'a> = &'a ParsedDeferredForExpression;
    type DeferredAttributesMut<'a> = ();

    fn resource_refs(&self) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, Self>> + '_> {
        Box::new(
            self.expressions
                .iter()
                .filter_map(|deferred| match &deferred.body {
                    DeferredForBody::Resource(resource) => {
                        Some(ResourceRef::Deferred { resource, deferred })
                    }
                    DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
                }),
        )
    }

    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, Self>> + '_> {
        Box::new(
            self.expressions
                .iter_mut()
                .filter_map(|deferred| match &mut deferred.body {
                    DeferredForBody::Resource(resource) => Some(ResourceMut::Deferred {
                        resource,
                        attributes: (),
                    }),
                    DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
                }),
        )
    }

    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_>> + '_> {
        Box::new(
            self.expressions
                .iter()
                .map(ParsedDeferredForExpression::site),
        )
    }

    fn deferred_header<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> &'a str {
        &deferred.header
    }

    fn deferred_line<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> usize {
        deferred.line
    }

    fn sync_deferred_attributes(
        _attributes: Self::DeferredAttributesMut<'_>,
        _resource: &Resource,
    ) {
    }
}

impl deferred_for_phase_private::Sealed for ResolvedDeferredForPhase {}

impl DeferredForPhaseTraversal for ResolvedDeferredForPhase {
    type DeferredRef<'a, S: 'a> = &'a DeferredForExpression<S>;
    type DeferredAttributesMut<'a> = &'a mut Vec<(String, Value)>;

    fn resource_refs(&self) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, Self>> + '_> {
        Box::new(
            self.expressions
                .iter()
                .map(|deferred| ResourceRef::Deferred {
                    resource: &deferred.template_resource,
                    deferred,
                }),
        )
    }

    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, Self>> + '_> {
        Box::new(
            self.expressions
                .iter_mut()
                .map(|deferred| ResourceMut::Deferred {
                    resource: &mut deferred.template_resource,
                    attributes: &mut deferred.attributes,
                }),
        )
    }

    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_>> + '_> {
        Box::new(self.expressions.iter().map(DeferredForExpression::site))
    }

    fn deferred_header<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> &'a str {
        &deferred.header
    }

    fn deferred_line<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> usize {
        deferred.line
    }

    fn sync_deferred_attributes(attributes: Self::DeferredAttributesMut<'_>, resource: &Resource) {
        *attributes = resource.attributes.clone().into_iter().collect();
    }
}

/// Parse result, generic over the export-parameter and deferred-loop phases.
///
/// Two phases share this struct via aliases (see [`ParsedFile`] and
/// [`InferredFile`]): the parser produces `File<ParsedExportParam>`
/// where `static_type` is an optional [`TypeExpr`]; the loader runs `apply_inference` and
/// yields `File<InferredExportParam>` where every export carries its origin
/// and unchecked reason. Every other field is identical between
/// phases. See `notes/specs/2026-05-03-typeexpr-stage2-design.md`.
///
/// Bare-binding provenance is read-only outside the parser pipeline in
/// production builds, so callers cannot erase it to bypass
/// [`crate::validation::resolve_bare_binding_uses`].
#[derive(Debug, Clone)]
pub struct File<E, D = ParsedDeferredForPhase> {
    pub providers: Vec<ProviderConfig>,
    /// Top-level managed infrastructure resources.
    pub resources: Vec<Resource>,
    /// Read-only data-source resources (`read`-keyword resources).
    pub data_sources: Vec<DataSource>,
    /// Virtual resources synthesized by module-call expansion.
    pub compositions: Vec<LiveComposition>,
    pub variables: IndexMap<String, Value>,
    /// Module `use` statements
    pub uses: Vec<UseStatement>,
    /// Module calls (instantiations)
    pub module_calls: Vec<ModuleCall>,
    /// Top-level argument parameters (directory-based module style)
    pub arguments: Vec<ArgumentParameter>,
    /// Top-level attribute parameters (directory-based module style)
    pub attribute_params: Vec<AttributeParameter>,
    /// Top-level export parameters (published to upstream_state consumers).
    /// Element type varies by phase — see [`ParsedExportParam`] / [`InferredExportParam`].
    pub export_params: Vec<E>,
    /// Backend configuration for state storage
    pub backend: Option<BackendConfig>,
    /// State manipulation blocks (import, removed, moved)
    pub state_blocks: Vec<StateBlock>,
    /// User-defined pure functions
    pub user_functions: HashMap<String, UserFunction>,
    /// Upstream state references (read-only views of other Carina configurations)
    pub upstream_states: Vec<UpstreamState>,
    /// `wait` bindings declared via `let <name> = wait <target> { ... }`.
    pub wait_bindings: Vec<WaitBinding>,
    /// Require blocks (cross-argument constraints)
    pub requires: Vec<RequireBlock>,
    /// Binding names that are structurally required (if/for/read expressions)
    /// and should not trigger unused-binding warnings.
    pub structural_bindings: HashSet<String>,
    /// Evaluator-only classification carried between directory parse passes.
    #[doc(hidden)]
    pub parser_binding_metadata: ParserBindingMetadata,
    /// Bare binding provenance and typed pending value slots.
    #[cfg(not(test))]
    #[doc(hidden)]
    parsed_binding_uses: ParsedBindingUses,
    /// Crate-local unit-test fixtures may initialize the otherwise-private
    /// provenance channel.
    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) parsed_binding_uses: ParsedBindingUses,
    /// Non-fatal warnings collected during parsing.
    pub warnings: Vec<ParseWarning>,
    /// Deferred-loop storage for this file's phase. Parser files carry
    /// [`ParsedDeferredForExpression`] bodies; module-resolved files carry
    /// only [`DeferredForExpression`] resource templates.
    pub(crate) deferred_for_phase: D,
    /// Plan-scoped lineage of leaf nodes back to the composition call
    /// sites that produced them (#3306).
    ///
    /// Populated by `ModuleResolver::expand_module_call`: every leaf
    /// resource added to the expanded `File` records its originating
    /// call-site chain (outermost first). Leaves declared at the DSL
    /// root are absent from the map.
    ///
    /// Not serialized to state — the trace is plan-scoped and rebuilt
    /// from DSL on every parse. (`File` itself does not derive Serde,
    /// so no annotation is required.)
    pub expansion_trace: crate::resource::ExpansionTrace,
}

/// Parse-phase file: exports retain their optional annotation shape.
pub type ParsedFile = File<ParsedExportParam>;

/// Post-inference file: every export carries origin and unchecked provenance.
pub type InferredFile = File<InferredExportParam>;

/// A file that has crossed module resolution and whose deferred-loop bodies
/// are resource-only by construction.
pub type ModuleResolvedFile<E> = File<E, ResolvedDeferredForPhase>;

/// Parse-export file after module resolution.
pub type ModuleResolvedParsedFile = ModuleResolvedFile<ParsedExportParam>;

/// Inferred-export file after module resolution.
pub type ModuleResolvedInferredFile = ModuleResolvedFile<InferredExportParam>;

impl<E> Default for File<E, ParsedDeferredForPhase> {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            resources: Vec::new(),
            data_sources: Vec::new(),
            compositions: Vec::new(),
            variables: IndexMap::new(),
            uses: Vec::new(),
            module_calls: Vec::new(),
            arguments: Vec::new(),
            attribute_params: Vec::new(),
            export_params: Vec::new(),
            backend: None,
            state_blocks: Vec::new(),
            user_functions: HashMap::new(),
            upstream_states: Vec::new(),
            wait_bindings: Vec::new(),
            requires: Vec::new(),
            structural_bindings: HashSet::new(),
            parser_binding_metadata: ParserBindingMetadata::default(),
            parsed_binding_uses: ParsedBindingUses::default(),
            warnings: Vec::new(),
            deferred_for_phase: ParsedDeferredForPhase::default(),
            expansion_trace: crate::resource::ExpansionTrace::new(),
        }
    }
}

impl<E> File<E, ParsedDeferredForPhase> {
    /// Parser-side deferred loops awaiting the module-resolution body seam.
    ///
    /// This slice is empty after successful resolution. Body-specific
    /// consumers must match every [`DeferredForBody`] variant explicitly;
    /// resource consumers should use [`Self::iter_all_resources`].
    pub fn parsed_deferred_for_expressions(&self) -> &[ParsedDeferredForExpression] {
        &self.deferred_for_phase.expressions
    }

    /// Deferred iterable sites whose identifier roots still require ordinary
    /// scope checking. Bare undefined iterables are excluded by their private
    /// parser state; their distinct typed diagnostic is owned by
    /// [`crate::parser::check_identifier_scope`] or finalization, depending on
    /// the pipeline.
    pub fn deferred_for_scope_reference_sites(&self) -> impl Iterator<Item = DeferredForSite<'_>> {
        self.deferred_for_phase
            .expressions
            .iter()
            .filter(|expression| {
                matches!(
                    expression.iterable_state,
                    ParsedDeferredForIterableState::Deferred
                )
            })
            .map(ParsedDeferredForExpression::site)
    }

    pub(crate) fn parsed_deferred_for_expressions_mut(
        &mut self,
    ) -> &mut Vec<ParsedDeferredForExpression> {
        &mut self.deferred_for_phase.expressions
    }

    pub fn into_parsed_deferred_for_expressions(self) -> Vec<ParsedDeferredForExpression> {
        self.deferred_for_phase.expressions
    }

    pub(crate) fn set_loop_binding_scope(
        &mut self,
        binding_scope: std::collections::BTreeSet<String>,
    ) {
        let binding_scope = std::sync::Arc::new(binding_scope);
        for resource in &mut self.resources {
            resource
                .loop_provenance
                .set_binding_scope(std::sync::Arc::clone(&binding_scope));
        }
        for call in &mut self.module_calls {
            if let Some(origin) = call.loop_origin_mut() {
                origin.set_binding_scope(std::sync::Arc::clone(&binding_scope));
            }
        }
        for deferred in &mut self.deferred_for_phase.expressions {
            deferred
                .target
                .set_loop_binding_scope(std::sync::Arc::clone(&binding_scope));
            match &mut deferred.body {
                DeferredForBody::Resource(resource) => resource
                    .loop_provenance
                    .set_binding_scope(std::sync::Arc::clone(&binding_scope)),
                DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => {}
            }
        }
    }

    pub(crate) fn empty_with_parsed_binding_uses(parsed_binding_uses: ParsedBindingUses) -> Self {
        Self {
            parsed_binding_uses,
            ..Self::default()
        }
    }

    /// Merge one same-phase file without exposing mutable bare-binding
    /// provenance. The exhaustive destructure keeps new `File` fields from
    /// being silently dropped by directory and module merges.
    pub(crate) fn merge_from(&mut self, source: Self) {
        let File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params,
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            deferred_for_phase,
            expansion_trace,
        } = source;

        self.providers.extend(providers);
        self.resources.extend(resources);
        self.data_sources.extend(data_sources);
        self.compositions.extend(compositions);
        self.variables.extend(variables);
        self.uses.extend(uses);
        self.module_calls.extend(module_calls);
        self.arguments.extend(arguments);
        self.attribute_params.extend(attribute_params);
        self.export_params.extend(export_params);
        self.state_blocks.extend(state_blocks);
        self.user_functions.extend(user_functions);
        self.upstream_states.extend(upstream_states);
        self.wait_bindings.extend(wait_bindings);
        self.requires.extend(requires);
        self.structural_bindings.extend(structural_bindings);
        self.parser_binding_metadata.extend(parser_binding_metadata);
        self.parsed_binding_uses.extend(parsed_binding_uses);
        self.warnings.extend(warnings);
        self.deferred_for_phase
            .expressions
            .extend(deferred_for_phase.expressions);
        for (leaf, call_sites) in expansion_trace.leaf_to_call_sites {
            self.expansion_trace.record(leaf, call_sites);
        }
        if let Some(backend) = backend {
            self.backend = Some(backend);
        }
    }

    /// Consume the parser-side deferred-loop phase and replace it with the
    /// resource-only phase produced by module finalization.
    ///
    /// This crate-private constructor is called only by the module resolver's
    /// finalization seam. Returning an auxiliary value lets that seam collect
    /// typed errors while constructing its scratch graph.
    pub(crate) fn map_deferred_for_phase<T>(
        self,
        map: impl FnOnce(Vec<ParsedDeferredForExpression>) -> (Vec<DeferredForExpression>, T),
    ) -> (ModuleResolvedFile<E>, T) {
        let File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params,
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            deferred_for_phase,
            expansion_trace,
        } = self;
        let (expressions, auxiliary) = map(deferred_for_phase.expressions);
        (
            File {
                providers,
                resources,
                data_sources,
                compositions,
                variables,
                uses,
                module_calls,
                arguments,
                attribute_params,
                export_params,
                backend,
                state_blocks,
                user_functions,
                upstream_states,
                wait_bindings,
                requires,
                structural_bindings,
                parser_binding_metadata,
                parsed_binding_uses,
                warnings,
                deferred_for_phase: ResolvedDeferredForPhase {
                    expressions: ResolvedDeferredForExpressions(expressions),
                },
                expansion_trace,
            },
            auxiliary,
        )
    }
}

impl<E, D> File<E, D> {
    pub fn bare_binding_uses(&self) -> &ParsedBindingUses {
        &self.parsed_binding_uses
    }

    /// Transform only the export-param phase (`File<E, D>` → `File<B, D>`),
    /// moving the deferred-loop phase through unchanged.
    pub fn map_export_params<B>(self, f: impl FnOnce(Vec<E>) -> Vec<B>) -> File<B, D> {
        let File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params,
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            deferred_for_phase,
            expansion_trace,
        } = self;

        File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params: f(export_params),
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            deferred_for_phase,
            expansion_trace,
        }
    }

    /// Iterate the top-level resources — managed, composition, and data
    /// source — as typed [`ResourceRef`]s, **excluding** deferred
    /// for-expression templates.
    pub fn iter_top_level_resources(&self) -> impl Iterator<Item = ResourceRef<'_>> {
        self.resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(self.compositions.iter().map(ResourceRef::LiveComposition))
            .chain(self.data_sources.iter().map(ResourceRef::DataSource))
    }

    /// Names declared by `upstream_state`, independent of whether loading a
    /// particular state produced a binding-map entry.
    pub fn upstream_binding_names(&self) -> HashSet<&str> {
        self.upstream_states
            .iter()
            .map(|upstream| upstream.binding.as_str())
            .collect()
    }
}

impl<E, D: DeferredForPhaseTraversal> File<E, D> {
    /// Iterate every resource reachable from the file, preserving the
    /// deferred-loop phase in each deferred resource arm.
    pub fn iter_all_resources(&self) -> impl Iterator<Item = ResourceRef<'_, ModuleSource, D>> {
        self.resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(self.compositions.iter().map(ResourceRef::LiveComposition))
            .chain(self.data_sources.iter().map(ResourceRef::DataSource))
            .chain(self.deferred_for_phase.resource_refs())
    }

    pub fn iter_all_resources_mut(&mut self) -> impl Iterator<Item = ResourceMut<'_, D>> {
        self.resources
            .iter_mut()
            .map(ResourceMut::Resource)
            .chain(
                self.compositions
                    .iter_mut()
                    .map(ResourceMut::LiveComposition),
            )
            .chain(self.data_sources.iter_mut().map(ResourceMut::DataSource))
            .chain(self.deferred_for_phase.resource_muts())
    }

    pub fn deferred_for_sites(&self) -> impl Iterator<Item = DeferredForSite<'_>> {
        self.deferred_for_phase.sites()
    }
}

impl<E> File<E, ResolvedDeferredForPhase> {
    /// Resource-only deferred loops finalized by module resolution.
    pub fn deferred_for_expressions(&self) -> &ResolvedDeferredForExpressions {
        &self.deferred_for_phase.expressions
    }

    /// Mutable resource-only deferred loops. The receiver's phase proves that
    /// unsupported parser bodies have already been rejected.
    pub fn deferred_for_expressions_mut(&mut self) -> &mut ResolvedDeferredForExpressions {
        &mut self.deferred_for_phase.expressions
    }

    pub fn take_deferred_for_expressions(&mut self) -> ResolvedDeferredForExpressions {
        let empty = self.deferred_for_phase.expressions.empty_like();
        std::mem::replace(&mut self.deferred_for_phase.expressions, empty)
    }

    pub fn replace_deferred_for_expressions(
        &mut self,
        expressions: ResolvedDeferredForExpressions,
    ) -> ResolvedDeferredForExpressions {
        std::mem::replace(&mut self.deferred_for_phase.expressions, expressions)
    }

    pub fn into_deferred_for_expressions(self) -> ResolvedDeferredForExpressions {
        self.deferred_for_phase.expressions
    }
}

impl<E, D> File<E, D> {
    /// Every non-variable binding name declared in the file.
    ///
    /// This intentionally excludes `variables`: parser-side variables
    /// include both plain value `let`s and placeholder entries for
    /// structural `let`s. Callers that need the complete in-scope set
    /// should union this with `variables`; callers that need to classify
    /// plain value lets can use this as the exclusion set.
    pub(crate) fn structural_binding_names(&self) -> HashSet<&str> {
        let mut names = HashSet::new();
        names.extend(self.iter_top_level_resources().filter_map(|r| r.binding()));
        names.extend(self.arguments.iter().map(|a| a.name.as_str()));
        names.extend(
            self.module_calls
                .iter()
                .filter_map(|c| c.binding_name.as_deref()),
        );
        names.extend(self.upstream_states.iter().map(|u| u.binding.as_str()));
        names.extend(self.wait_bindings.iter().map(|w| w.binding.as_str()));
        names.extend(self.uses.iter().map(|u| u.alias.as_str()));
        names.extend(self.user_functions.keys().map(String::as_str));
        names.extend(self.providers.iter().filter_map(|p| p.binding.as_deref()));
        names.extend(self.structural_bindings.iter().map(String::as_str));
        names.extend(
            self.parser_binding_metadata
                .structural_handles()
                .keys()
                .map(String::as_str),
        );
        names
    }

    /// Consume `self` and yield the top-level nodes as owned
    /// [`GraphNode`]s.
    ///
    /// The owned counterpart to
    /// [`iter_top_level_resources`](Self::iter_top_level_resources):
    /// when a caller needs to take ownership of the three typed slices
    /// and treat them uniformly (post-expansion plan-engine paths,
    /// ownership transfer into intermediate representations), this
    /// returns a single `Iterator<Item = GraphNode>` instead of three
    /// parallel `Vec`s. Deferred for-expression templates are excluded
    /// for the same reason as the borrowing iterator above.
    pub fn into_graph_nodes(self) -> impl Iterator<Item = GraphNode> {
        self.resources
            .into_iter()
            .map(GraphNode::from)
            .chain(self.compositions.into_iter().map(GraphNode::from))
            .chain(self.data_sources.into_iter().map(GraphNode::from))
    }

    /// Consume `self` and yield the leaf nodes as owned
    /// [`LeafNode`]s — i.e. resources and data sources only.
    ///
    /// `LeafNode` is the subset of [`GraphNode`] that has no
    /// `LiveComposition` variant by construction, so a downstream caller
    /// that takes `Vec<LeafNode>` (the post-expansion view) cannot
    /// be handed a composition through the type system. This is the
    /// type-level counterpart to
    /// [`into_graph_nodes`](Self::into_graph_nodes), and is the
    /// boundary the differ / executor pipeline crosses at the end of
    /// composition expansion.
    ///
    /// Compositions are dropped from the output; the originating
    /// `LiveComposition` → leaf lineage will be carried separately by the
    /// `ExpansionTrace` once #3295 introduces it.
    pub fn into_leaf_nodes(self) -> impl Iterator<Item = LeafNode> {
        self.resources
            .into_iter()
            .map(LeafNode::from)
            .chain(self.data_sources.into_iter().map(LeafNode::from))
    }

    /// Find a resource by resource type and name attribute value
    pub fn find_resource_by_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        attr_value: &str,
    ) -> Option<&Resource> {
        self.resources.iter().find(|r| {
            r.id.resource_type == resource_type
                && matches!(r.get_attr(attr_name), Some(Value::Concrete(ConcreteValue::String(n))) if n == attr_value)
        })
    }

    /// Print all collected warnings to stderr.
    ///
    /// Returns `true` iff at least one warning line was printed. Callers that
    /// interleave this with indicatif progress output use the return value to
    /// know the terminal's last line is now a newline-terminated `⚠` line
    /// (not an open spinner bar) — see `carina-cli`'s
    /// `ProgressBackend::note_external_line`.
    pub fn print_warnings(&self) -> bool {
        self.print_warnings_from(0)
    }

    /// Print collected warnings from `start` onward.
    ///
    /// This lets callers that already printed early parse warnings surface
    /// warnings added by later phases, such as module expansion, without
    /// duplicating the earlier lines.
    pub fn print_warnings_from(&self, start: usize) -> bool {
        let warnings = self.warnings.get(start..).unwrap_or_default();
        for w in warnings {
            let location = match &w.file {
                Some(f) => match &w.span {
                    Some(span) => format!("{}:{}:{}", f, span.start_line, span.start_column),
                    None => format!("{}:{}", f, w.line),
                },
                None => match &w.span {
                    Some(span) => format!("line {}:{}", span.start_line, span.start_column),
                    None => format!("line {}", w.line),
                },
            };
            eprintln!("  ⚠ {}: {}", location, w.message);
        }
        !warnings.is_empty()
    }
}

impl<E> File<E, ResolvedDeferredForPhase> {
    /// Expand deferred for-expressions against the resolved binding view.
    ///
    /// For each deferred for-expression whose iterable can now be resolved
    /// from `bindings`, expand the template into concrete resources and
    /// add them to `self.resources`. Resolved entries are removed from
    /// `deferred_for_expressions`; unresolved ones remain (with their
    /// warning preserved).
    ///
    /// `bindings` is an [`IterableBindings`]: every binding an iterable
    /// may reference — same-config `let` resources (post-refresh),
    /// `upstream_state` data, and `wait` aliases — merged into one view.
    /// Taking the typed view rather than the raw upstream-only map is the
    /// carina#3132 fix: a same-config `let cert` read iterable
    /// (`for _, opt in cert.domain_validation_options`) is in scope here
    /// only because the caller projects the post-refresh
    /// [`crate::binding_index::ResolvedBindings`] in.
    pub fn expand_deferred_for_expressions(
        &mut self,
        bindings: &IterableBindings<'_>,
    ) -> Result<(), DeferredForExpansionError> {
        let mut expanded_resources = Vec::new();
        let mut resolved_indices = Vec::new();
        // Indices where the iterable resolved but had the wrong shape. These
        // entries stay deferred (user must fix), but their parse-time "not yet
        // available" warning is replaced by the more specific shape-mismatch
        // warning collected in `new_warnings`.
        let mut mismatched_indices: Vec<usize> = Vec::new();
        let mut new_warnings: Vec<ParseWarning> = Vec::new();

        for (idx, deferred) in self.deferred_for_phase.expressions.iter().enumerate() {
            let iterable_value =
                match bindings
                    .resolve_value(&deferred.iterable)
                    .map_err(|message| DeferredForExpansionError::Resolution {
                        line: deferred.line,
                        expression: crate::value::format_value(&deferred.iterable),
                        message,
                    })? {
                    crate::resolver::ValueResolution::Present(value) => value,
                    crate::resolver::ValueResolution::Absent
                    | crate::resolver::ValueResolution::Unknown(_) => continue,
                };
            let miss_policy = bindings.access_miss_policy(&deferred.iterable);

            match (&deferred.binding, &iterable_value) {
                // Simple binding: only the value var is bound
                (ForBinding::Simple(_), Value::Concrete(ConcreteValue::List(_))) => {
                    expanded_resources.extend(expand_deferred_children(
                        deferred,
                        &iterable_value,
                        miss_policy,
                    )?);
                    resolved_indices.push(idx);
                }
                // Indexed binding: both index and value vars are bound
                (ForBinding::Indexed(_, _), Value::Concrete(ConcreteValue::List(_))) => {
                    expanded_resources.extend(expand_deferred_children(
                        deferred,
                        &iterable_value,
                        miss_policy,
                    )?);
                    resolved_indices.push(idx);
                }
                // Map binding expands over maps, substituting both key and value vars
                (ForBinding::Map(_, _), Value::Concrete(ConcreteValue::Map(_))) => {
                    expanded_resources.extend(expand_deferred_children(
                        deferred,
                        &iterable_value,
                        miss_policy,
                    )?);
                    resolved_indices.push(idx);
                }
                // Shape mismatch: replace the original "not yet available" warning
                // with a specific shape-mismatch warning, leave entry deferred.
                (ForBinding::Map(_, _), Value::Concrete(ConcreteValue::List(_))) => {
                    mismatched_indices.push(idx);
                    new_warnings.push(ParseWarning {
                        file: deferred.file.clone(),
                        line: deferred.line,
                        kind: WarningKind::DeferredFor,
                        span: None,
                        message: format!(
                            "for binding expected map iterable but `{}` resolved to a list. \
                             Fix either the upstream export shape or the downstream binding.",
                            crate::value::format_value(&deferred.iterable),
                        ),
                    });
                }
                (ForBinding::Simple(_), Value::Concrete(ConcreteValue::Map(_)))
                | (ForBinding::Indexed(_, _), Value::Concrete(ConcreteValue::Map(_))) => {
                    mismatched_indices.push(idx);
                    new_warnings.push(ParseWarning {
                        file: deferred.file.clone(),
                        line: deferred.line,
                        kind: WarningKind::DeferredFor,
                        span: None,
                        message: format!(
                            "for binding expected list iterable but `{}` resolved to a map. \
                             Fix either the upstream export shape or the downstream binding.",
                            crate::value::format_value(&deferred.iterable),
                        ),
                    });
                }
                _ => {
                    // Iterable is not a list or map — leave deferred
                }
            }
        }

        // Remove parse-time warnings for mismatched entries (the new
        // shape-mismatch warning replaces the generic "not yet available" one).
        for idx in &mismatched_indices {
            let deferred = &self.deferred_for_phase.expressions[*idx];
            retain_non_matching_deferred_for_warnings(&mut self.warnings, deferred);
        }

        // Remove resolved deferred entries (reverse order to preserve indices)
        for idx in resolved_indices.into_iter().rev() {
            // Also remove the corresponding warning
            let deferred = &self.deferred_for_phase.expressions[idx];
            retain_non_matching_deferred_for_warnings(&mut self.warnings, deferred);
            self.deferred_for_phase.expressions.remove(idx);
        }

        // carina#3181: a deferred for-expression's `template_resource` is
        // a `Resource`, so every expansion of it is managed — they
        // go straight into the managed `resources` slice. (A `read` /
        // composition for-body never reaches the deferred path.)
        self.resources.extend(expanded_resources);
        self.warnings.extend(new_warnings);
        Ok(())
    }
}

fn retain_non_matching_deferred_for_warnings(
    warnings: &mut Vec<ParseWarning>,
    deferred: &DeferredForExpression,
) {
    warnings.retain(|w| {
        w.kind != WarningKind::DeferredFor || w.line != deferred.line || w.file != deferred.file
    });
}

pub fn expand_deferred_children<S: Clone>(
    deferred: &DeferredForExpression<S>,
    iterable_value: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<Vec<Resource<S>>, DeferredForExpansionError> {
    match (&deferred.binding, iterable_value) {
        (ForBinding::Simple(_), Value::Concrete(ConcreteValue::List(items))) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                build_expanded_child(
                    deferred,
                    ForLoopElement::Index(index),
                    None,
                    None,
                    item,
                    miss_policy,
                )
            })
            .collect(),
        (ForBinding::Indexed(_, _), Value::Concrete(ConcreteValue::List(items))) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                build_expanded_child(
                    deferred,
                    ForLoopElement::Index(index),
                    Some(index as i64),
                    None,
                    item,
                    miss_policy,
                )
            })
            .collect(),
        (ForBinding::Map(_, _), Value::Concrete(ConcreteValue::Map(map))) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            keys.into_iter()
                .map(|key| {
                    build_expanded_child(
                        deferred,
                        ForLoopElement::Key(key.clone()),
                        None,
                        Some(key),
                        &map[key],
                        miss_policy,
                    )
                })
                .collect()
        }
        (ForBinding::Simple(_) | ForBinding::Indexed(_, _), other) => {
            Err(ShapeMismatch::new("list", deferred_iterable_kind(other)).into())
        }
        (ForBinding::Map(_, _), other) => {
            Err(ShapeMismatch::new("map", deferred_iterable_kind(other)).into())
        }
    }
}

fn deferred_iterable_kind(value: &Value) -> &'static str {
    match value {
        Value::Concrete(ConcreteValue::String(_)) => "string",
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "enum identifier",
        Value::Concrete(ConcreteValue::CanonicalEnum(_)) => "enum",
        Value::Concrete(ConcreteValue::Int(_)) => "int",
        Value::Concrete(ConcreteValue::Float(_)) => "float",
        Value::Concrete(ConcreteValue::Bool(_)) => "bool",
        Value::Concrete(ConcreteValue::Duration(_)) => "duration",
        Value::Concrete(ConcreteValue::List(_)) => "list",
        Value::Concrete(ConcreteValue::StringList(_)) => "string list",
        Value::Concrete(ConcreteValue::Map(_)) => "map",
        Value::Deferred(DeferredValue::ResourceRef { .. }) => "deferred resource reference",
        Value::Deferred(DeferredValue::BindingRef { .. }) => "deferred binding reference",
        Value::Deferred(DeferredValue::Coalesce { .. }) => "deferred coalesce expression",
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
            "deferred integer arithmetic expression"
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => "deferred interpolation",
        Value::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        ) => "deferred function call",
        Value::Deferred(DeferredValue::Unavailable { .. }) => "unavailable function result",
        Value::Deferred(DeferredValue::Access(_)) => "deferred access expression",
        Value::Deferred(DeferredValue::Secret(_)) => "secret",
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => "redacted secret",
        Value::Deferred(DeferredValue::Unknown(_)) => "unknown",
    }
}

fn build_expanded_child<S: Clone>(
    deferred: &DeferredForExpression<S>,
    element: ForLoopElement,
    index: Option<i64>,
    key: Option<&str>,
    item: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<Resource<S>, DeferredForExpansionError> {
    let mut resource = match &deferred.target {
        DeferredForTarget::Bound { binding } => {
            let address = element.address(binding);
            let mut resource = deferred
                .template_resource
                .instantiate_with_identity(ResourceIdentity::new(address.clone()));
            resource.binding = Some(address);
            resource
        }
        DeferredForTarget::Unbound { template } => {
            let mut resource = deferred.template_resource.instantiate();
            resource.binding = None;
            resource.loop_provenance =
                crate::resource::LoopProvenance::direct(template.element_origin(element));
            resource
        }
    };
    resource
        .dependency_bindings
        .extend(deferred.root_bindings());
    if let Err((attribute, error)) = substitute_attrs(&mut resource, index, key, item, miss_policy)
    {
        return Err(DeferredForExpansionError::ValueAccess {
            binding: deferred.target.scheduler_key().to_string(),
            line: error.line,
            attribute,
            message: error.message,
        });
    }
    Ok(resource)
}

/// Run `substitute_placeholder` over every attribute of `resource`, then
/// canonicalize each attribute in place. The canonicalize step lives here
/// (and not at parse time) because once placeholders have been replaced
/// with concrete scalars, surrounding `Interpolation` shapes can collapse
/// to a `String` (#2227).
fn substitute_attrs<S>(
    resource: &mut crate::resource::Resource<S>,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<(), (String, PlaceholderAccessError)> {
    for (name, attr) in resource.attributes.iter_mut() {
        substitute_placeholder_with_policy(attr, index, key, value, miss_policy)
            .map_err(|error| (name.clone(), error))?;
        attr.canonicalize_in_place();
    }
    Ok(())
}

/// Substitute deferred-for-expression placeholders in a Value tree.
///
/// Replaces `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue))` with `value`. If
/// `index` is supplied (indexed-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex))` with the integer index. If
/// `key` is supplied (map-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey))` with the key string. Recurses
/// into all compound Value variants so placeholders nested inside
/// interpolations / function calls / secrets are reached.
///
/// `UnknownReason::UpstreamExport` is the upstream-export marker, not a
/// for-binding placeholder, so it is preserved unchanged.
#[cfg(test)]
fn substitute_placeholder(
    v: &mut Value,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
) -> Result<(), PlaceholderAccessError> {
    substitute_placeholder_with_policy(
        v,
        index,
        key,
        value,
        crate::resource::AccessMissPolicy::Error,
    )
}

fn substitute_placeholder_with_policy(
    v: &mut Value,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<(), PlaceholderAccessError> {
    match v {
        Value::Deferred(DeferredValue::Unknown(reason)) => match reason.clone() {
            UnknownReason::ForValue => {
                *v = value.clone();
            }
            UnknownReason::ForKey => {
                if let Some(k) = key {
                    *v = Value::Concrete(ConcreteValue::String(k.to_string().into()));
                }
            }
            UnknownReason::ForIndex => {
                if let Some(i) = index {
                    *v = Value::Concrete(ConcreteValue::Int(i));
                }
            }
            // Upstream and empty-interpolation unknowns are not for-expansion placeholders.
            UnknownReason::UpstreamExport { .. }
            | UnknownReason::UpstreamAllExports { .. }
            | UnknownReason::EmptyInterpolation
            | UnknownReason::PendingReRead(_) => {}
            // Function placeholders are substituted by user-function evaluation, not for expansion.
            UnknownReason::FnParam { .. }
            | UnknownReason::FnLocal { .. }
            | UnknownReason::ModuleArgument { .. } => {}
        },
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items.iter_mut() {
                substitute_placeholder_with_policy(item, index, key, value, miss_policy)
                    .map_err(nested_placeholder_error)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for val in map.values_mut() {
                substitute_placeholder_with_policy(val, index, key, value, miss_policy)
                    .map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for arg in args.iter_mut() {
                substitute_placeholder_with_policy(
                    arg.walker_value_mut(),
                    index,
                    key,
                    value,
                    miss_policy,
                )
                .map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let mut resolved_left = left.as_ref().clone();
            match substitute_placeholder_with_policy(
                &mut resolved_left,
                index,
                key,
                value,
                miss_policy,
            ) {
                Ok(()) if resolved_left.is_fully_resolved() => {
                    // A present left operand wins without touching the fallback.
                    *v = resolved_left;
                }
                Ok(()) => {
                    let mut resolved_default = default.as_ref().clone();
                    substitute_placeholder_lazily(&mut resolved_default, index, key, value)
                        .map_err(nested_placeholder_error)?;
                    *v = Value::Deferred(DeferredValue::Coalesce {
                        value: Box::new(resolved_left),
                        default: Box::new(resolved_default),
                    });
                }
                Err(error) if error.coalesce_absence => {
                    let mut resolved_default = default.as_ref().clone();
                    substitute_placeholder_with_policy(
                        &mut resolved_default,
                        index,
                        key,
                        value,
                        miss_policy,
                    )?;
                    *v = resolved_default;
                }
                Err(error) => return Err(error),
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access = access.clone();
            let original = Value::Deferred(DeferredValue::Access(access.clone()));
            let mut base = access.base().clone();
            substitute_placeholder_with_policy(&mut base, index, key, value, miss_policy)?;
            // The placeholder may now be a concrete list/map. Re-run the
            // shared access primitive so expanded `for` bodies do not retain
            // a stale deferred wrapper after substitution.
            match access.rebuild_result(base, access.step().clone(), miss_policy) {
                Ok(crate::resource::ValuePathSegmentResult::Resolved(value))
                | Ok(crate::resource::ValuePathSegmentResult::Deferred(value)) => *v = value,
                Ok(crate::resource::ValuePathSegmentResult::DeferredMiss(_)) => *v = original,
                Err(error) => {
                    let coalesce_absence = matches!(
                        error,
                        crate::resource::ValueAccessError::MissingField { .. }
                            | crate::resource::ValueAccessError::MissingKey { .. }
                    );
                    return Err(PlaceholderAccessError {
                        line: access.source_line(),
                        message: error.to_string(),
                        coalesce_absence,
                    });
                }
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts.iter_mut() {
                if let crate::resource::InterpolationPart::Expr(inner) = part {
                    substitute_placeholder_with_policy(inner, index, key, value, miss_policy)
                        .map_err(nested_placeholder_error)?;
                }
            }
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let expression = expression.clone();
            let mut lhs = expression.lhs().clone();
            let mut rhs = expression.rhs().clone();
            substitute_placeholder_with_policy(&mut lhs, index, key, value, miss_policy)
                .map_err(nested_placeholder_error)?;
            substitute_placeholder_with_policy(&mut rhs, index, key, value, miss_policy)
                .map_err(nested_placeholder_error)?;
            *v = expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| PlaceholderAccessError {
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    coalesce_absence: false,
                })?;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            substitute_placeholder_with_policy(inner, index, key, value, miss_policy)?;
        }
        _ => {}
    }
    Ok(())
}

/// Substitute loop placeholders without evaluating any surrounding access or
/// function. Used only for an unselected `??` fallback: the loop item must be
/// captured now. Operand substitution still validates and folds typed integer
/// arithmetic so a bad materialized loop value cannot leave an invalid node.
fn substitute_placeholder_lazily(
    value: &mut Value,
    index: Option<i64>,
    key: Option<&str>,
    item: &Value,
) -> Result<(), PlaceholderAccessError> {
    match value {
        Value::Deferred(DeferredValue::Unknown(reason)) => match reason {
            UnknownReason::ForValue => *value = item.clone(),
            UnknownReason::ForKey => {
                if let Some(key) = key {
                    *value = Value::Concrete(ConcreteValue::String(key.to_string().into()));
                }
            }
            UnknownReason::ForIndex => {
                if let Some(index) = index {
                    *value = Value::Concrete(ConcreteValue::Int(index));
                }
            }
            UnknownReason::UpstreamExport { .. }
            | UnknownReason::UpstreamAllExports { .. }
            | UnknownReason::EmptyInterpolation
            | UnknownReason::PendingReRead(_)
            | UnknownReason::FnParam { .. }
            | UnknownReason::FnLocal { .. }
            | UnknownReason::ModuleArgument { .. } => {}
        },
        Value::Concrete(ConcreteValue::List(values)) => {
            for value in values {
                substitute_placeholder_lazily(value, index, key, item)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for value in values.values_mut() {
                substitute_placeholder_lazily(value, index, key, item)?;
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    substitute_placeholder_lazily(value, index, key, item)?;
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                substitute_placeholder_lazily(argument.walker_value_mut(), index, key, item)?;
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access = access.clone();
            let mut base = access.base().clone();
            substitute_placeholder_lazily(&mut base, index, key, item)?;
            *value = match access.rebuild_result(
                base.clone(),
                access.step().clone(),
                crate::resource::AccessMissPolicy::Error,
            ) {
                Ok(crate::resource::ValuePathSegmentResult::Resolved(value))
                | Ok(crate::resource::ValuePathSegmentResult::Deferred(value)) => value,
                Ok(crate::resource::ValuePathSegmentResult::DeferredMiss(_)) | Err(_) => {
                    access.rebuild_lazy_with_base(base)
                }
            };
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            substitute_placeholder_lazily(left, index, key, item)?;
            substitute_placeholder_lazily(default, index, key, item)?;
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let expression = expression.clone();
            let mut lhs = expression.lhs().clone();
            let mut rhs = expression.rhs().clone();
            substitute_placeholder_lazily(&mut lhs, index, key, item)?;
            substitute_placeholder_lazily(&mut rhs, index, key, item)?;
            *value = expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| PlaceholderAccessError {
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    coalesce_absence: false,
                })?;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            substitute_placeholder_lazily(inner, index, key, item)?;
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
            | DeferredValue::RedactedSecret { .. },
        ) => {}
    }
    Ok(())
}

#[cfg(test)]
mod substitute_placeholder_tests {
    use super::*;
    use crate::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, LiveCompositionSignature,
        ResourceId, UnknownReason, Value,
    };
    use std::collections::BTreeSet;

    #[test]
    fn lazy_coalesce_for_substitution_rejects_non_integer_items_without_panicking() {
        for item in [
            Value::Concrete(ConcreteValue::String("not-an-int".into())),
            Value::Concrete(ConcreteValue::Bool(true)),
        ] {
            let arithmetic = crate::integer_arithmetic::evaluate_integer_values(
                Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
                crate::integer_arithmetic::IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
            )
            .expect("a for-value placeholder is admitted until substitution");
            let mut value = Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("source", "optional", Vec::new())),
                default: Box::new(arithmetic),
            });

            let error = substitute_placeholder(&mut value, None, None, &item)
                .expect_err("a substituted non-Int arithmetic operand must be typed error");
            assert!(
                error.message.contains("operand must be Int"),
                "unexpected substitution error: {}",
                error.message
            );
        }
    }

    #[test]
    fn module_argument_rewrite_cannot_leave_its_access_base_stale() {
        let string = |value: &str| Value::Concrete(ConcreteValue::String(value.into()));
        let call = |value: &str| {
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: vec![string(value).into()],
                source_line: (0).into(),
            })
        };
        let mut argument = ModuleArgument::with_access_base(call("value"), call("access"));

        argument
            .try_rewrite(|value| match value {
                Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
                    Ok::<_, std::convert::Infallible>(args[0].walker_value().clone())
                }
                other => Ok(other.clone()),
            })
            .unwrap();

        assert_eq!(argument.value(), Some(&string("value")));
        assert_eq!(argument.access_base(), Some(&string("access")));
    }

    #[test]
    fn structural_binding_names_covers_every_non_variable_kind() {
        let mut parsed = ParsedFile::default();

        let mut resource = Resource::new("mock.compute.Instance", "resource_node");
        resource.binding = Some("resource_name".to_string());
        parsed.resources.push(resource); // allow: direct — fixture test inspection

        let mut data_source = DataSource::new("mock.compute.Instance", "data_node");
        data_source.binding = Some("data_source_name".to_string());
        parsed.data_sources.push(data_source);

        parsed.compositions.push(LiveComposition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual",
                "composition_node",
            )),
            signature: LiveCompositionSignature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
                pending_constraints: Vec::new(),
            },
            binding: Some("composition_name".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "module_name".to_string(),
            instance: "composition_name".to_string(),
            call_path: Default::default(),
            quoted_string_attrs: HashSet::new(),
        });

        parsed.arguments.push(ArgumentParameter {
            name: "argument_name".to_string(),
            static_type: TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        });
        parsed.module_calls.push(ModuleCall::test(
            "module",
            Some("module_call_name".to_string()),
            HashMap::new(),
        ));
        parsed.upstream_states.push(UpstreamState {
            binding: "upstream_name".to_string(),
            source: "upstream".into(),
        });
        parsed.wait_bindings.push(WaitBinding {
            binding: BindingName::from("wait_name"),
            target: BindingName::from("resource_name"),
            until_raw: "resource_name.status == 'ready'".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["resource_name".to_string(), "status".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("ready".into())),
            },
            timeout_secs: None,
            depends_on: Vec::new(),
            line: 1,
        });
        parsed.uses.push(UseStatement::dummy(
            "./module".to_string(),
            "use_alias".to_string(),
        ));
        parsed.user_functions.insert(
            "function_name".to_string(),
            UserFunction {
                name: "function_name".to_string(),
                params: Vec::new(),
                return_type: None,
                local_lets: Vec::new(),
                body: UserFunctionBody {
                    value: Value::Concrete(ConcreteValue::Int(1)),
                    source_line: 1,
                    source_file: None,
                },
            },
        );
        parsed.providers.push(ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: Some("provider_name".to_string()),
        });
        parsed
            .structural_bindings
            .insert("structural_name".to_string());
        parsed.variables.insert(
            "plain_value_name".to_string(),
            Value::Concrete(ConcreteValue::String("literal".into())),
        );

        let names = parsed.structural_binding_names();
        for expected in [
            "resource_name",
            "data_source_name",
            "composition_name",
            "argument_name",
            "module_call_name",
            "upstream_name",
            "wait_name",
            "use_alias",
            "function_name",
            "provider_name",
            "structural_name",
        ] {
            assert!(
                names.contains(expected),
                "structural_binding_names should include {expected}; got {names:?}"
            );
        }
        assert!(
            !names.contains("plain_value_name"),
            "plain value lets must be excluded from structural binding names"
        );

        // Coverage contract: this destructure must remain exhaustive. Adding a
        // field to `File` must fail to compile here until the author classifies
        // it as either binding-bearing (and updates `structural_binding_names`
        // plus this fixture) or explicitly non-binding-bearing. Do not add a
        // `..` rest pattern.
        let File {
            providers: _,
            resources: _,
            data_sources: _,
            compositions: _,
            variables: _,
            uses: _,
            module_calls: _,
            arguments: _,
            attribute_params: _,
            export_params: _,
            backend: _,
            state_blocks: _,
            user_functions: _,
            upstream_states: _,
            wait_bindings: _,
            requires: _,
            structural_bindings: _,
            parser_binding_metadata: _,
            parsed_binding_uses: _,
            warnings: _,
            deferred_for_phase: _,
            expansion_trace: _,
        } = parsed;
    }

    #[test]
    fn expand_deferred_children_returns_shape_mismatch_for_simple_binding_with_map_iterable() {
        let mut template_resource = Resource::new("mock.Target", "children");
        template_resource.binding = Some("children".to_string());
        template_resource.set_attr(
            "name",
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        );
        let deferred = DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.options".to_string(),
            resource_type: "mock.Target".to_string(),
            attributes: Vec::new(),
            target: DeferredForTarget::Bound {
                binding: "children".to_string(),
            },
            iterable: Value::resource_ref("cert", "options", Vec::new()),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };

        let err = expand_deferred_children(
            &deferred,
            &Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            crate::resource::AccessMissPolicy::Error,
        )
        .expect_err("simple for-binding over a map must be a shape mismatch");

        let DeferredForExpansionError::ShapeMismatch(err) = err else {
            panic!("expected shape mismatch")
        };

        assert_eq!(err.expected_kind(), "list");
        assert_eq!(err.got_kind(), "map");
    }

    #[test]
    fn deferred_list_and_map_expansion_produce_distinct_children() {
        let deferred = |binding| DeferredForExpression {
            file: None,
            line: 1,
            header: "for child in source.children".to_string(),
            resource_type: "mock.Target".to_string(),
            attributes: Vec::new(),
            target: DeferredForTarget::Bound {
                binding: "children".to_string(),
            },
            iterable: Value::resource_ref("source", "children", Vec::new()),
            binding,
            template_resource: Resource::pending_with_provider("mock", "Target", None),
        };

        let list_deferred = deferred(ForBinding::Indexed(
            "index".to_string(),
            "child".to_string(),
        ));
        let list_children = expand_deferred_children(
            &list_deferred,
            &Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("alpha".into())),
                Value::Concrete(ConcreteValue::String("beta".into())),
            ])),
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        assert_eq!(list_deferred.template_resource.id.identity_str(), None);
        assert_eq!(list_children[0].id.identity_str(), Some("children[0]"));
        assert_eq!(list_children[1].id.identity_str(), Some("children[1]"));
        assert_ne!(list_children[0].id, list_children[1].id);

        let map_deferred = deferred(ForBinding::Map("key".to_string(), "child".to_string()));
        let map_children = expand_deferred_children(
            &map_deferred,
            &Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "alpha".to_string(),
                    Value::Concrete(ConcreteValue::String("one".into())),
                ),
                (
                    "beta".to_string(),
                    Value::Concrete(ConcreteValue::String("two".into())),
                ),
            ]))),
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        assert_eq!(map_deferred.template_resource.id.identity_str(), None);
        assert_eq!(map_children[0].id.identity_str(), Some("children.alpha"));
        assert_eq!(map_children[1].id.identity_str(), Some("children.beta"));
        assert_ne!(map_children[0].id, map_children[1].id);
    }

    #[test]
    fn deferred_unbound_children_are_pending_with_origin() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("deferred unbound loop parses");
        let parsed =
            crate::module_resolver::resolve_modules_providerless(parsed, std::path::Path::new("."))
                .expect("deferred fixture crosses the module-resolution seam");
        let deferred = &parsed.deferred_for_expressions()[0];
        let children = expand_deferred_children(
            deferred,
            &Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("alpha".into())),
                Value::Concrete(ConcreteValue::String("beta".into())),
            ])),
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("deferred children expand");

        assert_eq!(children.len(), 2);
        for (index, child) in children.iter().enumerate() {
            assert!(child.id.identity_str().is_none());
            assert_eq!(child.binding, None);
            let origin = child
                .loop_provenance
                .direct_origin()
                .expect("unbound deferred child origin");
            assert_eq!(
                origin.kind(),
                &crate::resource::LoopOriginKind::For {
                    element: crate::resource::ForLoopElement::Index(index),
                }
            );
            assert_eq!(
                origin.legacy_address().exact_state_name(),
                format!("_values[{index}]")
            );
        }
    }

    #[test]
    fn deferred_for_expression_serde_skips_diagnostic_location() {
        let deferred = DeferredForExpression {
            file: Some("/abs/path.crn".to_string()),
            line: 42,
            header: "for opt in cert.options".to_string(),
            resource_type: "mock.Target".to_string(),
            attributes: Vec::new(),
            target: DeferredForTarget::Bound {
                binding: "children".to_string(),
            },
            iterable: Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("cert", "options", Vec::new())),
                default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
            }),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource: Resource::new("mock.Target", "children"),
        };

        let json = serde_json::to_string(&deferred).expect("serialize deferred for expression");
        assert!(
            !json.contains("/abs/path.crn"),
            "saved-plan JSON must not contain machine-local source paths: {json}"
        );

        let decoded = serde_json::from_str::<SavedDeferredForExpression>(&json)
            .expect("deserialize deferred for expression");
        assert_eq!(decoded.file, None);
        assert_eq!(decoded.line, 0);
        assert_eq!(decoded.iterable, deferred.iterable);
    }

    #[test]
    fn unbound_deferred_target_requires_serialized_loop_kind() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("deferred unbound loop parses");
        let parsed =
            crate::module_resolver::resolve_modules_providerless(parsed, std::path::Path::new("."))
                .expect("deferred fixture crosses the module-resolution seam");
        let target = &parsed.deferred_for_expressions()[0].target;
        let mut json = serde_json::to_value(target).expect("serialize deferred target");
        json["template"]["source"]
            .as_object_mut()
            .expect("loop source object")
            .remove("kind");

        let error = serde_json::from_value::<DeferredForTarget>(json)
            .expect_err("format 29 requires the serialized loop kind");
        assert!(
            error.to_string().contains("missing field `kind`"),
            "{error}"
        );
    }

    #[test]
    fn replaces_for_value_with_bound_value() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("acct-1".into())),
        )
        .unwrap();
        assert_eq!(v, Value::Concrete(ConcreteValue::String("acct-1".into())));
    }

    fn for_value_field(name: &str) -> Value {
        crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            PathSegment::Field {
                name: name.to_string(),
            },
            1,
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("deferred for-value access")
    }

    #[test]
    fn coalesce_placeholder_uses_present_left_without_evaluating_default() {
        let mut value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(for_value_field("name")),
            // This access would fail if the fallback were evaluated eagerly.
            default: Box::new(for_value_field("missing")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("present".into())),
        )])));

        substitute_placeholder(&mut value, None, None, &item).expect("substitute coalesce");
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("present".into())),
        );
    }

    #[test]
    fn coalesce_placeholder_uses_default_for_direct_missing_field() {
        let mut value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(for_value_field("optional")),
            default: Box::new(Value::Concrete(ConcreteValue::String("fallback".into()))),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::new()));

        substitute_placeholder(&mut value, None, None, &item).expect("substitute coalesce");
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
    }

    #[test]
    fn coalesce_placeholder_substitutes_default_when_left_remains_unknown() {
        let mut value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("up", "optional", Vec::new())),
            default: Box::new(for_value_field("name")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        )])));

        substitute_placeholder(&mut value, None, None, &item).expect("substitute coalesce");
        let Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) = value
        else {
            panic!("unknown left must retain coalesce");
        };
        assert!(matches!(
            left.as_ref(),
            Value::Deferred(DeferredValue::ResourceRef { .. })
        ));
        assert_eq!(
            default.as_ref(),
            &Value::Concrete(ConcreteValue::String("fallback".into())),
        );
    }

    #[test]
    fn coalesce_placeholder_defers_missing_default_access_while_left_is_unknown() {
        let mut value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("up", "optional", Vec::new())),
            default: Box::new(for_value_field("missing")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::new()));

        substitute_placeholder(&mut value, None, None, &item)
            .expect("an unselected fallback must not raise its access error");

        let Value::Deferred(DeferredValue::Coalesce { default, .. }) = value else {
            panic!("unknown left must retain coalesce")
        };
        assert!(matches!(
            default.as_ref(),
            Value::Deferred(DeferredValue::Access(_))
        ));
    }

    #[test]
    fn replaces_for_key_with_key_string() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
        substitute_placeholder(
            &mut v,
            None,
            Some("east"),
            &Value::Concrete(ConcreteValue::String("ignored".into())),
        )
        .unwrap();
        assert_eq!(v, Value::Concrete(ConcreteValue::String("east".into())));
    }

    #[test]
    fn replaces_for_index_with_integer_index() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex));
        substitute_placeholder(
            &mut v,
            Some(7),
            None,
            &Value::Concrete(ConcreteValue::String("ignored".into())),
        )
        .unwrap();
        assert_eq!(v, Value::Concrete(ConcreteValue::Int(7)));
    }

    #[test]
    fn leaves_upstream_export_unchanged() {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: path.clone(),
        }));
        substitute_placeholder(
            &mut v,
            Some(0),
            Some("k"),
            &Value::Concrete(ConcreteValue::String("anything".into())),
        )
        .unwrap();
        // `Value::Deferred(DeferredValue::Unknown)` never compares equal (see `impl PartialEq for Value`),
        // so destructure to verify the path survives.
        match v {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport { path: p })) => {
                assert_eq!(p, path)
            }
            other => panic!(
                "UpstreamExport must pass through unchanged, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn recurses_into_compound_values() {
        let mut v = Value::Concrete(ConcreteValue::List(vec![
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            Value::Concrete(ConcreteValue::Map({
                let mut m = indexmap::IndexMap::new();
                m.insert(
                    "k".into(),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
                );
                m
            })),
            Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
                Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            )])),
        ]));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("X".into())),
        )
        .unwrap();
        match &v {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items[0], Value::Concrete(ConcreteValue::String("X".into())));
                match &items[1] {
                    Value::Concrete(ConcreteValue::Map(m)) => {
                        assert_eq!(m["k"], Value::Concrete(ConcreteValue::String("X".into())));
                    }
                    other => panic!("expected Map, got {:?}", other),
                }
                match &items[2] {
                    Value::Deferred(DeferredValue::Interpolation(parts)) => match &parts[0] {
                        InterpolationPart::Expr(inner) => {
                            assert_eq!(*inner, Value::Concrete(ConcreteValue::String("X".into())));
                        }
                        _ => panic!("expected Expr part"),
                    },
                    other => panic!("expected Interpolation, got {:?}", other),
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn legacy_string_sentinel_is_no_longer_recognised() {
        // Only `Value::Deferred(DeferredValue::Unknown(For*))` is substituted. A bare string that
        // happens to match the historical sentinel must be left alone.
        let mut v = Value::Concrete(ConcreteValue::String("(known after upstream apply)".into()));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("X".into())),
        )
        .unwrap();
        assert_eq!(
            v,
            Value::Concrete(ConcreteValue::String("(known after upstream apply)".into()))
        );
    }
}
