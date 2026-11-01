//! Historical value trees. Hash leaves provide evidence, never evaluation input.

use super::*;
use std::convert::Infallible;

/// A leaf in a historical tree. The expression arm preserves real deferred
/// expressions and loop slots without treating a digest as pending work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SnapshotLeaf<P: PlaceholderKind = NoPlaceholder> {
    Expression(P),
    RedactedSecret { hash: RedactedSecretHash },
}

impl<P: PlaceholderKind> fmt::Display for SnapshotLeaf<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expression(p) => fmt::Display::fmt(p, f),
            Self::RedactedSecret { .. } => f.write_str("(secret)"),
        }
    }
}

impl<P: PlaceholderKind> ValueKind for SnapshotLeaf<P> {
    type DirectiveBool<B: BindingAddress> = P::DirectiveBool<B>;
    type DirectivePreventDestroy<B: BindingAddress> = P::DirectivePreventDestroy<B>;

    fn rebind_directives<B: BindingAddress, C: BindingAddress>(
        directives: Directives<B, Self>,
        resolve: &mut impl FnMut(B) -> C,
    ) -> Directives<C, Self> {
        snapshot_directives(P::rebind_directives(expression_directives(directives), resolve))
    }

    fn map_absolute_directive_values(
        directives: &mut Directives<AbsoluteBindingAddress, Self>,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let mut expression = expression_directives(std::mem::take(directives));
        P::map_absolute_directive_values(&mut expression, map);
        *directives = snapshot_directives(expression);
    }

    fn make_directive_sources_relative(directives: &mut Directives<AbsoluteBindingAddress, Self>, root: &Path) {
        let mut expression = expression_directives(std::mem::take(directives));
        P::make_directive_sources_relative(&mut expression, root);
        *directives = snapshot_directives(expression);
    }

    fn redacted_secret_hash(&self) -> Option<&RedactedSecretHash> {
        match self {
            Self::RedactedSecret { hash } => Some(hash),
            Self::Expression(_) => None,
        }
    }

    fn from_redacted_secret(hash: RedactedSecretHash) -> Result<Self, &'static str> {
        Ok(Self::RedactedSecret { hash })
    }

    fn loop_slot(&self) -> Option<LoopSlot> {
        match self {
            Self::Expression(p) => p.loop_slot(),
            Self::RedactedSecret { .. } => None,
        }
    }

    fn from_loop_slot(slot: LoopSlot) -> Result<Self, PlaceholderOutsideTemplate> {
        P::from_loop_slot(slot).map(Self::Expression)
    }

    fn parser_token_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::RedactedSecret { hash: a }, Self::RedactedSecret { hash: b }) => a == b,
            (Self::Expression(a), Self::Expression(b)) => a.parser_token_eq(b),
            _ => false,
        }
    }
}

pub(crate) fn snapshot_directives<B: BindingAddress, P: PlaceholderKind>(
    directives: Directives<B, P>,
) -> Directives<B, SnapshotLeaf<P>> {
    let Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance } = directives;
    Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance }
}

pub(crate) fn expression_directives<B: BindingAddress, P: PlaceholderKind>(
    directives: Directives<B, SnapshotLeaf<P>>,
) -> Directives<B, P> {
    let Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance } = directives;
    Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance }
}

/// A recursive snapshot with no resolution predicate. Only [`Self::to_value_at`]
/// can admit its leaves to an evaluable tree, after all hashes are rehydrated.
///
/// ```compile_fail
/// use carina_core::{resource::SnapshotValue,binding_index::ResolvedBindings,resolver::resolve_ref_value};
/// let snapshot: SnapshotValue = unimplemented!();
/// resolve_ref_value(&snapshot,&ResolvedBindings::default());
/// ```
///
/// ```compile_fail
/// use carina_core::{resource::SnapshotValue,parser::{ProviderContext,TypeExpr},validation::typecheck::{check_type_expr_value_providerless,TypecheckInference}};
/// let snapshot: SnapshotValue = unimplemented!();
/// check_type_expr_value_providerless(&TypeExpr::String,&snapshot,&ProviderContext::default(),TypecheckInference::Unavailable);
/// ```
///
/// ```compile_fail
/// use carina_core::{resource::{SnapshotValue,AccessMissPolicy},parser::{DeferredForExpression,expand_deferred_children}};
/// let snapshot: SnapshotValue = unimplemented!();
/// let template: DeferredForExpression = unimplemented!();
/// expand_deferred_children(&template,&snapshot,AccessMissPolicy::Error);
/// ```
///
/// ```compile_fail
/// use carina_core::resource::SnapshotValue;
/// let snapshot: SnapshotValue = unimplemented!();
/// snapshot.is_fully_resolved();
/// ```
pub type SnapshotValue<P = NoPlaceholder, B = AbsoluteBindingAddress> = Value<SnapshotLeaf<P>, B>;

