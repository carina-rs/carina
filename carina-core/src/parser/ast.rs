//! Parser AST types and the public `ParsedFile` result.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).
//!
//! Static-type states that cannot be represented by a sound [`TypeExpr`] live
//! in [`crate::static_type`] rather than as a sentinel variant here.

use super::deferred_if::DeferredIfBody;
use super::display_string_literal;
use super::error::{
    DeferredForBodyKind, DeferredForBodyUnsupportedError, DeferredForIterableResolution,
    ParseError, ParseWarning, WarningKind,
};
use super::expressions::for_expr::ForBinding;
use super::expressions::validate_expr::CompareOp;
use super::grammar::ParserBindingName;
use super::util::snake_to_pascal;
use crate::binding_index::IterableBindings;
use crate::eval_value::{StructuralHandle, StructuralHandleKind, StructuralHandleMetadata};
use crate::provider_source::ProviderSource;
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, Directives, ForLoopElement, FunctionArgument,
    GraphNode, LeafNode, LeftoverPlaceholderContext, LeftoverPlaceholderPolicy,
    LeftoverPlaceholders, LiveComposition, LoopSlot, ModuleSource, NoPlaceholder,
    ParserPlaceholder, PathSegment, PlaceholderKind, ResolvedResourceId, Resource, ResourceId,
    ResourceIdentity, ResourceIdentityError, SavedModuleSource, StructuralOperand,
    StructuralOperandRef, UnboundLoopTemplate, Value,
};
use crate::static_type::{ConsumerStaticType, ExportStaticType};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Typed identity of the draft resource that represents a bound deferred
/// `for` before its iterable materializes.
///
/// Construction is private to [`BoundDeferredTarget`], which is the only
/// production code that spells the placeholder suffix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeferredForTemplateAddress(ResourceIdentity);

impl DeferredForTemplateAddress {
    pub(crate) fn resource_identity(&self) -> &ResourceIdentity {
        &self.0
    }

    fn matches_resource_identity(&self, identity: &ResourceIdentity) -> bool {
        &self.0 == identity
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for DeferredForTemplateAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Bound destination of a deferred `for` expression.
///
/// The authored binding and its template address are validated and cached
/// together. The address cannot be constructed independently.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoundDeferredTarget {
    binding: ResourceIdentity,
    template_address: DeferredForTemplateAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BoundDeferredTargetError {
    #[error(transparent)]
    InvalidIdentity(#[from] ResourceIdentityError),
    #[error("deferred target binding cannot end in an index")]
    TrailingIndex,
}

impl BoundDeferredTarget {
    pub fn new(binding: impl Into<String>) -> Result<Self, BoundDeferredTargetError> {
        let binding = ResourceIdentity::try_from(binding.into())?;
        let final_segment = binding.as_str().rsplit('.').next().unwrap_or_default();
        if final_segment.contains('[') || final_segment.ends_with(']') {
            return Err(BoundDeferredTargetError::TrailingIndex);
        }
        Ok(Self::from_unindexed_binding(binding))
    }

    fn from_unindexed_binding(binding: ResourceIdentity) -> Self {
        let final_segment = binding.as_str().rsplit('.').next().unwrap_or_default();
        debug_assert!(!final_segment.contains('[') && !final_segment.ends_with(']'));
        let template_address =
            DeferredForTemplateAddress(ResourceIdentity::new(format!("{}[?]", binding.as_str())));
        Self {
            binding,
            template_address,
        }
    }

    pub(super) fn from_parser_binding(binding: &ParserBindingName) -> Self {
        Self::from_unindexed_binding(ResourceIdentity::new(binding.as_str()))
    }

    pub(crate) fn with_instance_prefix(&self, prefix: &str) -> Self {
        let binding = ResourceIdentity::new(format!("{prefix}.{}", self.binding()));
        Self::from_unindexed_binding(binding)
    }

    pub fn binding(&self) -> &str {
        self.binding.as_str()
    }

    pub fn template_address(&self) -> &DeferredForTemplateAddress {
        &self.template_address
    }

    pub(crate) fn template_resource_id(
        &self,
        provider: &str,
        resource_type: &str,
        provider_instance: Option<String>,
    ) -> ResourceId {
        ResourceId::with_provider_identity(
            provider,
            resource_type,
            self.template_address.resource_identity().clone(),
            provider_instance,
        )
    }

    fn scheduler_identity(&self) -> &ResourceIdentity {
        &self.binding
    }
}

impl serde::Serialize for BoundDeferredTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(serde::Serialize)]
        struct Wire<'a> {
            binding: &'a ResourceIdentity,
        }

        serde::Serialize::serialize(
            &Wire {
                binding: &self.binding,
            },
            serializer,
        )
    }
}

impl<'de> serde::Deserialize<'de> for BoundDeferredTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            binding: String,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.binding).map_err(serde::de::Error::custom)
    }
}

/// Whether a deferred `for` writes user-addressable bound resources or
/// anonymous resources carrying migration-only loop provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeferredForTarget {
    Bound(BoundDeferredTarget),
    Unbound { template: UnboundLoopTemplate },
}

impl DeferredForTarget {
    pub fn bound(binding: impl Into<String>) -> Result<Self, BoundDeferredTargetError> {
        BoundDeferredTarget::new(binding).map(Self::Bound)
    }

    pub fn bound_binding(&self) -> Option<&str> {
        match self {
            Self::Bound(target) => Some(target.binding()),
            Self::Unbound { .. } => None,
        }
    }

    pub fn scheduler_key(&self) -> &str {
        self.scheduler_identity().as_str()
    }

    fn scheduler_identity(&self) -> &ResourceIdentity {
        match self {
            Self::Bound(target) => target.scheduler_identity(),
            Self::Unbound { template } => template.scheduler_identity(),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Bound(target) => target.binding().to_string(),
            Self::Unbound { template } => template.display_name(),
        }
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bound(left), Self::Bound(right)) => left == right,
            (Self::Unbound { template: left }, Self::Unbound { template: right }) => {
                left.same_system_target(right)
            }
            (Self::Bound(_), Self::Unbound { .. }) | (Self::Unbound { .. }, Self::Bound(_)) => {
                false
            }
        }
    }

    pub(crate) fn matches_legacy_binding(&self, binding: &str) -> bool {
        match self {
            Self::Bound(target) => target.binding() == binding,
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

/// Resource body retained by a deferred `for` template.
///
/// The inner resource is private so its identity and binding can only be
/// refreshed from the owning target. Read access sees the materialized
/// resource. Its display type is a cache derived from the immutable id fields.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredForTemplateResource<S = ModuleSource, P: PlaceholderKind = LoopSlot> {
    resource: Resource<S, P>,
    display_type: String,
}

impl<S, P: PlaceholderKind> DeferredForTemplateResource<S, P> {
    pub(crate) fn new(resource: Resource<S, P>) -> Self {
        let display_type = resource.id.display_type();
        Self {
            resource,
            display_type,
        }
    }

    fn into_materialized_for_target(mut self, target: &DeferredForTarget) -> Self {
        materialize_template_address(target, &mut self.resource);
        self.display_type = self.resource.id.display_type();
        self
    }

    pub fn id(&self) -> &ResourceId {
        &self.resource.id
    }

    pub fn as_resource(&self) -> &Resource<S, P> {
        &self.resource
    }

    pub fn provider(&self) -> &str {
        &self.resource.id.provider
    }

    pub fn resource_type(&self) -> &str {
        &self.resource.id.resource_type
    }

    fn display_type(&self) -> &str {
        &self.display_type
    }

    pub fn provider_instance(&self) -> Option<&str> {
        self.resource.id.provider_instance.as_deref()
    }

    pub fn attributes(&self) -> &IndexMap<String, Value<P>> {
        &self.resource.attributes
    }

    pub fn directives(&self) -> &Directives {
        &self.resource.directives
    }

    pub fn binding(&self) -> Option<&str> {
        self.resource.binding.as_deref()
    }

    pub fn dependency_bindings(&self) -> &std::collections::BTreeSet<String> {
        &self.resource.dependency_bindings
    }

    pub fn module_source(&self) -> Option<&S> {
        self.resource.module_source.as_ref()
    }

    pub fn quoted_string_attrs(&self) -> &HashSet<String> {
        &self.resource.quoted_string_attrs
    }

    fn map_resource<S2, P2: PlaceholderKind>(
        self,
        map: impl FnOnce(Resource<S, P>) -> Resource<S2, P2>,
    ) -> DeferredForTemplateResource<S2, P2> {
        DeferredForTemplateResource::new(map(self.resource))
    }

    fn try_map_resource<S2, P2: PlaceholderKind, E>(
        self,
        map: impl FnOnce(Resource<S, P>) -> Result<Resource<S2, P2>, E>,
    ) -> Result<DeferredForTemplateResource<S2, P2>, E> {
        map(self.resource).map(DeferredForTemplateResource::new)
    }

    pub(crate) fn attributes_mut(&mut self) -> &mut IndexMap<String, Value<P>> {
        &mut self.resource.attributes
    }

    pub(crate) fn loop_provenance_mut(&mut self) -> &mut crate::resource::LoopProvenance {
        &mut self.resource.loop_provenance
    }

    #[cfg(test)]
    pub(crate) fn directives_mut(&mut self) -> &mut Directives {
        &mut self.resource.directives
    }

    pub(crate) fn id_and_attributes_mut(
        &mut self,
    ) -> (&ResourceId, &mut IndexMap<String, Value<P>>) {
        (&self.resource.id, &mut self.resource.attributes)
    }

    pub(crate) fn resolve_ignore_changes(
        &mut self,
        registry: &crate::schema::SchemaRegistry,
    ) -> Result<(), crate::resource::ResolvedResourceError> {
        crate::resource::resolve_resource_ignore_changes(&mut self.resource, registry)
    }
}

impl<S: Clone, P: PlaceholderKind> DeferredForTemplateResource<S, P> {
    pub(crate) fn instantiate(&self) -> Resource<S, P> {
        self.resource.instantiate()
    }

    pub(crate) fn instantiate_with_identity(&self, identity: ResourceIdentity) -> Resource<S, P> {
        self.resource.instantiate_with_identity(identity)
    }
}

fn materialize_template_address<S, P: PlaceholderKind>(
    target: &DeferredForTarget,
    resource: &mut Resource<S, P>,
) {
    match target {
        DeferredForTarget::Bound(target) => {
            resource.id = target.template_resource_id(
                &resource.id.provider,
                &resource.id.resource_type,
                resource.id.provider_instance.clone(),
            );
            resource.binding = Some(target.template_address().to_string());
        }
        DeferredForTarget::Unbound { .. } => {
            if resource.id.identity().is_some() {
                resource.id = ResourceId::pending_with_provider(
                    resource.id.provider.clone(),
                    resource.id.resource_type.clone(),
                    resource.id.provider_instance.clone(),
                );
            }
            resource.binding = None;
        }
    }
}

fn deferred_effect_id<S, P: PlaceholderKind>(
    target: &DeferredForTarget,
    resource: &DeferredForTemplateResource<S, P>,
) -> ResolvedResourceId {
    ResolvedResourceId::with_provider_identity(
        resource.provider(),
        resource.resource_type(),
        target.scheduler_identity().clone(),
        resource.provider_instance().map(str::to_string),
    )
}

/// A for-expression whose iterable is unresolved (e.g., upstream_state not yet available).
///
/// Parser-side deferred loops use [`ParsedDeferredForExpression`] instead.
/// This resource-only shape is the resolved typestate consumed by planning,
/// execution, display, and state persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredForExpression<
    S = ModuleSource,
    P: PlaceholderKind = NoPlaceholder,
    T: PlaceholderKind = LoopSlot,
> {
    pub file: Option<String>,
    // Parser diagnostics and warning correlation use the original source path,
    // independently of the portable diagnostic file. Never serialized.
    pub(crate) source_file: Option<String>,
    pub line: usize,
    pub header: String,
    target: DeferredForTarget,
    pub iterable: Value<P>,
    pub binding: ForBinding,
    template_resource: DeferredForTemplateResource<S, T>,
    deferred_effect_id: ResolvedResourceId,
}

impl<S, P: PlaceholderKind, T: PlaceholderKind> DeferredForExpression<S, P, T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file: Option<String>,
        line: usize,
        header: String,
        target: DeferredForTarget,
        iterable: Value<P>,
        binding: ForBinding,
        template_resource: Resource<S, T>,
    ) -> Self {
        let template_resource = DeferredForTemplateResource::new(template_resource);
        Self::from_template_resource(
            file,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_template_resource(
        file: Option<String>,
        line: usize,
        header: String,
        target: DeferredForTarget,
        iterable: Value<P>,
        binding: ForBinding,
        template_resource: DeferredForTemplateResource<S, T>,
    ) -> Self {
        let template_resource = template_resource.into_materialized_for_target(&target);
        let deferred_effect_id = deferred_effect_id(&target, &template_resource);
        Self {
            source_file: file.clone(),
            file,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource,
            deferred_effect_id,
        }
    }

    pub fn target(&self) -> &DeferredForTarget {
        &self.target
    }

    pub fn resource_type(&self) -> &str {
        self.template_resource.display_type()
    }

    pub fn attributes(&self) -> &IndexMap<String, Value<T>> {
        self.template_resource.attributes()
    }

    pub fn template_resource_id(&self) -> &ResourceId {
        self.template_resource.id()
    }

    pub fn template_resource(&self) -> &DeferredForTemplateResource<S, T> {
        &self.template_resource
    }

    pub fn template_resource_binding(&self) -> Option<&str> {
        self.template_resource.binding()
    }

    pub fn deferred_effect_id(&self) -> &ResolvedResourceId {
        &self.deferred_effect_id
    }

    pub(crate) fn template_attributes_mut(&mut self) -> &mut IndexMap<String, Value<T>> {
        self.template_resource.attributes_mut()
    }

    #[cfg(test)]
    pub(crate) fn template_directives_mut(&mut self) -> &mut Directives {
        self.template_resource.directives_mut()
    }

    pub(crate) fn resolve_template_ignore_changes(
        &mut self,
        registry: &crate::schema::SchemaRegistry,
    ) -> Result<(), crate::resource::ResolvedResourceError> {
        self.template_resource.resolve_ignore_changes(registry)
    }

    pub fn site(&self) -> DeferredForSite<'_, P> {
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
/// substituted caller arguments. Only the resource arm can cross the final
/// resolution seam into [`DeferredForExpression`].
#[derive(Debug, Clone)]
pub enum DeferredForBody<P: PlaceholderKind = ParserPlaceholder> {
    Resource(Box<DeferredForTemplateResource<ModuleSource, P>>),
    Read(Box<DataSource<ModuleSource, P>>),
    ModuleCall(ModuleCall<P>),
}

impl<P: PlaceholderKind> DeferredForBody<P> {
    pub(crate) fn resource(resource: Resource<ModuleSource, P>) -> Self {
        Self::Resource(Box::new(DeferredForTemplateResource::new(resource)))
    }

    fn into_materialized_for_target(self, target: &DeferredForTarget) -> Self {
        match self {
            Self::Resource(resource) => {
                Self::Resource(Box::new((*resource).into_materialized_for_target(target)))
            }
            Self::Read(_) | Self::ModuleCall(_) => self,
        }
    }
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
#[derive(Debug, Clone)]
pub struct ParsedDeferredForExpression<P: PlaceholderKind = ParserPlaceholder> {
    pub file: Option<String>,
    pub(crate) source_file: Option<String>,
    pub line: usize,
    pub span: SourceSpan,
    pub header: String,
    target: DeferredForTarget,
    pub iterable: Value<P>,
    pub(crate) iterable_state: ParsedDeferredForIterableState,
    pub binding: ForBinding,
    body: DeferredForBody<P>,
}

impl<P: PlaceholderKind> ParsedDeferredForExpression<P> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        file: Option<String>,
        line: usize,
        span: SourceSpan,
        header: String,
        target: DeferredForTarget,
        iterable: Value<P>,
        iterable_state: ParsedDeferredForIterableState,
        binding: ForBinding,
        body: DeferredForBody<P>,
    ) -> Self {
        let body = body.into_materialized_for_target(&target);
        Self {
            source_file: file.clone(),
            file,
            line,
            span,
            header,
            target,
            iterable,
            iterable_state,
            binding,
            body,
        }
    }

    pub fn target(&self) -> &DeferredForTarget {
        &self.target
    }

    pub fn body(&self) -> &DeferredForBody<P> {
        &self.body
    }

    #[cfg(test)]
    pub(crate) fn into_body(self) -> DeferredForBody<P> {
        self.body
    }

    pub(crate) fn template_attributes_mut(&mut self) -> Option<&mut IndexMap<String, Value<P>>> {
        match &mut self.body {
            DeferredForBody::Resource(resource) => Some(resource.attributes_mut()),
            DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
        }
    }

    pub(crate) fn template_loop_provenance_mut(
        &mut self,
    ) -> Option<&mut crate::resource::LoopProvenance> {
        match &mut self.body {
            DeferredForBody::Resource(resource) => Some(resource.loop_provenance_mut()),
            DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
        }
    }

    pub(crate) fn set_loop_display_file(&mut self, file: &str) {
        self.target.set_loop_display_file(file);
    }

    pub(crate) fn set_loop_binding_scope(
        &mut self,
        binding_scope: std::sync::Arc<std::collections::BTreeSet<String>>,
    ) {
        self.target.set_loop_binding_scope(binding_scope);
    }

    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        self.site().root_bindings()
    }

    pub fn site(&self) -> DeferredForSite<'_, P> {
        DeferredForSite {
            file: self.file.as_deref(),
            line: self.line,
            header: &self.header,
            binding: &self.binding,
            iterable: &self.iterable,
        }
    }

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
        Some(match &self.source_file {
            Some(path) => error.in_file(path),
            None => error,
        })
    }
}

impl ParsedDeferredForExpression<ParserPlaceholder> {
    pub(crate) fn into_resource_expression(
        self,
        known_bindings: &[String],
    ) -> Result<
        DeferredForExpression<ModuleSource, ParserPlaceholder, ParserPlaceholder>,
        DeferredForResourceExpressionError,
    > {
        if let Some(error) = self.undefined_iterable_error(known_bindings) {
            return Err(DeferredForResourceExpressionError::UndefinedIterable(error));
        }
        let Self {
            file,
            source_file,
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
                    file: source_file,
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
                    file: source_file,
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
        let mut expression = DeferredForExpression::from_template_resource(
            file,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource,
        );
        expression.source_file = source_file;
        Ok(expression)
    }
}

/// Phase-neutral, read-only metadata for a deferred `for` expression.
#[derive(Debug, Clone, Copy)]
pub struct DeferredForSite<'a, P: PlaceholderKind = NoPlaceholder> {
    pub file: Option<&'a str>,
    pub line: usize,
    pub header: &'a str,
    pub binding: &'a ForBinding,
    pub iterable: &'a Value<P>,
}

impl<P: PlaceholderKind> DeferredForSite<'_, P> {
    pub fn root_bindings(&self) -> std::collections::BTreeSet<String> {
        self.iterable.root_bindings()
    }
}

pub type SavedDeferredForExpression =
    DeferredForExpression<SavedModuleSource, NoPlaceholder, LoopSlot>;

impl From<DeferredForExpression> for SavedDeferredForExpression {
    fn from(expression: DeferredForExpression) -> Self {
        let DeferredForExpression {
            file,
            source_file: _,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource,
            deferred_effect_id: _,
        } = expression;
        Self::from_template_resource(
            file,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource.map_resource(Resource::<SavedModuleSource, LoopSlot>::from),
        )
    }
}

/// Saved deferred templates are the one resource-shaped payload allowed to
/// retain a pending identity: the identity is assigned only after the iterable
/// materializes. Keep that exception local to this field so ordinary resource
/// and state serialization continues to reject process-local pending tokens.
mod deferred_template_resource_serde {
    use super::*;
    use serde::{Deserialize, Serialize};
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
    #[serde(bound(serialize = "S: Serialize, T: PlaceholderKind + Serialize"))]
    struct ResourceRef<'a, S, T: PlaceholderKind> {
        id: ResourceIdRef<'a>,
        attributes: &'a IndexMap<String, Value<T>>,
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

    pub(super) struct SerializableResource<'a, S, T: PlaceholderKind>(
        pub(super) &'a Resource<S, T>,
    );

    impl<S, T> serde::Serialize for SerializableResource<'_, S, T>
    where
        S: Serialize,
        T: PlaceholderKind + Serialize,
    {
        fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
        where
            Ser: serde::Serializer,
        {
            let resource = self.0;
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
    }

    #[derive(Deserialize)]
    struct ResourceIdWire {
        provider: String,
        resource_type: String,
        #[serde(default, deserialize_with = "deserialize_present_option")]
        identity: Option<Option<ResourceIdentity>>,
        #[serde(default, deserialize_with = "deserialize_present_option")]
        name: Option<Option<ResourceIdentity>>,
        #[serde(default)]
        provider_instance: Option<String>,
    }

    fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }

    #[derive(Deserialize)]
    #[serde(bound(deserialize = "S: Deserialize<'de>, T: PlaceholderKind + Deserialize<'de>"))]
    struct ResourceWire<S, T: PlaceholderKind> {
        id: ResourceIdWire,
        attributes: IndexMap<String, Value<T>>,
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

    pub(super) struct DeserializableResource<S, T: PlaceholderKind> {
        pub(super) resource: Resource<S, T>,
        pub(super) legacy_null_name: bool,
    }

    impl<'de, S, T> serde::Deserialize<'de> for DeserializableResource<S, T>
    where
        S: Deserialize<'de>,
        T: PlaceholderKind + Deserialize<'de>,
    {
        fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
        where
            De: serde::Deserializer<'de>,
        {
            let ResourceWire {
                id:
                    ResourceIdWire {
                        provider,
                        resource_type,
                        identity,
                        name,
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
            let legacy_null_name = matches!(&name, Some(None));
            let identity = match (identity, name) {
                (Some(_), Some(_)) => {
                    return Err(serde::de::Error::custom(
                        "deferred template resource id cannot contain both `identity` and legacy `name`",
                    ));
                }
                (Some(identity), None) | (None, Some(identity)) => identity,
                (None, None) => None,
            };
            let id = match identity {
                Some(identity) => ResourceId::with_provider_identity(
                    provider,
                    resource_type,
                    identity,
                    provider_instance,
                ),
                None => {
                    ResourceId::pending_with_provider(provider, resource_type, provider_instance)
                }
            };
            Ok(Self {
                resource: Resource {
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
                },
                legacy_null_name,
            })
        }
    }
}

impl<S, P, T> serde::Serialize for DeferredForExpression<S, P, T>
where
    S: serde::Serialize,
    P: PlaceholderKind + serde::Serialize,
    T: PlaceholderKind + serde::Serialize,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::Serializer,
    {
        #[derive(serde::Serialize)]
        #[serde(bound(
            serialize = "S: serde::Serialize, P: PlaceholderKind + serde::Serialize, T: PlaceholderKind + serde::Serialize"
        ))]
        struct WireRef<'a, S, P: PlaceholderKind, T: PlaceholderKind> {
            file: Option<&'a str>,
            line: usize,
            header: &'a str,
            resource_type: &'a str,
            target: &'a DeferredForTarget,
            iterable: &'a Value<P>,
            binding: &'a ForBinding,
            template_resource: deferred_template_resource_serde::SerializableResource<'a, S, T>,
        }

        serde::Serialize::serialize(
            &WireRef {
                file: self
                    .file
                    .as_deref()
                    .filter(|file| std::path::Path::new(file).is_relative()),
                line: self.line,
                header: &self.header,
                resource_type: self.resource_type(),
                target: &self.target,
                iterable: &self.iterable,
                binding: &self.binding,
                template_resource: deferred_template_resource_serde::SerializableResource(
                    self.template_resource.as_resource(),
                ),
            },
            serializer,
        )
    }
}

impl<'de, S, P, T> serde::Deserialize<'de> for DeferredForExpression<S, P, T>
where
    S: serde::de::DeserializeOwned,
    P: PlaceholderKind + serde::de::DeserializeOwned,
    T: PlaceholderKind + serde::de::DeserializeOwned,
{
    fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[serde(bound(
            deserialize = "S: serde::Deserialize<'de>, P: PlaceholderKind + serde::Deserialize<'de>, T: PlaceholderKind + serde::Deserialize<'de>"
        ))]
        struct Wire<S, P: PlaceholderKind, T: PlaceholderKind> {
            #[serde(deserialize_with = "deserialize_relative_diagnostic_file")]
            file: Option<String>,
            line: usize,
            header: String,
            resource_type: String,
            target: DeferredForTarget,
            iterable: Value<P>,
            binding: ForBinding,
            template_resource: deferred_template_resource_serde::DeserializableResource<S, T>,
        }

        let wire = Wire::<S, P, T>::deserialize(deserializer)?;
        let deferred_template_resource_serde::DeserializableResource {
            resource: template_resource,
            legacy_null_name,
        } = wire.template_resource;
        let template_resource_type = template_resource.id.display_type();
        if wire.resource_type != template_resource_type {
            return Err(serde::de::Error::custom(format!(
                "Invalid saved plan: deferred template resource provider/type `{template_resource_type}` does not match deferred expression resource_type `{}`",
                wire.resource_type,
            )));
        }
        match &wire.target {
            DeferredForTarget::Bound(target) => {
                let expected = target.template_address();
                if !legacy_null_name
                    && !template_resource
                        .id
                        .identity()
                        .is_some_and(|identity| expected.matches_resource_identity(identity))
                {
                    return Err(serde::de::Error::custom(format!(
                        "Invalid saved plan: deferred template resource identity does not match bound deferred target `{}`",
                        target.binding()
                    )));
                }
                if template_resource.binding.as_deref() != Some(expected.as_str()) {
                    return Err(serde::de::Error::custom(format!(
                        "Invalid saved plan: deferred template resource binding does not match bound deferred target `{}`",
                        target.binding()
                    )));
                }
            }
            DeferredForTarget::Unbound { .. } => {
                if template_resource.id.identity().is_some() {
                    return Err(serde::de::Error::custom(
                        "Invalid saved plan: deferred template resource identity for an unbound deferred target must be pending",
                    ));
                }
                if template_resource.binding.is_some() {
                    return Err(serde::de::Error::custom(
                        "Invalid saved plan: deferred template resource binding for an unbound deferred target must be absent",
                    ));
                }
            }
        }

        let template_resource = DeferredForTemplateResource::new(template_resource);
        Ok(Self::from_template_resource(
            wire.file,
            wire.line,
            wire.header,
            wire.target,
            wire.iterable,
            wire.binding,
            template_resource,
        ))
    }
}

fn deserialize_relative_diagnostic_file<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let file = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    if file
        .as_deref()
        .is_some_and(|file| std::path::Path::new(file).is_absolute())
    {
        return Err(serde::de::Error::custom(
            "deferred-for diagnostic file must be relative",
        ));
    }
    Ok(file)
}

impl<S, P: PlaceholderKind, T: PlaceholderKind> DeferredForExpression<S, P, T> {
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

impl<S> DeferredForExpression<S, ParserPlaceholder, ParserPlaceholder> {
    fn into_expanded(
        self,
    ) -> Result<DeferredForExpression<S>, crate::resource::LeftoverPlaceholder> {
        let Self {
            file,
            source_file,
            line,
            header,
            target,
            iterable,
            binding,
            template_resource,
            deferred_effect_id: _,
        } = self;
        let template_resource = template_resource
            .try_map_resource(Resource::<S, ParserPlaceholder>::into_loop_template)?;
        let mut expression = DeferredForExpression::from_template_resource(
            file,
            line,
            header,
            target,
            iterable.into_expanded()?,
            binding,
            template_resource,
        );
        expression.source_file = source_file;
        Ok(expression)
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
    P: PlaceholderKind + 'a = NoPlaceholder,
    T: PlaceholderKind + 'a = P,
    D: DeferredForPhaseTraversal<P, T> + 'a = ResolvedDeferredForPhase<P, T>,
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
    P: PlaceholderKind + 'a = NoPlaceholder,
    T: PlaceholderKind + 'a = P,
    D: DeferredForPhaseTraversal<P, T> + 'a = ResolvedDeferredForPhase<P, T>,
> {
    /// A top-level resource.
    Resource(&'a Resource<S, P>),
    /// A top-level composition (module-expansion synthetic node).
    LiveComposition(&'a LiveComposition<P>),
    /// A top-level data source (`read`-keyword resource).
    DataSource(&'a DataSource<S, P>),
    /// The template body of a deferred `for` expression — always a
    /// resource (`for` bodies never carry `read` / composition).
    Deferred {
        resource: &'a DeferredForTemplateResource<S, T>,
        deferred: D::DeferredRef<'a, S>,
    },
}

/// Mutable counterpart of [`ResourceRef`] used by whole-file resource
/// transformations.
#[derive(Debug)]
pub enum ResourceMut<'a, P: PlaceholderKind = NoPlaceholder, T: PlaceholderKind = P> {
    Resource(&'a mut Resource<ModuleSource, P>),
    LiveComposition(&'a mut LiveComposition<P>),
    DataSource(&'a mut DataSource<ModuleSource, P>),
    Deferred {
        id: &'a ResourceId,
        attributes: &'a mut IndexMap<String, Value<T>>,
    },
}

/// A concrete top-level declaration. Unlike [`ResourceRef`], this type has no
/// deferred-template arm, so consumers of [`File::iter_top_level_resources`]
/// cannot encode an impossible fallback or panic.
#[derive(Debug, Clone, Copy)]
pub enum TopLevelResourceRef<'a, S = ModuleSource, P: PlaceholderKind = NoPlaceholder> {
    Resource(&'a Resource<S, P>),
    LiveComposition(&'a LiveComposition<P>),
    DataSource(&'a DataSource<S, P>),
}

impl<'a, S, P: PlaceholderKind> TopLevelResourceRef<'a, S, P> {
    pub fn id(&self) -> &'a ResourceId {
        match self {
            Self::Resource(resource) => &resource.id,
            Self::LiveComposition(composition) => &composition.id,
            Self::DataSource(data_source) => &data_source.id,
        }
    }

    pub fn binding(&self) -> Option<&'a str> {
        match self {
            Self::Resource(resource) => resource.binding.as_deref(),
            Self::LiveComposition(composition) => composition.binding.as_deref(),
            Self::DataSource(data_source) => data_source.binding.as_deref(),
        }
    }

    pub fn dependency_bindings(&self) -> &'a std::collections::BTreeSet<String> {
        match self {
            Self::Resource(resource) => &resource.dependency_bindings,
            Self::LiveComposition(composition) => &composition.dependency_bindings,
            Self::DataSource(data_source) => &data_source.dependency_bindings,
        }
    }

    pub fn directives(&self) -> Option<&'a Directives> {
        match self {
            Self::Resource(resource) => Some(&resource.directives),
            Self::LiveComposition(_) => None,
            Self::DataSource(data_source) => Some(&data_source.directives),
        }
    }

    pub fn quoted_string_attrs(&self) -> &'a HashSet<String> {
        match self {
            Self::Resource(resource) => &resource.quoted_string_attrs,
            Self::LiveComposition(composition) => &composition.quoted_string_attrs,
            Self::DataSource(data_source) => &data_source.quoted_string_attrs,
        }
    }

    pub fn attributes(&self) -> std::borrow::Cow<'a, IndexMap<String, Value<P>>> {
        match self {
            Self::Resource(resource) => std::borrow::Cow::Borrowed(&resource.attributes),
            Self::LiveComposition(composition) => std::borrow::Cow::Owned(
                composition
                    .signature
                    .attributes
                    .iter()
                    .map(|(name, attribute)| (name.clone(), attribute.to_value()))
                    .collect(),
            ),
            Self::DataSource(data_source) => std::borrow::Cow::Borrowed(&data_source.attributes),
        }
    }

    pub fn resolved_attributes(&self) -> HashMap<String, Value<P>> {
        crate::resource::attrs_to_hashmap(&self.attributes())
    }
}

impl<S, P: PlaceholderKind, T: PlaceholderKind, D: DeferredForPhaseTraversal<P, T>> Copy
    for ResourceContext<'_, S, P, T, D>
{
}

impl<S, P: PlaceholderKind, T: PlaceholderKind, D: DeferredForPhaseTraversal<P, T>> Clone
    for ResourceContext<'_, S, P, T, D>
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<S, P: PlaceholderKind, T: PlaceholderKind, D: DeferredForPhaseTraversal<P, T>> Copy
    for ResourceRef<'_, S, P, T, D>
{
}

impl<S, P: PlaceholderKind, T: PlaceholderKind, D: DeferredForPhaseTraversal<P, T>> Clone
    for ResourceRef<'_, S, P, T, D>
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, S, P, T, D> ResourceRef<'a, S, P, T, D>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: DeferredForPhaseTraversal<P, T>,
{
    /// Stable identifier of this resource.
    pub fn id(&self) -> &'a ResourceId {
        match self {
            ResourceRef::Resource(r) => &r.id,
            ResourceRef::LiveComposition(v) => &v.id,
            ResourceRef::DataSource(d) => &d.id,
            ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => resource.id(),
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
            } => resource.binding(),
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
            } => resource.dependency_bindings(),
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
            } => Some(resource.directives()),
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
            } => resource.quoted_string_attrs(),
        }
    }

    /// The [`ResourceContext`] an old `iter_all_resources` caller would
    /// have seen — `Deferred` for a for-expression template, `Direct`
    /// otherwise.
    pub fn context(&self) -> ResourceContext<'a, S, P, T, D> {
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
}