/// An opaque historical secret was selected where its value is required.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("secret value unavailable at {path}")]
pub struct UnavailableSecretError {
    path: String,
}

impl UnavailableSecretError {
    pub fn path(&self) -> &str { &self.path }
}

impl<P: PlaceholderKind, B: BindingAddress> Value<P, B> {
    pub fn to_snapshot(&self) -> SnapshotValue<P, B> {
        match self.clone().try_map_placeholder::<SnapshotLeaf<P>, Infallible>(&mut |leaf| {
            Ok(Value::Deferred(DeferredValue::Placeholder(SnapshotLeaf::Expression(leaf))))
        }) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }
}

impl<P: PlaceholderKind, B: BindingAddress> From<Value<P, B>> for SnapshotValue<P, B> {
    fn from(value: Value<P, B>) -> Self { value.to_snapshot() }
}

impl<P: PlaceholderKind, B: BindingAddress> SnapshotValue<P, B> {
    pub fn redacted(hash: RedactedSecretHash) -> Self {
        Self::Deferred(DeferredValue::Placeholder(SnapshotLeaf::RedactedSecret { hash }))
    }

    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Concrete(ConcreteValue::Map(values)) => values.get(key),
            _ => None,
        }
    }

    pub fn secret_hash(&self) -> Option<&RedactedSecretHash> {
        match self {
            Self::Deferred(DeferredValue::Placeholder(leaf)) => leaf.redacted_secret_hash(),
            _ => None,
        }
    }

    pub fn contains_secret_hash(&self) -> bool { self.first_secret_path("$").is_some() }

    fn first_secret_path(&self, path: &str) -> Option<String> {
        if self.secret_hash().is_some() { return Some(path.to_owned()); }
        match self {
            Self::Concrete(ConcreteValue::Map(values)) => values.iter().find_map(|(key, value)| {
                value.first_secret_path(&format!("{path}.{key}"))
            }),
            Self::Concrete(ConcreteValue::List(values)) => values.iter().enumerate().find_map(|(i, value)| {
                value.first_secret_path(&format!("{path}[{i}]"))
            }),
            Self::Deferred(DeferredValue::Secret(value)) => value.first_secret_path(path),
            Self::Deferred(DeferredValue::Coalesce { value, default }) => value.first_secret_path(&format!("{path}.value"))
                .or_else(|| default.first_secret_path(&format!("{path}.default"))),
            Self::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().enumerate().find_map(|(i, part)| match part {
                InterpolationPart::Expr(value) => value.first_secret_path(&format!("{path}.interpolation[{i}]")),
                InterpolationPart::Literal(_) => None,
            }),
            Self::Deferred(DeferredValue::FunctionCall { args, .. }
                | DeferredValue::Unavailable { args, .. }
                | DeferredValue::ResolvedFunctionCall { args, .. }) => args.iter().enumerate().find_map(|(i, argument)| {
                    argument.walker_value().first_secret_path(&format!("{path}.args[{i}]"))
                }),
            Self::Deferred(DeferredValue::Access(access)) => access.base().first_secret_path(&format!("{path}.base")),
            Self::Deferred(DeferredValue::IntegerArithmetic(expr)) => expr.lhs().first_secret_path(&format!("{path}.lhs"))
                .or_else(|| expr.rhs().first_secret_path(&format!("{path}.rhs"))),
            Self::Deferred(DeferredValue::ModuleIf(deferred)) => deferred.values().into_iter().find_map(|value| value.first_secret_path(path)),
            _ => None,
        }
    }

    /// Convert only a snapshot whose root and recursive children are available.
    pub fn to_value_at(&self, path: impl AsRef<str>) -> Result<Value<P, B>, UnavailableSecretError> {
        let unavailable = self.first_secret_path(path.as_ref());
        self.clone().try_map_placeholder(&mut |leaf| match leaf {
            SnapshotLeaf::Expression(p) => Ok(Value::Deferred(DeferredValue::Placeholder(p))),
            SnapshotLeaf::RedactedSecret { .. } => Err(UnavailableSecretError {
                path: unavailable.clone().expect("hash traversal found this leaf"),
            }),
        })
    }
}