impl<'a, S, P, D> ResourceRef<'a, S, P, P, D>
where
    P: PlaceholderKind,
    D: DeferredForPhaseTraversal<P, P>,
{
    /// Source-order preserving attribute map as `Value`s.
    ///
    /// Returned as a [`Cow`](std::borrow::Cow) because
    /// [`LiveComposition`] stores its attributes typed as
    /// [`LiveCompositionAttribute`](crate::resource::LiveCompositionAttribute)
    /// since #3294, and must materialize a `Value`-typed view on
    /// demand. The other three variants return a borrowed reference.
    pub fn attributes(&self) -> std::borrow::Cow<'a, IndexMap<String, Value<P>>> {
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
            ResourceRef::Deferred { resource, .. } => {
                std::borrow::Cow::Borrowed(resource.attributes())
            }
        }
    }

    /// Attributes projected to a `HashMap<String, Value>` — the
    /// lookup-shaped view consumed by schema validation.
    pub fn resolved_attributes(&self) -> HashMap<String, Value<P>> {
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
pub struct ArgumentParameter<T = TypeExpr, P: PlaceholderKind = ParserPlaceholder> {
    pub name: String,
    pub static_type: T,
    pub default: Option<Value<P>>,
    /// Optional description (from block form)
    pub description: Option<String>,
    /// Optional validation blocks (from block form). Multiple blocks are allowed.
    pub validations: Vec<ValidationBlock<P>>,
}

pub type ConsumerArgumentParameter = ArgumentParameter<ConsumerStaticType>;

impl<T> ArgumentParameter<T, ParserPlaceholder> {
    fn into_expanded(
        self,
    ) -> Result<ArgumentParameter<T, NoPlaceholder>, crate::resource::LeftoverPlaceholder> {
        Ok(ArgumentParameter {
            name: self.name,
            static_type: self.static_type,
            default: self.default.map(Value::into_expanded).transpose()?,
            description: self.description,
            validations: self
                .validations
                .into_iter()
                .map(ValidationBlock::into_expanded)
                .collect::<Result<_, _>>()?,
        })
    }
}

/// A validate block: `validation { condition = <expr> error_message = "..." }`
#[derive(Debug, Clone)]
pub struct ValidationBlock<P: PlaceholderKind = NoPlaceholder> {
    pub condition: ValidateExpr<P>,
    pub error_message: Option<String>,
    source: DeclarationSource,
}

impl<P: PlaceholderKind> ValidationBlock<P> {
    pub(crate) fn new_in_source(
        condition: ValidateExpr<P>,
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
    pub(crate) fn dummy(condition: ValidateExpr<P>, error_message: Option<String>) -> Self {
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

    pub(crate) fn condition_for_module_if_audit(&self) -> &ValidateExpr<P> {
        let Self {
            condition,
            error_message: _error_message,
            source: _source,
        } = self;
        condition
    }
}

impl ValidationBlock<ParserPlaceholder> {
    fn into_expanded(self) -> Result<ValidationBlock, crate::resource::LeftoverPlaceholder> {
        Ok(ValidationBlock {
            condition: self.condition.into_expanded()?,
            error_message: self.error_message,
            source: self.source,
        })
    }
}

// `CompareOp` lives in `expressions::validate_expr` (re-exported above).

/// Validate expression AST node
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ValidateExpr<P: PlaceholderKind = NoPlaceholder> {
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
        value: Value<P>,
    },
    /// Binary Int arithmetic used by `require` and argument validation.
    IntegerArithmetic {
        lhs: Box<ValidateExpr<P>>,
        op: crate::integer_arithmetic::IntegerOp,
        rhs: Box<ValidateExpr<P>>,
        #[serde(
            skip,
            default = "crate::integer_arithmetic::IntegerOperatorSpan::synthetic"
        )]
        operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
    },
    /// Comparison: lhs op rhs
    Compare {
        lhs: Box<ValidateExpr<P>>,
        op: CompareOp,
        rhs: Box<ValidateExpr<P>>,
    },
    /// Logical AND
    And(Box<ValidateExpr<P>>, Box<ValidateExpr<P>>),
    /// Logical OR
    Or(Box<ValidateExpr<P>>, Box<ValidateExpr<P>>),
    /// Logical NOT
    Not(Box<ValidateExpr<P>>),
    /// Function call (e.g., len(x))
    FunctionCall {
        name: String,
        args: Vec<ValidateExpr<P>>,
    },
    /// Null literal
    Null,
}

impl ValidateExpr<ParserPlaceholder> {
    pub(crate) fn collect_leftover_placeholders(
        &self,
        context: &LeftoverPlaceholderContext,
        policy: LeftoverPlaceholderPolicy,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    ) {
        match self {
            Self::BoundArgument {
                argument: _argument,
                operand: _operand,
                value,
            } => {
                value.collect_leftover_placeholders(context.clone(), policy, errors);
            }
            Self::IntegerArithmetic {
                lhs,
                op: _op,
                rhs,
                operator_span: _operator_span,
            } => {
                lhs.collect_leftover_placeholders(context, policy, errors);
                rhs.collect_leftover_placeholders(context, policy, errors);
            }
            Self::Compare { lhs, op: _op, rhs } => {
                lhs.collect_leftover_placeholders(context, policy, errors);
                rhs.collect_leftover_placeholders(context, policy, errors);
            }
            Self::And(lhs, rhs) | Self::Or(lhs, rhs) => {
                lhs.collect_leftover_placeholders(context, policy, errors);
                rhs.collect_leftover_placeholders(context, policy, errors);
            }
            Self::Not(value) => value.collect_leftover_placeholders(context, policy, errors),
            Self::FunctionCall { name: _name, args } => {
                for argument in args {
                    argument.collect_leftover_placeholders(context, policy, errors);
                }
            }
            Self::Bool(_value) => {}
            Self::Int(_value) => {}
            Self::Float(_value) => {}
            Self::Duration(_value) => {}
            Self::String(_value) => {}
            Self::Var(_name) => {}
            Self::Access {
                argument: _argument,
                path: _path,
            } => {}
            Self::Null => {}
        }
    }

    pub(crate) fn into_expanded(
        self,
    ) -> Result<ValidateExpr, crate::resource::LeftoverPlaceholder> {
        Ok(match self {
            Self::Bool(value) => ValidateExpr::Bool(value),
            Self::Int(value) => ValidateExpr::Int(value),
            Self::Float(value) => ValidateExpr::Float(value),
            Self::Duration(value) => ValidateExpr::Duration(value),
            Self::String(value) => ValidateExpr::String(value),
            Self::Var(value) => ValidateExpr::Var(value),
            Self::Access { argument, path } => ValidateExpr::Access { argument, path },
            Self::BoundArgument {
                argument,
                operand,
                value,
            } => ValidateExpr::BoundArgument {
                argument,
                operand,
                value: value.into_expanded()?,
            },
            Self::IntegerArithmetic {
                lhs,
                op,
                rhs,
                operator_span,
            } => ValidateExpr::IntegerArithmetic {
                lhs: Box::new(lhs.into_expanded()?),
                op,
                rhs: Box::new(rhs.into_expanded()?),
                operator_span,
            },
            Self::Compare { lhs, op, rhs } => ValidateExpr::Compare {
                lhs: Box::new(lhs.into_expanded()?),
                op,
                rhs: Box::new(rhs.into_expanded()?),
            },
            Self::And(lhs, rhs) => ValidateExpr::And(
                Box::new(lhs.into_expanded()?),
                Box::new(rhs.into_expanded()?),
            ),
            Self::Or(lhs, rhs) => ValidateExpr::Or(
                Box::new(lhs.into_expanded()?),
                Box::new(rhs.into_expanded()?),
            ),
            Self::Not(value) => ValidateExpr::Not(Box::new(value.into_expanded()?)),
            Self::FunctionCall { name, args } => ValidateExpr::FunctionCall {
                name,
                args: args
                    .into_iter()
                    .map(ValidateExpr::into_expanded)
                    .collect::<Result<_, _>>()?,
            },
            Self::Null => ValidateExpr::Null,
        })
    }
}

/// A require block: `require <condition>, "error message"`
/// Used for cross-argument constraints at the module top level.
#[derive(Debug, Clone)]
pub struct RequireBlock<P: PlaceholderKind = NoPlaceholder> {
    pub condition: ValidateExpr<P>,
    pub error_message: String,
    source: DeclarationSource,
}

impl<P: PlaceholderKind> RequireBlock<P> {
    pub(crate) fn new_in_source(
        condition: ValidateExpr<P>,
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
    pub(crate) fn dummy(condition: ValidateExpr<P>, error_message: String) -> Self {
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

    pub(crate) fn condition_for_module_if_audit(&self) -> &ValidateExpr<P> {
        let Self {
            condition,
            error_message: _error_message,
            source: _source,
        } = self;
        condition
    }
}

impl RequireBlock<ParserPlaceholder> {
    fn into_expanded(self) -> Result<RequireBlock, crate::resource::LeftoverPlaceholder> {
        Ok(RequireBlock {
            condition: self.condition.into_expanded()?,
            error_message: self.error_message,
            source: self.source,
        })
    }
}

/// Attribute parameter definition (in `attributes { ... }` block)
#[derive(Debug, Clone)]
pub struct AttributeParameter<T = TypeExpr, P: PlaceholderKind = ParserPlaceholder> {
    pub name: String,
    pub static_type: Option<T>,
    pub value: Option<Value<P>>,
    source: DeclarationSource,
}

impl<P: PlaceholderKind> AttributeParameter<TypeExpr, P> {
    pub(crate) fn new_in_source(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value<P>>,
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
    pub(crate) fn dummy(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value<P>>,
    ) -> Self {
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

impl<T, P: PlaceholderKind> AttributeParameter<T, P> {
    pub(crate) fn value_for_module_if_audit(&self) -> Option<&Value<P>> {
        let Self {
            name: _name,
            static_type: _static_type,
            value,
            source: _source,
        } = self;
        value.as_ref()
    }

    pub(crate) fn map_type_expr<U>(self, map: impl FnOnce(T) -> U) -> AttributeParameter<U, P> {
        AttributeParameter {
            name: self.name,
            static_type: self.static_type.map(map),
            value: self.value,
            source: self.source,
        }
    }
}

impl<T> AttributeParameter<T, ParserPlaceholder> {
    fn into_expanded(
        self,
    ) -> Result<AttributeParameter<T, NoPlaceholder>, crate::resource::LeftoverPlaceholder> {
        let name = self.name;
        Ok(AttributeParameter {
            name: name.clone(),
            static_type: self.static_type,
            value: self
                .value
                .map(Value::into_expanded)
                .transpose()
                .map_err(|error| error.in_attribute_default(name))?,
            source: self.source,
        })
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
pub struct ParsedExportParam<P: PlaceholderKind = ParserPlaceholder> {
    pub name: String,
    pub static_type: Option<TypeExpr>,
    pub value: Option<Value<P>>,
    source: DeclarationSource,
}

impl<P: PlaceholderKind> ParsedExportParam<P> {
    pub(crate) fn new_in_source(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value<P>>,
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
    pub(crate) fn dummy(
        name: String,
        type_expr: Option<TypeExpr>,
        value: Option<Value<P>>,
    ) -> Self {
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

impl<P: PlaceholderKind + PartialEq> PartialEq for ParsedExportParam<P> {
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
pub type ExportParameter = ParsedExportParam<ParserPlaceholder>;

/// Phase-agnostic accessor over an export parameter. Implemented by both
/// [`ParsedExportParam`] (parser phase) and [`InferredExportParam`]
/// (post-loader phase) so reference resolution, type validation, module
/// resolution, and upstream-state checks work uniformly across both shapes.
pub trait ExportParamLike<P: PlaceholderKind = ParserPlaceholder> {
    fn name(&self) -> &str;
    fn value(&self) -> Option<&Value<P>>;
    /// Replace the value after binding-aware export resolution.
    ///
    /// This mutator is intentionally part of the phase-agnostic trait so the
    /// export boundary can return the same parser/inference shape wrapped in
    /// a checked type instead of maintaining parallel representations.
    fn set_value(&mut self, value: Option<Value<P>>);
    fn type_expr_opt(&self) -> Option<&TypeExpr>;
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr>;
    fn inference_failed(&self) -> bool;
}

impl<P: PlaceholderKind + PartialEq> ExportParamLike<P> for ParsedExportParam<P> {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value<P>> {
        self.value.as_ref()
    }
    fn set_value(&mut self, value: Option<Value<P>>) {
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

#[doc(hidden)]
pub trait IntoExpandedExportParam: ExportParamLike<ParserPlaceholder> {
    type Expanded: ExportParamLike<NoPlaceholder>;

    fn collect_leftover_placeholders(&self, errors: &mut Vec<crate::resource::LeftoverPlaceholder>);

    fn into_expanded(self) -> Result<Self::Expanded, crate::resource::LeftoverPlaceholder>;
}

impl IntoExpandedExportParam for ParsedExportParam<ParserPlaceholder> {
    type Expanded = ParsedExportParam<NoPlaceholder>;

    fn collect_leftover_placeholders(
        &self,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    ) {
        let Self {
            name,
            static_type,
            value,
            source,
        } = self;
        let _non_value_fields = (static_type, source);
        if let Some(value) = value {
            value.collect_leftover_placeholders(
                LeftoverPlaceholderContext::Export { name: name.clone() },
                LeftoverPlaceholderPolicy::REJECT_ALL,
                errors,
            );
        }
    }

    fn into_expanded(self) -> Result<Self::Expanded, crate::resource::LeftoverPlaceholder> {
        let Self {
            name,
            static_type,
            value,
            source,
        } = self;
        Ok(ParsedExportParam {
            name: name.clone(),
            static_type,
            value: value
                .map(Value::into_expanded)
                .transpose()
                .map_err(|error| error.in_export(name))?,
            source,
        })
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
pub enum StateBlock<P: PlaceholderKind = NoPlaceholder> {
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
        id: Value<P>,
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

impl StateBlock<ParserPlaceholder> {
    fn into_expanded(self) -> Result<StateBlock, crate::resource::LeftoverPlaceholder> {
        Ok(match self {
            Self::Import { to, to_span, id } => StateBlock::Import {
                to,
                to_span,
                id: id.into_expanded()?,
            },
            Self::Removed { from } => StateBlock::Removed { from },
            Self::Moved { addresses } => StateBlock::Moved { addresses },
        })
    }
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
    pub default: Option<Value<ParserPlaceholder>>,
}

/// The body of a user-defined function: a value expression.
/// Functions are pure value transformations only.
#[derive(Debug, Clone)]
pub struct UserFunctionBody {
    pub value: Value<ParserPlaceholder>,
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
    pub local_lets: Vec<(String, Value<ParserPlaceholder>, usize)>,
    /// The body of the function
    pub body: UserFunctionBody,
}

/// Module call (instantiation)
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleArgument<P: PlaceholderKind = ParserPlaceholder> {
    operand: StructuralOperand<P>,
}

impl<P: PlaceholderKind> ModuleArgument<P> {
    pub(crate) fn value_ref(&self) -> StructuralOperandRef<'_, P> {
        self.operand.value_ref()
    }

    pub(crate) fn access_ref(&self) -> StructuralOperandRef<'_, P> {
        self.operand.access_ref()
    }

    pub(crate) fn from_parts(value: Value<P>, access_base: Option<Value<P>>) -> Self {
        Self {
            operand: StructuralOperand::from_value_with_access_base(value, access_base),
        }
    }

    pub fn new(value: Value<P>) -> Self {
        Self::from_parts(value, None)
    }

    pub(crate) fn with_access_base(value: Value<P>, access_base: Value<P>) -> Self {
        Self::from_parts(value, Some(access_base))
    }

    pub fn value(&self) -> Option<&Value<P>> {
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

    pub(crate) fn access_base(&self) -> Option<&Value<P>> {
        self.operand.explicit_access_base()
    }

    /// Value-shaped projection reserved for generic reference/dependency
    /// walkers. Semantic consumers must use the typed operand accessors.
    pub(crate) fn walker_value(&self) -> &Value<P> {
        self.operand.traversal_value()
    }

    pub(crate) fn try_visit_values<E>(
        &self,
        visit: &mut impl FnMut(&Value<P>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.operand.try_visit_values(visit)
    }

    pub(crate) fn try_rewrite<E>(
        &mut self,
        rewrite: impl FnMut(&Value<P>) -> Result<Value<P>, E>,
    ) -> Result<(), E> {
        self.operand = self.operand.try_map(rewrite)?;
        Ok(())
    }
}

impl ModuleArgument<ParserPlaceholder> {
    pub(crate) fn from_handle(handle: StructuralHandle) -> Self {
        Self {
            operand: StructuralOperand::from_handle(handle),
        }
    }

    fn into_expanded(
        self,
    ) -> Result<ModuleArgument<NoPlaceholder>, crate::resource::LeftoverPlaceholder> {
        Ok(ModuleArgument {
            operand: self.operand.try_map_stage(&mut Value::into_expanded)?,
        })
    }
}

impl<P: PlaceholderKind> From<Value<P>> for ModuleArgument<P> {
    fn from(value: Value<P>) -> Self {
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
        value: Arc<Value<ParserPlaceholder>>,
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

    pub(crate) fn try_visit_values<E>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), E>,
    ) -> Result<(), E> {
        let Self {
            records,
            record_indices_by_owner: _record_indices_by_owner,
        } = self;
        for record in records {
            match &record.source {
                BareBindingSource::Value {
                    value,
                    declared_type: _declared_type,
                } => visit(value)?,
                BareBindingSource::Structural(_handle) => {}
            }
        }
        Ok(())
    }

    pub(crate) fn try_rewrite_values<E>(
        &mut self,
        mut rewrite: impl FnMut(&Value<ParserPlaceholder>) -> Result<Value<ParserPlaceholder>, E>,
    ) -> Result<(), E> {
        let Self {
            records,
            record_indices_by_owner: _record_indices_by_owner,
        } = self;
        for record in records {
            if let BareBindingSource::Value {
                value,
                declared_type: _declared_type,
            } = &mut record.source
            {
                *value = Arc::new(rewrite(value.as_ref())?);
            }
        }
        Ok(())
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
pub struct ModuleCall<P: PlaceholderKind = ParserPlaceholder> {
    pub module_name: String,
    pub binding_name: Option<String>,
    arguments: HashMap<String, AuthoredModuleArgument<P>>,
    source: Box<ModuleCallSite>,
    loop_origin: Option<Box<crate::resource::LoopElementOrigin>>,
}

impl<P: PlaceholderKind> ModuleCall<P> {
    pub(super) fn from_authored_arguments_in_source(
        module_name: impl Into<String>,
        binding_name: Option<String>,
        call_span: SourceSpan,
        arguments: HashMap<String, AuthoredModuleArgument<P>>,
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
        arguments: HashMap<String, ModuleArgument<P>>,
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
    pub fn arguments(&self) -> impl Iterator<Item = (&str, &ModuleArgument<P>)> {
        self.arguments
            .iter()
            .map(|(name, argument)| (name.as_str(), &argument.argument))
    }

    /// Iterate borrowed authored arguments with their parser key and value spans.
    ///
    /// Every authored argument carries both spans by construction.
    pub fn arguments_with_source_spans(&self) -> impl Iterator<Item = AuthoredArgumentRef<'_, P>> {
        self.arguments
            .iter()
            .map(|(name, argument)| AuthoredArgumentRef {
                name,
                argument: &argument.argument,
                key_span: argument.key_span,
                value_span: argument.value_span,
            })
    }

    pub fn argument(&self, name: &str) -> Option<&ModuleArgument<P>> {
        self.arguments.get(name).map(|argument| &argument.argument)
    }

    pub(crate) fn try_visit_values_for_module_if_audit<E>(
        &self,
        visit: &mut impl FnMut(&Value<P>) -> Result<(), E>,
    ) -> Result<(), E> {
        let Self {
            module_name: _module_name,
            binding_name: _binding_name,
            arguments,
            source: _source,
            loop_origin: _loop_origin,
        } = self;
        for argument in arguments.values() {
            argument.argument.try_visit_values(visit)?;
        }
        Ok(())
    }

    pub fn authored_argument(&self, name: &str) -> Option<AuthoredArgumentRef<'_, P>> {
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

    pub(crate) fn arguments_mut(&mut self) -> impl Iterator<Item = &mut ModuleArgument<P>> {
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

impl ModuleCall<ParserPlaceholder> {
    fn into_expanded(
        self,
    ) -> Result<ModuleCall<NoPlaceholder>, crate::resource::LeftoverPlaceholder> {
        Ok(ModuleCall {
            module_name: self.module_name,
            binding_name: self.binding_name,
            arguments: self
                .arguments
                .into_iter()
                .map(|(name, argument)| {
                    Ok((
                        name,
                        AuthoredModuleArgument {
                            argument: argument.argument.into_expanded()?,
                            key_span: argument.key_span,
                            value_span: argument.value_span,
                        },
                    ))
                })
                .collect::<Result<_, crate::resource::LeftoverPlaceholder>>()?,
            source: self.source,
            loop_origin: self.loop_origin,
        })
    }
}

/// Borrowed view of one parser-authored module-call argument.
#[derive(Debug, Clone, Copy)]
pub struct AuthoredArgumentRef<'a, P: PlaceholderKind = ParserPlaceholder> {
    name: &'a str,
    argument: &'a ModuleArgument<P>,
    key_span: SourceSpan,
    value_span: SourceSpan,
}

impl<'a, P: PlaceholderKind> AuthoredArgumentRef<'a, P> {
    pub fn name(&self) -> &'a str {
        self.name
    }

    pub fn argument(&self) -> &'a ModuleArgument<P> {
        self.argument
    }

    pub fn key_span(&self) -> SourceSpan {
        self.key_span
    }

    pub fn value_span(&self) -> SourceSpan {
        self.value_span
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AuthoredModuleArgument<P: PlaceholderKind = ParserPlaceholder> {
    argument: ModuleArgument<P>,
    key_span: SourceSpan,
    value_span: SourceSpan,
}

impl<P: PlaceholderKind> AuthoredModuleArgument<P> {
    pub(crate) fn new(
        argument: ModuleArgument<P>,
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
    template_dependency_values: HashMap<ResourceId, IndexMap<String, Value<ParserPlaceholder>>>,
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
    pub(crate) arguments: Vec<FunctionArgument<ParserPlaceholder>>,
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

    pub(crate) fn insert_template_dependency_values(
        &mut self,
        id: ResourceId,
        values: IndexMap<String, Value<ParserPlaceholder>>,
    ) {
        self.template_dependency_values.insert(id, values);
    }

    pub(crate) fn template_dependency_values(
        &self,
        id: &ResourceId,
    ) -> Option<&IndexMap<String, Value<ParserPlaceholder>>> {
        self.template_dependency_values.get(id)
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

    pub(crate) fn try_visit_values<E>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), E>,
    ) -> Result<(), E> {
        let Self {
            structural_handles: _structural_handles,
            structural_aliases: _structural_aliases,
            value_structural_handles,
            resource_source_files: _resource_source_files,
            template_dependency_values,
            argument_type_sources: _argument_type_sources,
            pending_function_type_checks,
        } = self;
        for metadata in value_structural_handles.values() {
            metadata.try_visit_values(visit)?;
        }
        for values in template_dependency_values.values() {
            for value in values.values() {
                visit(value)?;
            }
        }
        for check in pending_function_type_checks {
            for argument in &check.arguments {
                argument.try_visit_values(visit)?;
            }
        }
        Ok(())
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
        self.template_dependency_values
            .extend(other.template_dependency_values);
        self.argument_type_sources
            .extend(other.argument_type_sources);
        for check in other.pending_function_type_checks {
            self.insert_pending_function_type_check(check);
        }
    }
}

/// Provider configuration
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "P: PlaceholderKind + serde::Serialize",
    deserialize = "P: PlaceholderKind + serde::Deserialize<'de>"
))]
pub struct ProviderConfig<P: PlaceholderKind = NoPlaceholder> {
    pub name: String,
    pub attributes: IndexMap<String, Value<P>>,
    /// Default tags to apply to all resources that support tags.
    /// Extracted from `default_tags = { ... }` in the provider block.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub default_tags: IndexMap<String, Value<P>>,
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
    pub unresolved_attributes: IndexMap<String, Value<P>>,
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

impl<P: PlaceholderKind> ProviderConfig<P> {
    /// Whether this entry is the kind's default instance.
    ///
    /// Default instances come from top-level `provider <kind> { ... }`
    /// blocks and have no binding. Named instances always carry the `let`
    /// binding that declared them.
    pub fn is_default(&self) -> bool {
        self.binding.is_none()
    }

    /// Project parser/runtime configuration down to the metadata needed to
    /// locate and construct a provider factory. Attribute values are
    /// deliberately excluded: factories read source/version/revision and
    /// binding identity, while configured values are consumed later by the
    /// provider instance.
    pub fn loading_projection(&self) -> ProviderConfig {
        let Self {
            name,
            attributes,
            default_tags,
            source,
            version,
            revision,
            unresolved_attributes,
            binding,
        } = self;
        let _configuration_values = (attributes, default_tags, unresolved_attributes);
        ProviderConfig {
            name: name.clone(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: source.clone(),
            version: version.clone(),
            revision: revision.clone(),
            unresolved_attributes: IndexMap::new(),
            binding: binding.clone(),
        }
    }
}

impl ProviderConfig<ParserPlaceholder> {
    fn into_expanded(self) -> Result<ProviderConfig, crate::resource::LeftoverPlaceholder> {
        Ok(ProviderConfig {
            name: self.name,
            attributes: crate::resource::into_expanded_value_map(self.attributes, None)?,
            default_tags: crate::resource::into_expanded_value_map(self.default_tags, None)?,
            source: self.source,
            version: self.version,
            revision: self.revision,
            unresolved_attributes: crate::resource::into_expanded_value_map(
                self.unresolved_attributes,
                None,
            )?,
            binding: self.binding,
        })
    }
}

/// Backend configuration for state storage
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendConfig<P: PlaceholderKind = NoPlaceholder> {
    /// Backend type (e.g., "s3", "gcs", "local")
    pub backend_type: String,
    /// Backend-specific attributes
    pub attributes: HashMap<String, Value<P>>,
    /// Parser-authored source spans for attribute keys. Runtime and saved-plan
    /// representations deliberately omit this diagnostic-only metadata.
    #[serde(skip)]
    pub attribute_spans: HashMap<String, SourceSpan>,
}

impl BackendConfig<ParserPlaceholder> {
    pub fn into_expanded(self) -> Result<BackendConfig, crate::resource::LeftoverPlaceholder> {
        Ok(BackendConfig {
            backend_type: self.backend_type,
            attributes: crate::resource::into_expanded_value_map(self.attributes, None)?,
            attribute_spans: self.attribute_spans,
        })
    }
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
///
/// The RHS cannot carry a parser placeholder:
///
/// ```compile_fail
/// use carina_core::parser::UntilPredicateAst;
/// use carina_core::resource::{DeferredValue, ParserPlaceholder, Value};
///
/// let _: UntilPredicateAst = UntilPredicateAst {
///     lhs_segments: vec!["target".into(), "status".into()],
///     rhs: Value::<ParserPlaceholder>::Deferred(DeferredValue::Placeholder(
///         ParserPlaceholder::FnParam { name: "status".into() },
///     )),
/// };
/// ```
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
pub struct InferredExportParam<P: PlaceholderKind = ParserPlaceholder> {
    pub name: String,
    pub static_type: ExportStaticType,
    pub value: Option<Value<P>>,
}

impl<P: PlaceholderKind + PartialEq> ExportParamLike<P> for InferredExportParam<P> {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value<P>> {
        self.value.as_ref()
    }
    fn set_value(&mut self, value: Option<Value<P>>) {
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

impl IntoExpandedExportParam for InferredExportParam<ParserPlaceholder> {
    type Expanded = InferredExportParam<NoPlaceholder>;

    fn collect_leftover_placeholders(
        &self,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    ) {
        let Self {
            name,
            static_type,
            value,
        } = self;
        let _static_type = static_type;
        if let Some(value) = value {
            value.collect_leftover_placeholders(
                LeftoverPlaceholderContext::Export { name: name.clone() },
                LeftoverPlaceholderPolicy::REJECT_ALL,
                errors,
            );
        }
    }

    fn into_expanded(self) -> Result<Self::Expanded, crate::resource::LeftoverPlaceholder> {
        let Self {
            name,
            static_type,
            value,
        } = self;
        Ok(InferredExportParam {
            name: name.clone(),
            static_type,
            value: value
                .map(Value::into_expanded)
                .transpose()
                .map_err(|error| error.in_export(name))?,
        })
    }
}

/// Deferred-loop bodies before the module-resolution seam.
#[derive(Debug, Clone)]
pub struct ParsedDeferredForPhase<P: PlaceholderKind = ParserPlaceholder> {
    pub(crate) expressions: Vec<ParsedDeferredForExpression<P>>,
}

impl<P: PlaceholderKind> Default for ParsedDeferredForPhase<P> {
    fn default() -> Self {
        Self {
            expressions: Vec::new(),
        }
    }
}

/// Resource-only deferred loops carried by a finalized file.
#[derive(Debug, Clone)]
pub struct ResolvedDeferredForExpressions<
    P: PlaceholderKind = NoPlaceholder,
    T: PlaceholderKind = LoopSlot,
>(Vec<DeferredForExpression<ModuleSource, P, T>>);

impl<P: PlaceholderKind, T: PlaceholderKind> ResolvedDeferredForExpressions<P, T> {
    pub fn as_slice(&self) -> &[DeferredForExpression<ModuleSource, P, T>] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, DeferredForExpression<ModuleSource, P, T>> {
        self.0.iter()
    }

    pub fn iter_mut(
        &mut self,
    ) -> std::slice::IterMut<'_, DeferredForExpression<ModuleSource, P, T>> {
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
        mut predicate: impl FnMut(&DeferredForExpression<ModuleSource, P, T>) -> bool,
    ) -> (Self, Self) {
        let (selected, retained) = self.0.into_iter().partition(|item| predicate(item));
        (Self(selected), Self(retained))
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_expressions_for_test(
        expressions: impl IntoIterator<Item = DeferredForExpression<ModuleSource, P, T>>,
    ) -> Self {
        Self(expressions.into_iter().collect())
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn pop(&mut self) -> Option<DeferredForExpression<ModuleSource, P, T>> {
        self.0.pop()
    }

    pub(crate) fn remove(&mut self, index: usize) -> DeferredForExpression<ModuleSource, P, T> {
        self.0.remove(index)
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    pub fn append_finalized(&mut self, other: Self) {
        self.extend(other);
    }
}

impl<P: PlaceholderKind, T: PlaceholderKind> std::ops::Index<usize>
    for ResolvedDeferredForExpressions<P, T>
{
    type Output = DeferredForExpression<ModuleSource, P, T>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl<P: PlaceholderKind, T: PlaceholderKind> std::ops::Deref
    for ResolvedDeferredForExpressions<P, T>
{
    type Target = [DeferredForExpression<ModuleSource, P, T>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<P: PlaceholderKind, T: PlaceholderKind> IntoIterator for ResolvedDeferredForExpressions<P, T> {
    type Item = DeferredForExpression<ModuleSource, P, T>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, P: PlaceholderKind, T: PlaceholderKind> IntoIterator
    for &'a ResolvedDeferredForExpressions<P, T>
{
    type Item = &'a DeferredForExpression<ModuleSource, P, T>;
    type IntoIter = std::slice::Iter<'a, DeferredForExpression<ModuleSource, P, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Deferred-loop phase reached only by module-resolution finalization.
#[derive(Debug, Clone)]
pub struct ResolvedDeferredForPhase<
    P: PlaceholderKind = NoPlaceholder,
    T: PlaceholderKind = LoopSlot,
> {
    pub(crate) expressions: ResolvedDeferredForExpressions<P, T>,
}

impl<P: PlaceholderKind, T: PlaceholderKind> Default for ResolvedDeferredForPhase<P, T> {
    fn default() -> Self {
        Self {
            expressions: ResolvedDeferredForExpressions(Vec::new()),
        }
    }
}

mod deferred_for_phase_private {
    pub trait Sealed {}
}

#[doc(hidden)]
pub trait DeferredForPhaseTraversal<P: PlaceholderKind = ParserPlaceholder, T: PlaceholderKind = P>:
    deferred_for_phase_private::Sealed
{
    type DeferredRef<'a, S: 'a>: Copy
    where
        Self: 'a,
        P: 'a,
        T: 'a;

    fn resource_refs(
        &self,
    ) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, P, T, Self>> + '_>
    where
        Self: Sized;

    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, P, T>> + '_>
    where
        Self: Sized;

    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_, P>> + '_>;

    fn deferred_header<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> &'a str
    where
        P: 'a,
        T: 'a;

    fn deferred_line<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> usize
    where
        P: 'a,
        T: 'a;
}

#[doc(hidden)]
pub trait ParserDeferredForPhaseAudit:
    DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder>
{
    fn try_visit_values_for_module_if_audit<E>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), E>,
    ) -> Result<(), E>;
}

impl<P: PlaceholderKind> deferred_for_phase_private::Sealed for ParsedDeferredForPhase<P> {}

impl<P: PlaceholderKind> DeferredForPhaseTraversal<P, P> for ParsedDeferredForPhase<P> {
    type DeferredRef<'a, S: 'a>
        = &'a ParsedDeferredForExpression<P>
    where
        P: 'a;

    fn resource_refs(
        &self,
    ) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, P, P, Self>> + '_> {
        Box::new(
            self.expressions
                .iter()
                .filter_map(|deferred| match deferred.body() {
                    DeferredForBody::Resource(resource) => {
                        Some(ResourceRef::Deferred { resource, deferred })
                    }
                    DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
                }),
        )
    }

    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, P, P>> + '_> {
        Box::new(
            self.expressions
                .iter_mut()
                .filter_map(|deferred| match &mut deferred.body {
                    DeferredForBody::Resource(resource) => {
                        let (id, attributes) = resource.id_and_attributes_mut();
                        Some(ResourceMut::Deferred { id, attributes })
                    }
                    DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => None,
                }),
        )
    }

    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_, P>> + '_> {
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
}

impl ParserDeferredForPhaseAudit for ParsedDeferredForPhase<ParserPlaceholder> {
    fn try_visit_values_for_module_if_audit<E>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), E>,
    ) -> Result<(), E> {
        for expression in &self.expressions {
            visit(&expression.iterable)?;
            match expression.body() {
                DeferredForBody::Resource(resource) => {
                    for value in resource.attributes().values() {
                        visit(value)?;
                    }
                }
                DeferredForBody::Read(data_source) => {
                    for value in data_source.attributes.values() {
                        visit(value)?;
                    }
                }
                DeferredForBody::ModuleCall(call) => {
                    call.try_visit_values_for_module_if_audit(visit)?;
                }
            }
        }
        Ok(())
    }
}

impl<P: PlaceholderKind, T: PlaceholderKind> deferred_for_phase_private::Sealed
    for ResolvedDeferredForPhase<P, T>
{
}

impl<P: PlaceholderKind, T: PlaceholderKind> DeferredForPhaseTraversal<P, T>
    for ResolvedDeferredForPhase<P, T>
{
    type DeferredRef<'a, S: 'a>
        = &'a DeferredForExpression<S, P, T>
    where
        P: 'a,
        T: 'a;

    fn resource_refs(
        &self,
    ) -> Box<dyn Iterator<Item = ResourceRef<'_, ModuleSource, P, T, Self>> + '_> {
        Box::new(
            self.expressions
                .iter()
                .map(|deferred| ResourceRef::Deferred {
                    resource: deferred.template_resource(),
                    deferred,
                }),
        )
    }

    fn resource_muts(&mut self) -> Box<dyn Iterator<Item = ResourceMut<'_, P, T>> + '_> {
        Box::new(self.expressions.iter_mut().map(|deferred| {
            let (id, attributes) = deferred.template_resource.id_and_attributes_mut();
            ResourceMut::Deferred { id, attributes }
        }))
    }

    fn sites(&self) -> Box<dyn Iterator<Item = DeferredForSite<'_, P>> + '_> {
        Box::new(self.expressions.iter().map(DeferredForExpression::site))
    }

    fn deferred_header<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> &'a str
    where
        P: 'a,
        T: 'a,
    {
        &deferred.header
    }

    fn deferred_line<'a, S: 'a>(deferred: Self::DeferredRef<'a, S>) -> usize
    where
        P: 'a,
        T: 'a,
    {
        deferred.line
    }
}

impl ParserDeferredForPhaseAudit
    for ResolvedDeferredForPhase<ParserPlaceholder, ParserPlaceholder>
{
    fn try_visit_values_for_module_if_audit<E>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), E>,
    ) -> Result<(), E> {
        for expression in &self.expressions {
            visit(&expression.iterable)?;
            for value in expression.attributes().values() {
                visit(value)?;
            }
        }
        Ok(())
    }
}

/// Parse result, generic over export parameters, ordinary values, template
/// values, and the deferred-loop phase.
#[derive(Debug, Clone)]
pub struct File<
    E,
    P: PlaceholderKind = ParserPlaceholder,
    T: PlaceholderKind = P,
    D = ParsedDeferredForPhase<P>,
> {
    pub providers: Vec<ProviderConfig<P>>,
    pub resources: Vec<Resource<ModuleSource, P>>,
    pub data_sources: Vec<DataSource<ModuleSource, P>>,
    pub compositions: Vec<LiveComposition<P>>,
    pub variables: IndexMap<String, Value<P>>,
    pub uses: Vec<UseStatement>,
    pub module_calls: Vec<ModuleCall<P>>,
    pub arguments: Vec<ArgumentParameter<TypeExpr, P>>,
    pub attribute_params: Vec<AttributeParameter<TypeExpr, P>>,
    pub export_params: Vec<E>,
    pub backend: Option<BackendConfig<P>>,
    pub state_blocks: Vec<StateBlock<P>>,
    pub user_functions: HashMap<String, UserFunction>,
    pub upstream_states: Vec<UpstreamState>,
    pub wait_bindings: Vec<WaitBinding>,
    pub requires: Vec<RequireBlock<P>>,
    pub structural_bindings: HashSet<String>,
    #[doc(hidden)]
    pub parser_binding_metadata: ParserBindingMetadata,
    #[doc(hidden)]
    #[cfg(not(test))]
    parsed_binding_uses: ParsedBindingUses,
    #[cfg(test)]
    pub(crate) parsed_binding_uses: ParsedBindingUses,
    pub warnings: Vec<ParseWarning>,
    pub(crate) deferred_for_phase: D,
    /// Carries the deferred-template placeholder stage even when `D` is an
    /// opaque phase parameter. The marker has no runtime representation.
    pub(crate) template_placeholder: std::marker::PhantomData<fn() -> T>,
    pub deferred_module_ifs: Vec<super::DeferredStructuralIf<P>>,
    pub expansion_trace: crate::resource::ExpansionTrace,
}

pub type ParsedFile = File<
    ParsedExportParam<ParserPlaceholder>,
    ParserPlaceholder,
    ParserPlaceholder,
    ParsedDeferredForPhase<ParserPlaceholder>,
>;

pub type InferredFile = File<
    InferredExportParam<ParserPlaceholder>,
    ParserPlaceholder,
    ParserPlaceholder,
    ParsedDeferredForPhase<ParserPlaceholder>,
>;

pub type ModuleResolvedFile<E> = File<
    E,
    ParserPlaceholder,
    ParserPlaceholder,
    ResolvedDeferredForPhase<ParserPlaceholder, ParserPlaceholder>,
>;

pub type ModuleResolvedParsedFile = ModuleResolvedFile<ParsedExportParam<ParserPlaceholder>>;

pub type ModuleResolvedInferredFile = ModuleResolvedFile<InferredExportParam<ParserPlaceholder>>;

pub type ExpandedFile<E> =
    File<E, NoPlaceholder, LoopSlot, ResolvedDeferredForPhase<NoPlaceholder, LoopSlot>>;

pub type ExpandedParsedFile = ExpandedFile<ParsedExportParam<NoPlaceholder>>;
pub type ExpandedInferredFile = ExpandedFile<InferredExportParam<NoPlaceholder>>;
pub type ExpandedInferredExportParam = InferredExportParam<NoPlaceholder>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserPlaceholderBoundary {
    ExpandedFile,
    ModuleDefinition,
}

impl<E, P, T, D> Default for File<E, P, T, D>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: Default,
{
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
            deferred_for_phase: D::default(),
            template_placeholder: std::marker::PhantomData,
            deferred_module_ifs: Vec::new(),
            expansion_trace: crate::resource::ExpansionTrace::new(),
        }
    }
}

impl<E> File<E, ParserPlaceholder, ParserPlaceholder, ParsedDeferredForPhase<ParserPlaceholder>> {
    pub(crate) fn capture_template_dependency_values(&mut self) {
        fn capture_deferred(
            deferred: &super::DeferredStructuralIf<ParserPlaceholder>,
            captured: &mut Vec<(ResourceId, IndexMap<String, Value<ParserPlaceholder>>)>,
        ) {
            for branch in deferred.branches() {
                match branch {
                    DeferredIfBody::Resource(resource) => {
                        captured.push((resource.id.clone(), resource.attributes.clone()));
                    }
                    DeferredIfBody::DataSource(data_source) => {
                        captured.push((data_source.id.clone(), data_source.attributes.clone()));
                    }
                    DeferredIfBody::ModuleCall(_) => {}
                    DeferredIfBody::ExpandedModuleCall(contribution) => {
                        for nested in &contribution.deferred_module_ifs {
                            capture_deferred(nested, captured);
                        }
                    }
                }
            }
        }

        let mut captured =
            self.resources
                .iter()
                .map(|resource| (resource.id.clone(), resource.attributes.clone()))
                .chain(
                    self.data_sources.iter().map(|data_source| {
                        (data_source.id.clone(), data_source.attributes.clone())
                    }),
                )
                .chain(
                    self.deferred_for_phase.expressions.iter().filter_map(
                        |deferred| match &deferred.body {
                            DeferredForBody::Resource(resource) => {
                                Some((resource.id().clone(), resource.attributes().clone()))
                            }
                            DeferredForBody::Read(data_source) => {
                                Some((data_source.id.clone(), data_source.attributes.clone()))
                            }
                            DeferredForBody::ModuleCall(_) => None,
                        },
                    ),
                )
                .collect::<Vec<_>>();
        for deferred in &self.deferred_module_ifs {
            capture_deferred(deferred, &mut captured);
        }
        for (id, values) in captured {
            self.parser_binding_metadata
                .insert_template_dependency_values(id, values);
        }
    }

    pub fn parsed_deferred_for_expressions(&self) -> &[ParsedDeferredForExpression] {
        &self.deferred_for_phase.expressions
    }

    pub fn deferred_for_scope_reference_sites(
        &self,
    ) -> impl Iterator<Item = DeferredForSite<'_, ParserPlaceholder>> {
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
            deferred.set_loop_binding_scope(std::sync::Arc::clone(&binding_scope));
            match &mut deferred.body {
                DeferredForBody::Resource(resource) => resource
                    .loop_provenance_mut()
                    .set_binding_scope(std::sync::Arc::clone(&binding_scope)),
                DeferredForBody::Read(_) | DeferredForBody::ModuleCall(_) => {}
            }
        }
        for deferred in &mut self.deferred_module_ifs {
            for branch in deferred.branches_mut() {
                match branch {
                    DeferredIfBody::Resource(resource) => resource
                        .loop_provenance
                        .set_binding_scope(std::sync::Arc::clone(&binding_scope)),
                    DeferredIfBody::DataSource(_) => {}
                    DeferredIfBody::ModuleCall(call) => {
                        if let Some(origin) = call.loop_origin_mut() {
                            origin.set_binding_scope(std::sync::Arc::clone(&binding_scope));
                        }
                    }
                    DeferredIfBody::ExpandedModuleCall(_) => {}
                }
            }
        }
    }

    pub(crate) fn empty_with_parsed_binding_uses(parsed_binding_uses: ParsedBindingUses) -> Self {
        Self {
            parsed_binding_uses,
            ..Self::default()
        }
    }

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
            template_placeholder: _,
            deferred_module_ifs,
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
        self.deferred_module_ifs.extend(deferred_module_ifs);
        for (leaf, call_sites) in expansion_trace.leaf_to_call_sites {
            self.expansion_trace.record(leaf, call_sites);
        }
        if let Some(backend) = backend {
            self.backend = Some(backend);
        }
    }

    pub(crate) fn map_deferred_for_phase<A>(
        self,
        map: impl FnOnce(
            Vec<ParsedDeferredForExpression>,
        ) -> (
            Vec<DeferredForExpression<ModuleSource, ParserPlaceholder, ParserPlaceholder>>,
            A,
        ),
    ) -> (ModuleResolvedFile<E>, A) {
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
            template_placeholder,
            deferred_module_ifs,
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
                template_placeholder,
                deferred_module_ifs,
                expansion_trace,
            },
            auxiliary,
        )
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl<E: IntoExpandedExportParam>
    File<E, ParserPlaceholder, ParserPlaceholder, ParsedDeferredForPhase<ParserPlaceholder>>
{
    /// Unit-test convenience for fixtures that do not exercise the module
    /// resolver itself. Production callers must cross the resolver's typed
    /// deferred-body finalization seam before publishing an expanded file.
    pub fn into_expanded(self) -> Result<ExpandedFile<E::Expanded>, LeftoverPlaceholders> {
        let mut known_bindings = self
            .structural_binding_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        known_bindings.extend(self.variables.keys().cloned());
        known_bindings.sort_unstable();
        known_bindings.dedup();
        let (resolved, errors) = self.map_deferred_for_phase(|pending| {
            let mut resolved = Vec::with_capacity(pending.len());
            let mut errors = Vec::new();
            for expression in pending {
                match expression.into_resource_expression(&known_bindings) {
                    Ok(expression) => resolved.push(expression),
                    Err(error) => errors.push(error),
                }
            }
            (resolved, errors)
        });
        assert!(
            errors.is_empty(),
            "test fixture bypassed deferred-body finalization with unsupported bodies: {errors:?}"
        );
        resolved.into_expanded()
    }
}

impl<E, P, T, D> File<E, P, T, D>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
{
    pub fn bare_binding_uses(&self) -> &ParsedBindingUses {
        &self.parsed_binding_uses
    }

    pub(crate) fn try_visit_bare_binding_values<A>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), A>,
    ) -> Result<(), A> {
        self.parsed_binding_uses.try_visit_values(visit)
    }

    pub(crate) fn assert_module_if_audit_classifies_every_field(&self) {
        let File {
            providers: _providers,
            resources: _resources,
            data_sources: _data_sources,
            compositions: _compositions,
            variables: _variables,
            uses: _uses,
            module_calls: _module_calls,
            arguments: _arguments,
            attribute_params: _attribute_params,
            export_params: _export_params,
            backend: _backend,
            state_blocks: _state_blocks,
            user_functions: _user_functions,
            upstream_states: _upstream_states,
            wait_bindings: _wait_bindings,
            requires: _requires,
            structural_bindings: _structural_bindings,
            parser_binding_metadata: _parser_binding_metadata,
            parsed_binding_uses: _parsed_binding_uses,
            warnings: _warnings,
            deferred_for_phase: _deferred_for_phase,
            template_placeholder: _template_placeholder,
            deferred_module_ifs: _deferred_module_ifs,
            expansion_trace: _expansion_trace,
        } = self;
    }

    pub fn map_export_params<B>(self, f: impl FnOnce(Vec<E>) -> Vec<B>) -> File<B, P, T, D> {
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
            template_placeholder,
            deferred_module_ifs,
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
            template_placeholder,
            deferred_module_ifs,
            expansion_trace,
        }
    }

    pub fn iter_top_level_resources(
        &self,
    ) -> impl Iterator<Item = TopLevelResourceRef<'_, ModuleSource, P>> {
        self.resources
            .iter()
            .map(TopLevelResourceRef::Resource)
            .chain(
                self.compositions
                    .iter()
                    .map(TopLevelResourceRef::LiveComposition),
            )
            .chain(
                self.data_sources
                    .iter()
                    .map(TopLevelResourceRef::DataSource),
            )
    }

    pub fn upstream_binding_names(&self) -> HashSet<&str> {
        self.upstream_states
            .iter()
            .map(|upstream| upstream.binding.as_str())
            .collect()
    }
}

impl<E, P, T, D> File<E, P, T, D>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: DeferredForPhaseTraversal<P, T>,
{
    pub fn iter_all_resources(
        &self,
    ) -> impl Iterator<Item = ResourceRef<'_, ModuleSource, P, T, D>> {
        self.resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(self.compositions.iter().map(ResourceRef::LiveComposition))
            .chain(self.data_sources.iter().map(ResourceRef::DataSource))
            .chain(self.deferred_for_phase.resource_refs())
            .chain(
                self.deferred_module_ifs
                    .iter()
                    .flat_map(|deferred| deferred.branches())
                    .filter_map(|branch| match branch {
                        DeferredIfBody::Resource(resource) => Some(ResourceRef::Resource(resource)),
                        DeferredIfBody::DataSource(data_source) => {
                            Some(ResourceRef::DataSource(data_source))
                        }
                        DeferredIfBody::ModuleCall(_) | DeferredIfBody::ExpandedModuleCall(_) => {
                            None
                        }
                    }),
            )
    }

    pub fn iter_all_resources_mut(&mut self) -> impl Iterator<Item = ResourceMut<'_, P, T>> {
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
            .chain(
                self.deferred_module_ifs
                    .iter_mut()
                    .flat_map(|deferred| deferred.branches_mut())
                    .filter_map(|branch| match branch {
                        DeferredIfBody::Resource(resource) => Some(ResourceMut::Resource(resource)),
                        DeferredIfBody::DataSource(data_source) => {
                            Some(ResourceMut::DataSource(data_source))
                        }
                        DeferredIfBody::ModuleCall(_) | DeferredIfBody::ExpandedModuleCall(_) => {
                            None
                        }
                    }),
            )
    }

    pub fn deferred_for_sites(&self) -> impl Iterator<Item = DeferredForSite<'_, P>> {
        self.deferred_for_phase.sites()
    }
}

impl<E, D> File<E, ParserPlaceholder, ParserPlaceholder, D>
where
    D: ParserDeferredForPhaseAudit,
{
    pub(crate) fn try_visit_deferred_for_values_for_module_if_audit<A>(
        &self,
        visit: &mut impl FnMut(&Value<ParserPlaceholder>) -> Result<(), A>,
    ) -> Result<(), A> {
        self.deferred_for_phase
            .try_visit_values_for_module_if_audit(visit)
    }
}

impl<E, P, T> File<E, P, T, ResolvedDeferredForPhase<P, T>>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
{
    pub fn deferred_for_expressions(&self) -> &ResolvedDeferredForExpressions<P, T> {
        &self.deferred_for_phase.expressions
    }

    pub fn deferred_for_expressions_mut(&mut self) -> &mut ResolvedDeferredForExpressions<P, T> {
        &mut self.deferred_for_phase.expressions
    }

    pub fn take_deferred_for_expressions(&mut self) -> ResolvedDeferredForExpressions<P, T> {
        let empty = self.deferred_for_phase.expressions.empty_like();
        std::mem::replace(&mut self.deferred_for_phase.expressions, empty)
    }

    pub fn replace_deferred_for_expressions(
        &mut self,
        expressions: ResolvedDeferredForExpressions<P, T>,
    ) -> ResolvedDeferredForExpressions<P, T> {
        std::mem::replace(&mut self.deferred_for_phase.expressions, expressions)
    }

    pub fn into_deferred_for_expressions(self) -> ResolvedDeferredForExpressions<P, T> {
        self.deferred_for_phase.expressions
    }
}

impl<E, P, T, D> File<E, P, T, D>
where
    P: PlaceholderKind,
    T: PlaceholderKind,
{
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
        names.extend(
            self.deferred_module_ifs
                .iter()
                .flat_map(|deferred| deferred.branches())
                .filter_map(DeferredIfBody::binding),
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
    pub fn into_graph_nodes(self) -> impl Iterator<Item = GraphNode>
    where
        GraphNode: From<Resource<ModuleSource, P>>
            + From<LiveComposition<P>>
            + From<DataSource<ModuleSource, P>>,
    {
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
    pub fn into_leaf_nodes(self) -> impl Iterator<Item = LeafNode>
    where
        LeafNode: From<Resource<ModuleSource, P>> + From<DataSource<ModuleSource, P>>,
    {
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
    ) -> Option<&Resource<ModuleSource, P>> {
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

trait ParserBoundaryDeferredPhase {
    fn collect_leftover_placeholders(
        &self,
        module_expansion_field: LeftoverPlaceholderPolicy,
        deferred_template: LeftoverPlaceholderPolicy,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    );
}

impl ParserBoundaryDeferredPhase for ParsedDeferredForPhase<ParserPlaceholder> {
    fn collect_leftover_placeholders(
        &self,
        module_expansion_field: LeftoverPlaceholderPolicy,
        deferred_template: LeftoverPlaceholderPolicy,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    ) {
        for deferred in &self.expressions {
            deferred.iterable.collect_leftover_placeholders(
                LeftoverPlaceholderContext::DeferredForIterable {
                    header: deferred.header.clone(),
                },
                module_expansion_field,
                errors,
            );
            match &deferred.body {
                DeferredForBody::Resource(resource) => {
                    collect_deferred_for_resource_placeholders(
                        &deferred.header,
                        resource.as_resource(),
                        deferred_template,
                        errors,
                    );
                }
                DeferredForBody::Read(data_source) => {
                    for (attribute, value) in &data_source.attributes {
                        value.collect_leftover_placeholders(
                            LeftoverPlaceholderContext::DeferredForAttribute {
                                header: deferred.header.clone(),
                                attribute: attribute.clone(),
                            },
                            deferred_template,
                            errors,
                        );
                    }
                }
                DeferredForBody::ModuleCall(call) => {
                    collect_module_call_placeholders(call, deferred_template, errors);
                }
            }
        }
    }
}

impl ParserBoundaryDeferredPhase
    for ResolvedDeferredForPhase<ParserPlaceholder, ParserPlaceholder>
{
    fn collect_leftover_placeholders(
        &self,
        module_expansion_field: LeftoverPlaceholderPolicy,
        deferred_template: LeftoverPlaceholderPolicy,
        errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
    ) {
        for deferred in &self.expressions {
            deferred.iterable.collect_leftover_placeholders(
                LeftoverPlaceholderContext::DeferredForIterable {
                    header: deferred.header.clone(),
                },
                module_expansion_field,
                errors,
            );
            collect_deferred_for_resource_placeholders(
                &deferred.header,
                deferred.template_resource.as_resource(),
                deferred_template,
                errors,
            );
        }
    }
}

fn collect_deferred_for_resource_placeholders(
    header: &str,
    resource: &Resource<ModuleSource, ParserPlaceholder>,
    policy: LeftoverPlaceholderPolicy,
    errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
) {
    for (attribute, value) in &resource.attributes {
        value.collect_leftover_placeholders(
            LeftoverPlaceholderContext::DeferredForAttribute {
                header: header.to_string(),
                attribute: attribute.clone(),
            },
            policy,
            errors,
        );
    }
}

fn collect_module_call_placeholders(
    call: &ModuleCall<ParserPlaceholder>,
    policy: LeftoverPlaceholderPolicy,
    errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
) {
    let module_call_name = call
        .binding_name
        .as_ref()
        .unwrap_or(&call.module_name)
        .clone();
    for (argument_name, authored_argument) in &call.arguments {
        authored_argument
            .argument
            .operand
            .visit_values(&mut |value| {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ModuleCallArgument {
                        module_call: module_call_name.clone(),
                        argument: argument_name.clone(),
                    },
                    policy,
                    errors,
                );
            });
    }
}

fn collect_deferred_if_placeholders(
    deferred: &super::DeferredStructuralIf<ParserPlaceholder>,
    policy: LeftoverPlaceholderPolicy,
    errors: &mut Vec<crate::resource::LeftoverPlaceholder>,
) {
    deferred.condition.value.collect_leftover_placeholders(
        LeftoverPlaceholderContext::Value,
        policy,
        errors,
    );
    for branch in deferred.branches() {
        match branch {
            DeferredIfBody::Resource(resource) => {
                for (attribute, value) in &resource.attributes {
                    value.collect_leftover_placeholders(
                        LeftoverPlaceholderContext::ResourceAttribute {
                            resource: resource.id.to_string(),
                            attribute: attribute.clone(),
                        },
                        policy,
                        errors,
                    );
                }
            }
            DeferredIfBody::DataSource(data_source) => {
                for (attribute, value) in &data_source.attributes {
                    value.collect_leftover_placeholders(
                        LeftoverPlaceholderContext::ResourceAttribute {
                            resource: data_source.id.to_string(),
                            attribute: attribute.clone(),
                        },
                        policy,
                        errors,
                    );
                }
            }
            DeferredIfBody::ModuleCall(call) => {
                collect_module_call_placeholders(call, policy, errors);
            }
            DeferredIfBody::ExpandedModuleCall(contribution) => {
                errors.extend(
                    contribution.leftover_placeholders(ParserPlaceholderBoundary::ModuleDefinition),
                );
            }
        }
    }
}

#[allow(private_bounds)]
impl<E, D> File<E, ParserPlaceholder, ParserPlaceholder, D>
where
    E: IntoExpandedExportParam,
    D: ParserBoundaryDeferredPhase,
{
    fn leftover_placeholders(
        &self,
        boundary: ParserPlaceholderBoundary,
    ) -> Vec<crate::resource::LeftoverPlaceholder> {
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
            template_placeholder: _template_placeholder,
            deferred_module_ifs,
            expansion_trace,
        } = self;
        let _non_value_fields = (
            uses,
            user_functions,
            upstream_states,
            wait_bindings,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            expansion_trace,
        );

        let mut errors = Vec::new();
        let reject_all = LeftoverPlaceholderPolicy::REJECT_ALL;
        let allow_module_arguments = boundary == ParserPlaceholderBoundary::ModuleDefinition;
        let module_expansion_field =
            LeftoverPlaceholderPolicy::module_expansion_field(allow_module_arguments);
        let deferred_template =
            LeftoverPlaceholderPolicy::deferred_template(allow_module_arguments);

        for provider in providers {
            let ProviderConfig {
                name,
                attributes,
                default_tags,
                source,
                version,
                revision,
                unresolved_attributes,
                binding,
            } = provider;
            let _provider_metadata = (source, version, revision, binding);
            for (attribute, value) in attributes {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ProviderAttribute {
                        provider: name.clone(),
                        attribute: attribute.clone(),
                    },
                    reject_all,
                    &mut errors,
                );
            }
            for (attribute, value) in default_tags {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ProviderAttribute {
                        provider: name.clone(),
                        attribute: format!("default_tags.{attribute}"),
                    },
                    reject_all,
                    &mut errors,
                );
            }
            for (attribute, value) in unresolved_attributes {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ProviderAttribute {
                        provider: name.clone(),
                        attribute: attribute.clone(),
                    },
                    reject_all,
                    &mut errors,
                );
            }
        }

        for resource in resources {
            let Resource {
                id,
                attributes,
                directives,
                resolved_ignore_changes,
                prefixes,
                binding,
                dependency_bindings,
                module_source,
                default_tag_keys,
                loop_provenance,
                quoted_string_attrs,
            } = resource;
            let _resource_metadata = (
                directives,
                resolved_ignore_changes,
                prefixes,
                binding,
                dependency_bindings,
                module_source,
                default_tag_keys,
                loop_provenance,
                quoted_string_attrs,
            );
            for (attribute, value) in attributes {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ResourceAttribute {
                        resource: id.to_string(),
                        attribute: attribute.clone(),
                    },
                    module_expansion_field,
                    &mut errors,
                );
            }
        }

        for data_source in data_sources {
            let DataSource {
                id,
                attributes,
                directives,
                binding,
                dependency_bindings,
                module_source,
                quoted_string_attrs,
            } = data_source;
            let _data_source_metadata = (
                directives,
                binding,
                dependency_bindings,
                module_source,
                quoted_string_attrs,
            );
            for (attribute, value) in attributes {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ResourceAttribute {
                        resource: id.to_string(),
                        attribute: attribute.clone(),
                    },
                    module_expansion_field,
                    &mut errors,
                );
            }
        }

        for composition in compositions {
            composition.collect_leftover_placeholders(module_expansion_field, &mut errors);
        }

        for (name, value) in variables {
            value.collect_leftover_placeholders(
                LeftoverPlaceholderContext::Variable { name: name.clone() },
                module_expansion_field,
                &mut errors,
            );
        }

        for module_call in module_calls {
            let ModuleCall {
                module_name,
                binding_name,
                arguments,
                source,
                loop_origin,
            } = module_call;
            let _module_call_metadata = (source, loop_origin);
            let module_call_name = binding_name.as_ref().unwrap_or(module_name).clone();
            for (argument_name, authored_argument) in arguments {
                let AuthoredModuleArgument {
                    argument,
                    key_span,
                    value_span,
                } = authored_argument;
                let _argument_source = (key_span, value_span);
                let ModuleArgument { operand } = argument;
                operand.visit_values(&mut |value| {
                    value.collect_leftover_placeholders(
                        LeftoverPlaceholderContext::ModuleCallArgument {
                            module_call: module_call_name.clone(),
                            argument: argument_name.clone(),
                        },
                        module_expansion_field,
                        &mut errors,
                    );
                });
            }
        }

        for argument in arguments {
            let ArgumentParameter {
                name,
                static_type,
                default,
                description,
                validations,
            } = argument;
            let _argument_metadata = (static_type, description);
            if let Some(value) = default {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::ArgumentDefault { name: name.clone() },
                    module_expansion_field,
                    &mut errors,
                );
            }
            for validation in validations {
                let ValidationBlock {
                    condition,
                    error_message,
                    source,
                } = validation;
                let _validation_metadata = (error_message, source);
                condition.collect_leftover_placeholders(
                    &LeftoverPlaceholderContext::ArgumentValidation { name: name.clone() },
                    module_expansion_field,
                    &mut errors,
                );
            }
        }

        for attribute in attribute_params {
            let AttributeParameter {
                name,
                static_type,
                value,
                source,
            } = attribute;
            let _attribute_metadata = (static_type, source);
            if let Some(value) = value {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::AttributeDefault { name: name.clone() },
                    module_expansion_field,
                    &mut errors,
                );
            }
        }

        for export in export_params {
            export.collect_leftover_placeholders(&mut errors);
        }

        if let Some(backend) = backend {
            let BackendConfig {
                backend_type,
                attributes,
                attribute_spans,
            } = backend;
            let _attribute_spans = attribute_spans;
            for (attribute, value) in attributes {
                value.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::BackendAttribute {
                        backend: backend_type.clone(),
                        attribute: attribute.clone(),
                    },
                    reject_all,
                    &mut errors,
                );
            }
        }

        for state_block in state_blocks {
            match state_block {
                StateBlock::Import {
                    to,
                    to_span: _to_span,
                    id,
                } => id.collect_leftover_placeholders(
                    LeftoverPlaceholderContext::StateImport {
                        target: to.to_string(),
                    },
                    reject_all,
                    &mut errors,
                ),
                StateBlock::Removed { from: _from } => {}
                StateBlock::Moved {
                    addresses: _addresses,
                } => {}
            }
        }

        for (index, require) in requires.iter().enumerate() {
            let RequireBlock {
                condition,
                error_message,
                source,
            } = require;
            let _require_metadata = (error_message, source);
            condition.collect_leftover_placeholders(
                &LeftoverPlaceholderContext::Require {
                    label: (index + 1).to_string(),
                },
                module_expansion_field,
                &mut errors,
            );
        }

        deferred_for_phase.collect_leftover_placeholders(
            module_expansion_field,
            deferred_template,
            &mut errors,
        );

        for deferred in deferred_module_ifs {
            collect_deferred_if_placeholders(deferred, module_expansion_field, &mut errors);
        }

        errors
    }

    /// Check every value-bearing field at the parser-placeholder boundary.
    pub fn validate_expansion_boundary(
        &self,
        boundary: ParserPlaceholderBoundary,
    ) -> Result<(), LeftoverPlaceholders> {
        let errors = self.leftover_placeholders(boundary);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(LeftoverPlaceholders::new(errors))
        }
    }
}

impl<E: IntoExpandedExportParam>
    File<
        E,
        ParserPlaceholder,
        ParserPlaceholder,
        ResolvedDeferredForPhase<ParserPlaceholder, ParserPlaceholder>,
    >
{
    #[doc(hidden)]
    pub fn into_expanded(self) -> Result<ExpandedFile<E::Expanded>, LeftoverPlaceholders> {
        self.validate_expansion_boundary(ParserPlaceholderBoundary::ExpandedFile)?;

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
            template_placeholder: _template_placeholder,
            deferred_module_ifs,
            expansion_trace,
        } = self;

        Ok(File {
            providers: providers
                .into_iter()
                .map(ProviderConfig::into_expanded)
                .collect::<Result<_, _>>()?,
            resources: resources
                .into_iter()
                .map(Resource::into_expanded)
                .collect::<Result<_, _>>()?,
            data_sources: data_sources
                .into_iter()
                .map(DataSource::into_expanded)
                .collect::<Result<_, _>>()?,
            compositions: compositions
                .into_iter()
                .map(LiveComposition::into_expanded)
                .collect::<Result<_, _>>()?,
            variables: crate::resource::into_expanded_value_map(variables, None)?,
            uses,
            module_calls: module_calls
                .into_iter()
                .map(ModuleCall::into_expanded)
                .collect::<Result<_, _>>()?,
            arguments: arguments
                .into_iter()
                .map(ArgumentParameter::into_expanded)
                .collect::<Result<_, _>>()?,
            attribute_params: attribute_params
                .into_iter()
                .map(AttributeParameter::into_expanded)
                .collect::<Result<_, _>>()?,
            export_params: export_params
                .into_iter()
                .map(IntoExpandedExportParam::into_expanded)
                .collect::<Result<_, _>>()?,
            backend: backend.map(BackendConfig::into_expanded).transpose()?,
            state_blocks: state_blocks
                .into_iter()
                .map(StateBlock::into_expanded)
                .collect::<Result<_, _>>()?,
            user_functions,
            upstream_states,
            wait_bindings,
            requires: requires
                .into_iter()
                .map(RequireBlock::into_expanded)
                .collect::<Result<_, _>>()?,
            structural_bindings,
            parser_binding_metadata,
            parsed_binding_uses,
            warnings,
            deferred_for_phase: ResolvedDeferredForPhase {
                expressions: ResolvedDeferredForExpressions(
                    deferred_for_phase
                        .expressions
                        .into_iter()
                        .map(DeferredForExpression::into_expanded)
                        .collect::<Result<_, _>>()?,
                ),
            },
            template_placeholder: std::marker::PhantomData,
            deferred_module_ifs: {
                assert!(
                    deferred_module_ifs.is_empty(),
                    "module resolution must select every structural if before expansion"
                );
                Vec::new()
            },
            expansion_trace,
        })
    }
}