impl<P: PlaceholderKind> SnapshotValue<P> {
    /// The single verification decision shared by the differ and rehydration.
    pub fn verify_secret(&self, material: &Value<P>) -> Result<bool, crate::value::SerializationError> {
        match self.secret_hash() {
            Some(hash) => Ok(hash.verify(crate::value::canonical_secret_hash_material(material)?)),
            None => Ok(false),
        }
    }

}

/// The two value phases admitted by plan carriers. Template leaves retain the
/// matching loop phase; only the evaluable implementation enters execution.
pub trait PlanValueKind: StateValueLeaf + ValueKind<
    DirectiveBool<AbsoluteBindingAddress> = bool,
    DirectivePreventDestroy<AbsoluteBindingAddress> = PreventDestroy,
> + SerializableDirectivePhase<AbsoluteBindingAddress> + for<'de> Deserialize<'de> {
    type Template: ValueKind<
        DirectiveBool<AbsoluteBindingAddress> = LoopDirective<bool, AbsoluteBindingAddress>,
        DirectivePreventDestroy<AbsoluteBindingAddress> = LoopDirective<PreventDestroy, AbsoluteBindingAddress>,
    > + SerializableDirectivePhase<AbsoluteBindingAddress> + for<'de> Deserialize<'de>;
}
impl PlanValueKind for NoPlaceholder { type Template = LoopSlot; }
impl PlanValueKind for SnapshotLeaf<NoPlaceholder> { type Template = SnapshotLeaf<LoopSlot>; }

impl<S, P: ValueKind, B: BindingAddress, D> Resource<S, P, B, D> {
    pub(crate) fn with_value_stage<Q: ValueKind>(
        self,
        attributes: IndexMap<String, Value<Q, B>>,
        directives: Directives<B, Q>,
    ) -> Resource<S, Q, B, D> {
        let Resource { id, attributes: _, directives: _, resolved_ignore_changes,
            prefixes, binding, dependencies, module_source, default_tag_keys,
            loop_provenance, quoted_string_attrs } = self;
        Resource { id, attributes, directives, resolved_ignore_changes, prefixes,
            binding, dependencies, module_source, default_tag_keys, loop_provenance,
            quoted_string_attrs }
    }
}

impl<S: Clone, P: PlaceholderKind, B: BindingAddress, D: Clone> Resource<S, P, B, D> {
    pub fn to_snapshot(&self) -> Resource<S, SnapshotLeaf<P>, B, D> {
        self.clone().with_value_stage(
            self.attributes.iter().map(|(key, value)| (key.clone(), value.to_snapshot())).collect(),
            snapshot_directives(self.directives.clone()),
        )
    }
}

impl<S: Clone, P: PlaceholderKind, B: BindingAddress, D: Clone> Resource<S, SnapshotLeaf<P>, B, D> {
    pub fn to_value_resource(&self) -> Result<Resource<S, P, B, D>, UnavailableSecretError> {
        let attributes = self.attributes.iter().map(|(key, value)| {
            value.to_value_at(format!("{}.{}", self.id, key)).map(|value| (key.clone(), value))
        }).collect::<Result<_,_>>()?;
        Ok(self.clone().with_value_stage(attributes, expression_directives(self.directives.clone())))
    }
}

impl<S, P: ValueKind, B: BindingAddress, D> DataSource<S, P, B, D> {
    pub(crate) fn with_value_stage<Q: ValueKind>(
        self,
        attributes: IndexMap<String, Value<Q, B>>,
        directives: Directives<B, Q>,
    ) -> DataSource<S, Q, B, D> {
        let DataSource { id, attributes: _, directives: _, binding, dependencies,
            module_source, quoted_string_attrs } = self;
        DataSource { id, attributes, directives, binding, dependencies, module_source,
            quoted_string_attrs }
    }
}

impl<S: Clone, P: PlaceholderKind, B: BindingAddress, D: Clone> DataSource<S, P, B, D> {
    pub fn to_snapshot(&self) -> DataSource<S, SnapshotLeaf<P>, B, D> {
        self.clone().with_value_stage(
            self.attributes.iter().map(|(key, value)| (key.clone(), value.to_snapshot())).collect(),
            snapshot_directives(self.directives.clone()),
        )
    }
}