impl<E> ExpandedFile<E> {
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
                        file: deferred.source_file.clone(),
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
                        file: deferred.source_file.clone(),
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
        w.kind != WarningKind::DeferredFor
            || w.line != deferred.line
            || w.file != deferred.source_file
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
        Value::Deferred(DeferredValue::ModuleIf(_)) => "deferred module conditional",
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
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
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
    let mut resource = match deferred.target() {
        DeferredForTarget::Bound(target) => {
            let address = element.address(target.binding());
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
    let resource = substitute_attrs(resource, index, key, item, miss_policy).map_err(
        |(attribute, error)| DeferredForExpansionError::ValueAccess {
            binding: deferred.target().scheduler_key().to_string(),
            line: error.line,
            attribute,
            message: error.message,
        },
    )?;
    Ok(resource)
}

/// Run `substitute_placeholder` over every attribute of `resource`, then
/// canonicalize each attribute in place. The canonicalize step lives here
/// (and not at parse time) because once placeholders have been replaced
/// with concrete scalars, surrounding `Interpolation` shapes can collapse
/// to a `String` (#2227).
fn substitute_attrs<S>(
    resource: crate::resource::Resource<S, LoopSlot>,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<crate::resource::Resource<S>, (String, PlaceholderAccessError)> {
    let crate::resource::Resource {
        id,
        attributes,
        directives,
        resolved_ignore_changes,
        prefixes,
        binding,
        dependency_bindings,
        module_source,
        default_tag_keys,
        loop_provenance,
        quoted_string_attrs,
    } = resource;
    let mut expanded_attributes = IndexMap::with_capacity(attributes.len());
    for (name, attr) in attributes {
        let mut attr = substitute_placeholder_with_policy(attr, index, key, value, miss_policy)
            .map_err(|error| (name.clone(), error))?;
        attr.canonicalize_in_place();
        expanded_attributes.insert(name, attr);
    }
    Ok(crate::resource::Resource {
        id,
        attributes: expanded_attributes,
        directives,
        resolved_ignore_changes,
        prefixes,
        binding,
        dependency_bindings,
        module_source,
        default_tag_keys,
        loop_provenance,
        quoted_string_attrs,
    })
}

/// Substitute deferred-for-expression placeholders in a Value tree.
///
/// Replaces `Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value))` with `value`. If
/// `index` is supplied (indexed-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Placeholder(LoopSlot::Index))` with the integer index. If
/// `key` is supplied (map-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Placeholder(LoopSlot::Key))` with the key string. Recurses
/// into all compound Value variants so placeholders nested inside
/// interpolations / function calls / secrets are reached.
///
/// `ApplyTimeUnknown::UpstreamExport` is the upstream-export marker, not a
/// for-binding placeholder, so it is preserved unchanged.
#[cfg(test)]
fn substitute_placeholder(
    v: Value<LoopSlot>,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
) -> Result<Value, PlaceholderAccessError> {
    substitute_placeholder_with_policy(
        v,
        index,
        key,
        value,
        crate::resource::AccessMissPolicy::Error,
    )
}

fn substitute_placeholder_with_policy(
    v: Value<LoopSlot>,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<Value, PlaceholderAccessError> {
    let mut expanded = v.try_map_placeholder(&mut |slot| match slot {
        LoopSlot::Value => Ok(value.clone()),
        LoopSlot::Key => key
            .map(|key| Value::Concrete(ConcreteValue::String(key.to_string().into())))
            .ok_or_else(|| PlaceholderAccessError {
                line: 0,
                message: "for-loop key slot used outside a map binding".to_string(),
                coalesce_absence: false,
            }),
        LoopSlot::Index => index
            .map(|index| Value::Concrete(ConcreteValue::Int(index)))
            .ok_or_else(|| PlaceholderAccessError {
                line: 0,
                message: "for-loop index slot used outside an indexed binding".to_string(),
                coalesce_absence: false,
            }),
    })?;
    normalize_substituted_value(&mut expanded, miss_policy)?;
    Ok(expanded)
}

fn normalize_substituted_value(
    v: &mut Value,
    miss_policy: crate::resource::AccessMissPolicy,
) -> Result<(), PlaceholderAccessError> {
    match v {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            **deferred = deferred.try_map(|branch| {
                let mut branch = branch.clone();
                normalize_substituted_value(&mut branch, miss_policy)?;
                Ok::<_, PlaceholderAccessError>(branch)
            })?;
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items.iter_mut() {
                normalize_substituted_value(item, miss_policy).map_err(nested_placeholder_error)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for val in map.values_mut() {
                normalize_substituted_value(val, miss_policy).map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for arg in args.iter_mut() {
                normalize_substituted_value(arg.walker_value_mut(), miss_policy)
                    .map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let mut resolved_left = left.as_ref().clone();
            match normalize_substituted_value(&mut resolved_left, miss_policy) {
                Ok(()) if resolved_left.is_fully_resolved() => {
                    // A present left operand wins without touching the fallback.
                    *v = resolved_left;
                }
                Ok(()) => {
                    let mut resolved_default = default.as_ref().clone();
                    normalize_substituted_lazily(&mut resolved_default)?;
                    *v = Value::Deferred(DeferredValue::Coalesce {
                        value: Box::new(resolved_left),
                        default: Box::new(resolved_default),
                    });
                }
                Err(error) if error.coalesce_absence => {
                    let mut resolved_default = default.as_ref().clone();
                    normalize_substituted_value(&mut resolved_default, miss_policy)?;
                    *v = resolved_default;
                }
                Err(error) => return Err(error),
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access = access.clone();
            let original = Value::Deferred(DeferredValue::Access(access.clone()));
            let mut base = access.base().clone();
            normalize_substituted_value(&mut base, miss_policy)?;
            // The placeholder may now be a concrete list/map. Re-run the
            // shared access primitive so expanded `for` bodies do not retain
            // a stale deferred wrapper after substitution.
            match access.rebuild_result(base, access.step().clone(), miss_policy) {
                Ok(crate::resource::ValuePathSegmentResult::Resolved(value))
                | Ok(crate::resource::ValuePathSegmentResult::Deferred(value)) => *v = value,
                Ok(crate::resource::ValuePathSegmentResult::DeferredMiss(_)) => {
                    *v = original.retain_deferred_miss_display_root();
                }
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
                    normalize_substituted_value(inner, miss_policy)
                        .map_err(nested_placeholder_error)?;
                }
            }
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let expression = expression.clone();
            let mut lhs = expression.lhs().clone();
            let mut rhs = expression.rhs().clone();
            normalize_substituted_value(&mut lhs, miss_policy).map_err(nested_placeholder_error)?;
            normalize_substituted_value(&mut rhs, miss_policy).map_err(nested_placeholder_error)?;
            *v = expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| PlaceholderAccessError {
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    coalesce_absence: false,
                })?;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            normalize_substituted_value(inner, miss_policy)?;
        }
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
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
        ) => {}
    }
    Ok(())
}

/// Normalize an already-substituted unselected `??` fallback. Concrete
/// accesses may collapse, while missing accesses and surrounding functions
/// remain lazy until the fallback is selected.
fn normalize_substituted_lazily(value: &mut Value) -> Result<(), PlaceholderAccessError> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            **deferred = deferred.try_map(|branch| {
                let mut branch = branch.clone();
                normalize_substituted_lazily(&mut branch)?;
                Ok::<_, PlaceholderAccessError>(branch)
            })?;
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            for value in values {
                normalize_substituted_lazily(value).map_err(nested_placeholder_error)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for value in values.values_mut() {
                normalize_substituted_lazily(value).map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    normalize_substituted_lazily(value).map_err(nested_placeholder_error)?;
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                normalize_substituted_lazily(argument.walker_value_mut())
                    .map_err(nested_placeholder_error)?;
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access = access.clone();
            let mut base = access.base().clone();
            normalize_substituted_lazily(&mut base)?;
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
            normalize_substituted_lazily(left)?;
            normalize_substituted_lazily(default)?;
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let expression = expression.clone();
            let mut lhs = expression.lhs().clone();
            let mut rhs = expression.rhs().clone();
            normalize_substituted_lazily(&mut lhs).map_err(nested_placeholder_error)?;
            normalize_substituted_lazily(&mut rhs).map_err(nested_placeholder_error)?;
            *value = expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| PlaceholderAccessError {
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    coalesce_absence: false,
                })?;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            normalize_substituted_lazily(inner)?;
        }
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
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
        ) => {}
    }
    Ok(())
}

#[cfg(test)]
mod substitute_placeholder_tests {
    use super::*;
    use crate::resource::{
        AccessPath, ApplyTimeUnknown, ConcreteValue, DeferredValue, InterpolationPart,
        LiveCompositionSignature, ResourceId, Value,
    };
    use std::collections::BTreeSet;