impl<S: Clone, P: PlaceholderKind, B: BindingAddress, D: Clone> DataSource<S, SnapshotLeaf<P>, B, D> {
    pub fn to_value_data_source(&self) -> Result<DataSource<S, P, B, D>, UnavailableSecretError> {
        let attributes = self.attributes.iter().map(|(key, value)| {
            value.to_value_at(format!("{}.{}", self.id, key)).map(|value| (key.clone(), value))
        }).collect::<Result<_,_>>()?;
        Ok(self.clone().with_value_stage(attributes, expression_directives(self.directives.clone())))
    }
}

mod state_leaf {
    pub trait Sealed {}
    impl Sealed for super::NoPlaceholder {}
    impl Sealed for super::SnapshotLeaf {}
}

/// Leaves admitted to persisted provider state encoders.
pub trait StateValueLeaf: ValueKind + state_leaf::Sealed {
    fn snapshot_leaf(self) -> SnapshotLeaf;
}
impl StateValueLeaf for NoPlaceholder {
    fn snapshot_leaf(self) -> SnapshotLeaf { match self {} }
}
impl StateValueLeaf for SnapshotLeaf {
    fn snapshot_leaf(self) -> SnapshotLeaf { self }
}

pub(crate) fn state_value_snapshot<P: StateValueLeaf>(value: &Value<P>) -> SnapshotValue {
    match value.clone().try_map_placeholder::<SnapshotLeaf, Infallible>(&mut |leaf| {
        Ok(Value::Deferred(DeferredValue::Placeholder(leaf.snapshot_leaf())))
    }) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

impl<P: ValueKind, B: BindingAddress> AccessExpr<P, B> {
    pub(crate) fn with_stage_base<Q: ValueKind>(&self, base: Value<Q, B>) -> AccessExpr<Q, B> {
        AccessExpr { base: Box::new(base), step: self.step.clone(), source_line: self.source_line,
            display_root: self.display_root.clone() }
    }
}


pub(crate) fn plan_directives<P: PlanValueKind, Q: PlanValueKind>(directives: Directives<AbsoluteBindingAddress, P>) -> Directives<AbsoluteBindingAddress, Q> {
    let Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance } = directives;
    Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance }
}

pub(crate) fn template_directives<P: PlanValueKind, Q: PlanValueKind>(directives: Directives<AbsoluteBindingAddress, P::Template>) -> Directives<AbsoluteBindingAddress, Q::Template> {
    let Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance } = directives;
    Directives { force_delete, create_before_destroy, prevent_destroy,
        depends_on_diagnostic_sources, ignore_changes, provider_instance }
}

/// An unavailable historical secret or an invalid selection in a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotSelectionError {
    #[error(transparent)]
    UnavailableSecret(#[from] UnavailableSecretError),
    #[error(transparent)]
    Access(#[from] ValueAccessError),
    #[error(transparent)]
    DeferredMiss(ValueAccessError),
}

impl<P: PlaceholderKind,B: BindingAddress> SnapshotValue<P,B> {
    /// Select before converting so unavailable siblings remain historical.
    pub fn select_path(&self, segments: &[PathSegment], path: &str, policy: AccessMissPolicy)
        -> Result<Value<P,B>,SnapshotSelectionError> {
        let mut selected = self.clone();
        for segment in segments {
            if selected.secret_hash().is_some() {
                return selected.to_value_at(path).map_err(Into::into);
            }
            selected = match apply_value_path_segment(selected, segment.clone(), 0, policy)? {
                ValuePathSegmentResult::Resolved(value) | ValuePathSegmentResult::Deferred(value) => value,
                ValuePathSegmentResult::DeferredMiss(error) => return Err(SnapshotSelectionError::DeferredMiss(error)),
            };
        }
        selected.to_value_at(path).map_err(Into::into)
    }
}

impl From<State<NoPlaceholder>> for SnapshotState {
    fn from(state: State<NoPlaceholder>) -> Self {
        let State { id, identifier, attributes, exists, dependency_bindings, partial_read } = state;
        Self { id, identifier, attributes: attributes.into_iter().map(|(key,value)| (key,value.to_snapshot())).collect(),
            exists, dependency_bindings, partial_read }
    }
}