    type LoopValue = Value<LoopSlot>;

    #[test]
    fn lazy_coalesce_for_substitution_rejects_non_integer_items_without_panicking() {
        for item in [
            Value::Concrete(ConcreteValue::String("not-an-int".into())),
            Value::Concrete(ConcreteValue::Bool(true)),
        ] {
            let arithmetic: LoopValue = crate::integer_arithmetic::evaluate_integer_values(
                Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
                crate::integer_arithmetic::IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
            )
            .expect("a for-value placeholder is admitted until substitution");
            let value: LoopValue = Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("source", "optional", Vec::new())),
                default: Box::new(arithmetic),
            });

            let error = substitute_placeholder(value, None, None, &item)
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
        let string = |value: &str| -> Value<ParserPlaceholder> {
            Value::Concrete(ConcreteValue::String(value.into()))
        };
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
            template_placeholder: _,
            deferred_module_ifs: _,
            expansion_trace: _,
        } = parsed;
    }

    #[test]
    fn expand_deferred_children_returns_shape_mismatch_for_simple_binding_with_map_iterable() {
        let mut template_resource = Resource::new("mock.Target", "children");
        template_resource.binding = Some("children".to_string());
        template_resource.set_attr(
            "name",
            Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        );
        let deferred = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.options".to_string(),
            DeferredForTarget::bound("children").expect("fixture binding is valid"),
            Value::resource_ref("cert", "options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );

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
    fn expand_deferred_children_consumes_loop_slot_template_values() {
        let mut template_resource: Resource<ModuleSource, LoopSlot> =
            Resource::new("mock.Target", "children");
        template_resource.binding = Some("children".to_string());
        template_resource.set_attr(
            "name",
            Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        );
        let deferred = DeferredForExpression::new(
            None,
            1,
            "for child in source.children".to_string(),
            DeferredForTarget::bound("children").expect("fixture binding is valid"),
            Value::resource_ref("source", "children", Vec::new()),
            ForBinding::Simple("child".to_string()),
            template_resource,
        );

        let children = expand_deferred_children(
            &deferred,
            &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("alpha".into()),
            )])),
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("the loop slot must be consumed while the child is materialized");

        assert_eq!(
            children[0].get_attr("name"),
            Some(&Value::Concrete(ConcreteValue::String("alpha".into())))
        );
    }

    #[test]
    fn deferred_list_and_map_expansion_produce_distinct_children() {
        let deferred = |binding| {
            DeferredForExpression::new(
                None,
                1,
                "for child in source.children".to_string(),
                DeferredForTarget::bound("children").expect("fixture binding is valid"),
                Value::resource_ref("source", "children", Vec::new()),
                binding,
                Resource::pending_with_provider("mock", "Target", None),
            )
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
        assert_eq!(
            list_deferred.template_resource_id().identity_str(),
            Some("children[?]")
        );
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
        assert_eq!(
            map_deferred.template_resource_id().identity_str(),
            Some("children[?]")
        );
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
        let parsed = parsed.into_expanded().expect("template must expand");
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
    fn deferred_for_expression_serde_omits_absolute_file_but_preserves_line() {
        let deferred: DeferredForExpression = DeferredForExpression::new(
            Some("/abs/path.crn".to_string()),
            42,
            "for opt in cert.options".to_string(),
            DeferredForTarget::bound("children").expect("fixture binding is valid"),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::resource_ref("cert", "options", Vec::new())),
                default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
            }),
            ForBinding::Simple("opt".to_string()),
            Resource::new("mock.Target", "children"),
        );

        let json = serde_json::to_string(&deferred).expect("serialize deferred for expression");
        assert!(
            !json.contains("/abs/path.crn"),
            "saved-plan JSON must not contain machine-local source paths: {json}"
        );

        let decoded = serde_json::from_str::<SavedDeferredForExpression>(&json)
            .expect("deserialize deferred for expression");
        assert_eq!(decoded.file, None);
        assert_eq!(decoded.line, 42);
        assert_eq!(decoded.iterable, deferred.iterable);
    }

    #[test]
    fn deferred_for_expression_serde_preserves_relative_diagnostic_location() {
        let deferred: DeferredForExpression = DeferredForExpression::new(
            Some("modules/child/main.crn".to_string()),
            17,
            "for opt in cert.options".to_string(),
            DeferredForTarget::bound("children").expect("fixture binding is valid"),
            Value::resource_ref("cert", "options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            Resource::new("mock.Target", "children"),
        );

        let json = serde_json::to_string(&deferred).expect("serialize deferred for expression");
        let decoded = serde_json::from_str::<SavedDeferredForExpression>(&json)
            .expect("deserialize deferred for expression");

        assert_eq!(decoded.file.as_deref(), Some("modules/child/main.crn"));
        assert_eq!(decoded.line, 17);
    }

    #[test]
    fn deferred_for_wire_requires_both_diagnostic_location_fields() {
        let deferred: DeferredForExpression = DeferredForExpression::new(
            None,
            0,
            "for opt in cert.options".into(),
            DeferredForTarget::bound("children").unwrap(),
            Value::resource_ref("cert", "options", Vec::new()),
            ForBinding::Simple("opt".into()),
            Resource::new("mock.Target", "children"),
        );
        let wire = serde_json::to_value(&deferred).unwrap();
        for field in ["file", "line"] {
            assert!(
                wire.get(field).is_some(),
                "required field {field} must be serialized even when empty: {wire}"
            );
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<SavedDeferredForExpression>(missing).is_err(),
                "missing {field} must be rejected"
            );
        }
    }

    #[test]
    fn deferred_for_wire_rejects_absolute_diagnostic_file() {
        let deferred: DeferredForExpression = DeferredForExpression::new(
            Some("main.crn".into()),
            1,
            "for opt in cert.options".into(),
            DeferredForTarget::bound("children").unwrap(),
            Value::resource_ref("cert", "options", Vec::new()),
            ForBinding::Simple("opt".into()),
            Resource::new("mock.Target", "children"),
        );
        let mut wire = serde_json::to_value(&deferred).unwrap();
        wire["file"] = serde_json::json!("/machine/local/main.crn");
        assert!(
            serde_json::from_value::<SavedDeferredForExpression>(wire).is_err(),
            "saved diagnostic files must be relative"
        );
    }

    #[test]
    fn unbound_deferred_for_round_trips_null_template_identity() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("unbound deferred loop parses");
        let parsed = parsed.into_expanded().expect("template must expand");
        let deferred = &parsed.deferred_for_expressions()[0];

        let json = serde_json::to_value(deferred).expect("serialize unbound deferred template");
        assert!(json["template_resource"]["id"]["identity"].is_null());
        let decoded = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect("deserialize unbound deferred template through Resource serde");
        assert!(matches!(
            decoded.target(),
            DeferredForTarget::Unbound { .. }
        ));
        assert_eq!(decoded.template_resource_id().identity(), None);
    }

    fn saved_bound_deferred_expression_fixture() -> DeferredForExpression {
        let mut resource: Resource<ModuleSource, LoopSlot> =
            Resource::pending_with_provider("mock", "test.resource", None);
        resource.set_attr(
            "name",
            Value::Concrete(ConcreteValue::String("good".into())),
        );
        DeferredForExpression::new(
            None,
            0,
            "for child in parent.children".to_string(),
            DeferredForTarget::bound("children").expect("valid bound target"),
            Value::resource_ref("parent", "children", Vec::new()),
            ForBinding::Simple("child".to_string()),
            resource,
        )
    }

    #[test]
    fn saved_deferred_for_rejects_top_level_resource_type_mismatch() {
        let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
            .expect("serialize deferred expression");
        json["resource_type"] = serde_json::json!("other.x");

        let error = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect_err("top-level resource type must match the template")
            .to_string();
        assert!(
            error.contains("Invalid saved plan") && error.contains("resource_type"),
            "unexpected validation error: {error}",
        );
    }

    #[test]
    fn saved_deferred_for_rejects_top_level_attributes() {
        let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
            .expect("serialize deferred expression");
        json["attributes"] = serde_json::json!([]);

        let error = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect_err("attributes must live only in the template resource")
            .to_string();
        assert!(error.contains("unknown field `attributes`"), "{error}");
    }

    #[test]
    fn saved_deferred_for_reports_template_provider_type_mismatch_first() {
        let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
            .expect("serialize deferred expression");
        json["template_resource"]["id"]["provider"] = serde_json::json!("other");

        let error = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect_err("template provider must match the expression resource type")
            .to_string();
        assert!(
            error.contains("Invalid saved plan")
                && error.contains("template resource")
                && error.contains("provider/type"),
            "unexpected validation error: {error}",
        );
    }

    #[test]
    fn deferred_template_resource_equality_covers_the_whole_resource() {
        let left = DeferredForTemplateResource::new(
            Resource::<ModuleSource, LoopSlot>::pending_with_provider(
                "mock",
                "test.resource",
                None,
            ),
        );
        let mut right = left.clone();
        right.resource.binding = Some("different-derived-binding".to_string());

        assert_ne!(
            left, right,
            "wrapper equality must delegate to the complete Resource value",
        );
    }

    #[test]
    fn parsed_deferred_for_rederives_template_identity_from_its_target() {
        let owning_target = DeferredForTarget::bound("right").expect("valid owning target");
        let mut resource = Resource::<ModuleSource, ParserPlaceholder>::with_provider(
            "mock",
            "test.resource",
            "wrong[?]",
            None,
        );
        resource.binding = Some("wrong[?]".to_string());
        let body = DeferredForBody::resource(resource);

        let parsed = ParsedDeferredForExpression::new(
            None,
            1,
            SourceSpan::dummy(),
            "for item in upstream.items".to_string(),
            owning_target,
            Value::resource_ref("upstream", "items", Vec::new()),
            ParsedDeferredForIterableState::Deferred,
            ForBinding::Simple("item".to_string()),
            body,
        );
        let DeferredForBody::Resource(template) = parsed.body() else {
            panic!("fixture body must remain a resource")
        };
        assert_eq!(template.id().identity_str(), Some("right[?]"));
        assert_eq!(template.binding(), Some("right[?]"));

        let expression = parsed
            .into_resource_expression(&[])
            .unwrap_or_else(|_| panic!("resource template must resolve"));
        assert_eq!(
            expression.template_resource_id().identity_str(),
            Some("right[?]")
        );
        assert_eq!(expression.template_resource_binding(), Some("right[?]"));
        assert_eq!(expression.deferred_effect_id().identity_str(), "right");
    }

    #[test]
    fn parsed_deferred_for_conversion_rederives_template_identity_from_its_target() {
        let target = DeferredForTarget::bound("right").expect("valid target");
        let body = DeferredForBody::resource(
            Resource::<ModuleSource, ParserPlaceholder>::pending_with_provider(
                "mock",
                "test.resource",
                None,
            ),
        );
        let mut parsed = ParsedDeferredForExpression::new(
            None,
            1,
            SourceSpan::dummy(),
            "for item in upstream.items".to_string(),
            target,
            Value::resource_ref("upstream", "items", Vec::new()),
            ParsedDeferredForIterableState::Deferred,
            ForBinding::Simple("item".to_string()),
            body,
        );

        let DeferredForBody::Resource(template) = &mut parsed.body else {
            panic!("fixture body must remain a resource")
        };
        template.resource.id =
            ResourceId::with_provider_identity("mock", "test.resource", "wrong[?]", None);
        template.resource.binding = Some("wrong[?]".to_string());

        let expression = parsed
            .into_resource_expression(&[])
            .unwrap_or_else(|_| panic!("resource template must resolve"));
        assert_eq!(
            expression.template_resource_id().identity_str(),
            Some("right[?]")
        );
        assert_eq!(expression.template_resource_binding(), Some("right[?]"));
        assert_eq!(expression.deferred_effect_id().identity_str(), "right");
    }

    #[test]
    fn parsed_deferred_for_preserves_an_unbound_template_pending_identity() {
        let target = DeferredForTarget::Unbound {
            template: UnboundLoopTemplate::new(
                crate::resource::LoopSource::authored(
                    std::path::Path::new("main.crn"),
                    SourceSpan::dummy(),
                    "for item in upstream.items",
                    crate::resource::LoopKind::For,
                ),
                crate::resource::LegacyLoopAddress::new("_for0"),
            ),
        };
        let template = DeferredForTemplateResource::new(
            Resource::<ModuleSource, ParserPlaceholder>::pending_with_provider(
                "mock",
                "test.resource",
                None,
            ),
        );
        let pending_id = template.id().clone();
        let parsed = ParsedDeferredForExpression::new(
            None,
            1,
            SourceSpan::dummy(),
            "for item in upstream.items".to_string(),
            target,
            Value::resource_ref("upstream", "items", Vec::new()),
            ParsedDeferredForIterableState::Deferred,
            ForBinding::Simple("item".to_string()),
            DeferredForBody::Resource(Box::new(template)),
        );
        let DeferredForBody::Resource(template) = parsed.body() else {
            panic!("fixture body must remain a resource")
        };
        assert_eq!(template.id(), &pending_id);

        let expression = parsed
            .into_resource_expression(&[])
            .unwrap_or_else(|_| panic!("resource template must resolve"));
        assert_eq!(expression.template_resource_id(), &pending_id);
    }

    #[test]
    fn saved_deferred_for_rejects_non_object_template_resource_without_panicking() {
        for malformed in [
            serde_json::json!(5),
            serde_json::json!([1]),
            serde_json::json!("x"),
        ] {
            let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
                .expect("serialize deferred expression");
            json["template_resource"] = malformed;

            let result = std::panic::catch_unwind(|| {
                serde_json::from_value::<SavedDeferredForExpression>(json)
            });
            let error = result
                .expect("malformed saved-plan input must return an error instead of panicking")
                .expect_err("a deferred template resource must be an object")
                .to_string();
            assert!(error.contains("invalid type"), "unexpected error: {error}");
        }
    }

    #[test]
    fn saved_deferred_for_rejects_non_object_template_resource_id_without_panicking() {
        for malformed in [
            serde_json::json!(5),
            serde_json::json!([1]),
            serde_json::json!("x"),
        ] {
            let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
                .expect("serialize deferred expression");
            json["template_resource"]["id"] = malformed;

            let result = std::panic::catch_unwind(|| {
                serde_json::from_value::<SavedDeferredForExpression>(json)
            });
            let error = result
                .expect("malformed saved-plan input must return an error instead of panicking")
                .expect_err("a deferred template resource id must be an object")
                .to_string();
            assert!(error.contains("invalid type"), "unexpected error: {error}");
        }
    }

    #[test]
    fn unbound_deferred_for_accepts_legacy_null_name_identity() {
        let parsed = crate::parser::parse(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.values {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            &crate::parser::ProviderContext::default(),
        )
        .expect("unbound deferred loop parses")
        .into_expanded()
        .expect("template must expand");
        let mut json = serde_json::to_value(&parsed.deferred_for_expressions()[0])
            .expect("serialize unbound deferred template");
        let id = json["template_resource"]["id"]
            .as_object_mut()
            .expect("template resource id");
        id.remove("identity");
        id.insert("name".to_string(), serde_json::Value::Null);

        let decoded = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect("legacy null name must retain its historical pending-identity meaning");
        assert_eq!(decoded.template_resource_id().identity(), None);
    }

    #[test]
    fn bound_deferred_for_accepts_legacy_null_name_identity() {
        let mut json = serde_json::to_value(saved_bound_deferred_expression_fixture())
            .expect("serialize bound deferred template");
        let id = json["template_resource"]["id"]
            .as_object_mut()
            .expect("template resource id");
        id.remove("identity");
        id.insert("name".to_string(), serde_json::Value::Null);

        let decoded = serde_json::from_value::<SavedDeferredForExpression>(json)
            .expect("legacy null name must derive the bound template identity from its target");
        assert_eq!(
            decoded.template_resource_id().identity_str(),
            Some("children[?]")
        );
    }

    #[test]
    fn bound_deferred_target_rejects_indexed_final_segment() {
        assert_eq!(
            BoundDeferredTarget::new("rs[?]"),
            Err(BoundDeferredTargetError::TrailingIndex),
        );
        assert_eq!(
            BoundDeferredTarget::new("rs[0]"),
            Err(BoundDeferredTargetError::TrailingIndex),
        );
        assert_eq!(
            BoundDeferredTarget::new("rs]"),
            Err(BoundDeferredTargetError::TrailingIndex),
        );
    }

    #[test]
    fn bound_deferred_target_accepts_index_in_earlier_segment() {
        let target = BoundDeferredTarget::new("ms[0].i_0123456789abcdef.rs")
            .expect("a nested binding may contain an index before its final segment");
        assert_eq!(target.binding(), "ms[0].i_0123456789abcdef.rs");
        assert_eq!(
            target.template_address().to_string(),
            "ms[0].i_0123456789abcdef.rs[?]"
        );
    }

    #[test]
    fn bound_deferred_target_rejects_unknown_saved_plan_fields() {
        let error = serde_json::from_str::<DeferredForTarget>(
            r#"{"kind":"bound","binding":"children","template":"x"}"#,
        )
        .expect_err("unknown bound target fields must not be ignored")
        .to_string();
        assert!(error.contains("unknown field `template`"), "{error}");
    }

    #[test]
    fn bound_deferred_for_wire_matches_resource_shape_byte_for_byte() {
        #[derive(serde::Serialize)]
        struct ResourceShapedWire<'a> {
            file: Option<&'a str>,
            line: usize,
            header: &'a str,
            resource_type: &'a str,
            target: &'a DeferredForTarget,
            iterable: &'a Value,
            binding: &'a ForBinding,
            template_resource: &'a Resource<ModuleSource, LoopSlot>,
        }

        let mut resource: Resource<ModuleSource, LoopSlot> =
            Resource::with_provider("mock", "Target", "legacy-independent-identity", None);
        resource.binding = Some("children[?]".to_string());
        resource.directives.set_prevent_destroy(true);
        resource.set_attr(
            "tags",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "Environment".to_string(),
                Value::Concrete(ConcreteValue::String("test".into())),
            )]))),
        );
        resource.default_tag_keys.insert("Environment".to_string());
        let source = crate::resource::LoopSource::authored(
            std::path::Path::new("resource-shaped.crn"),
            SourceSpan {
                start_byte: 0,
                end_byte: 5,
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 6,
            },
            "for x",
            crate::resource::LoopKind::For,
        );
        resource.loop_provenance =
            crate::resource::LoopProvenance::direct(crate::resource::LoopElementOrigin::new(
                crate::resource::LoopOriginKind::For {
                    element: crate::resource::ForLoopElement::Index(0),
                },
                source,
                crate::resource::LegacyLoopAddress::new("_for0[0]"),
            ));
        let expression = DeferredForExpression::new(
            Some("ignored.crn".to_string()),
            9,
            "for child in parent.children".to_string(),
            DeferredForTarget::bound("children").expect("fixture binding is non-empty"),
            Value::resource_ref("parent", "children", Vec::new()),
            ForBinding::Simple("child".to_string()),
            resource,
        );
        assert_eq!(
            expression.template_resource_id().identity_str(),
            Some("children[?]")
        );
        assert_eq!(expression.template_resource_binding(), Some("children[?]"));
        let resource_type = expression.resource_type();
        let resource_shaped = ResourceShapedWire {
            file: expression.file.as_deref(),
            line: expression.line,
            header: &expression.header,
            resource_type,
            target: expression.target(),
            iterable: &expression.iterable,
            binding: &expression.binding,
            template_resource: expression.template_resource().as_resource(),
        };

        assert_eq!(
            serde_json::to_vec(&expression).expect("serialize typed deferred expression"),
            serde_json::to_vec(&resource_shaped).expect("serialize resource-shaped expression"),
        );
        assert_eq!(
            serde_json::to_string(expression.target()).expect("serialize bound target"),
            r#"{"kind":"bound","binding":"children"}"#,
        );
    }

    #[test]
    fn bound_deferred_target_rejects_empty_binding_without_panicking() {
        let construction = std::panic::catch_unwind(|| BoundDeferredTarget::new(""));
        assert!(
            construction.is_ok(),
            "public construction must not panic for an empty runtime binding"
        );
        assert_eq!(
            construction.unwrap(),
            Err(BoundDeferredTargetError::InvalidIdentity(
                ResourceIdentityError::Empty,
            )),
        );
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
        let target = parsed.deferred_for_expressions()[0].target();
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
        let v = LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Value));
        let v = substitute_placeholder(
            v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("acct-1".into())),
        )
        .unwrap();
        assert_eq!(v, Value::Concrete(ConcreteValue::String("acct-1".into())));
    }

    fn for_value_field(name: &str) -> LoopValue {
        crate::resource::apply_postfix_access(
            LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
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
        let value = LoopValue::Deferred(DeferredValue::Coalesce {
            value: Box::new(for_value_field("name")),
            // This access would fail if the fallback were evaluated eagerly.
            default: Box::new(for_value_field("missing")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("present".into())),
        )])));

        let value = substitute_placeholder(value, None, None, &item).expect("substitute coalesce");
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("present".into())),
        );
    }

    #[test]
    fn coalesce_placeholder_uses_default_for_direct_missing_field() {
        let value = LoopValue::Deferred(DeferredValue::Coalesce {
            value: Box::new(for_value_field("optional")),
            default: Box::new(LoopValue::Concrete(ConcreteValue::String(
                "fallback".into(),
            ))),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::new()));

        let value = substitute_placeholder(value, None, None, &item).expect("substitute coalesce");
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
    }

    #[test]
    fn coalesce_placeholder_substitutes_default_when_left_remains_unknown() {
        let value = LoopValue::Deferred(DeferredValue::Coalesce {
            value: Box::new(LoopValue::resource_ref("up", "optional", Vec::new())),
            default: Box::new(for_value_field("name")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        )])));

        let value = substitute_placeholder(value, None, None, &item).expect("substitute coalesce");
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
        let value = LoopValue::Deferred(DeferredValue::Coalesce {
            value: Box::new(LoopValue::resource_ref("up", "optional", Vec::new())),
            default: Box::new(for_value_field("missing")),
        });
        let item = Value::Concrete(ConcreteValue::Map(IndexMap::new()));

        let value = substitute_placeholder(value, None, None, &item)
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
        let v = LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Key));
        let v = substitute_placeholder(
            v,
            None,
            Some("east"),
            &Value::Concrete(ConcreteValue::String("ignored".into())),
        )
        .unwrap();
        assert_eq!(v, Value::Concrete(ConcreteValue::String("east".into())));
    }

    #[test]
    fn replaces_for_index_with_integer_index() {
        let v = LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Index));
        let v = substitute_placeholder(
            v,
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
        let v = LoopValue::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
            path: path.clone(),
        }));
        let v = substitute_placeholder(
            v,
            Some(0),
            Some("k"),
            &Value::Concrete(ConcreteValue::String("anything".into())),
        )
        .unwrap();
        // `Value::Deferred(DeferredValue::Unknown)` never compares equal (see `impl PartialEq for Value`),
        // so destructure to verify the path survives.
        match v {
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: p,
            })) => {
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
        let v = LoopValue::Concrete(ConcreteValue::List(vec![
            LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
            LoopValue::Concrete(ConcreteValue::Map({
                let mut m = indexmap::IndexMap::new();
                m.insert(
                    "k".into(),
                    LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
                );
                m
            })),
            LoopValue::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
                LoopValue::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
            )])),
        ]));
        let v = substitute_placeholder(
            v,
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
        // Only a typed loop slot is substituted. A bare string that
        // happens to match the historical sentinel must be left alone.
        let v = LoopValue::Concrete(ConcreteValue::String("(known after upstream apply)".into()));
        let v = substitute_placeholder(
            v,
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
