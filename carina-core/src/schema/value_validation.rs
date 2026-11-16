//! Local checks and provider dispatch share a value traversal and Union choice.

use super::diagnostic::PrimaryValueFinding;
use super::*;
use crate::parser::retained::{
    AuthoredCursor, AuthoredForm, AuthoredNavigation, AuthoredUse, CursorKind, EdgeRole,
    ExpansionInstanceId, NodeId, ProjectionGeneration, UseId,
};
use crate::resource::ValueKind;
use crate::resource::knowledge::{KnownView, ShapeView};
use crate::validation::admission::assignment_seam_error as seam_error;

mod references;
mod syntax_children;
use references::{
    OrdinaryCheck, ReferenceMemo, ReferencePolicy, SupplementalData, SupplementalFindings,
};
use syntax_children::{ChildStep, DeclineSink};

/// Scalar membership and union ranking share this rule. Widening admits the
/// original integer value; it does not replace it with a float.
pub(crate) enum ScalarKindFit {
    Exact,
    IntToFloat,
    Rejected,
}

pub(crate) fn scalar_kind_fit(
    expected: RefinedScalarKind,
    actual: RefinedScalarKind,
) -> ScalarKindFit {
    use RefinedScalarKind::{Float, Int, String};
    match (expected, actual) {
        (String, String) | (Int, Int) | (Float, Float) => ScalarKindFit::Exact,
        (Float, Int) => ScalarKindFit::IntToFloat,
        (String, Int | Float) | (Int, String | Float) | (Float, String) => ScalarKindFit::Rejected,
    }
}

impl ScalarKindFit {
    pub(super) fn exactness(self) -> Option<FitExactness> {
        match self {
            Self::Exact => Some(FitExactness::EXACT),
            Self::IntToFloat => Some(FitExactness::COERCIBLE),
            Self::Rejected => None,
        }
    }
}

pub(super) fn scalar_value_kind_fit<R: PatternForm, P: ValueKind, B: BindingAddress>(
    declaration: &PatternAttributeType<R>,
    value: ConcreteValueRef<'_, P, B>,
) -> Option<ScalarKindFit> {
    let expected = match declaration.kind() {
        AttrTypeKind::String { .. } => RefinedScalarKind::String,
        AttrTypeKind::Int { .. } => RefinedScalarKind::Int,
        AttrTypeKind::Float { .. } => RefinedScalarKind::Float,
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. }
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => return None,
    };
    let actual = match value {
        ConcreteValueRef::String(_) => RefinedScalarKind::String,
        ConcreteValueRef::Int(_) => RefinedScalarKind::Int,
        ConcreteValueRef::Float(_) => RefinedScalarKind::Float,
        ConcreteValueRef::EnumIdentifier(_)
        | ConcreteValueRef::CanonicalEnum(_)
        | ConcreteValueRef::Bool(_)
        | ConcreteValueRef::Duration(_)
        | ConcreteValueRef::List(_)
        | ConcreteValueRef::StringList(_)
        | ConcreteValueRef::Map(_) => return None,
    };
    Some(scalar_kind_fit(expected, actual))
}

#[cfg(test)]
mod cursor_allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    }
    struct CountingAllocator;
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    fn allocated() {
        let _ = COUNT.try_with(|count| {
            if let Some(current) = count.get() {
                count.set(Some(current + 1));
            }
        });
    }
    // This test-only allocator forwards every operation and its layout to
    // System. Only a scoped counter on the current test thread is added.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            allocated();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            allocated();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            allocated();
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
    pub(super) fn count(run: impl FnOnce()) -> usize {
        COUNT.set(Some(0));
        run();
        COUNT.replace(None).unwrap()
    }
}

#[cfg(test)]
pub(crate) fn cursor_step_allocations(cursor: AuthoredCursor<'_>) -> usize {
    let coverage = Default::default();
    let walk = AssignmentWalk {
        cursor,
        callbacks: WalkCallbacks::from(AssignmentCallbacks {
            ordinary: &|_, _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>, _, _| {
                Ok(Default::default())
            },
            authored: None,
        }),
        coverage: &coverage,
        claim: Claim::None,
    };
    // Warm the use interner; repeated transport of the same authored edge
    // must not allocate a new cursor or clone an owned module environment.
    std::hint::black_box(
        walk.step(|cursor| cursor.child(0, EdgeRole::ListItem))
            .unwrap(),
    );
    cursor_allocations::count(|| {
        for _ in 0..128 {
            std::hint::black_box(
                walk.step(|cursor| cursor.child(0, EdgeRole::ListItem))
                    .unwrap(),
            );
        }
    })
}

pub(crate) fn resolve_assignment_declaration<'a>(
    attr: &'a CompiledAttributeType,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
) -> Result<&'a CompiledAttributeType, TypeError> {
    resolve_refs_on_path(attr, defs, &mut Vec::new())
        .map(|resolved| resolved.attr)
        .map_err(|error| error.into_validation_error())
}

/// Assignment checks distinguish a current projection from authored syntax
/// discarded by selection. Discarded syntax never supplies a fabricated Value.
#[derive(Debug)]
pub(crate) enum CheckValue<'v, P: ValueKind, B: BindingAddress> {
    /// The live destination is known but its container has no value yet.
    Pending,
    Current(&'v Value<P, B>),
    /// Live authored syntax whose lowering belongs to its enclosing expression.
    /// No operand value is fabricated and ordinary assignment is disabled.
    Covered,
    Discarded,
}

impl<P: ValueKind, B: BindingAddress> Copy for CheckValue<'_, P, B> {}
impl<P: ValueKind, B: BindingAddress> Clone for CheckValue<'_, P, B> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Visits belong to use edges, even when several uses share a syntax node.
#[derive(Debug, Default)]
pub struct WalkCoverage {
    conversions: crate::ConversionChannel,
    subject: crate::validation::FindingSubject,
    visits: std::collections::HashSet<(NodeId, UseId, ExpansionInstanceId, ProjectionGeneration)>,
    work: usize,
    generations: std::collections::HashMap<(UseId, ExpansionInstanceId), ProjectionGeneration>,
    nodes: std::collections::HashSet<(NodeId, crate::parser::retained::ExpressionEnvironmentId)>,
    unavailable: std::collections::HashSet<crate::parser::retained::UnreachedSlot>,
}

impl WalkCoverage {
    pub(crate) fn record_conversions(&mut self, findings: crate::ConversionChannel) {
        self.conversions
            .append(findings.in_subject(self.subject.clone()));
    }
    pub(crate) fn take_conversions(&mut self) -> crate::ConversionChannel {
        std::mem::take(&mut self.conversions)
    }
    pub(crate) fn in_subject<T>(
        &mut self,
        subject: crate::validation::FindingSubject,
        walk: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = std::mem::replace(&mut self.subject, subject);
        let output = walk(self);
        self.subject = previous;
        output
    }
    fn bump(&mut self, identity: (UseId, ExpansionInstanceId), generation: ProjectionGeneration) {
        self.generations
            .entry(identity)
            .and_modify(|current| *current = current.latest(generation))
            .or_insert(generation);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.record_conversions(other.conversions);
        self.visits.extend(other.visits);
        self.nodes.extend(other.nodes);
        self.unavailable.extend(other.unavailable);
        self.work += other.work;
        #[allow(clippy::iter_over_hash_type)]
        // commutative: bump keeps a per-key maximum, so merge order cannot change the result
        for (identity, generation) in other.generations {
            self.bump(identity, generation);
        }
    }

    pub(crate) fn no_authored_evidence(
        &mut self,
        missing: &crate::parser::retained::NoAuthoredEvidence,
    ) {
        self.unavailable.extend(missing.slots().iter().cloned());
    }

    pub(crate) fn unreached_slots(
        &self,
    ) -> impl Iterator<Item = &crate::parser::retained::UnreachedSlot> {
        self.unavailable.iter().filter(|slot| {
            let generation = self
                .current_generation(slot.occurrence(), slot.instance())
                .map_or(slot.generation(), |current| {
                    current.latest(slot.generation())
                });
            !self.visits.contains(&(
                slot.node_id(),
                slot.occurrence(),
                slot.instance(),
                generation,
            ))
        })
    }

    pub(crate) fn record<B: BindingAddress>(&mut self, authored: &AuthoredUse<'_, B>) {
        self.work += 1;
        let generation = authored.node().generation();
        self.bump((authored.occurrence(), authored.instance()), generation);
        self.nodes.insert((
            authored.node().node_id(),
            authored.node().environment().id(),
        ));
        self.visits.insert((
            authored.node().node_id(),
            authored.occurrence(),
            authored.instance(),
            authored.node().generation(),
        ));
    }

    /// Admission can materialize a borrowed slot after the immutable syntax
    /// report was sealed. Retain its current generation, without allowing old
    /// coverage to reach a newer report's projection.
    pub(crate) fn current_generation(
        &self,
        usage: UseId,
        instance: ExpansionInstanceId,
    ) -> Option<ProjectionGeneration> {
        self.generations.get(&(usage, instance)).copied()
    }

    pub(crate) fn reached<B: BindingAddress>(&self, usage: &AuthoredUse<'_, B>) -> bool {
        self.visits.contains(&(
            usage.node().node_id(),
            usage.occurrence(),
            usage.instance(),
            usage.node().generation(),
        ))
    }
    pub(crate) fn has_node(
        &self,
        node: NodeId,
        environment: &crate::parser::retained::ExpressionInstance,
    ) -> bool {
        self.nodes.contains(&(node, environment.id()))
    }
    #[cfg(test)]
    pub(crate) fn work(&self) -> usize {
        self.work
    }

    pub fn len(&self) -> usize {
        self.visits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visits.is_empty()
    }
}

/// Proof that this exact declaration accepted the value, including its canonical
/// scalar spelling. Only the value validator can construct this proof.
pub struct LocallyValidatedCustomType<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: Value,
    canonical: crate::provider::CanonicalCustomTypeValue,
}

impl LocallyValidatedCustomType<'_> {
    pub fn declaration(&self) -> crate::parser::IdentifiedCustomTypeDeclaration<'_> {
        self.declaration
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn canonical(&self) -> &crate::provider::CanonicalCustomTypeValue {
        &self.canonical
    }
}

/// A provider bridge can consume only a locally validated, canonical leaf.
pub trait ValidatedCustomTypeLookup: Send + Sync {
    fn validate(&self, value: LocallyValidatedCustomType<'_>) -> Result<(), TypeError>;

    /// Preserve owner attribution when local canonicalization cannot construct
    /// a dispatch proof. This reports a failure without invoking the provider.
    fn canonicalization_error(
        &self,
        _declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
        error: TypeError,
    ) -> TypeError {
        error
    }
}

// Schema tests inject routing failures without needing a provider factory.
#[cfg(test)]
impl<F> ValidatedCustomTypeLookup for F
where
    F: Fn(&CompiledAttributeType, &Value) -> Result<(), TypeError> + Send + Sync,
{
    fn validate(&self, value: LocallyValidatedCustomType<'_>) -> Result<(), TypeError> {
        self(value.declaration.declared_type(), value.value())
    }
}

/// Outer structure recorded by the classified validation walk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ValueStructure {
    #[default]
    Scalar,
    List,
    Map,
    Protected,
    Pending,
    Failed,
}

fn value_structure<P: PlaceholderKind, B: BindingAddress, T>(
    value: &Classification<'_, P, B, T>,
) -> ValueStructure {
    match value {
        Classification::Known(value) => value.visit(
            |view| match view {
                KnownView::List(_) | KnownView::StringList(_) => ValueStructure::List,
                KnownView::Map(_) => ValueStructure::Map,
                KnownView::String(_)
                | KnownView::EnumIdentifier(_)
                | KnownView::CanonicalEnum(_)
                | KnownView::Int(_)
                | KnownView::Float(_)
                | KnownView::Bool(_)
                | KnownView::Duration(_) => ValueStructure::Scalar,
            },
            |_| ValueStructure::Protected,
        ),
        Classification::ShapeKnown(value) => value.visit(
            |view| match view {
                ShapeView::List(_) => ValueStructure::List,
                ShapeView::Map(_) => ValueStructure::Map,
            },
            |_| ValueStructure::Protected,
        ),
        Classification::Pending(_) => ValueStructure::Pending,
        Classification::Failed(_) => ValueStructure::Failed,
    }
}

/// The callback that produced a symbolic diagnostic, preserved independently
/// of its message, path and diagnostic protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolicFindingOrigin {
    Ordinary,
    AuthoredHook,
    RecoveredReference,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolicFinding {
    origin: SymbolicFindingOrigin,
    path: LeafPath,
    error: TypeError,
}
impl SymbolicFinding {
    fn ordinary(path: LeafPath, error: TypeError) -> Self {
        Self {
            origin: SymbolicFindingOrigin::Ordinary,
            path,
            error,
        }
    }
    fn authored_hook(path: LeafPath, error: TypeError) -> Self {
        Self {
            origin: SymbolicFindingOrigin::AuthoredHook,
            path,
            error,
        }
    }
    fn recovered_reference(path: LeafPath, error: TypeError) -> Self {
        Self {
            origin: SymbolicFindingOrigin::RecoveredReference,
            path,
            error,
        }
    }
    pub fn origin(&self) -> SymbolicFindingOrigin {
        self.origin
    }
    pub fn diagnostic(&self) -> (&LeafPath, &TypeError) {
        (&self.path, &self.error)
    }
    /// A reference inside secret uses the ordinary protected type diagnostic.
    /// Execution retains the masked reference origin in the underlying error.
    pub fn diagnostic_message(&self) -> String {
        use crate::resource::SecretDiagnosticError;
        use crate::validation::admission::AdmissionReferenceError;
        if let TypeError::Reference { error, .. } = &self.error {
            if self.origin == SymbolicFindingOrigin::RecoveredReference
                && matches!(self.path.1, super::LeafPathRendering::Redacted)
                && let SecretDiagnosticError::Public(AdmissionReferenceError::Computed(
                    crate::validation::DeferredAccessTypeError::TypeMismatch { types, .. },
                )) = error.as_ref()
            {
                return TypeError::TypeMismatch {
                    types: types.clone(),
                }
                .to_string();
            }
            return error.to_string();
        }
        self.error.to_string()
    }
    /// Existing symbolic checks retain main's attribute-wide ownership. A
    /// restored reference owns its exact error, preserving other value errors.
    pub fn covers_path(&self) -> bool {
        match self.origin {
            SymbolicFindingOrigin::Ordinary | SymbolicFindingOrigin::AuthoredHook => true,
            SymbolicFindingOrigin::RecoveredReference => false,
        }
    }
}
impl InsideSecret for SymbolicFinding {
    fn inside_secret(self) -> Self {
        Self {
            origin: self.origin,
            path: self.path.inside_secret(),
            error: self.error.inside_secret(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct OrdinaryAssignmentFindings {
    errors: Vec<(LeafPath, TypeError)>,
    #[cfg(test)]
    deferred: Vec<DeferredHook>,
    local: Vec<crate::validation::admission::LocalPath>,
    schema: crate::validation::admission::SchemaObligation,
}
impl InsideSecret for OrdinaryAssignmentFindings {
    fn inside_secret(self) -> Self {
        Self {
            local: self.local.inside_secret(),
            schema: self.schema,
            errors: self
                .errors
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            #[cfg(test)]
            deferred: self
                .deferred
                .into_iter()
                .map(|mut hook| {
                    hook.secret = true;
                    hook
                })
                .collect(),
        }
    }
}

/// Cached findings for one attribute. The primary error retains the CLI's
/// established nesting; paths retain the LSP's established element locations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValueValidationFindings {
    claim_effect: ClaimEffect,
    structure: ValueStructure,
    local_error: Option<PrimaryValueFinding>,
    additional_local_errors: Vec<PrimaryValueFinding>,
    local: Vec<(FieldPath, TypeError)>,
    pub(super) located: Vec<(LeafPath, TypeError)>,
    assignment: OrdinaryAssignmentFindings,
    pub(crate) symbolic: Vec<SymbolicFinding>,
    pub(super) provider: Vec<(FieldPath, TypeError)>,
}

impl ValueValidationFindings {
    pub(crate) fn rejected_by_claim(&self) -> bool {
        self.claim_effect == ClaimEffect::Rejected
    }

    pub(crate) fn retained_failure(mut self) -> Self {
        self.structure = ValueStructure::Failed;
        self
    }

    pub(crate) fn take_assignment(&mut self) -> AssignmentFindings {
        let obligations = std::mem::take(&mut self.assignment);
        AssignmentFindings {
            local: obligations.local,
            schema: obligations.schema,
            errors: obligations.errors,
            #[cfg(test)]
            deferred: obligations.deferred,
            ..Default::default()
        }
    }

    pub(crate) fn into_located(self) -> Vec<(LeafPath, TypeError)> {
        self.located
            .into_iter()
            .chain(
                self.provider
                    .into_iter()
                    .map(|(path, error)| (path.canonical(), error)),
            )
            .collect()
    }

    pub fn structure(&self) -> ValueStructure {
        self.structure
    }

    pub(super) fn into_local_result(self) -> Result<(), TypeError> {
        self.local_error
            .map_or(Ok(()), |finding| Err(finding.error))
    }

    pub fn into_local_findings(self) -> Vec<(FieldPath, TypeError)> {
        self.local
    }

    /// Retain the key target while keeping the established container display path.
    pub fn into_diagnostic_findings(self, root: &str) -> Vec<(FieldPath, SchemaDiagnostic)> {
        let Self { local, located, .. } = self;
        local
            .into_iter()
            .map(|(path, error)| {
                let precise = if super::diagnostic::is_unknown_key(&error) {
                    let prefix = path.canonical();
                    located.iter().find(|(target, cause)| {
                        cause == &error && target.segments().starts_with(prefix.segments())
                    })
                } else {
                    None
                };
                let diagnostic = match precise {
                    Some((target, _)) => SchemaDiagnostic::from_value(root, target.clone(), error),
                    None => SchemaDiagnostic::from_container(root, &path, error),
                };
                (path, diagnostic)
            })
            .collect()
    }

    pub fn symbolic_errors(&self) -> impl Iterator<Item = (&LeafPath, &TypeError)> {
        self.symbolic.iter().map(SymbolicFinding::diagnostic)
    }

    pub fn symbolic_findings(&self) -> &[SymbolicFinding] {
        &self.symbolic
    }

    pub fn local_findings(&self) -> &[(FieldPath, TypeError)] {
        &self.local
    }

    pub fn local_error(&self) -> Option<&TypeError> {
        self.local_error.as_ref().map(|finding| &finding.error)
    }

    pub(crate) fn local_errors(&self) -> impl Iterator<Item = &TypeError> {
        self.primary_findings().map(|finding| &finding.error)
    }

    pub(super) fn primary_findings(&self) -> impl Iterator<Item = &PrimaryValueFinding> {
        self.local_error.iter().chain(&self.additional_local_errors)
    }

    #[cfg(test)]
    pub(super) fn into_errors(self) -> Vec<TypeError> {
        self.local_error
            .into_iter()
            .chain(self.additional_local_errors)
            .map(|finding| finding.error)
            .chain(self.provider.into_iter().map(|(_, error)| error))
            .collect()
    }
}

impl InsideSecret for ValueValidationFindings {
    fn inside_secret(self) -> Self {
        Self {
            claim_effect: self.claim_effect,
            structure: ValueStructure::Protected,
            local_error: self.local_error.map(InsideSecret::inside_secret),
            additional_local_errors: self.additional_local_errors.inside_secret(),
            local: self.local.inside_secret(),
            located: self
                .located
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            assignment: self.assignment.inside_secret(),
            symbolic: self.symbolic.inside_secret(),
            provider: self.provider.inside_secret(),
        }
    }
}

/// One validation report, usable by both CLI and LSP without revalidation.
#[derive(Debug, Default)]
pub struct ResourceValueValidation {
    pub(super) errors: Vec<SchemaDiagnostic>,
    raw_errors: OnceLock<Vec<TypeError>>,
    pub(super) attributes: indexmap::IndexMap<String, ValueValidationFindings>,
}

/// CLI preserves main's path ownership. Exact finding ownership is reserved
/// for diagnostics produced by the authored hook, never ordinary checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolicDiagnosticCoverage {
    Unowned,
    Path,
    Finding,
}
impl SymbolicDiagnosticCoverage {
    pub fn owns_path(self) -> bool {
        !matches!(self, Self::Unowned)
    }
    pub fn owns_finding(self) -> bool {
        matches!(self, Self::Finding)
    }
}

impl ResourceValueValidation {
    pub(crate) fn from_errors(errors: Vec<TypeError>) -> Self {
        Self {
            errors: errors
                .into_iter()
                .map(SchemaDiagnostic::unlocated)
                .collect(),
            ..Default::default()
        }
    }
    pub(crate) fn error_slice(&self) -> &[TypeError] {
        self.raw_errors
            .get_or_init(|| self.errors().cloned().collect())
    }

    /// Match report errors to their symbolic owner before consuming attributes.
    /// Diagnostic adapters share this index while retaining their rendering rules.
    pub fn diagnostic_errors(
        &self,
    ) -> impl Iterator<Item = (&SchemaDiagnostic, SymbolicDiagnosticCoverage)> {
        let mut symbolic = HashMap::<_, Vec<_>>::new();
        for (name, findings) in &self.attributes {
            for finding in &findings.symbolic {
                symbolic
                    .entry((name.as_str(), &finding.path))
                    .or_default()
                    .push(finding);
            }
        }
        self.errors.iter().map(move |diagnostic| {
            // Root unknowns were unowned before they gained an explicit key target.
            // Nested findings retain the established symbolic path coverage.
            if matches!(diagnostic.error(), TypeError::UnknownAttribute { .. }) {
                return (diagnostic, SymbolicDiagnosticCoverage::Unowned);
            }
            let error = diagnostic.error();
            let coverage = diagnostic
                .path()
                .and_then(|(name, path)| {
                    symbolic.get(&(name, path)).map(|findings| {
                        if findings.iter().any(|finding| {
                            matches!(
                                finding.origin,
                                SymbolicFindingOrigin::AuthoredHook
                                    | SymbolicFindingOrigin::RecoveredReference
                            ) && (error == &finding.error
                                || *error == finding.error.clone().with_attribute(name))
                        }) {
                            SymbolicDiagnosticCoverage::Finding
                        } else if findings.iter().any(|finding| finding.covers_path()) {
                            SymbolicDiagnosticCoverage::Path
                        } else {
                            SymbolicDiagnosticCoverage::Unowned
                        }
                    })
                })
                .unwrap_or(SymbolicDiagnosticCoverage::Unowned);
            (diagnostic, coverage)
        })
    }

    pub(crate) fn push_attribute_error(&mut self, name: &str, error: TypeError) {
        self.raw_errors.take();
        self.errors.push(SchemaDiagnostic::from_value(
            name,
            LeafPath::from_segments([]),
            error.with_attribute(name),
        ));
    }

    pub(super) fn append_values(&mut self, values: Self) {
        self.raw_errors.take();
        self.errors.extend(values.errors);
        self.attributes.extend(values.attributes);
    }

    /// Render a destination once, using the location retained by the shared
    /// walk. The original TypeError remains available through errors().
    pub fn diagnostic_error(&self, error: &TypeError) -> TypeError {
        let Some(attribute) = error.attribute_name() else {
            return error.clone();
        };
        let Some(findings) = self.attributes.get(attribute) else {
            return error.clone();
        };
        let path = if findings
            .local_error
            .as_ref()
            .is_some_and(|primary| primary.error.clone().with_attribute(attribute) == *error)
        {
            findings.located.first().map(|(path, _)| path.clone())
        } else {
            findings
                .provider
                .iter()
                .find(|(_, finding)| finding.clone().with_attribute(attribute) == *error)
                .map(|(path, _)| path.canonical())
        };
        match path {
            // List and map wrappers already identify their elements and keep
            // the established top-level attribute label. Struct paths use the
            // canonical dotted destination from the same recorded location.
            Some(path)
                if !path.segments().is_empty()
                    && path.segments().iter().all(|segment| {
                        matches!(segment, super::LeafPathSegment::StructField(_))
                    }) =>
            {
                let path = LeafPath::from_segments(
                    std::iter::once(super::LeafPathSegment::StructField(attribute.into()))
                        .chain(path.segments().iter().cloned()),
                );
                error.clone().with_attribute(path.to_string())
            }
            Some(_) | None => error.clone(),
        }
    }

    pub fn is_retained_failure(&self, name: &str) -> bool {
        self.attributes
            .get(name)
            .is_some_and(|findings| findings.structure() == ValueStructure::Failed)
    }

    pub fn take_attribute(&mut self, name: &str) -> Option<ValueValidationFindings> {
        self.attributes.shift_remove(name)
    }

    pub fn symbolic_errors(&self) -> impl Iterator<Item = (&str, &LeafPath, &TypeError)> {
        self.attributes.iter().flat_map(|(name, findings)| {
            findings
                .symbolic
                .iter()
                .map(move |finding| (name.as_str(), &finding.path, &finding.error))
        })
    }

    pub fn errors(&self) -> impl Iterator<Item = &TypeError> {
        self.errors.iter().map(SchemaDiagnostic::error)
    }

    #[cfg(test)]
    pub(crate) fn into_errors(self) -> Vec<TypeError> {
        self.errors
            .into_iter()
            .map(|finding| finding.error().clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn into_result(self) -> Result<(), Vec<TypeError>> {
        let errors = self.into_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Acceptance and diagnostic ranking are separate: a locally rejected value
/// cannot carry a fit that an enclosing Union could mistake for acceptance.
#[derive(Clone)]
enum LocalOutcome<F> {
    Accepted(F),
    Rejected { primary: PrimaryValueFinding },
}

impl<F> LocalOutcome<F> {
    fn error(&self) -> Option<&TypeError> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected { primary } => Some(&primary.error),
        }
    }

    fn fit(&self) -> Option<&F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected { .. } => None,
        }
    }

    fn into_fit(self) -> Option<F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected { .. } => None,
        }
    }

    fn map<G>(self, f: impl FnOnce(F) -> G) -> LocalOutcome<G> {
        match self {
            Self::Accepted(fit) => LocalOutcome::Accepted(f(fit)),
            Self::Rejected { primary } => LocalOutcome::Rejected { primary },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct NoConversions;
pub(crate) trait FindingsChannel:
    Clone + Default + PartialEq + std::fmt::Debug + InsideSecret
{
    fn append(&mut self, other: Self);
    fn findings(&self) -> &[crate::ConversionError];
}
impl FindingsChannel for crate::ConversionChannel {
    fn append(&mut self, other: Self) {
        crate::ConversionChannel::append(self, other);
    }
    fn findings(&self) -> &[crate::ConversionError] {
        crate::ConversionChannel::findings(self)
    }
}
impl InsideSecret for NoConversions {
    fn inside_secret(self) -> Self {
        self
    }
}
impl FindingsChannel for NoConversions {
    fn append(&mut self, _other: Self) {}
    fn findings(&self) -> &[crate::ConversionError] {
        &[]
    }
}
#[derive(Clone, Default)]
pub(crate) struct NoAuthored;
#[derive(Clone, Default)]
pub(crate) struct WithAuthored;
pub(crate) struct PolicyPosition<'a, 'w, B: BindingAddress, M: WalkMode> {
    cursor: AuthoredCursor<'a, B>,
    claim: &'w mut M::Claim<'a, B>,
}
pub(crate) trait WalkMode: Clone + Default {
    type Channel: FindingsChannel + 'static;
    type Declines: Default;
    type Policy<'a, P: ValueKind + 'a, B: BindingAddress>: Copy;
    type Claim<'a, B: BindingAddress>: ClaimState<'a, B, Channel = Self::Channel>;
    fn shorten<'a: 'b, 'b, P: ValueKind + 'a, B: BindingAddress>(
        policy: Self::Policy<'a, P, B>,
    ) -> Self::Policy<'b, P, B>;
    fn reject_transport<B: BindingAddress>(
        cursor: &AuthoredCursor<'_, B>,
        path: &LeafPath,
    ) -> AssignmentFindings<Self::Channel>;
    fn decline_child<P: ValueKind, B: BindingAddress>(
        policy: Self::Policy<'_, P, B>,
        memo: &mut Self::Declines,
        child: &AuthoredUse<'_, B>,
        reason: &crate::parser::retained::conversion::DeclineReason,
        path: &LeafPath,
        secret: bool,
    ) -> AssignmentFindings<Self::Channel>;
    fn judge_position<'s, 'c, 'd, 'v, P: PlaceholderKind, B: BindingAddress>(
        callbacks: &WalkCallbacks<'c, P, B, Self>,
        declaration: &'d CompiledAttributeType,
        checked: CheckValue<'v, P, B>,
        path: &LeafPath,
        position: PolicyPosition<'s, '_, B, Self>,
    ) -> (
        AssignmentFindings<Self::Channel>,
        CheckedPosition<'s, 'c, 'd, 'v, P, B, Self>,
    ) {
        let cursor = position.cursor.clone();
        let (findings, decision) =
            Self::apply_policy(callbacks, declaration, checked, path, position);
        (
            findings,
            CheckedPosition {
                cursor,
                callbacks: callbacks.clone(),
                declaration,
                checked,
                path: path.clone(),
                decision,
            },
        )
    }
    fn apply_policy<'a, P: PlaceholderKind, B: BindingAddress>(
        callbacks: &WalkCallbacks<'_, P, B, Self>,
        declaration: &CompiledAttributeType,
        checked: CheckValue<'_, P, B>,
        path: &LeafPath,
        position: PolicyPosition<'a, '_, B, Self>,
    ) -> (AssignmentFindings<Self::Channel>, PositionDecision);
}
impl WalkMode for NoAuthored {
    type Channel = NoConversions;
    type Declines = ();
    type Policy<'a, P: ValueKind + 'a, B: BindingAddress> = ();
    type Claim<'a, B: BindingAddress> = NoClaim;
    fn shorten<'a: 'b, 'b, P: ValueKind + 'a, B: BindingAddress>(_: ()) {}
    fn reject_transport<B: BindingAddress>(
        _: &AuthoredCursor<'_, B>,
        path: &LeafPath,
    ) -> AssignmentFindings<NoConversions> {
        AssignmentFindings {
            errors: vec![(
                path.clone(),
                seam_error(crate::parser::retained::MissingEvidence::new(
                    "ordinary syntax descent",
                )),
            )],
            ..Default::default()
        }
    }
    fn decline_child<P: ValueKind, B: BindingAddress>(
        _policy: (),
        _memo: &mut (),
        child: &AuthoredUse<'_, B>,
        _reason: &crate::parser::retained::conversion::DeclineReason,
        path: &LeafPath,
        _secret: bool,
    ) -> AssignmentFindings<NoConversions> {
        match child.node().judgement().no_conversion() {
            Some(_proof) => Default::default(),
            None => Self::reject_transport(&AuthoredCursor::authored(child.clone()), path),
        }
    }
    fn apply_policy<'a, P: PlaceholderKind, B: BindingAddress>(
        _callbacks: &WalkCallbacks<'_, P, B, Self>,
        _declaration: &CompiledAttributeType,
        _checked: CheckValue<'_, P, B>,
        _path: &LeafPath,
        position: PolicyPosition<'a, '_, B, Self>,
    ) -> (AssignmentFindings<NoConversions>, PositionDecision) {
        let decision = match position.cursor.no_conversion() {
            Some(proof) => PositionDecision::ordinary(proof),
            None => PositionDecision::invalid(seam_error(
                crate::parser::retained::MissingEvidence::new("ordinary projected proof"),
            )),
        };
        (Default::default(), decision)
    }
}
impl WalkMode for WithAuthored {
    type Channel = crate::ConversionChannel;
    type Declines = crate::parser::retained::conversion::DeclinedConversions;
    type Policy<'a, P: ValueKind + 'a, B: BindingAddress> = AuthoredChecks<'a, P, B>;
    type Claim<'a, B: BindingAddress> = Claim<'a, B>;
    fn shorten<'a: 'b, 'b, P: ValueKind + 'a, B: BindingAddress>(
        policy: AuthoredChecks<'a, P, B>,
    ) -> AuthoredChecks<'b, P, B> {
        policy
    }
    fn reject_transport<B: BindingAddress>(
        cursor: &AuthoredCursor<'_, B>,
        path: &LeafPath,
    ) -> AssignmentFindings {
        let verdict = match crate::parser::retained::ConversionVerdict::proof_mismatch(cursor) {
            Ok(verdict) => verdict,
            Err(error) => {
                crate::parser::retained::ConversionVerdict::seam_rejection(path.clone(), error)
            }
        };
        verdict.decide(
            |findings, _| findings,
            |mut findings| {
                findings.claim_effect = ClaimEffect::Rejected;
                findings
            },
        )
    }
    fn decline_child<P: ValueKind, B: BindingAddress>(
        policy: AuthoredChecks<'_, P, B>,
        memo: &mut Self::Declines,
        child: &AuthoredUse<'_, B>,
        reason: &crate::parser::retained::conversion::DeclineReason,
        path: &LeafPath,
        secret: bool,
    ) -> AssignmentFindings {
        match policy {
            AuthoredChecks::Validation(_) => AssignmentFindings {
                conversions: memo.reject(child.node().judgement(), reason, secret),
                ..Default::default()
            },
            // The sealed module preflight never admits values or publishes
            // verdicts for its separate parse. Its instance supplies the check.
            AuthoredChecks::Preflight(_) => Default::default(),
            AuthoredChecks::Projected(_) => {
                Self::reject_transport(&AuthoredCursor::authored(child.clone()), path)
            }
        }
    }
    fn apply_policy<'a, P: PlaceholderKind, B: BindingAddress>(
        callbacks: &WalkCallbacks<'_, P, B, Self>,
        declaration: &CompiledAttributeType,
        checked: CheckValue<'_, P, B>,
        path: &LeafPath,
        position: PolicyPosition<'a, '_, B, Self>,
    ) -> (AssignmentFindings, PositionDecision) {
        let PolicyPosition { cursor, claim } = position;
        let outcome = callbacks.policy(declaration, checked, path, cursor.clone());
        claim.record(outcome, cursor)
    }
}
pub(crate) trait ClaimState<'a, B: BindingAddress>: Clone + Default {
    type Channel: FindingsChannel;
    type Conversion;
    fn is_claimed(&self) -> bool;
    fn is_rejected(&self) -> bool;
    fn owner(&self) -> Option<&AuthoredCursor<'a, B>>;
    fn conversion(&self) -> Option<&Self::Conversion>;
    fn reject_destination(conversion: &Self::Conversion, cause: TypeError) -> Self::Channel;
}
#[derive(Clone, Default)]
pub(crate) struct NoClaim;
impl<'a, B: BindingAddress> ClaimState<'a, B> for NoClaim {
    type Channel = NoConversions;
    type Conversion = std::convert::Infallible;
    fn is_claimed(&self) -> bool {
        false
    }
    fn is_rejected(&self) -> bool {
        false
    }
    fn owner(&self) -> Option<&AuthoredCursor<'a, B>> {
        None
    }
    fn conversion(&self) -> Option<&Self::Conversion> {
        None
    }
    fn reject_destination(conversion: &Self::Conversion, _cause: TypeError) -> NoConversions {
        match *conversion {}
    }
}
impl<'a, B: BindingAddress> ClaimState<'a, B> for Claim<'a, B> {
    type Channel = crate::ConversionChannel;
    type Conversion = crate::parser::retained::conversion::AcceptedConversion;
    fn is_claimed(&self) -> bool {
        Claim::is_claimed(self)
    }
    fn is_rejected(&self) -> bool {
        Claim::is_rejected(self)
    }
    fn owner(&self) -> Option<&AuthoredCursor<'a, B>> {
        Claim::owner(self)
    }
    fn conversion(&self) -> Option<&Self::Conversion> {
        Claim::conversion(self)
    }
    fn reject_destination(conversion: &Self::Conversion, cause: TypeError) -> Self::Channel {
        std::iter::once(conversion.reject_destination(cause)).collect()
    }
}
type WalkHandle<'a, P, B, M> = AssignmentWalk<'a, P, B, M>;
type PreparedWalk<'a, P, B, M> = (
    AssignmentWalk<'a, P, B, M>,
    AssignmentProbe<<M as WalkMode>::Channel>,
    Vec<AuthoredCursor<'a, B>>,
);

#[derive(Clone, Default)]
struct CheckedFindings<M: WalkMode = NoAuthored> {
    local: Vec<(FieldPath, TypeError)>,
    pub(super) located: Vec<(FieldPath, TypeError)>,
    assignment: AssignmentFindings<M::Channel>,
    // Kept on the probe, separately from main's ordinary assignment work.
    authored: SupplementalFindings<M::Channel>,
    // Other undecided branches have independent primary local errors. Ordinary
    // containers still retain their established first-error CLI precedence.
    additional_local_errors: Vec<PrimaryValueFinding>,
    provider: Vec<(FieldPath, TypeError)>,
}

impl<M: WalkMode> CheckedFindings<M> {
    fn push_local(&mut self, path: FieldPath, error: TypeError) {
        self.located.push((path.clone(), error.clone()));
        self.local.push((path, error));
    }
}

impl<M: WalkMode> InsideSecret for CheckedFindings<M> {
    fn inside_secret(self) -> Self {
        Self {
            local: self.local.inside_secret(),
            located: self
                .located
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            assignment: self.assignment.inside_secret(),
            authored: self.authored.inside_secret(),
            additional_local_errors: self.additional_local_errors.inside_secret(),
            provider: self.provider.inside_secret(),
        }
    }
}

#[derive(Clone)]
struct WalkCheckedValue<'a, M: WalkMode = NoAuthored> {
    local: LocalOutcome<Fit<'a>>,
    findings: CheckedFindings<M>,
    selected: Option<&'a CompiledAttributeType>,
}

impl<M: WalkMode> InsideSecret for WalkCheckedValue<'_, M> {
    fn inside_secret(self) -> Self {
        Self {
            local: match self.local {
                LocalOutcome::Accepted(fit) => LocalOutcome::Accepted(fit.inside_secret()),
                LocalOutcome::Rejected { primary } => LocalOutcome::Rejected {
                    primary: primary.inside_secret(),
                },
            },
            findings: self.findings.inside_secret(),
            selected: self.selected,
        }
    }
}

struct UnionFailure<'a, M: WalkMode = NoAuthored> {
    local: LocalOutcome<UnionFit<'a>>,
    findings: CheckedFindings<M>,
    selected: Option<&'a CompiledAttributeType>,
}

struct UnionProviderFailure<'a, M: WalkMode = NoAuthored> {
    local_fit: UnionFit<'a>,
    provider: Vec<(FieldPath, TypeError)>,
    authored: SupplementalFindings<M::Channel>,
}

impl<'a, M: WalkMode> WalkCheckedValue<'a, M> {
    fn accepted(
        attr: &'a CompiledAttributeType,
        exactness: FitExactness,
        kind: FitKind<'a>,
    ) -> Self {
        Self {
            local: LocalOutcome::Accepted(Fit::with_kind(exactness, attr, kind)),
            findings: Default::default(),
            selected: Some(attr),
        }
    }

    fn rejected(path: &FieldPath, error: TypeError) -> Self {
        let primary = PrimaryValueFinding::new(path.clone(), error);
        Self {
            local: LocalOutcome::Rejected {
                primary: primary.clone(),
            },
            findings: CheckedFindings {
                located: vec![(path.clone(), primary.error.clone())],
                local: vec![(path.clone(), primary.error)],
                ..Default::default()
            },
            selected: None,
        }
    }

    fn accepts(&self) -> bool {
        matches!(self.local, LocalOutcome::Accepted(_)) && self.findings.provider.is_empty()
    }

    fn reject(&mut self, path: &FieldPath, error: TypeError) {
        self.reject_primary(PrimaryValueFinding::new(path.clone(), error));
    }

    fn reject_primary(&mut self, primary: PrimaryValueFinding) {
        if matches!(self.local, LocalOutcome::Accepted(_)) {
            self.local = LocalOutcome::Rejected { primary };
        }
    }

    fn extend_assignment(&mut self, path: &FieldPath, findings: AssignmentFindings<M::Channel>) {
        for (location, error) in &findings.errors {
            self.reject(path, error.clone());
            self.findings.located.push((
                FieldPath::from_canonical_segments(location.segments().iter().cloned()),
                error.clone(),
            ));
        }
        self.findings.assignment.extend(findings);
    }

    /// Container context applies equally to every undecided branch's CLI
    /// error. Path-tagged LSP findings keep their precise child locations.
    fn wrap_local_errors(mut self, wrap: impl Fn(TypeError) -> TypeError) -> Self {
        if let LocalOutcome::Rejected { primary } = &mut self.local {
            primary.error = wrap(primary.error.clone());
        }
        self.findings.additional_local_errors = self
            .findings
            .additional_local_errors
            .into_iter()
            .map(|mut finding| {
                finding.error = wrap(finding.error);
                finding
            })
            .collect();
        self
    }

    fn retain_primary_finding(&mut self) {
        if let LocalOutcome::Rejected { primary } = &self.local
            && self.findings.local.is_empty()
        {
            // Containers aggregate primary errors separately from their
            // precise child findings. Supply a fallback only when no child
            // or container constraint has already supplied a finding.
            self.findings
                .local
                .push((primary.path.clone(), primary.error.clone()));
        }
        if let LocalOutcome::Rejected { primary } = &self.local
            && self.findings.located.is_empty()
        {
            self.findings
                .located
                .push((primary.key_path().clone(), primary.error.clone()));
        }
    }

    fn first_error(&self) -> Option<&TypeError> {
        self.local
            .error()
            .or_else(|| self.findings.provider.first().map(|(_, error)| error))
    }

    fn into_parts(mut self) -> (ValueValidationFindings, M::Channel) {
        self.retain_primary_finding();
        let SupplementalData {
            authored,
            references,
        } = std::mem::take(&mut self.findings.authored).into_parts();
        let symbolic =
            self.findings
                .assignment
                .errors
                .iter()
                .map(|(path, error)| SymbolicFinding::ordinary(path.clone(), error.clone()))
                .chain(authored.errors.iter().map(|(path, error)| {
                    SymbolicFinding::authored_hook(path.clone(), error.clone())
                }))
                .collect();
        for (path, error) in authored.errors.clone() {
            let path = FieldPath::from_canonical_segments(path.segments().iter().cloned());
            if self.local.error().is_some() {
                self.findings
                    .additional_local_errors
                    .push(PrimaryValueFinding::new(path.clone(), error.clone()));
            }
            self.reject(&path, error.clone());
            self.findings.local.push((path.clone(), error.clone()));
            self.findings.located.push((path, error));
        }
        self.findings.assignment.extend(authored);
        let conversions = std::mem::take(&mut self.findings.assignment.conversions);
        let mut findings = ValueValidationFindings {
            claim_effect: self.findings.assignment.claim_effect,
            structure: ValueStructure::Scalar,
            local_error: match self.local {
                LocalOutcome::Accepted(_) => None,
                LocalOutcome::Rejected { primary } => Some(primary),
            },
            local: self.findings.local,
            located: self
                .findings
                .located
                .into_iter()
                .map(|(path, error)| (path.canonical(), error))
                .collect(),
            assignment: OrdinaryAssignmentFindings {
                local: self.findings.assignment.local,
                schema: self.findings.assignment.schema,
                errors: self.findings.assignment.errors,
                #[cfg(test)]
                deferred: self.findings.assignment.deferred,
            },
            symbolic,
            additional_local_errors: self.findings.additional_local_errors,
            provider: self.findings.provider,
        };
        references.attach(&mut findings);
        (findings, conversions)
    }
}

type OrdinaryCheckedValue<'a> = WalkCheckedValue<'a, NoAuthored>;
type CheckedValue<'a> = OrdinaryCheckedValue<'a>;
type AuthoredCheckedValue<'a> = WalkCheckedValue<'a, WithAuthored>;
impl OrdinaryCheckedValue<'_> {
    fn into_findings(self) -> ValueValidationFindings {
        let (findings, NoConversions) = self.into_parts();
        findings
    }
}
impl AuthoredCheckedValue<'_> {
    fn into_findings_with_coverage(self, coverage: &mut WalkCoverage) -> ValueValidationFindings {
        let (findings, conversions) = self.into_parts();
        coverage.record_conversions(conversions);
        findings
    }
}

#[derive(Clone, Copy)]
enum ValueValidationMode {
    Declaration,
    FinalProviderInput,
}

struct ValueValidator<'a, 'lookup, M: WalkMode = NoAuthored> {
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    lookup: Option<CustomTypeLookup<'lookup>>,
    scope: AssignmentScope,
    checked: std::cell::RefCell<HashMap<CheckedNode<'a>, WalkCheckedValue<'a, M>>>,
    reference_checked: ReferenceMemo<'a, M>,
    mode: ValueValidationMode,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CheckedNode<'a> {
    declaration: *const CompiledAttributeType,
    evidence: crate::resource::knowledge::KnownNodeId,
    ancestry: FieldAncestry,
    references: Vec<&'a str>,
    authored: Option<(
        NodeId,
        UseId,
        ExpansionInstanceId,
        ProjectionGeneration,
        bool,
    )>,
}

impl<'a> CheckedNode<'a> {
    #[inline(never)]
    fn new<P: PlaceholderKind, B: BindingAddress, M: WalkMode>(
        declaration: &'a CompiledAttributeType,
        evidence: crate::resource::knowledge::KnownNodeId,
        ancestry: FieldAncestry,
        references: &[&'a str],
        assignment: Option<&WalkHandle<'_, P, B, M>>,
    ) -> Self {
        Self {
            declaration,
            evidence,
            ancestry,
            references: references.to_vec(),
            authored: assignment.and_then(|walk| {
                walk.cursor
                    .identity()
                    .map(|(node, usage, instance, generation)| {
                        (
                            node,
                            usage,
                            instance,
                            generation,
                            matches!(&walk.cursor.kind, CursorKind::CoveredBy(_)),
                        )
                    })
            }),
        }
    }
}

impl<'a, 'lookup> ValueValidator<'a, 'lookup> {
    fn for_final_provider_input(
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        lookup: CustomTypeLookup<'lookup>,
    ) -> Self {
        Self {
            mode: ValueValidationMode::FinalProviderInput,
            ..Self::new(defs, Some(lookup))
        }
    }

    fn new(
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        lookup: Option<CustomTypeLookup<'lookup>>,
    ) -> Self {
        Self::with_mode(defs, lookup)
    }
}
impl<'a, 'lookup, M: WalkMode> ValueValidator<'a, 'lookup, M> {
    fn with_mode(
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        lookup: Option<CustomTypeLookup<'lookup>>,
    ) -> Self {
        Self {
            defs,
            lookup,
            scope: AssignmentScope::Full,
            checked: Default::default(),
            reference_checked: Default::default(),
            mode: ValueValidationMode::Declaration,
        }
    }

    fn with_scope(mut self, scope: AssignmentScope) -> Self {
        self.scope = scope;
        self
    }
}

pub(crate) fn untyped_symbolic_errors(errors: Vec<TypeError>) -> ValueValidationFindings {
    let mut findings = ValueValidationFindings::default();
    for error in errors {
        findings.symbolic.push(SymbolicFinding::ordinary(
            LeafPath::from_segments([]),
            error.clone(),
        ));
        findings.local.push((FieldPath::new(), error.clone()));
        findings
            .located
            .push((LeafPath::from_segments([]), error.clone()));
        if findings.local_error.is_none() {
            findings.local_error = Some(PrimaryValueFinding::new(FieldPath::new(), error));
        } else {
            findings
                .additional_local_errors
                .push(PrimaryValueFinding::new(FieldPath::new(), error));
        }
    }
    findings
}

pub(crate) fn classification_error(error: TypeError) -> ValueValidationFindings {
    CheckedValue::rejected(&FieldPath::new(), error).into_findings()
}
pub(crate) fn validate_value_locally<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: Option<CustomTypeLookup<'_>>,
) -> ValueValidationFindings {
    let mut findings = ValueValidator::new(defs, lookup)
        .walk(
            attr,
            value.into(),
            &FieldPath::new(),
            ancestry,
            &mut Vec::new(),
        )
        .into_findings();
    findings.structure = value_structure(value);
    findings
}

/// Consume the ordinary validator's selected fit, retaining the caller's
/// non-consuming reference path while comparing declaration-ordered members.
pub(crate) struct SelectedInputUnion<'a> {
    pub(crate) member: &'a CompiledAttributeType,
    pub(crate) fit: Fit<'a>,
}

pub(crate) fn select_input_union_fit_on_path<
    'a,
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    members: &'a [CompiledAttributeType],
    value: &Classification<'v, P, B, T>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    visited: &mut Vec<&'a str>,
    lookup: CustomTypeLookup<'_>,
) -> Option<SelectedInputUnion<'a>> {
    ValueValidator::new(defs, Some(lookup))
        .union_with_assignment(
            members,
            value.into(),
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            visited,
            None,
        )
        .ok()
        .map(|admitted| SelectedInputUnion {
            member: admitted.selected.member,
            fit: admitted.selected.fit,
        })
}

#[cfg(test)]
pub(crate) fn select_union_member_from_knowledge_on_path<
    'a,
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    members: &'a [CompiledAttributeType],
    value: &Classification<'v, P, B, T>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    visited: &mut Vec<&'a str>,
) -> Option<&'a CompiledAttributeType> {
    // This test-only helper exercises structural fitting, not stage input authority.
    ValueValidator::new(defs, None)
        .union_with_assignment(
            members,
            value.into(),
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            visited,
            None,
        )
        .ok()
        .map(|admitted| admitted.selected.member)
}

#[cfg(test)]
pub(super) fn select_union_member_on_path<'a>(
    members: &'a [CompiledAttributeType],
    value: &Value,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    visited: &mut Vec<&'a str>,
) -> Option<&'a CompiledAttributeType> {
    let knowledge = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .expect("evaluable union fixture");
    select_union_member_from_knowledge_on_path(members, &knowledge, defs, visited)
}

#[cfg(test)]
pub(crate) fn validate_value_with_lookup<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
) -> ValueValidationFindings {
    let mut findings = ValueValidator::new(defs, Some(lookup))
        .walk(
            attr,
            value.into(),
            &FieldPath::new(),
            ancestry,
            &mut Vec::new(),
        )
        .into_findings();
    findings.structure = value_structure(value);
    findings
}

/// Final provider values use the same declaration walk, including provider
/// callbacks for compact StringLists introduced by normalization. Authored and
/// historical StringLists retain their established validation policy.
/// The ordinary walk has an uninhabited callback type and a NoConversions store.
pub(crate) fn validate_final_provider_value<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
) -> ValueValidationFindings {
    ValueValidator::for_final_provider_input(defs, lookup)
        .walk(
            attr,
            value.into(),
            &FieldPath::new(),
            ancestry,
            &mut Vec::new(),
        )
        .into_findings()
}

/// Standalone declarations enter the same leaf validator as resource values.
/// The continuation receives the sealed proof and cannot be called on a failed
/// local check. This also gives annotations their local declaration guard.
pub(crate) fn validate_custom_type_leaf<'a, R>(
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: &KnownValue<'_>,
    validated: impl FnOnce(Option<LocallyValidatedCustomType<'a>>) -> R,
) -> Result<R, TypeError> {
    let validator = ValueValidator::new(empty_defs_for_schema_walks(), None);
    let checked = validator.walk(
        declaration.declared_type(),
        ClassifiedRef::<crate::resource::NoPlaceholder,crate::resource::AbsoluteBindingAddress>::Known(value),
        &FieldPath::new(),
        FieldAncestry::WRITABLE,
        &mut Vec::new(),
    );
    if let Some(error) = checked.first_error() {
        return Err(error.clone());
    }
    let proof = validator
        .canonical_leaf(declaration.declared_type(), value, checked.local.fit())
        .map_err(|failure| *failure.error)?;
    Ok(validated(proof))
}

/// Annotation declarations check the original scalar before canonicalization.
/// In particular, enumerated strings and numeric enum identifiers must retain
/// their local shape errors instead of being decoded as provider bridge text.
pub(crate) fn validate_custom_type_annotation_leaf<'a, R>(
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: &KnownValue<'_>,
    validated: impl FnOnce(Option<LocallyValidatedCustomType<'a>>) -> R,
) -> Result<R, TypeError> {
    declaration
        .declared_type()
        .validate_leaf_value(&value.to_value())?;
    let validator = ValueValidator::new(empty_defs_for_schema_walks(), None);
    let proof = validator
        .canonical_leaf(declaration.declared_type(), value, None)
        .map_err(|failure| *failure.error)?;
    Ok(validated(proof))
}

struct CanonicalLeafError<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    error: Box<TypeError>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ClaimEffect {
    #[default]
    None,
    Rejected,
}

/// An admission callback participates in this traversal and its Union choice.
/// It never classifies a value or walks children.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AssignmentFindings<C = crate::ConversionChannel> {
    pub(crate) claim_effect: ClaimEffect,
    pub(crate) conversions: C,
    pub(crate) local: Vec<crate::validation::admission::LocalPath>,
    pub(crate) schema: crate::validation::admission::SchemaObligation,
    pub(crate) errors: Vec<(LeafPath, TypeError)>,
    #[cfg(test)]
    pub(crate) deferred: Vec<DeferredHook>,
    // Consumed policy verdicts distinguish a matching member's source/value
    // rejection from an earlier member with the wrong destination identity.
    pub(crate) matched_destination: bool,
}
impl<C: FindingsChannel> AssignmentFindings<C> {
    pub(crate) fn extend(&mut self, other: Self) {
        self.conversions.append(other.conversions);
        self.local.extend(other.local);
        self.schema.extend(other.schema);
        self.errors.extend(other.errors);
        #[cfg(test)]
        self.deferred.extend(other.deferred);
        self.matched_destination |= other.matched_destination;
        if other.claim_effect == ClaimEffect::Rejected {
            self.claim_effect = ClaimEffect::Rejected;
        }
    }
}
impl<C: FindingsChannel> InsideSecret for AssignmentFindings<C> {
    fn inside_secret(self) -> Self {
        Self {
            conversions: self.conversions.inside_secret(),
            local: self
                .local
                .into_iter()
                .map(InsideSecret::inside_secret)
                .collect(),
            #[cfg(test)]
            deferred: self
                .deferred
                .into_iter()
                .map(|mut hook| {
                    hook.secret = true;
                    hook
                })
                .collect(),
            schema: self.schema,
            matched_destination: self.matched_destination,
            claim_effect: self.claim_effect,
            errors: self
                .errors
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
        }
    }
}
/// The destination, value path and definitions still belong to the existing
/// schema walk. Authored authority is mandatory and moves into each check.
pub(crate) type ProvenAssignmentCheck<'a, P, B, C = crate::ConversionChannel> =
    &'a dyn Fn(
        &CompiledAttributeType,
        CheckValue<'_, P, B>,
        &LeafPath,
        AuthoredCursor<'_, B>,
        crate::parser::retained::NoConversion,
    ) -> Result<AssignmentFindings<C>, TypeError>;

#[cfg(test)]
pub(crate) type AssignmentCheck<'a, P, B, C = crate::ConversionChannel> =
    &'a dyn Fn(
        &CompiledAttributeType,
        CheckValue<'_, P, B>,
        &LeafPath,
        AuthoredCursor<'_, B>,
    ) -> Result<AssignmentFindings<C>, TypeError>;

pub(crate) type ReferenceJudge<'a, B> = &'a dyn Fn(
    &CompiledAttributeType,
    &LeafPath,
    crate::parser::retained::SourceReference<'_, B>,
) -> Result<(), TypeError>;

pub(crate) struct ReferenceCheck<'a, B: BindingAddress> {
    pub(crate) schema_source: &'a dyn Fn(&B) -> bool,
    pub(crate) check: ReferenceJudge<'a, B>,
}

/// The caller must explicitly choose whether authored policy owns this position.
/// No default can turn a missed conversion into ordinary assignment.
#[derive(Debug)]
pub(crate) enum AuthoredOutcome<
    Judgement = crate::parser::retained::ConversionJudgement,
    Deferral = std::convert::Infallible,
    Channel = crate::ConversionChannel,
> {
    Continue(
        crate::parser::retained::NoConversion,
        AssignmentFindings<Channel>,
    ),
    Transport(crate::parser::retained::TransportJudgement),
    Claimed(Judgement),
    Deferred(Deferral),
}

// Only a consumed authored witness or an explicit hook error reaches this
// internal verdict. Callers cannot claim a position with empty findings.
enum PolicyOutcome {
    Continue(crate::parser::retained::NoConversion, AssignmentFindings),
    Transport(crate::parser::retained::conversion::TransportDescent),
    Claimed(crate::parser::retained::ConversionVerdict),
    Deferred(crate::parser::retained::conversion::TemplateDeferral),
}
// A check cannot infer permission from the absence of an ordinary proof.
// This token is produced by policy for this value position, before checking it.
pub(crate) struct PositionDecision(PositionDecisionKind);
enum PositionDecisionKind {
    Ordinary(crate::parser::retained::NoConversion),
    Claimed,
    Transport(crate::parser::retained::conversion::TransportDescent),
    Invalid(TypeError),
}
impl PositionDecision {
    fn ordinary(proof: crate::parser::retained::NoConversion) -> Self {
        Self(PositionDecisionKind::Ordinary(proof))
    }
    fn claimed() -> Self {
        Self(PositionDecisionKind::Claimed)
    }
    fn transport(descent: crate::parser::retained::conversion::TransportDescent) -> Self {
        Self(PositionDecisionKind::Transport(descent))
    }
    fn invalid(error: TypeError) -> Self {
        Self(PositionDecisionKind::Invalid(error))
    }
}
pub(crate) type AuthoredAssignmentCheck<'a, P, B> =
    &'a dyn Fn(
        &CompiledAttributeType,
        CheckValue<'_, P, B>,
        &LeafPath,
        AuthoredCursor<'_, B>,
    ) -> Result<AuthoredOutcome, TypeError>;

/// Operational phases prove that their input is projected. Validation always
/// supplies policy, including the explicit classified-input producer.
pub(crate) enum AuthoredChecks<'a, P: ValueKind, B: BindingAddress> {
    Validation(AuthoredAssignmentCheck<'a, P, B>),
    Preflight(crate::validation::admission::TemplatePreflight),
    Projected(crate::parser::retained::ProjectedPhase),
}
impl<P: ValueKind, B: BindingAddress> Copy for AuthoredChecks<'_, P, B> {}
impl<P: ValueKind, B: BindingAddress> Clone for AuthoredChecks<'_, P, B> {
    fn clone(&self) -> Self {
        *self
    }
}

/// LSP's unrendered sibling slots need conversion evidence without repeating
/// main's ordinary/provider work for those files. Both modes use this walk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentScope {
    Full,
    ClaimedValues,
}

pub(crate) struct RequiredAssignmentCallbacks<'a, P: ValueKind, B: BindingAddress> {
    pub(crate) ordinary: ProvenAssignmentCheck<'a, P, B>,
    pub(crate) authored: AuthoredChecks<'a, P, B>,
    pub(crate) scope: AssignmentScope,
}

// The unchanged seam tests exercise observer callbacks, including their
// selected-member call count. This adapter has no production admission entry.
#[cfg(test)]
pub(crate) struct AssignmentCallbacks<
    'a,
    P: ValueKind,
    B: BindingAddress,
    C = crate::ConversionChannel,
> {
    pub(crate) ordinary: AssignmentCheck<'a, P, B, C>,
    pub(crate) authored: Option<AssignmentCheck<'a, P, B, C>>,
}

#[cfg(test)]
impl<P: ValueKind, B: BindingAddress, C> Copy for AssignmentCallbacks<'_, P, B, C> {}
#[cfg(test)]
impl<P: ValueKind, B: BindingAddress, C> Clone for AssignmentCallbacks<'_, P, B, C> {
    fn clone(&self) -> Self {
        *self
    }
}

#[derive(Debug, Default)]
pub(crate) struct AssignmentProbe<C = crate::ConversionChannel> {
    pub(crate) ordinary: AssignmentFindings<C>,
    pub(crate) authored: AssignmentFindings<C>,
}

impl<C: FindingsChannel> AssignmentProbe<C> {
    fn extend(&mut self, other: Self) {
        self.ordinary.extend(other.ordinary);
        self.authored.extend(other.authored);
    }
}

/// Member probes collect calls, never execute policy. Only the selected
/// probe's tokens survive in CheckedValue; each token owns one FnOnce call.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DeferredHook {
    index: usize,
    secret: bool,
}

#[cfg(test)]
type HookCall<'a, C> = Box<dyn FnOnce() -> AssignmentFindings<C> + 'a>;
#[cfg(test)]
#[derive(Default)]
struct AuthoredHooks<'a, C> {
    calls: std::cell::RefCell<Vec<Option<HookCall<'a, C>>>>,
}
#[cfg(test)]
impl<'a, C: FindingsChannel> AuthoredHooks<'a, C> {
    fn record(&self, call: HookCall<'a, C>) -> AssignmentFindings<C> {
        let mut calls = self.calls.borrow_mut();
        let index = calls.len();
        calls.push(Some(call));
        AssignmentFindings {
            deferred: vec![DeferredHook {
                index,
                secret: false,
            }],
            ..Default::default()
        }
    }
    fn finish(&self, findings: &mut AssignmentFindings<C>) {
        for hook in std::mem::take(&mut findings.deferred) {
            let call = self.calls.borrow_mut()[hook.index].take();
            if let Some(call) = call {
                let result = call();
                findings.extend(if hook.secret {
                    result.inside_secret()
                } else {
                    result
                });
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum HookExecution {
    Immediate,
    UnionProbe,
}

#[cfg(test)]
#[derive(Clone)]
struct AuthoredHook<'a, P: ValueKind, B: BindingAddress, C> {
    callback: AssignmentCheck<'a, P, B, C>,
    hooks: std::rc::Rc<AuthoredHooks<'a, C>>,
    execution: HookExecution,
}
#[cfg(test)]
impl<'a, P: ValueKind, B: BindingAddress, C: FindingsChannel + 'a> AuthoredHook<'a, P, B, C> {
    fn record(
        &self,
        attr: &'a CompiledAttributeType,
        value: CheckValue<'a, P, B>,
        path: &LeafPath,
        cursor: AuthoredCursor<'a, B>,
    ) -> AssignmentFindings<C> {
        let callback = self.callback;
        let path = path.clone();
        let call = move || match callback(attr, value, &path, cursor) {
            Ok(findings) => findings,
            Err(error) => AssignmentFindings {
                errors: vec![(path, error)],
                ..Default::default()
            },
        };
        match self.execution {
            HookExecution::Immediate => call(),
            HookExecution::UnionProbe => self.hooks.record(Box::new(call)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct WalkCallbacks<
    'a,
    P: ValueKind + 'a,
    B: BindingAddress,
    M: WalkMode = WithAuthored,
> {
    ordinary: OrdinaryCheck<'a, P, B, M::Channel>,
    policy: M::Policy<'a, P, B>,
    scope: AssignmentScope,
    #[cfg(test)]
    authored: Option<AuthoredHook<'a, P, B, M::Channel>>,
}
impl<'a, P: ValueKind, B: BindingAddress, M: WalkMode> WalkCallbacks<'a, P, B, M> {
    // A top-level walk owns its observer queue. Rebuild only that test-only
    // state so callback borrows can shorten to the walk's local coverage.
    fn for_walk<'w>(self) -> WalkCallbacks<'w, P, B, M>
    where
        'a: 'w,
    {
        WalkCallbacks {
            ordinary: self.ordinary,
            policy: M::shorten(self.policy),
            scope: self.scope,
            #[cfg(test)]
            authored: self.authored.map(|hook| AuthoredHook {
                callback: hook.callback,
                hooks: Default::default(),
                execution: HookExecution::Immediate,
            }),
        }
    }
    #[cfg(test)]
    pub(crate) fn observe(
        mut self,
        observer: Option<AssignmentCheck<'a, P, B, M::Channel>>,
    ) -> Self {
        self.authored = observer.map(|callback| AuthoredHook {
            callback,
            hooks: Default::default(),
            execution: HookExecution::Immediate,
        });
        self
    }
    fn probing(&self) -> Self {
        #[cfg(test)]
        {
            let mut callbacks = self.clone();
            if let Some(authored) = &mut callbacks.authored {
                authored.execution = HookExecution::UnionProbe;
            }
            callbacks
        }
        #[cfg(not(test))]
        {
            self.clone()
        }
    }
    fn finish(&self, _findings: &mut AssignmentFindings<M::Channel>) {
        #[cfg(test)]
        if let Some(authored) = &self.authored
            && matches!(authored.execution, HookExecution::Immediate)
        {
            authored.hooks.finish(_findings);
        }
    }
}

impl<'a, P: ValueKind, B: BindingAddress> WalkCallbacks<'a, P, B> {
    fn policy(
        &self,
        attr: &CompiledAttributeType,
        value: CheckValue<'_, P, B>,
        path: &LeafPath,
        cursor: AuthoredCursor<'_, B>,
    ) -> PolicyOutcome {
        let rejected = |error| {
            PolicyOutcome::Claimed(crate::parser::retained::ConversionVerdict::seam_rejection(
                path.clone(),
                error,
            ))
        };
        match self.policy {
            AuthoredChecks::Validation(check) => match check(attr, value, path, cursor.clone()) {
                Ok(AuthoredOutcome::Continue(proof, findings)) => {
                    if cursor.no_conversion() == Some(proof) {
                        PolicyOutcome::Continue(proof, findings)
                    } else {
                        match crate::parser::retained::ConversionVerdict::proof_mismatch(&cursor) {
                            Ok(verdict) => PolicyOutcome::Claimed(verdict),
                            Err(error) => rejected(error),
                        }
                    }
                }
                Ok(AuthoredOutcome::Transport(witness)) => match witness.consume(&cursor) {
                    Ok(descent) => PolicyOutcome::Transport(descent),
                    Err(error) => PolicyOutcome::Claimed(
                        crate::parser::retained::ConversionVerdict::invalid_witness(error),
                    ),
                },
                Ok(AuthoredOutcome::Deferred(never)) => match never {},
                Ok(AuthoredOutcome::Claimed(witness)) => match witness.consume(attr, &cursor) {
                    Ok(verdict) => PolicyOutcome::Claimed(verdict),
                    Err(error) => PolicyOutcome::Claimed(
                        crate::parser::retained::ConversionVerdict::invalid_witness(error),
                    ),
                },
                Err(error) => rejected(error),
            },
            AuthoredChecks::Preflight(boundary) => {
                match crate::parser::retained::conversion::check_template(boundary, attr, &cursor) {
                    AuthoredOutcome::Continue(proof, findings) => {
                        if cursor.no_conversion() != Some(proof) {
                            return rejected(seam_error(
                                crate::parser::retained::MissingEvidence::new(
                                    "preflight ordinary proof/cursor pairing",
                                ),
                            ));
                        }
                        let AssignmentFindings {
                            claim_effect,
                            conversions: NoConversions,
                            local,
                            schema,
                            errors,
                            #[cfg(test)]
                            deferred,
                            matched_destination,
                        } = findings;
                        PolicyOutcome::Continue(
                            proof,
                            AssignmentFindings {
                                claim_effect,
                                conversions: Default::default(),
                                local,
                                schema,
                                errors,
                                #[cfg(test)]
                                deferred,
                                matched_destination,
                            },
                        )
                    }
                    AuthoredOutcome::Transport(witness) => match witness.consume(&cursor) {
                        Ok(descent) => PolicyOutcome::Transport(descent),
                        Err(error) => PolicyOutcome::Claimed(
                            crate::parser::retained::ConversionVerdict::invalid_witness(error),
                        ),
                    },
                    AuthoredOutcome::Claimed(never) => match never {},
                    AuthoredOutcome::Deferred(deferral) => PolicyOutcome::Deferred(deferral),
                }
            }
            AuthoredChecks::Projected(phase) => match cursor.kind {
                CursorKind::Projected(
                    crate::parser::retained::ProjectionAuthority::Operational(current),
                ) if current == phase => PolicyOutcome::Continue(
                    crate::parser::retained::NoConversion::operational(phase),
                    Default::default(),
                ),
                CursorKind::Projected(_) | CursorKind::Authored(_) | CursorKind::CoveredBy(_) => {
                    rejected(seam_error(crate::parser::retained::MissingEvidence::new(
                        "projected callback/cursor pairing",
                    )))
                }
            },
        }
    }
}

impl<'a, P: ValueKind, B: BindingAddress> From<RequiredAssignmentCallbacks<'a, P, B>>
    for WalkCallbacks<'a, P, B>
{
    fn from(callbacks: RequiredAssignmentCallbacks<'a, P, B>) -> Self {
        Self {
            ordinary: OrdinaryCheck::Direct(callbacks.ordinary),
            policy: callbacks.authored,
            scope: callbacks.scope,
            #[cfg(test)]
            authored: None,
        }
    }
}
#[cfg(test)]
impl<'a, P: ValueKind, B: BindingAddress> From<AssignmentCallbacks<'a, P, B>>
    for WalkCallbacks<'a, P, B>
{
    fn from(callbacks: AssignmentCallbacks<'a, P, B>) -> Self {
        Self {
            ordinary: OrdinaryCheck::Observer(callbacks.ordinary),
            policy: AuthoredChecks::Validation(&|_, _, _, cursor| {
                Ok(AuthoredOutcome::Continue(
                    cursor.no_conversion().expect("ordinary seam fixture"),
                    Default::default(),
                ))
            }),
            scope: AssignmentScope::Full,
            authored: None,
        }
        .observe(callbacks.authored)
    }
}

#[cfg(test)]
impl<'a, P: ValueKind, B: BindingAddress> WalkCallbacks<'a, P, B, NoAuthored> {
    fn new(callbacks: AssignmentCallbacks<'a, P, B, NoConversions>) -> Self {
        Self {
            ordinary: OrdinaryCheck::Observer(callbacks.ordinary),
            policy: (),
            scope: AssignmentScope::Full,
            authored: None,
        }
        .observe(callbacks.authored)
    }
}

pub(crate) struct AcceptedClaim<'a, B: BindingAddress> {
    cursor: AuthoredCursor<'a, B>,
    conversion: crate::parser::retained::conversion::AcceptedConversion,
}
#[derive(Clone, Default)]
pub(crate) enum Claim<'a, B: BindingAddress> {
    #[default]
    None,
    Accepted {
        owner: std::rc::Rc<AcceptedClaim<'a, B>>,
    },
    Rejected {
        owner: std::rc::Rc<AuthoredCursor<'a, B>>,
    },
    Deferred {
        owner: std::rc::Rc<AuthoredCursor<'a, B>>,
    },
}
impl<'a, B: BindingAddress> Claim<'a, B> {
    fn is_claimed(&self) -> bool {
        match self {
            Self::None => false,
            Self::Accepted { .. } | Self::Rejected { .. } | Self::Deferred { .. } => true,
        }
    }
    fn is_rejected(&self) -> bool {
        match self {
            Self::None | Self::Accepted { .. } | Self::Deferred { .. } => false,
            Self::Rejected { .. } => true,
        }
    }
    fn owner(&self) -> Option<&AuthoredCursor<'a, B>> {
        match self {
            Self::None => None,
            Self::Accepted { owner } => Some(&owner.cursor),
            Self::Rejected { owner } | Self::Deferred { owner } => Some(owner),
        }
    }
    fn conversion(&self) -> Option<&crate::parser::retained::conversion::AcceptedConversion> {
        match self {
            Self::Accepted { owner } => Some(&owner.conversion),
            Self::None | Self::Rejected { .. } | Self::Deferred { .. } => None,
        }
    }
    fn record(
        &mut self,
        outcome: PolicyOutcome,
        cursor: AuthoredCursor<'a, B>,
    ) -> (AssignmentFindings, PositionDecision) {
        match outcome {
            PolicyOutcome::Continue(proof, findings) => (
                findings,
                if self.is_claimed() {
                    PositionDecision::claimed()
                } else {
                    PositionDecision::ordinary(proof)
                },
            ),
            PolicyOutcome::Transport(descent) => (
                Default::default(),
                if self.is_claimed() {
                    PositionDecision::claimed()
                } else {
                    PositionDecision::transport(descent)
                },
            ),
            PolicyOutcome::Deferred(deferral) => {
                deferral.consume();
                if !self.is_rejected() {
                    *self = Self::Deferred {
                        owner: std::rc::Rc::new(cursor),
                    };
                }
                (Default::default(), PositionDecision::claimed())
            }
            PolicyOutcome::Claimed(verdict) => {
                let (claim, findings) = verdict.decide(
                    |findings, conversion| {
                        (
                            Self::Accepted {
                                owner: std::rc::Rc::new(AcceptedClaim {
                                    cursor: cursor.clone(),
                                    conversion,
                                }),
                            },
                            findings,
                        )
                    },
                    |mut findings| {
                        findings.claim_effect = ClaimEffect::Rejected;
                        (
                            Self::Rejected {
                                owner: std::rc::Rc::new(cursor.clone()),
                            },
                            findings,
                        )
                    },
                );
                // A child acceptance cannot revoke a wrapper's rejected claim.
                if !self.is_rejected() {
                    *self = claim;
                }
                (findings, PositionDecision::claimed())
            }
        }
    }
}

/// An ordinary check cannot be rebound to a different value position after
/// policy. Construction is private; check consumes the complete captured input.
pub(crate) struct CheckedPosition<
    's,
    'c,
    'd,
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    M: WalkMode,
> {
    cursor: AuthoredCursor<'s, B>,
    callbacks: WalkCallbacks<'c, P, B, M>,
    declaration: &'d CompiledAttributeType,
    checked: CheckValue<'v, P, B>,
    path: LeafPath,
    decision: PositionDecision,
}
impl<P: PlaceholderKind, B: BindingAddress, M: WalkMode> CheckedPosition<'_, '_, '_, '_, P, B, M> {
    fn check(self) -> Result<AssignmentFindings<M::Channel>, TypeError> {
        self.check_as(false)
    }
    fn check_selection(self) -> Result<AssignmentFindings<M::Channel>, TypeError> {
        self.check_as(true)
    }
    fn check_as(self, selection: bool) -> Result<AssignmentFindings<M::Channel>, TypeError> {
        match self.decision.0 {
            PositionDecisionKind::Ordinary(proof) => self.callbacks.ordinary.call(
                self.declaration,
                self.checked,
                &self.path,
                if selection {
                    self.cursor.selection()
                } else {
                    self.cursor
                },
                proof,
            ),
            PositionDecisionKind::Claimed => Ok(Default::default()),
            PositionDecisionKind::Transport(descent) => {
                descent.consume();
                Ok(Default::default())
            }
            PositionDecisionKind::Invalid(error) => Err(error),
        }
    }
}
enum AssignmentPosition<T> {
    SchemaOnly,
    Judged(T),
}
impl<'s, 'c, 'd, 'v, P: PlaceholderKind, B: BindingAddress, M: WalkMode>
    AssignmentPosition<CheckedPosition<'s, 'c, 'd, 'v, P, B, M>>
{
    fn check(self) -> Result<AssignmentFindings<M::Channel>, TypeError> {
        match self {
            Self::SchemaOnly => Ok(Default::default()),
            Self::Judged(position) => position.check(),
        }
    }
    fn check_selection(self) -> Result<AssignmentFindings<M::Channel>, TypeError> {
        match self {
            Self::SchemaOnly => Ok(Default::default()),
            Self::Judged(position) => position.check_selection(),
        }
    }
}

/// Cursor and callback travel together, including through union probes. A
/// structural schema-only walk has no assignment work; live admission always
/// constructs this complete input.
#[derive(Clone)]
struct AssignmentWalk<'a, P: PlaceholderKind, B: BindingAddress, M: WalkMode = WithAuthored> {
    cursor: AuthoredCursor<'a, B>,
    callbacks: WalkCallbacks<'a, P, B, M>,
    coverage: &'a std::cell::RefCell<WalkCoverage>,
    claim: M::Claim<'a, B>,
}

impl<'a, P: PlaceholderKind, B: BindingAddress, M: WalkMode> AssignmentWalk<'a, P, B, M> {
    fn with_cursor(&self, cursor: AuthoredCursor<'a, B>) -> Self {
        Self {
            cursor,
            ..self.clone()
        }
    }
    fn step(
        &self,
        advance: impl FnOnce(
            &AuthoredCursor<'a, B>,
        )
            -> Result<AuthoredCursor<'a, B>, crate::parser::retained::MissingEvidence>,
    ) -> Result<Self, TypeError> {
        Ok(Self {
            cursor: advance(&self.cursor).map_err(seam_error)?,
            claim: Default::default(),
            ..self.clone()
        })
    }
    fn apply_policy<'d, 'v>(
        &mut self,
        attr: &'d CompiledAttributeType,
        value: CheckValue<'v, P, B>,
        path: &FieldPath,
        findings: &mut AssignmentFindings<M::Channel>,
    ) -> CheckedPosition<'a, 'a, 'd, 'v, P, B, M> {
        let (policy, position) = M::judge_position(
            &self.callbacks,
            attr,
            value,
            &path.canonical(),
            PolicyPosition {
                cursor: self.cursor.clone(),
                claim: &mut self.claim,
            },
        );
        findings.extend(policy);
        position
    }

    fn authored(
        &self,
        _attr: &'a CompiledAttributeType,
        _value: CheckValue<'a, P, B>,
        _path: &FieldPath,
    ) -> AssignmentFindings<M::Channel> {
        if self.cursor.is_projection_only() {
            return Default::default();
        }
        #[cfg(test)]
        if let Some(hook) = &self.callbacks.authored {
            return hook.record(_attr, _value, &_path.canonical(), self.cursor.clone());
        }
        Default::default()
    }

    fn authored_parts(
        &self,
        attr: &'a CompiledAttributeType,
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> AssignmentProbe<M::Channel> {
        let mut findings = AssignmentProbe::default();
        if !self.cursor.is_projection_only()
            && let CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) = &self.cursor.kind
            && matches!(usage.navigation(), Ok(AuthoredNavigation::Parts))
        {
            let inspect_parts = usage.node().judgement().contains_any_conversion();
            #[cfg(test)]
            let inspect_parts = inspect_parts || self.callbacks.authored.is_some();
            if !inspect_parts {
                return findings;
            }
            for index in 0..usage.child_count() {
                let result = usage.part(index).map_err(seam_error).and_then(|child| {
                    visit_syntax(
                        attr,
                        defs,
                        path,
                        ancestry,
                        child,
                        self.callbacks.clone(),
                        &mut self.coverage.borrow_mut(),
                        SyntaxVisit::Covered,
                    )
                });
                match result {
                    Ok(probe) => findings.extend(probe),
                    Err(error) => findings.ordinary.errors.push((path.canonical(), error)),
                }
            }
        }
        findings
    }

    fn prepare<'v: 'a, T: StructuralPending<'v, P, B>>(
        mut self,
        attr: &'a CompiledAttributeType,
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> Result<PreparedWalk<'a, P, B, M>, TypeError> {
        let mut findings = AssignmentProbe::default();
        #[cfg(test)]
        let mut wrappers = Vec::new();
        #[cfg(not(test))]
        let wrappers = Vec::new();
        let mut followed = std::collections::HashSet::new();
        while let CursorKind::Authored(usage) = self.cursor.kind.clone() {
            self.coverage.borrow_mut().record(&usage);
            if !followed.insert((usage.node().node_id(), usage.occurrence())) {
                return Err(seam_error(crate::parser::retained::MissingEvidence::new(
                    "cyclic authored use",
                )));
            }
            self.cursor = match usage.navigation().map_err(seam_error)? {
                AuthoredNavigation::Forward { child, authored } => {
                    if authored {
                        self.apply_policy(
                            attr,
                            CheckValue::Current(value.diagnostic_value()),
                            path,
                            &mut findings.authored,
                        );
                    }
                    #[cfg(test)]
                    if authored && self.callbacks.authored.is_some() {
                        wrappers.push(self.cursor.clone());
                    }
                    AuthoredCursor::authored(child)
                }
                AuthoredNavigation::Covered | AuthoredNavigation::Parts => self.cursor.covered(),
                AuthoredNavigation::Alternatives { first, .. }
                    if usage.form() == AuthoredForm::If && value.branches().is_none() =>
                {
                    let decision = self.apply_policy(
                        attr,
                        CheckValue::Current(value.diagnostic_value()),
                        path,
                        &mut findings.authored,
                    );
                    findings.ordinary.extend(decision.check_selection()?);
                    #[cfg(test)]
                    if self.callbacks.authored.is_some() {
                        wrappers.push(self.cursor.selection());
                    }
                    let selected = usize::from(!usage.known_condition().map_err(seam_error)?);
                    let discarded = 1 - selected;
                    if first + discarded < usage.child_count() {
                        findings.extend(visit_discarded(
                            attr,
                            defs,
                            path,
                            ancestry,
                            usage.alternative(discarded).map_err(seam_error)?,
                            self.callbacks.clone(),
                            &mut self.coverage.borrow_mut(),
                        )?);
                    }
                    if first + selected == usage.child_count() {
                        // Main's empty projection for a false if without else.
                        self.cursor.covered_projection()
                    } else {
                        AuthoredCursor::authored(usage.alternative(selected).map_err(seam_error)?)
                    }
                }
                AuthoredNavigation::Alternatives { .. }
                    if usage.form() == AuthoredForm::Coalesce && value.coalesce().is_none() =>
                {
                    let decision = self.apply_policy(
                        attr,
                        CheckValue::Current(value.diagnostic_value()),
                        path,
                        &mut findings.authored,
                    );
                    findings.ordinary.extend(decision.check_selection()?);
                    #[cfg(test)]
                    if self.callbacks.authored.is_some() {
                        wrappers.push(self.cursor.selection());
                    }
                    findings.extend(visit_discarded(
                        attr,
                        defs,
                        path,
                        ancestry,
                        usage.alternative(1).map_err(seam_error)?,
                        self.callbacks.clone(),
                        &mut self.coverage.borrow_mut(),
                    )?);
                    AuthoredCursor::authored(usage.alternative(0).map_err(seam_error)?)
                }
                AuthoredNavigation::Secret(_)
                | AuthoredNavigation::Alternatives { .. }
                | AuthoredNavigation::List
                | AuthoredNavigation::Map
                | AuthoredNavigation::Leaf => break,
            };
        }
        Ok((self, findings, wrappers))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_authored_value_for_admission<
    's,
    'v,
    'c,
    P: PlaceholderKind + 'c,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
    cursor: AuthoredCursor<'s, B>,
    callbacks: impl Into<WalkCallbacks<'c, P, B>>,
    coverage: &mut WalkCoverage,
) -> ValueValidationFindings {
    validate_authored_value_with_callbacks(
        attr, value, defs, ancestry, lookup, cursor, callbacks, None, coverage,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_authored_value_for_admission_with_references<
    's,
    'v,
    'c,
    P: PlaceholderKind + 'c,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
    cursor: AuthoredCursor<'s, B>,
    callbacks: impl Into<WalkCallbacks<'c, P, B>>,
    reference: ReferenceCheck<'_, B>,
    coverage: &mut WalkCoverage,
) -> ValueValidationFindings {
    validate_authored_value_with_callbacks(
        attr,
        value,
        defs,
        ancestry,
        lookup,
        cursor,
        callbacks,
        Some(reference),
        coverage,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_authored_value_with_callbacks<
    's,
    'v,
    'c,
    P: PlaceholderKind + 'c,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
    cursor: AuthoredCursor<'s, B>,
    callbacks: impl Into<WalkCallbacks<'c, P, B>>,
    reference: Option<ReferenceCheck<'_, B>>,
    coverage: &mut WalkCoverage,
) -> ValueValidationFindings {
    let visits = std::cell::RefCell::new(std::mem::take(coverage));
    let callbacks = callbacks.into();
    let policy = reference.map(|reference| ReferencePolicy::new(callbacks.ordinary, reference));
    let checked = {
        let mut callbacks = callbacks.for_walk();
        if !cursor.is_projection_only()
            && let Some(policy) = &policy
        {
            callbacks.ordinary = OrdinaryCheck::References(policy);
        }
        let mut checked = ValueValidator::<WithAuthored>::with_mode(defs, Some(lookup))
            .with_scope(callbacks.scope)
            .walk_with_assignment(
                attr,
                value.into(),
                &FieldPath::new(),
                ancestry,
                &mut Vec::new(),
                Some(AssignmentWalk {
                    cursor,
                    callbacks: callbacks.clone(),
                    coverage: &visits,
                    claim: Claim::None,
                }),
            );
        checked.findings.authored.finish(&callbacks);
        checked
    };
    *coverage = visits.into_inner();
    let mut findings = checked.into_findings_with_coverage(coverage);
    findings.structure = value_structure(value);
    findings
}

fn step_assignment<'a, P: PlaceholderKind + 'a, B: BindingAddress, M: WalkMode>(
    assignment: Option<&WalkHandle<'a, P, B, M>>,
    advance: impl FnOnce(
        &AuthoredCursor<'a, B>,
    ) -> Result<AuthoredCursor<'a, B>, crate::parser::retained::MissingEvidence>,
) -> Result<Option<WalkHandle<'a, P, B, M>>, TypeError> {
    let Some(walk) = assignment else {
        return Ok(None);
    };
    walk.step(advance).map(Some)
}

fn alternative_assignment<'a, P: PlaceholderKind + 'a, B: BindingAddress, M: WalkMode>(
    assignment: Option<&WalkHandle<'a, P, B, M>>,
    index: usize,
) -> Result<Option<WalkHandle<'a, P, B, M>>, TypeError> {
    let Some(walk) = assignment else {
        return Ok(None);
    };
    Ok(Some(walk.with_cursor(
        walk.cursor.alternative(index).map_err(seam_error)?,
    )))
}

/// One container edge owns its role, correspondence and diagnostic path in
/// both walks. A map key can never acquire a struct-field path by accident.
#[derive(Clone)]
enum ChildRoute<'k> {
    ListItem(usize),
    MapValue(std::borrow::Cow<'k, crate::parser::AttributeKey>),
    StructField {
        key: std::borrow::Cow<'k, crate::parser::AttributeKey>,
        alias: Option<&'k str>,
    },
}
impl ChildRoute<'_> {
    fn path(&self, parent: &FieldPath) -> FieldPath {
        match self {
            Self::ListItem(index) => parent.push_index(*index),
            Self::MapValue(key) => parent.push_map_key(key.to_string()),
            Self::StructField { key, .. } => parent.push_field(key.to_string()),
        }
    }
    fn step<'s, B: BindingAddress>(
        &self,
        cursor: &AuthoredCursor<'s, B>,
    ) -> Result<AuthoredCursor<'s, B>, crate::parser::retained::MissingEvidence> {
        match self {
            Self::ListItem(index) => cursor.child(*index, EdgeRole::ListItem),
            Self::MapValue(key) => cursor.map_child(key, EdgeRole::MapValue),
            Self::StructField { key, alias } => cursor.field_child(key, *alias),
        }
    }
}

/// Discarded alternatives consume syntax and declarations only. No value is
/// evaluated, classified, normalized or sent to a provider here. Union probes
/// own their findings and coverage; only a chosen authored probe survives.
#[allow(clippy::too_many_arguments)]
pub(crate) fn visit_discarded<
    'w,
    's: 'w,
    P: PlaceholderKind + 'w,
    B: BindingAddress,
    M: WalkMode,
>(
    attr: &'w CompiledAttributeType,
    defs: &'w BTreeMap<String, CompiledAttributeType>,
    path: &FieldPath,
    ancestry: FieldAncestry,
    cursor: AuthoredUse<'s, B>,
    callbacks: impl Into<WalkCallbacks<'w, P, B, M>>,
    coverage: &mut WalkCoverage,
) -> Result<AssignmentProbe<M::Channel>, TypeError> {
    let callbacks = callbacks.into();
    let mut probe = visit_syntax(
        attr,
        defs,
        path,
        ancestry,
        cursor,
        callbacks.clone(),
        coverage,
        SyntaxVisit::Discarded,
    )?;
    callbacks.finish(&mut probe.authored);
    Ok(probe)
}

#[derive(Clone, Copy)]
enum SyntaxVisit {
    Pending,
    PendingCovered,
    Discarded,
    DiscardedCovered,
    Covered,
}
impl SyntaxVisit {
    // Discarded/covered syntax is judged without admitting a value; its
    // placement and kind verdicts do not require a live child contract.
    fn requires_child_contract(self) -> bool {
        match self {
            Self::Pending => true,
            Self::PendingCovered | Self::Discarded | Self::DiscardedCovered | Self::Covered => {
                false
            }
        }
    }

    fn value<'v, P: PlaceholderKind, B: BindingAddress>(self) -> CheckValue<'v, P, B> {
        match self {
            Self::Pending => CheckValue::Pending,
            Self::PendingCovered => CheckValue::Covered,
            Self::Discarded | Self::DiscardedCovered => CheckValue::Discarded,
            Self::Covered => CheckValue::Covered,
        }
    }
    fn covered(self) -> Self {
        match self {
            Self::Discarded => Self::DiscardedCovered,
            Self::Pending => Self::PendingCovered,
            covered @ (Self::Covered | Self::DiscardedCovered | Self::PendingCovered) => covered,
        }
    }
}

#[derive(Clone)]
struct SyntaxVisitState<'a, B: BindingAddress, M: WalkMode> {
    mode: SyntaxVisit,
    claim: M::Claim<'a, B>,
}
impl<B: BindingAddress, M: WalkMode> SyntaxVisitState<'_, B, M> {
    fn value<'v, P: PlaceholderKind>(&self) -> CheckValue<'v, P, B> {
        self.mode.value()
    }
    fn covered(self) -> Self {
        Self {
            mode: self.mode.covered(),
            ..self
        }
    }
}

// Only a completed descent can be reused. An in-progress carrying use is a
// failed descent, never an ordinary acceptance.
enum SyntaxProgress {
    Entered,
    Completed(CompletedSyntax),
}
struct CompletedSyntax;
type SyntaxVisits = std::collections::HashMap<
    (
        (
            NodeId,
            UseId,
            crate::parser::retained::ExpansionInstanceId,
            crate::parser::retained::ProjectionGeneration,
        ),
        usize,
    ),
    SyntaxProgress,
>;

#[allow(clippy::too_many_arguments)]
fn visit_syntax<'w, 's: 'w, P: PlaceholderKind, B: BindingAddress, M: WalkMode>(
    attr: &'w CompiledAttributeType,
    defs: &'w BTreeMap<String, CompiledAttributeType>,
    path: &FieldPath,
    ancestry: FieldAncestry,
    cursor: AuthoredUse<'s, B>,
    callbacks: WalkCallbacks<'w, P, B, M>,
    coverage: &mut WalkCoverage,
    visit: SyntaxVisit,
) -> Result<AssignmentProbe<M::Channel>, TypeError> {
    fn descend<'w, 's: 'w, P: PlaceholderKind, B: BindingAddress, M: WalkMode>(
        attr: &'w CompiledAttributeType,
        defs: &'w BTreeMap<String, CompiledAttributeType>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        cursor: AuthoredUse<'s, B>,
        callbacks: WalkCallbacks<'w, P, B, M>,
        visit: SyntaxVisitState<'s, B, M>,
        coverage: &mut WalkCoverage,
        findings: &mut AssignmentProbe<M::Channel>,
        visited: &mut SyntaxVisits,
        declines: &mut M::Declines,
    ) -> Result<(), TypeError> {
        let mut references = Vec::new();
        let resolved = resolve_refs_on_path(attr, defs, &mut references)
            .map_err(|error| error.into_validation_error())?;
        let attr = resolved.attr;
        let navigation = cursor.navigation().map_err(seam_error)?;
        let key = (
            cursor.identity(),
            attr as *const CompiledAttributeType as usize,
        );
        match visited.get(&key) {
            Some(SyntaxProgress::Completed(CompletedSyntax)) => return Ok(()),
            Some(SyntaxProgress::Entered) => {
                if cursor.node().judgement().contains_any_conversion() {
                    findings.authored.extend(M::reject_transport(
                        &AuthoredCursor::authored(cursor),
                        &path.canonical(),
                    ));
                }
                return Ok(());
            }
            None => {}
        }
        visited.insert(key, SyntaxProgress::Entered);
        let result = (|| {
            if let AuthoredNavigation::Forward {
                child,
                authored: false,
            } = navigation
            {
                return descend(
                    attr,
                    defs,
                    path,
                    ancestry,
                    child,
                    callbacks.clone(),
                    visit,
                    coverage,
                    findings,
                    visited,
                    declines,
                );
            }
            let policy_position = (visit.claim.is_claimed()
                || cursor.node().judgement().contains_any_conversion())
                && match &navigation {
                    AuthoredNavigation::Forward { .. }
                    | AuthoredNavigation::Secret(_)
                    | AuthoredNavigation::Alternatives { .. }
                    | AuthoredNavigation::Parts
                    | AuthoredNavigation::Leaf
                    | AuthoredNavigation::Covered => true,
                    AuthoredNavigation::List | AuthoredNavigation::Map => false,
                };
            // Authored wrappers and alternatives retain the original declaration
            // until their policy judges the value position. Container probes still
            // choose child contracts; no-as probes keep their existing timing.
            if !policy_position && let AttrTypeKind::Union(members) = &attr.kind {
                // Syntax, never a fabricated Value, determines which container
                // declarations can supply child contracts to this authored probe.
                let mut shape = cursor.clone();
                let shape = loop {
                    match shape.navigation().map_err(seam_error)? {
                        AuthoredNavigation::Forward { child: next, .. }
                        | AuthoredNavigation::Secret(next) => shape = next,
                        navigation @ (AuthoredNavigation::Alternatives { .. }
                        | AuthoredNavigation::List
                        | AuthoredNavigation::Map
                        | AuthoredNavigation::Parts
                        | AuthoredNavigation::Leaf
                        | AuthoredNavigation::Covered) => break navigation,
                    }
                };
                let mut best = None;
                for member in members.as_raw_slice() {
                    let resolved = resolve_refs_on_path(member, defs, &mut Vec::new())
                        .map_err(|error| error.into_validation_error())?;
                    let compatible = match &shape {
                        AuthoredNavigation::List => matches!(
                            resolved.attr.kind,
                            AttrTypeKind::List { .. } | AttrTypeKind::Union(_)
                        ),
                        AuthoredNavigation::Map => matches!(
                            resolved.attr.kind,
                            AttrTypeKind::Map { .. }
                                | AttrTypeKind::Struct { .. }
                                | AttrTypeKind::Union(_)
                        ),
                        AuthoredNavigation::Forward { .. }
                        | AuthoredNavigation::Secret(_)
                        | AuthoredNavigation::Alternatives { .. }
                        | AuthoredNavigation::Parts
                        | AuthoredNavigation::Leaf
                        | AuthoredNavigation::Covered => true,
                    };
                    if !compatible {
                        continue;
                    }
                    let mut probe = AssignmentProbe::default();
                    let mut reached = WalkCoverage::default();
                    descend(
                        member,
                        defs,
                        path,
                        ancestry,
                        cursor.clone(),
                        callbacks.probing(),
                        visit.clone(),
                        &mut reached,
                        &mut probe,
                        &mut Default::default(),
                        &mut Default::default(),
                    )?;
                    let rank = (
                        std::cmp::Reverse(authored_shape_matches(resolved.attr, &cursor)),
                        probe.ordinary.errors.len()
                            + probe.authored.errors.len()
                            + probe.authored.conversions.findings().len(),
                        std::cmp::Reverse(reached.len()),
                    );
                    if best.as_ref().is_none_or(|(current, _, _)| rank < *current) {
                        best = Some((rank, probe, reached));
                    }
                }
                if let Some((_, probe, reached)) = best {
                    findings.extend(probe);
                    coverage.extend(reached);
                    return Ok(());
                }
                if visit.mode.requires_child_contract()
                    && cursor.node().judgement().contains_any_conversion()
                {
                    findings.authored.extend(M::reject_transport(
                        &AuthoredCursor::authored(cursor.clone()),
                        &path.canonical(),
                    ));
                    coverage.record(&cursor);
                    return Ok(());
                }
            }
            let mut visit = if matches!(
                navigation,
                AuthoredNavigation::Covered | AuthoredNavigation::Parts
            ) {
                visit.covered()
            } else {
                visit
            };
            let authored = match visit.mode {
                SyntaxVisit::Discarded | SyntaxVisit::Pending => {
                    AuthoredCursor::authored(cursor.clone())
                }
                SyntaxVisit::DiscardedCovered
                | SyntaxVisit::Covered
                | SyntaxVisit::PendingCovered => AuthoredCursor::covered_by(cursor.clone()),
            };
            let (policy, position) = M::judge_position(
                &callbacks,
                attr,
                visit.value(),
                &path.canonical(),
                PolicyPosition {
                    cursor: authored.clone(),
                    claim: &mut visit.claim,
                },
            );
            findings.authored.extend(policy);
            if !matches!(visit.mode, SyntaxVisit::Covered) {
                findings.ordinary.extend(position.check()?);
            }
            #[cfg(test)]
            if let Some(hook) = &callbacks.authored {
                findings.authored.extend(hook.record(
                    attr,
                    visit.value(),
                    &path.canonical(),
                    authored,
                ));
            }
            coverage.record(&cursor);
            let same_position = matches!(
                navigation,
                AuthoredNavigation::Forward { .. }
                    | AuthoredNavigation::Secret(_)
                    | AuthoredNavigation::Alternatives { .. }
            );
            let mut child = |step: ChildStep<'w, 's, B>,
                             findings: &mut AssignmentProbe<M::Channel>,
                             declines: &mut M::Declines| {
                step.visit(|child| {
                    let nested = if same_position {
                        visit.clone()
                    } else {
                        SyntaxVisitState::<B, M> {
                            claim: Default::default(),
                            ..visit.clone()
                        }
                    };
                    descend(
                        child.declaration,
                        defs,
                        &child.path,
                        child.ancestry,
                        child.cursor,
                        callbacks.clone(),
                        nested,
                        coverage,
                        findings,
                        visited,
                        declines,
                    )
                })
            };
            match navigation {
                AuthoredNavigation::Forward { child: next, .. } => child(
                    ChildStep::descend(attr, next, path.clone(), ancestry),
                    findings,
                    declines,
                ),
                AuthoredNavigation::Secret(next) => child(
                    ChildStep::descend(attr, next, path.clone(), ancestry.inside_secret()),
                    findings,
                    declines,
                ),
                AuthoredNavigation::Alternatives { first, .. } => {
                    for index in 0..cursor.child_count() - first {
                        child(
                            ChildStep::descend(
                                attr,
                                cursor.alternative(index).map_err(seam_error)?,
                                path.clone(),
                                ancestry,
                            ),
                            findings,
                            declines,
                        )?;
                    }
                    Ok(())
                }
                AuthoredNavigation::List => {
                    let element = match &attr.kind {
                        AttrTypeKind::List { element_type, .. } => element_type.as_ref(),
                        AttrTypeKind::UnloadedSchemaType(_)
                        | AttrTypeKind::Reference { .. }
                        | AttrTypeKind::String { .. }
                        | AttrTypeKind::ResourceHandle { .. }
                        | AttrTypeKind::StringLiteralSet { .. }
                        | AttrTypeKind::Int { .. }
                        | AttrTypeKind::Float { .. }
                        | AttrTypeKind::Bool
                        | AttrTypeKind::Duration
                        | AttrTypeKind::Enum { .. }
                        | AttrTypeKind::Union(_)
                        | AttrTypeKind::Ref(_)
                        | AttrTypeKind::Map { .. }
                        | AttrTypeKind::Struct { .. } => attr,
                    };
                    for index in 0..cursor.child_count() {
                        let step = ChildStep::list(element, &cursor, index, path, ancestry)?;
                        child(step, findings, declines)?;
                    }
                    Ok(())
                }
                AuthoredNavigation::Map => {
                    for key in cursor.map_keys() {
                        let step = ChildStep::map(
                            attr,
                            &cursor,
                            key,
                            path,
                            ancestry,
                            DeclineSink::new(&callbacks, declines, &mut findings.authored),
                        )?;
                        child(step, findings, declines)?;
                    }
                    Ok(())
                }
                AuthoredNavigation::Parts => {
                    for index in 0..cursor.child_count() {
                        child(
                            ChildStep::descend(
                                attr,
                                cursor.part(index).map_err(seam_error)?,
                                path.clone(),
                                ancestry,
                            ),
                            findings,
                            declines,
                        )?;
                    }
                    Ok(())
                }
                AuthoredNavigation::Leaf | AuthoredNavigation::Covered => Ok(()),
            }
        })();
        if result.is_ok() {
            visited.insert(key, SyntaxProgress::Completed(CompletedSyntax));
        }
        result
    }
    let mut findings = AssignmentProbe::default();
    let mut completed = WalkCoverage::default();
    descend(
        attr,
        defs,
        path,
        ancestry,
        cursor,
        callbacks.clone(),
        SyntaxVisitState::<B, M> {
            mode: visit,
            claim: Default::default(),
        },
        &mut completed,
        &mut findings,
        &mut Default::default(),
        &mut Default::default(),
    )?;
    coverage.extend(completed);
    Ok(findings)
}

/// Shape evidence is already judged syntax, not a manufactured discarded
/// value. It ranks compatible probes without evaluating scalar constraints.
fn authored_shape_matches<B: BindingAddress>(
    attr: &CompiledAttributeType,
    cursor: &AuthoredUse<'_, B>,
) -> bool {
    use crate::parser::TypeExpr;
    let kind = match &attr.kind {
        AttrTypeKind::Enum { base, .. } => &base.kind,
        kind => kind,
    };
    matches!(
        (kind, cursor.node().judgement().static_type()),
        (
            AttrTypeKind::String { .. }
                | AttrTypeKind::Reference { .. }
                | AttrTypeKind::StringLiteralSet { .. },
            Ok(TypeExpr::String)
        ) | (AttrTypeKind::Int { .. }, Ok(TypeExpr::Int))
            | (AttrTypeKind::Float { .. }, Ok(TypeExpr::Float))
            | (AttrTypeKind::Bool, Ok(TypeExpr::Bool))
            | (AttrTypeKind::List { .. }, Ok(TypeExpr::List(_)))
            | (
                AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. },
                Ok(TypeExpr::Map(_))
            )
    )
}

/// The declaration selected by the resource schema walk, including its slot.
pub(crate) struct SchemaAttribute<'s> {
    pub(crate) ordinal: usize,
    pub(crate) effective: EffectiveAttribute<'s>,
}

pub(crate) type KnownAttributeAdmission<'a, 'v, P, B, T> =
    &'a dyn Fn(&str, SchemaAttribute<'_>, &Classification<'v, P, B, T>) -> ValueValidationFindings;

type UnknownAttributeAdmission<'a, 'v, P, B, T> =
    &'a dyn Fn(&str, &Classification<'v, P, B, T>) -> ValueValidationFindings;

pub(crate) struct AttributeAdmission<'a, 'v, P: PlaceholderKind, B: BindingAddress, T> {
    pub(crate) known: KnownAttributeAdmission<'a, 'v, P, B, T>,
    pub(crate) unknown: Option<UnknownAttributeAdmission<'a, 'v, P, B, T>>,
}

struct AdmittedUnion<'a, M: WalkMode = NoAuthored> {
    selected: UnionFit<'a>,
    assignment: AssignmentFindings<M::Channel>,
    authored: SupplementalFindings<M::Channel>,
}

/// Compatibility for the schema's structural-only fixtures. Infallible makes
/// installing an assignment callback impossible without supplying a cursor.
#[cfg(test)]
pub(crate) fn validate_value_for_admission<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
    assignment: Option<std::convert::Infallible>,
) -> ValueValidationFindings {
    match assignment {
        None => validate_value_with_lookup(attr, value, defs, ancestry, lookup),
        Some(never) => match never {},
    }
}

impl<'a> ValueValidator<'a, '_, NoAuthored> {
    fn walk<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> WalkCheckedValue<'a> {
        self.walk_with_assignment(attr, value, path, ancestry, visited, None)
    }
}

impl<'a, M: WalkMode> ValueValidator<'a, '_, M> {
    fn walk_with_assignment<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let (assignment, mut extra, mut wrappers) = match assignment {
            Some(assignment) => match assignment.prepare(attr, self.defs, value, path, ancestry) {
                Ok((assignment, findings, wrappers)) => (Some(assignment), findings, wrappers),
                Err(error) => return WalkCheckedValue::rejected(path, error),
            },
            None => (None, AssignmentProbe::default(), Vec::new()),
        };
        if let Some(checked) = self.walk_reference(
            attr,
            value,
            path,
            ancestry,
            visited,
            assignment.as_ref(),
            &mut extra,
            &mut wrappers,
        ) {
            return std::rc::Rc::unwrap_or_clone(checked);
        }
        let node = match value {
            ClassifiedRef::Known(value) => Some(CheckedNode::new(
                attr,
                value.node_id(),
                ancestry,
                visited,
                assignment.as_ref(),
            )),
            ClassifiedRef::Shape(_) | ClassifiedRef::Pending(_) | ClassifiedRef::Failed(_) => None,
        };
        if let Some(checked) = node
            .as_ref()
            .and_then(|node| self.checked.borrow().get(node).cloned())
        {
            return checked;
        }
        let mut checked = self.walk_value(attr, value, path, ancestry, visited, assignment.clone());
        self.complete_authored_walk(&mut checked, attr, value, path, assignment, extra, wrappers);
        checked.retain_primary_finding();
        if let Some(node) = node {
            self.checked.borrow_mut().insert(node, checked.clone());
        }
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_authored_walk<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        checked: &mut WalkCheckedValue<'a, M>,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        assignment: Option<WalkHandle<'w, P, B, M>>,
        extra: AssignmentProbe<M::Channel>,
        wrappers: Vec<AuthoredCursor<'w, B>>,
    ) where
        'a: 'w,
    {
        let mut authored = SupplementalFindings::<M::Channel>::default();
        if let Some(walk) = assignment {
            for cursor in wrappers {
                let wrapper = walk.with_cursor(cursor);
                authored.extend(wrapper.authored(
                    checked.selected.unwrap_or(attr),
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                ));
            }
        }
        authored.extend(extra.authored);
        for (_, error) in &extra.ordinary.errors {
            checked.reject(path, error.clone());
        }
        authored.extend(std::mem::take(&mut checked.findings.authored));
        checked.findings.authored = authored;
        checked.findings.assignment.extend(extra.ordinary);
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_secret<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        evidence: ClassifiedRef<'_, 'v, P, B, T>,
        original: &'v Value<P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let (assignment, wrapper) = match assignment {
            Some(walk) => match &walk.cursor.kind {
                CursorKind::Authored(usage) => match usage.navigation().map_err(seam_error) {
                    Ok(AuthoredNavigation::Secret(next)) => (
                        Some(walk.with_cursor(AuthoredCursor::authored(next))),
                        Some(walk),
                    ),
                    Ok(_) => (Some(walk), None),
                    Err(error) => return WalkCheckedValue::rejected(path, error),
                },
                _ => (Some(walk), None),
            },
            None => (None, None),
        };
        let mut checked = self.walk_with_assignment(
            attr,
            evidence,
            path,
            ancestry.inside_secret(),
            visited,
            assignment,
        );
        if let Some(wrapper) = wrapper {
            let mut authored: SupplementalFindings<M::Channel> = wrapper
                .authored(
                    checked.selected.unwrap_or(attr),
                    CheckValue::Current(original),
                    path,
                )
                .into();
            authored.extend(std::mem::take(&mut checked.findings.authored));
            checked.findings.authored = authored;
        }
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_coalesce<
        'w,
        'c,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'c, 'v, P, B, T>,
        left_value: ClassifiedRef<'c, 'v, P, B, T>,
        default: ClassifiedRef<'c, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        // A coalesce has an authored assignment of its own. Retain that
        // judgement while the existing walk checks both classified
        // alternatives, including their provider callbacks.
        let mut assignment = assignment;
        let (parent_authored, decision) = match &mut assignment {
            Some(walk) => {
                let mut policy = AssignmentFindings::<M::Channel>::default();
                let decision = walk.apply_policy(
                    attr,
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                    &mut policy,
                );
                policy.extend(walk.authored(
                    attr,
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                ));
                (policy, AssignmentPosition::Judged(decision))
            }
            None => (
                AssignmentFindings::<M::Channel>::default(),
                AssignmentPosition::SchemaOnly,
            ),
        };
        let parent_assignment = decision.check();
        let left_assignment = match alternative_assignment(assignment.as_ref(), 0) {
            Ok(walk) => walk,
            Err(error) => return WalkCheckedValue::rejected(path, error),
        };
        let right_assignment = match alternative_assignment(assignment.as_ref(), 1) {
            Ok(walk) => walk,
            Err(error) => return WalkCheckedValue::rejected(path, error),
        };
        let mut left = self.walk_with_assignment(
            attr,
            left_value,
            path,
            ancestry,
            &mut Vec::new(),
            left_assignment,
        );
        let right = self.walk_with_assignment(
            attr,
            default,
            path,
            ancestry,
            &mut Vec::new(),
            right_assignment,
        );
        // Deferred coalesces have one authored parent location. Keep the
        // established primary local diagnostic there, while retaining every
        // provider finding from the independently known branches.
        left.findings.local = left
            .local
            .error()
            .or_else(|| right.local.error())
            .map(|error| vec![(path.clone(), error.clone())])
            .unwrap_or_default();
        left.findings.assignment.extend(right.findings.assignment);
        left.findings.authored.extend(right.findings.authored);
        left.findings.located.extend(right.findings.located);
        left.findings.provider.extend(right.findings.provider);
        left.findings
            .additional_local_errors
            .extend(right.findings.additional_local_errors);
        left.local = match (left.local, right.local) {
            (LocalOutcome::Accepted(value), LocalOutcome::Accepted(default)) => {
                let mut exactness = value.exactness;
                exactness.combine(default.exactness);
                LocalOutcome::Accepted(Fit::with_kind(
                    exactness,
                    attr,
                    FitKind::Coalesce {
                        value: Arc::new(value),
                        default: Arc::new(default),
                    },
                ))
            }
            (LocalOutcome::Rejected { primary }, _) | (_, LocalOutcome::Rejected { primary }) => {
                LocalOutcome::Rejected { primary }
            }
        };
        match parent_assignment {
            Ok(findings) => left.findings.assignment.extend(findings),
            // A branch's local error is the established primary diagnostic
            // at this authored coalesce. Do not add its inferred echo.
            Err(_) if left.local.error().is_some() => {}
            Err(error) => {
                left.reject(path, error.clone());
                left.findings
                    .assignment
                    .errors
                    .push((path.canonical(), error));
            }
        }
        let mut authored: SupplementalFindings<M::Channel> = parent_authored.into();
        if let Some(walk) = &assignment {
            let parts = walk.authored_parts(attr, self.defs, path, ancestry);
            left.extend_assignment(path, parts.ordinary);
            authored.extend(parts.authored);
        }
        authored.extend(std::mem::take(&mut left.findings.authored));
        left.findings.authored = authored;
        left.selected = Some(attr);
        left
    }

    fn walk_branches<
        'w,
        'c,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'c, 'v, P, B, T>,
        branches: Vec<ClassifiedRef<'c, 'v, P, B, T>>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let ClassifiedRef::Pending(pending) = value else {
            unreachable!("only an undecided conditional has branches")
        };
        let mut assignment = assignment;
        let (parent_policy, decision) = match &mut assignment {
            Some(walk) => {
                let mut policy = AssignmentFindings::<M::Channel>::default();
                let decision = walk.apply_policy(
                    attr,
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                    &mut policy,
                );
                (policy, AssignmentPosition::Judged(decision))
            }
            None => (
                AssignmentFindings::<M::Channel>::default(),
                AssignmentPosition::SchemaOnly,
            ),
        };
        let mut checked =
            WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred(pending.fit()));
        checked.findings.authored.extend(parent_policy);
        if let Some(walk) = &assignment
            && matches!(&walk.cursor.kind, CursorKind::Authored(usage) if usage.form() == AuthoredForm::If)
        {
            let selection = walk.with_cursor(walk.cursor.selection());
            checked.findings.authored.extend(selection.authored(
                attr,
                CheckValue::Current(value.diagnostic_value()),
                path,
            ));
            match decision.check_selection() {
                Ok(findings) => checked.findings.assignment.extend(findings),
                Err(error) => return WalkCheckedValue::rejected(path, error),
            }
        }
        if let Some(walk) = &assignment
            && !matches!(&walk.cursor.kind, CursorKind::Authored(usage) if usage.form() == AuthoredForm::If)
        {
            checked.findings.authored.extend(walk.authored(
                attr,
                CheckValue::Current(value.diagnostic_value()),
                path,
            ));
        }
        for (index, branch) in branches.into_iter().enumerate() {
            let branch_assignment = match alternative_assignment(assignment.as_ref(), index) {
                Ok(walk) => walk,
                Err(error) => return WalkCheckedValue::rejected(path, error),
            };
            let branch = self.walk_with_assignment(
                attr,
                branch,
                path,
                ancestry,
                &mut Vec::new(),
                branch_assignment,
            );
            // Expansion removes decided ModuleIf values. Every branch
            // still present here is independently checked in both stages.
            if let LocalOutcome::Rejected { primary } = &branch.local {
                if checked.local.error().is_some() {
                    checked
                        .findings
                        .additional_local_errors
                        .push(primary.clone());
                } else {
                    checked.reject_primary(primary.clone());
                }
            }
            checked
                .findings
                .assignment
                .extend(branch.findings.assignment);
            checked.findings.authored.extend(branch.findings.authored);
            checked.findings.located.extend(branch.findings.located);
            checked.findings.local.extend(branch.findings.local);
            checked
                .findings
                .additional_local_errors
                .extend(branch.findings.additional_local_errors);
            checked.findings.provider.extend(branch.findings.provider);
        }
        if let Some(walk) = &assignment {
            let parts = walk.authored_parts(attr, self.defs, path, ancestry);
            checked.extend_assignment(path, parts.ordinary);
            checked.findings.authored.extend(parts.authored);
        }
        checked
    }

    fn walk_value<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        if let Some(inner) = value.protected_for_schema(&SchemaEvidenceAuthority(())) {
            return inner.validate(|evidence| {
                self.walk_secret(
                    attr,
                    evidence,
                    value.diagnostic_value(),
                    path,
                    ancestry,
                    visited,
                    assignment,
                )
            });
        }
        if let Some((left, right)) = value.coalesce() {
            return self.walk_coalesce(attr, value, left, right, path, ancestry, assignment);
        }
        if let Some(branches) = value.branches() {
            return self.walk_branches(attr, value, branches, path, ancestry, assignment);
        }
        let resolution = match resolve_refs_on_path(attr, self.defs, visited) {
            Ok(resolution) => resolution,
            Err(error) => return WalkCheckedValue::rejected(path, error.into_validation_error()),
        };
        let checked =
            self.walk_resolved(resolution.attr, value, path, ancestry, visited, assignment);
        visited.truncate(resolution.restore_len);
        checked
    }

    fn walk_resolved<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let mut assignment = assignment;
        let (policy, decision) = match &mut assignment {
            Some(walk) => {
                let mut policy = AssignmentFindings::<M::Channel>::default();
                let decision = walk.apply_policy(
                    attr,
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                    &mut policy,
                );
                (policy, AssignmentPosition::Judged(decision))
            }
            None => (
                AssignmentFindings::<M::Channel>::default(),
                AssignmentPosition::SchemaOnly,
            ),
        };
        // Main's union-level ordinary check remains before member trials.
        // The authored hook belongs to the chosen member's probe instead.
        let rejected_scalar = assignment
            .as_ref()
            .is_some_and(|walk| walk.claim.is_rejected())
            && !matches!(
                value.concrete(),
                Some(ConcreteValueRef::List(_) | ConcreteValueRef::Map(_))
            );
        let probes = !rejected_scalar
            && matches!(attr.kind, AttrTypeKind::Union(_))
            && !matches!(value, ClassifiedRef::Failed(_))
            && !matches!(value, ClassifiedRef::Pending(pending) if !pending.arithmetic());
        let mut authored = assignment
            .as_ref()
            .filter(|_| !probes)
            .map(|walk| {
                SupplementalFindings::from(walk.authored(
                    attr,
                    CheckValue::Current(value.diagnostic_value()),
                    path,
                ))
            })
            .unwrap_or_default();
        authored.extend(policy);
        let findings = match decision.check() {
            Ok(findings) if !findings.errors.is_empty() => {
                let mut checked = WalkCheckedValue::rejected(path, findings.errors[0].1.clone());
                checked.findings.additional_local_errors.extend(
                    findings.errors.iter().skip(1).map(|(path, error)| {
                        PrimaryValueFinding::new(
                            FieldPath::from_canonical_segments(path.segments().iter().cloned()),
                            error.clone(),
                        )
                    }),
                );
                checked
                    .findings
                    .located
                    .extend(findings.errors.iter().map(|(path, error)| {
                        (
                            FieldPath::from_canonical_segments(path.segments().iter().cloned()),
                            error.clone(),
                        )
                    }));
                checked.findings.assignment = findings;
                checked.findings.authored = authored;
                checked.selected = Some(attr);
                return checked;
            }
            Ok(findings) => findings,
            Err(error) => {
                let mut checked = WalkCheckedValue::rejected(path, error.clone());
                checked
                    .findings
                    .assignment
                    .errors
                    .push((path.canonical(), error));
                checked.findings.authored = authored;
                checked.selected = Some(attr);
                return checked;
            }
        };
        let mut checked =
            self.walk_resolved_value(attr, value, path, ancestry, visited, assignment.clone());
        if let Some(walk) = assignment.as_ref()
            && let Some(conversion) = walk.claim.conversion()
        {
            if checked.findings.authored.policy_errors().is_empty()
                && checked.findings.authored.conversion_errors().is_empty()
                && let Some(cause) = checked.first_error().cloned()
            {
                checked.findings.authored.extend(AssignmentFindings {
                    conversions: <M::Claim<'_, B> as ClaimState<'_, B>>::reject_destination(
                        conversion, cause,
                    ),
                    claim_effect: ClaimEffect::Rejected,
                    ..Default::default()
                });
            }
            if !checked.findings.authored.policy_errors().is_empty()
                || !checked.findings.authored.conversion_errors().is_empty()
            {
                checked.local = LocalOutcome::Accepted(Fit::with_kind(
                    FitExactness::EXACT,
                    attr,
                    FitKind::Failed,
                ));
                checked.findings.local.clear();
                checked.findings.located.clear();
                checked.findings.additional_local_errors.clear();
                checked.findings.provider.clear();
            }
        }
        checked.findings.assignment.extend(findings);
        let mut authored = authored;
        if !probes && let Some(walk) = &assignment {
            let parts = walk.authored_parts(attr, self.defs, path, ancestry);
            checked.extend_assignment(path, parts.ordinary);
            authored.extend(parts.authored);
        }
        authored.extend(std::mem::take(&mut checked.findings.authored));
        checked.findings.authored = authored;
        checked.selected.get_or_insert(attr);
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_resolved_value<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        if assignment
            .as_ref()
            .is_some_and(|walk| walk.claim.is_rejected())
            && !matches!(
                (&attr.kind, value.concrete()),
                (AttrTypeKind::List { .. }, Some(ConcreteValueRef::List(_)))
                    | (
                        AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. },
                        Some(ConcreteValueRef::Map(_))
                    )
                    | (
                        AttrTypeKind::Union(_),
                        Some(ConcreteValueRef::List(_) | ConcreteValueRef::Map(_))
                    )
            )
        {
            // A failed claim has no destination value check. Structural child
            // positions still enter the same walk with fresh ordinary policy.
            return WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Failed);
        }
        if let AttrTypeKind::UnloadedSchemaType(_) = &attr.kind {
            return WalkCheckedValue::rejected(
                path,
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded.into(),
            );
        }
        if let Some(walk) = &assignment
            && let CursorKind::Authored(usage) = &walk.cursor.kind
            && usage.node().judgement().contains_any_conversion()
        {
            // A transport cannot use a changed projection to bypass its
            // authored children. Incompatible containers stop at main's
            // ordinary shape check, before any child conversion is entered.
            let incompatible = match usage.form() {
                AuthoredForm::List => (!matches!(
                    attr.kind,
                    AttrTypeKind::List { .. } | AttrTypeKind::Union(_)
                ))
                .then_some("List"),
                AuthoredForm::Map => (!matches!(
                    attr.kind,
                    AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. } | AttrTypeKind::Union(_)
                ))
                .then_some("Map"),
                AuthoredForm::Forward
                | AuthoredForm::Parens
                | AuthoredForm::Convert
                | AuthoredForm::Definition
                | AuthoredForm::Binding
                | AuthoredForm::Reference
                | AuthoredForm::Literal
                | AuthoredForm::Secret
                | AuthoredForm::If
                | AuthoredForm::Coalesce
                | AuthoredForm::Computed
                | AuthoredForm::Sink => None,
            };
            if let Some(shape) = incompatible {
                return WalkCheckedValue::rejected(
                    path,
                    TypeError::value_type_mismatch(attr.type_name(), shape.into()),
                );
            }
        }
        if matches!(value, ClassifiedRef::Pending(_) | ClassifiedRef::Failed(_))
            && let Some(walk) = &assignment
            && let CursorKind::Authored(usage) = &walk.cursor.kind
            && usage.node().judgement().contains_any_conversion()
            && matches!(
                (&attr.kind, usage.form()),
                (AttrTypeKind::List { .. }, AuthoredForm::List)
                    | (
                        AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. },
                        AuthoredForm::Map
                    )
                    | (
                        AttrTypeKind::Union(_),
                        AuthoredForm::List | AuthoredForm::Map
                    )
            )
        {
            let mut checked =
                WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Failed);
            match visit_syntax(
                attr,
                self.defs,
                path,
                ancestry,
                usage.clone(),
                walk.callbacks.clone(),
                &mut walk.coverage.borrow_mut(),
                SyntaxVisit::Pending,
            ) {
                Ok(probe) => {
                    checked.extend_assignment(path, probe.ordinary);
                    checked.findings.authored.extend(probe.authored);
                }
                Err(error) => checked.reject(path, error),
            }
            return checked;
        }
        match value {
            ClassifiedRef::Failed(_) => {
                return WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Failed);
            }
            ClassifiedRef::Known(_) | ClassifiedRef::Shape(_) | ClassifiedRef::Pending(_) => {}
        }
        if let ClassifiedRef::Pending(pending) = value
            && !pending.arithmetic()
        {
            // An unresolved value has no arm to select yet. In particular,
            // unavailable member schemas cannot reject a deferred Union.
            return WalkCheckedValue::accepted(
                attr,
                FitExactness::EXACT,
                FitKind::Deferred(pending.fit()),
            );
        }
        if let AttrTypeKind::Union(members) = &attr.kind {
            return match self.union_with_assignment(
                members.as_raw_slice(),
                value,
                path,
                ancestry,
                visited,
                assignment,
            ) {
                Ok(admitted) => {
                    let mut checked = WalkCheckedValue::accepted(
                        attr,
                        admitted.selected.fit.exactness,
                        FitKind::Union {
                            member: admitted.selected.member,
                            fit: Arc::new(admitted.selected.fit),
                        },
                    );
                    checked.findings.assignment = admitted.assignment;
                    checked.findings.authored = admitted.authored;
                    checked.selected = Some(admitted.selected.member);
                    checked
                }
                Err(failure) => WalkCheckedValue {
                    local: failure.local.map(|selected| {
                        Fit::with_kind(
                            selected.fit.exactness,
                            attr,
                            FitKind::Union {
                                member: selected.member,
                                fit: Arc::new(selected.fit),
                            },
                        )
                    }),
                    findings: failure.findings,
                    selected: failure.selected,
                },
            };
        }
        let Some(concrete) = value.concrete() else {
            // Arithmetic has a known numeric result shape even while deferred.
            let fit = inspect_structure_with_defs_on_path(
                attr,
                value,
                self.defs,
                ancestry,
                CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
                visited,
            );
            return match fit {
                Some(fit) => WalkCheckedValue {
                    local: LocalOutcome::Accepted(fit),
                    findings: Default::default(),
                    selected: Some(attr),
                },
                None => WalkCheckedValue::rejected(
                    path,
                    TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
                ),
            };
        };
        match (&attr.kind, concrete) {
            (
                AttrTypeKind::List {
                    element_type,
                    constraints,
                    ..
                },
                ConcreteValueRef::List(_items),
            ) => self.list(
                attr,
                element_type,
                *constraints,
                &value.list().expect("classified list membership"),
                false,
                path,
                ancestry,
                assignment,
            ),
            (
                AttrTypeKind::List {
                    element_type,
                    constraints,
                    ..
                },
                ConcreteValueRef::StringList(items),
            ) => {
                let items = items
                    .iter()
                    .map(|item| Value::Concrete(ConcreteValue::String(item.clone())))
                    .collect::<Vec<Value>>();
                let classified = match items
                    .iter()
                    .map(|value| {
                        value
                            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                            .map(|proof| proof.into_classification())
                    })
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(classified) => classified,
                    Err(error) => return WalkCheckedValue::rejected(path, error.into()),
                };
                let cached = classified
                    .iter()
                    .map(ClassifiedRef::from)
                    .collect::<Vec<_>>();
                // Final inputs include semantic callbacks for compact lists;
                // authored and historical StringLists retain local checks.
                ValueValidator::<M>::with_mode(
                    self.defs,
                    match self.mode {
                        ValueValidationMode::Declaration => None,
                        ValueValidationMode::FinalProviderInput => self.lookup,
                    },
                )
                .with_scope(self.scope)
                .list(
                    attr,
                    element_type,
                    *constraints,
                    &cached,
                    true,
                    path,
                    ancestry,
                    None,
                )
            }
            (AttrTypeKind::Struct { .. }, ConcreteValueRef::Map(map)) => {
                self.struct_value(attr, value, map, path, ancestry, assignment)
            }
            (AttrTypeKind::Map { .. }, ConcreteValueRef::Map(map)) => {
                self.map_value(attr, value, map, path, ancestry, assignment)
            }
            (AttrTypeKind::Struct { name, .. }, ConcreteValueRef::List(_)) => {
                WalkCheckedValue::rejected(
                    path,
                    TypeError::BlockSyntaxNotAllowed {
                        attribute: name.clone(),
                    },
                )
            }
            (
                AttrTypeKind::List { .. } | AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. },
                _,
            ) => WalkCheckedValue::rejected(
                path,
                TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
            ),
            _ => match value {
                ClassifiedRef::Known(value) => {
                    if self.scope == AssignmentScope::ClaimedValues
                        && assignment
                            .as_ref()
                            .is_none_or(|walk| !walk.claim.is_claimed())
                    {
                        WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::PassThrough)
                    } else {
                        self.leaf(attr, value, path, ancestry, visited)
                    }
                }
                ClassifiedRef::Shape(_) => WalkCheckedValue::rejected(
                    path,
                    TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
                ),
                ClassifiedRef::Pending(_) => {
                    unreachable!("numeric pending values are fitted above")
                }
                ClassifiedRef::Failed(_) => unreachable!("attempted failures were handled above"),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn struct_value<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        map: &indexmap::IndexMap<String, Value<P, B>>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let AttrTypeKind::Struct {
            name,
            fields,
            field_groups,
            ..
        } = &attr.kind
        else {
            unreachable!("struct dispatch")
        };
        let mut checked = WalkCheckedValue::accepted(
            attr,
            FitExactness::EXACT,
            FitKind::Struct(Vec::new().into()),
        );
        for field in fields {
            if ancestry.effective_input_mode(field).is_required()
                && !struct_field_is_present(fields, map, field, ancestry)
            {
                let error = TypeError::MissingRequired {
                    name: field.name.clone(),
                };
                checked.reject(path, error.clone());
                checked
                    .findings
                    .push_local(path.push_field(field.name.clone()), error);
            }
        }
        let groups =
            violated_struct_field_groups(fields, field_groups, map, ancestry).collect::<Vec<_>>();
        for group in &groups {
            checked.findings.push_local(
                path.clone(),
                TypeError::FieldGroupViolation {
                    path: path.clone(),
                    group: (*group).clone(),
                },
            );
        }
        let known = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>();
        let mut fits = Vec::new();
        let mut exactness = FitExactness::EXACT;
        let cached = value.map().expect("classified map membership");
        for (key, _value) in map {
            let value = cached[key.as_str()];
            let route = ChildRoute::StructField {
                key: std::borrow::Cow::Owned(crate::parser::AttributeKey::literal(key.to_owned())),
                alias: None,
            };
            let child_path = route.path(path);
            let field = struct_field_for_key(fields, key, ancestry);
            let child = match field {
                Some(field) => match validate_struct_field_assignment(field, key, ancestry) {
                    Some(error) => WalkCheckedValue::rejected(&child_path, error),
                    None => match step_assignment(assignment.as_ref(), |cursor| {
                        ChildRoute::StructField {
                            key: std::borrow::Cow::Owned(crate::parser::AttributeKey::literal(
                                key.to_owned(),
                            )),
                            alias: ancestry.block_name(field),
                        }
                        .step(cursor)
                    }) {
                        Err(error) => WalkCheckedValue::rejected(&child_path, error),
                        Ok(child_assignment) => self.walk_with_assignment(
                            &field.field_type,
                            value,
                            &child_path,
                            ancestry.descend(field),
                            &mut Vec::new(),
                            child_assignment,
                        ),
                    },
                },
                None => WalkCheckedValue::rejected(
                    &child_path,
                    TypeError::UnknownStructField {
                        struct_name: name.clone(),
                        field: key.clone(),
                        suggestion: suggest_similar_name(key, &known, &[]),
                    },
                ),
            };
            let child = child.wrap_local_errors(|error| {
                // Unknown/writability errors are already anchored to their field.
                if field.is_none() {
                    error
                } else {
                    error.at_struct_field(key)
                }
            });
            if let LocalOutcome::Rejected { primary } = &child.local {
                checked.reject_primary(primary.clone());
            }
            checked
                .findings
                .assignment
                .extend(child.findings.assignment);
            checked.findings.authored.extend(child.findings.authored);
            checked.findings.located.extend(child.findings.located);
            checked.findings.local.extend(child.findings.local);
            checked
                .findings
                .additional_local_errors
                .extend(child.findings.additional_local_errors);
            checked.findings.provider.extend(child.findings.provider);
            if let Some(fit) = child.local.into_fit() {
                exactness.combine(fit.exactness);
                fits.push(fit);
            }
        }
        if let Some(group) = groups.first() {
            checked.reject(
                path,
                TypeError::FieldGroupViolation {
                    path: FieldPath::new(),
                    group: (*group).clone(),
                },
            );
        }
        checked.local = checked
            .local
            .map(|_| Fit::with_kind(exactness, attr, FitKind::Struct(fits.into())));
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn map_value<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        map: &indexmap::IndexMap<String, Value<P, B>>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let AttrTypeKind::Map {
            key,
            value: value_type,
            constraints,
        } = &attr.kind
        else {
            unreachable!("map dispatch")
        };
        let mut checked = WalkCheckedValue::accepted(
            attr,
            FitExactness::GENERIC_MAP,
            FitKind::Map(Vec::new().into()),
        );
        if let Err(error) = constraints.check(map.len()) {
            checked.reject(path, error.clone());
            checked.findings.push_local(path.clone(), error);
        }
        let mut fits = Vec::new();
        let mut exactness = FitExactness::GENERIC_MAP;
        let mut first_value_error = None;
        let mut additional_value_errors = Vec::new();
        let cached = value.map().expect("classified map membership");
        for (map_key, _map_value) in map {
            let map_value = cached[map_key.as_str()];
            let route = ChildRoute::MapValue(std::borrow::Cow::Owned(
                crate::parser::AttributeKey::literal(map_key.to_owned()),
            ));
            let next_path = route.path(path);
            let key_value: Value<P, B> = lift_map_key(key, map_key, self.defs);
            let key_proof = match key_value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
            {
                Ok(proof) => proof,
                Err(error) => return WalkCheckedValue::rejected(path, error.into()),
            };
            // The already-visited container covers its keys. Assignment
            // and provider callbacks remain disabled for key validation.
            let key_checked = ValueValidator::<M>::with_mode(self.defs, None)
                .with_scope(self.scope)
                .walk_with_assignment(
                    key,
                    (&key_proof).into(),
                    &next_path,
                    ancestry,
                    &mut Vec::new(),
                    None,
                );
            if let Some(error) = key_checked.local.error().cloned() {
                let error = TypeError::MapKeyError {
                    key: map_key.clone(),
                    inner: Box::new(error),
                };
                checked.reject(path, error.clone());
                checked.findings.push_local(next_path.clone(), error);
            }
            let child = match step_assignment(assignment.as_ref(), |cursor| route.step(cursor)) {
                Err(error) => WalkCheckedValue::rejected(&next_path, error),
                Ok(child_assignment) => self.walk_with_assignment(
                    value_type,
                    map_value,
                    &next_path,
                    ancestry,
                    &mut Vec::new(),
                    child_assignment,
                ),
            };
            let has_coarse_error = child
                .findings
                .local
                .iter()
                .any(|(_, error)| !matches!(error, TypeError::FieldGroupViolation { .. }));
            if let Some(error) = child.local.error().cloned().filter(|_| has_coarse_error) {
                // The collector's coarse diagnostic always uses
                // MapValueError, even when the primary inner error
                // is a field group with its own precise path.
                first_value_error.get_or_insert(TypeError::MapValueError {
                    key: map_key.clone(),
                    inner: Box::new(error),
                });
            }
            let child = child.wrap_local_errors(|error| error.at_map_value(map_key));
            // Ordinary map entries retain the collector's first-error
            // precedence. Undecided branches each retain a finding;
            // field groups already have precise findings below.
            additional_value_errors.extend(
                child
                    .findings
                    .additional_local_errors
                    .iter()
                    .filter(|finding| {
                        !matches!(finding.error, TypeError::FieldGroupViolation { .. })
                    })
                    .cloned(),
            );
            if let LocalOutcome::Rejected { primary } = &child.local {
                checked.reject_primary(primary.clone());
            }
            checked
                .findings
                .assignment
                .extend(child.findings.assignment);
            checked.findings.authored.extend(child.findings.authored);
            checked.findings.located.extend(child.findings.located);
            checked.findings.local.extend(
                child
                    .findings
                    .local
                    .into_iter()
                    .filter(|(_, error)| matches!(error, TypeError::FieldGroupViolation { .. })),
            );
            checked.findings.provider.extend(child.findings.provider);
            checked
                .findings
                .additional_local_errors
                .extend(child.findings.additional_local_errors);
            if let (Some(key_fit), Some(value_fit)) =
                (key_checked.local.into_fit(), child.local.into_fit())
            {
                exactness.combine(key_fit.exactness);
                exactness.combine(value_fit.exactness);
                fits.push(MapEntryFit {
                    canonical_key: canonical_enum_key_from_fit(&key_fit),
                    value: value_fit,
                });
            }
        }
        if let Some(error) = first_value_error {
            checked.findings.local.push((path.clone(), error));
        }
        checked.findings.local.extend(
            additional_value_errors
                .into_iter()
                .map(|finding| (path.clone(), finding.error)),
        );
        checked.local = checked
            .local
            .map(|_| Fit::with_kind(exactness, attr, FitKind::Map(fits.into())));
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn list<'w, 'v: 'w, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        inner: &'a CompiledAttributeType,
        constraints: ListConstraints,
        items: &[ClassifiedRef<'_, 'v, P, B, T>],
        string_list: bool,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> WalkCheckedValue<'a, M>
    where
        'a: 'w,
    {
        let mut children = Vec::new();
        let mut child_fits = Vec::new();
        for (i, value) in items.iter().enumerate() {
            let child = match step_assignment(assignment.as_ref(), |cursor| {
                cursor.child(i, EdgeRole::ListItem)
            }) {
                Err(error) => WalkCheckedValue::rejected(&path.push_index(i), error),
                Ok(child_assignment) => self.walk_with_assignment(
                    inner,
                    *value,
                    &path.push_index(i),
                    ancestry,
                    &mut Vec::new(),
                    child_assignment,
                ),
            }
            .wrap_local_errors(|error| error.at_list_index(i));
            let (fit, error) = match child.local {
                LocalOutcome::Accepted(fit) => (Some(fit), None),
                LocalOutcome::Rejected { primary } => (None, Some(primary)),
            };
            child_fits.push(fit);
            children.push((error, child.findings));
        }
        let mut child_fits = child_fits.into_iter();
        let mut checked =
            WalkCheckedValue::accepted(attr, FitExactness::EXACT, FitKind::List(Vec::new().into()));
        let mut fits = Vec::new();
        let mut exactness = FitExactness::EXACT;
        let mut constraints_errors = Vec::new();
        for result in constraints.check(
            items.iter().map(|value| value.diagnostic_value()),
            self.defs,
            ancestry,
            CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
            |_| child_fits.next().flatten(),
        ) {
            match result {
                Ok((index, fit)) => {
                    if let Some(fit) = fit {
                        exactness.combine(fit.exactness);
                        fits.push(fit);
                    }
                    constraints_errors.push((Some(index), None));
                }
                Err(ListConstraintError { index, error }) => {
                    constraints_errors.push((index, Some(*error)))
                }
            }
        }
        // CLI keeps count/first-element precedence; the LSP keeps all count,
        // duplicate and element findings from this same traversal.
        for (index, error) in constraints_errors {
            if let Some(error) = error {
                checked.reject(path, error.clone());
                checked.findings.push_local(
                    index.map_or_else(|| path.clone(), |i| path.push_index(i)),
                    error,
                );
            } else if let Some(i) = index
                && let Some(error) = &children[i].0
            {
                checked.reject_primary(error.clone());
            }
        }
        for (_, findings) in children {
            checked.findings.assignment.extend(findings.assignment);
            checked.findings.authored.extend(findings.authored);
            checked.findings.located.extend(findings.located);
            checked.findings.local.extend(findings.local);
            checked
                .findings
                .additional_local_errors
                .extend(findings.additional_local_errors);
            checked.findings.provider.extend(findings.provider);
        }
        checked.local = checked.local.map(|_| {
            Fit::with_kind(
                exactness,
                attr,
                if string_list {
                    FitKind::StringList(fits.into())
                } else {
                    FitKind::List(fits.into())
                },
            )
        });
        checked
    }

    fn union_with_assignment<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        members: &'a [CompiledAttributeType],
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<WalkHandle<'w, P, B, M>>,
    ) -> Result<AdmittedUnion<'a, M>, Box<UnionFailure<'a, M>>>
    where
        'a: 'w,
    {
        let assignment = assignment.map(|mut walk| {
            walk.callbacks = walk.callbacks.probing();
            walk
        });
        let mut best_accepted: Option<AdmittedUnion<'a, M>> = None;
        let mut failure: Option<BestUnionMemberFailures> = None;
        let mut provider_failure: Option<UnionProviderFailure<'a, M>> = None;
        let mut sibling_provider_errors = Vec::new();
        let mut sibling_local_errors = Vec::new();
        let mut sibling_assignment = AssignmentFindings::<M::Channel>::default();
        let mut sibling_authored = SupplementalFindings::<M::Channel>::default();
        let mut selected_failure = None;
        let mut unmatched: Option<(&CompiledAttributeType, SupplementalFindings<M::Channel>)> =
            None;
        let mut claimed_failure: Option<UnionFailure<'a, M>> = None;
        for member in members {
            let member_assignment = assignment.clone().map(|mut walk| {
                if let Some(owner) = walk.claim.owner() {
                    walk.cursor = owner.clone();
                    walk.claim = Default::default();
                }
                walk
            });
            let checked = self.walk_with_assignment(
                member,
                value,
                path,
                ancestry,
                visited,
                member_assignment,
            );
            if checked.findings.authored.claim_effect() == ClaimEffect::Rejected {
                if claimed_failure.as_ref().is_none_or(|current| {
                    checked.findings.authored.matched_destination()
                        && !current.findings.authored.matched_destination()
                }) {
                    claimed_failure = Some(UnionFailure {
                        selected: Some(member),
                        local: checked.local.map(|fit| UnionFit { member, fit }),
                        findings: checked.findings,
                    });
                }
                continue;
            }
            let accepts = checked.accepts();
            if accepts {
                if let LocalOutcome::Accepted(fit) = checked.local {
                    if fit.exactness.is_maximal() {
                        return Ok(AdmittedUnion {
                            selected: UnionFit { member, fit },
                            assignment: checked.findings.assignment,
                            authored: checked.findings.authored,
                        });
                    }
                    if best_accepted
                        .as_ref()
                        .is_none_or(|current| fit > current.selected.fit)
                    {
                        best_accepted = Some(AdmittedUnion {
                            selected: UnionFit { member, fit },
                            assignment: checked.findings.assignment,
                            authored: checked.findings.authored,
                        });
                    }
                }
                continue;
            }
            let error = match checked.local {
                LocalOutcome::Accepted(fit) => {
                    // This is a local fit only. It supports list uniqueness
                    // when every provider rejects, but never Union acceptance.
                    match &mut provider_failure {
                        Some(best) => {
                            if fit > best.local_fit.fit {
                                best.local_fit = UnionFit { member, fit };
                                best.authored = checked.findings.authored;
                            }
                            // Provider-only failures use count and declaration
                            // order, independently of structural/local fit.
                            if checked.findings.provider.len() < best.provider.len() {
                                best.provider = checked.findings.provider;
                            }
                        }
                        None => {
                            provider_failure = Some(UnionProviderFailure {
                                local_fit: UnionFit { member, fit },
                                provider: checked.findings.provider,
                                authored: checked.findings.authored,
                            })
                        }
                    }
                    continue;
                }
                LocalOutcome::Rejected { primary } => primary.error,
            };
            let judgement = value
                .concrete()
                .map(|concrete| {
                    union_member_judgement_on_path(member, concrete, self.defs, ancestry, visited)
                })
                .unwrap_or_else(UnionMemberJudgement::no_match);
            if !judgement.failure_rank.is_match() {
                unmatched.get_or_insert((member, checked.findings.authored));
                continue;
            }
            let candidate = UnionMemberFailure {
                error,
                diagnostic_identity: judgement.diagnostic_identity,
            };
            match &mut failure {
                Some(current) if judgement.failure_rank > current.failure_rank => {
                    *current = BestUnionMemberFailures::new(judgement.failure_rank, candidate);
                    sibling_provider_errors = checked.findings.provider;
                    sibling_local_errors = checked.findings.additional_local_errors;
                    sibling_assignment = checked.findings.assignment;
                    sibling_authored = checked.findings.authored;
                    selected_failure = Some(member);
                }
                Some(current) if judgement.failure_rank == current.failure_rank => {
                    current.tied.push(candidate);
                    // Local diagnostics may describe all tied forms, but the
                    // provider findings belong only to the first tied arm.
                }
                Some(_) => {}
                None => {
                    failure = Some(BestUnionMemberFailures::new(
                        judgement.failure_rank,
                        candidate,
                    ));
                    sibling_provider_errors = checked.findings.provider;
                    sibling_local_errors = checked.findings.additional_local_errors;
                    sibling_assignment = checked.findings.assignment;
                    sibling_authored = checked.findings.authored;
                    selected_failure = Some(member);
                }
            }
        }
        if let Some(best) = best_accepted {
            return Ok(best);
        }
        if let Some(failure) = claimed_failure {
            return Err(Box::new(failure));
        }
        // A locally rejected arm's diagnostic rank cannot replace the
        // provider findings of a locally accepted arm, including at a tie.
        if let Some(failure) = provider_failure {
            return Err(Box::new(UnionFailure {
                selected: Some(failure.local_fit.member),
                local: LocalOutcome::Accepted(failure.local_fit),
                findings: CheckedFindings {
                    provider: failure.provider,
                    authored: failure.authored,
                    ..Default::default()
                },
            }));
        }
        let error = match (failure, value.concrete()) {
            (Some(failure), Some(concrete)) => failure.into_error(
                concrete,
                UnionMembers::new_after_union_on_path(members, self.defs, visited.clone()),
                ancestry,
            ),
            _ => TypeError::value_type_mismatch(
                members
                    .iter()
                    .map(CompiledAttributeType::type_name)
                    .collect::<Vec<_>>()
                    .join(" | "),
                value.type_name(),
            ),
        };
        if selected_failure.is_none()
            && let Some((member, authored)) = unmatched
        {
            selected_failure = Some(member);
            sibling_authored = authored;
        }
        Err(Box::new(UnionFailure {
            selected: selected_failure,
            local: LocalOutcome::Rejected {
                primary: PrimaryValueFinding::from_container(path.clone(), error),
            },
            findings: CheckedFindings {
                additional_local_errors: sibling_local_errors,
                provider: sibling_provider_errors,
                assignment: sibling_assignment,
                authored: sibling_authored,
                ..Default::default()
            },
        }))
    }

    fn leaf<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &KnownValue<'_, P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> WalkCheckedValue<'a, M> {
        let proof = value;
        let raw = value.to_value();
        let value = &raw;
        let fit = if matches!(attr.kind, AttrTypeKind::Enum { .. }) {
            match inspect_structure_with_defs_on_path(
                attr,
                ClassifiedRef::<P, B>::Known(proof),
                self.defs,
                ancestry,
                CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
                visited,
            ) {
                Some(fit) => fit,
                None => {
                    return WalkCheckedValue::rejected(
                        path,
                        attr.validate_leaf_value(value).err().unwrap_or_else(|| {
                            TypeError::value_type_mismatch(attr.type_name(), value.type_name())
                        }),
                    );
                }
            }
        } else {
            if let Err(error) = attr.validate_leaf_value(value) {
                return WalkCheckedValue::rejected(path, error);
            }
            let exactness = match value
                .as_concrete()
                .and_then(|value| scalar_value_kind_fit(attr, value))
            {
                Some(fit) => match fit.exactness() {
                    Some(exactness) => exactness,
                    None => {
                        return WalkCheckedValue::rejected(
                            path,
                            TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
                        );
                    }
                },
                None => FitExactness::EXACT,
            };
            Fit::with_kind(exactness, attr, FitKind::PassThrough)
        };
        let mut checked = WalkCheckedValue {
            local: LocalOutcome::Accepted(fit),
            findings: Default::default(),
            selected: Some(attr),
        };
        if let Some(lookup) = self.lookup
            && !has_native_validator(attr)
        {
            match self.canonical_leaf(attr, proof, checked.local.fit()) {
                Ok(Some(proof)) => {
                    if let Err(error) = lookup.validate(proof) {
                        checked.findings.provider.push((path.clone(), error));
                    }
                }
                Ok(None) => {}
                Err(CanonicalLeafError { declaration, error }) => {
                    checked.findings.provider.push((
                        path.clone(),
                        lookup.canonicalization_error(declaration, *error),
                    ));
                }
            }
        }
        checked
    }

    fn canonical_leaf<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &KnownValue<'_, P, B>,
        fit: Option<&Fit<'a>>,
    ) -> Result<Option<LocallyValidatedCustomType<'a>>, CanonicalLeafError<'a>> {
        let raw = value.to_value();
        let value = &raw;
        // Native validation already ran. No dispatch means no value expansion
        // or canonical spelling is needed, including for standalone annotations.
        if has_native_validator(attr) {
            return Ok(None);
        }
        let Some(declaration) = crate::parser::IdentifiedCustomTypeDeclaration::new(attr) else {
            return Ok(None);
        };
        // Native callbacks already ran after their portable guard; references
        // carry no provider validation capability.
        if matches!(&attr.kind, AttrTypeKind::Reference { .. }) {
            return Ok(None);
        }
        let value = if let Some(Fit {
            kind: FitKind::Enum { canonical, .. },
            ..
        }) = fit
        {
            Value::Concrete(ConcreteValue::CanonicalEnum(canonical.clone()))
        } else {
            let Some(value) = value
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
            else {
                return Ok(None);
            };
            value
        };
        let canonical =
            crate::provider::canonicalize_custom_type_value(attr, &value).map_err(|error| {
                CanonicalLeafError {
                    declaration,
                    error: Box::new(error.into_type_error()),
                }
            })?;
        Ok(canonical.map(|canonical| LocallyValidatedCustomType {
            declaration,
            value,
            canonical,
        }))
    }
}

fn has_native_validator(attr: &CompiledAttributeType) -> bool {
    match &attr.kind {
        AttrTypeKind::String {
            identity_layer: crate::schema::StringIdentityLayer { binding, .. },
            ..
        }
        | AttrTypeKind::Int {
            identity_layer: crate::schema::IntIdentityLayer { binding, .. },
            ..
        }
        | AttrTypeKind::Float { binding, .. } => binding.callback().is_some(),
        AttrTypeKind::Enum { validate, .. } => validate.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::CompiledAttributeType as AT;
    use super::*;

    #[test]
    fn unknown_key_target_survives_report_consumption() {
        let unknown = |name: &str| {
            ResourceValueValidation::from_errors(vec![TypeError::UnknownAttribute {
                name: name.into(),
                suggestion: None,
            }])
        };
        let mut report = unknown("first");
        assert_eq!(report.error_slice().len(), 1);
        report.attributes.insert("first".into(), Default::default());
        assert!(report.take_attribute("first").is_some());
        report.append_values(unknown("second"));
        assert_eq!(report.error_slice().len(), 2);
        let names = report
            .diagnostic_errors()
            .map(|(diagnostic, _)| diagnostic.path().map(|(name, _)| name))
            .collect::<Vec<_>>();
        assert_eq!(names, [Some("first"), Some("second")]);
    }

    #[test]
    fn unknown_key_is_not_covered_by_a_reference_finding() {
        let mut report = ResourceValueValidation::from_errors(vec![TypeError::UnknownAttribute {
            name: "typo".into(),
            suggestion: None,
        }]);
        report.attributes.insert(
            "typo".into(),
            ValueValidationFindings {
                symbolic: vec![SymbolicFinding::authored_hook(
                    LeafPath::from_segments([]),
                    TypeError::MissingRequired {
                        name: "other".into(),
                    },
                )],
                ..Default::default()
            },
        );
        let (_, coverage) = report.diagnostic_errors().next().unwrap();
        assert_eq!(coverage, SymbolicDiagnosticCoverage::Unowned);
    }

    #[test]
    fn nested_unknown_key_keeps_existing_symbolic_coverage() {
        let path = LeafPath::from_segments([LeafPathSegment::StructField("typo".into())]);
        let report = ResourceValueValidation {
            errors: vec![SchemaDiagnostic::from_value(
                "config",
                path.clone(),
                TypeError::UnknownStructField {
                    struct_name: "Config".into(),
                    field: "typo".into(),
                    suggestion: None,
                },
            )],
            attributes: indexmap::IndexMap::from([(
                "config".into(),
                ValueValidationFindings {
                    symbolic: vec![SymbolicFinding::ordinary(
                        path,
                        TypeError::MissingRequired {
                            name: "other".into(),
                        },
                    )],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let (_, coverage) = report.diagnostic_errors().next().unwrap();
        assert_eq!(coverage, SymbolicDiagnosticCoverage::Path);
    }

    #[test]
    fn taking_an_attribute_preserves_remaining_symbolic_diagnostic_order() {
        let mut report = ResourceValueValidation {
            attributes: ["zulu", "alpha", "quartz", "bravo", "mango", "delta"]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        ValueValidationFindings {
                            symbolic: vec![SymbolicFinding::authored_hook(
                                LeafPath::from_segments([]),
                                TypeError::UnknownAttribute {
                                    name: name.into(),
                                    suggestion: None,
                                },
                            )],
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        assert!(report.take_attribute("alpha").is_some());
        assert_eq!(
            report
                .symbolic_errors()
                .map(|(name, _, _)| name)
                .collect::<Vec<_>>(),
            ["zulu", "quartz", "bravo", "mango", "delta"],
        );
    }

    #[test]
    fn round7_symbolic_coverage_preserves_distinct_ordinary_findings() {
        let path = LeafPath::from_segments([]);
        let ordinary = TypeError::MissingRequired {
            name: "ordinary finding".into(),
        };
        let hook = TypeError::MissingRequired {
            name: "hook finding".into(),
        };
        let report = ResourceValueValidation {
            errors: vec![
                SchemaDiagnostic::from_value("input", path.clone(), ordinary),
                SchemaDiagnostic::from_value("input", path.clone(), hook.clone()),
            ],
            attributes: indexmap::IndexMap::from([(
                "input".into(),
                ValueValidationFindings {
                    symbolic: vec![SymbolicFinding::authored_hook(path, hook)],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let coverage = report
            .diagnostic_errors()
            .map(|(_, coverage)| coverage)
            .collect::<Vec<_>>();
        assert_eq!(
            coverage,
            [
                SymbolicDiagnosticCoverage::Path,
                SymbolicDiagnosticCoverage::Finding
            ]
        );
        assert!(
            coverage[0].owns_path(),
            "preserve CLI's established path ownership"
        );
        assert!(
            !coverage[0].owns_finding(),
            "LSP must retain a different ordinary finding"
        );
    }

    #[test]
    fn round8_ordinary_and_hook_findings_keep_distinct_origins() {
        let path = LeafPath::from_segments([]);
        let error = TypeError::MissingRequired {
            name: "same finding".into(),
        };
        for (finding, expected) in [
            (
                SymbolicFinding::ordinary(path.clone(), error.clone()),
                SymbolicDiagnosticCoverage::Path,
            ),
            (
                SymbolicFinding::authored_hook(path.clone(), error.clone()),
                SymbolicDiagnosticCoverage::Finding,
            ),
        ] {
            let origin = finding.origin();
            let report = ResourceValueValidation {
                errors: vec![SchemaDiagnostic::from_value(
                    "input",
                    path.clone(),
                    error.clone(),
                )],
                attributes: indexmap::IndexMap::from([(
                    "input".into(),
                    ValueValidationFindings {
                        symbolic: vec![finding],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            };
            let (_, coverage) = report.diagnostic_errors().next().unwrap();
            assert_eq!(coverage, expected);
            assert!(
                coverage.owns_path(),
                "CLI path ownership includes both origins"
            );
            assert_eq!(
                coverage.owns_finding(),
                origin == SymbolicFindingOrigin::AuthoredHook
            );
        }
    }

    #[test]
    fn round8_symbolic_origin_survives_diagnostic_protection() {
        let path = LeafPath::from_segments([LeafPathSegment::StructField("private_field".into())]);
        let error = TypeError::MissingRequired {
            name: "field".into(),
        };
        for finding in [
            SymbolicFinding::ordinary(path.clone(), error.clone()),
            SymbolicFinding::authored_hook(path, error),
        ] {
            let origin = finding.origin();
            let protected = finding.inside_secret();
            assert_eq!(protected.origin(), origin);
            assert!(
                !protected
                    .diagnostic()
                    .0
                    .to_string()
                    .contains("private_field")
            );
        }
    }

    fn text(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    #[test]
    fn issue_4276_round4_nested_module_if_wraps_every_branch_error() {
        let leaf = AT::refined_string(None, None, Some((None, Some(5))), None);
        let branch = Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    crate::parser::ParsedValue::resource_ref("gate", "flag", Vec::new()),
                    "child/main.crn".into(),
                    2,
                ),
                Value::Concrete(ConcreteValue::String("aaaaaa".into())),
                Value::Concrete(ConcreteValue::String("bbbbbbb".into())),
            ),
        )));
        for (ty, value, context) in [
            (
                AT::list(leaf.clone()),
                Value::Concrete(ConcreteValue::List(vec![branch.clone()])),
                "List item at index 0",
            ),
            (
                AT::struct_(
                    "Named",
                    vec![PatternStructField::optional("name", leaf.clone()).build()],
                ),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "name".into(),
                    branch.clone(),
                )]))),
                "Struct field 'name'",
            ),
            (
                AT::map(leaf.clone()),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "name".into(),
                    branch.clone(),
                )]))),
                "Map value for key 'name'",
            ),
            (
                AT::list(AT::map(leaf)),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(indexmap::IndexMap::from([("name".into(), branch)])),
                )])),
                "List item at index 0: Map value for key 'name'",
            ),
        ] {
            let findings = validate_value_locally(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                None,
            );
            assert_eq!(
                findings
                    .into_errors()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec![
                    format!(
                        "{context}: Invalid value 'aaaaaa': length 6 is outside allowed range unbounded..=5"
                    ),
                    format!(
                        "{context}: Invalid value 'bbbbbbb': length 7 is outside allowed range unbounded..=5"
                    ),
                ]
            );
        }
    }

    #[test]
    fn issue_4276_round4_raw_resolver_entry_guards_local_validation() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(5))),
            None,
        );
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&ty).unwrap();
        let calls = std::sync::Mutex::new(Vec::new());
        let resolver = |_: crate::parser::CustomTypeValidationTarget<'_>, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Ok(())
        };
        let target = crate::parser::CustomTypeValidationTarget::Declaration {
            owner: "test",
            declaration,
        };
        let result = crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("aaaaaa"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        );
        assert!(matches!(
            result,
            Err(crate::parser::CustomTypeValidationError::InvalidDeclarationValue { .. })
        ));
        assert!(calls.lock().unwrap().is_empty());
        crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("ok"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![text("ok")]);
    }

    #[test]
    fn issue_4276_round4_resolver_implements_only_proof_dispatch() {
        struct Resolver(std::sync::Mutex<Vec<String>>);
        impl crate::parser::CustomTypeResolver for Resolver {
            fn validate_validated(
                &self,
                owner: &str,
                proof: LocallyValidatedCustomType<'_>,
            ) -> Result<(), crate::parser::CustomTypeValidationError> {
                assert_eq!(owner, "test");
                assert_eq!(proof.declaration().identity(), &TypeIdentity::bare("Short"));
                self.0
                    .lock()
                    .unwrap()
                    .push(proof.canonical().as_str().to_string());
                Ok(())
            }
        }
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(5))),
            None,
        );
        let target = crate::parser::CustomTypeValidationTarget::Declaration {
            owner: "test",
            declaration: crate::parser::IdentifiedCustomTypeDeclaration::new(&ty).unwrap(),
        };
        let resolver = Resolver(Default::default());
        let error = crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("aaaaaa"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "test rejected `Short`: Invalid Short value 'aaaaaa': length 6 is outside allowed range unbounded..=5"
        );
        assert!(resolver.0.lock().unwrap().is_empty());
        crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("ok"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*resolver.0.lock().unwrap(), vec!["ok"]);
    }

    #[test]
    fn issue_4276_round2_a_local_union_keeps_selected_provider_findings() {
        let remote = |name| AT::refined_string(Some(TypeIdentity::bare(name)), None, None, None);
        let lookup = |declaration: &AT, _: &Value| {
            let identity = crate::provider::identified_custom_type_identity(declaration).unwrap();
            Err(TypeError::ValidationFailed {
                message: format!("{} rejects bad", identity.kind).into(),
            })
        };
        let cases = [
            (
                AT::union(vec![
                    AT::struct_(
                        "Named",
                        vec![
                            CompiledStructField::optional("name", remote("VpcId")).build(),
                            CompiledStructField::required("size", AT::int()).build(),
                        ],
                    ),
                    AT::bool(),
                ]),
                "name",
                "Required attribute 'size' is missing",
            ),
            (
                AT::union(vec![
                    AT::struct_(
                        "First",
                        vec![
                            CompiledStructField::required("v", remote("VpcId")).build(),
                            CompiledStructField::required("x", AT::int()).build(),
                        ],
                    ),
                    AT::struct_(
                        "Second",
                        vec![
                            CompiledStructField::required("v", remote("SubnetId")).build(),
                            CompiledStructField::required("y", AT::int()).build(),
                        ],
                    ),
                ]),
                "v",
                "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
            ),
        ];
        for (ty, field, local_message) in cases {
            let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                field.into(),
                text("bad"),
            )])));
            let errors = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            )
            .into_errors();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec![local_message, "Validation failed: VpcId rejects bad",]
            );
        }
    }

    #[test]
    fn issue_4276_round2_b_provider_only_union_uses_count_then_declaration_order() {
        let remote = |name| AT::refined_string(Some(TypeIdentity::bare(name)), None, None, None);
        let lookup = |declaration: &AT, value: &Value| {
            let identity = crate::provider::identified_custom_type_identity(declaration).unwrap();
            if identity.kind == "SubnetId" && value == &text("subnet-1") {
                return Ok(());
            }
            Err(TypeError::ValidationFailed {
                message: format!(
                    "{} rejects {}",
                    identity.kind,
                    if value == &text("bad") {
                        "bad"
                    } else {
                        "subnet-1"
                    }
                )
                .into(),
            })
        };
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".into(), text("bad")),
            ("b".into(), text("subnet-1")),
        ])));
        for tie in [false, true] {
            for reverse in [false, true] {
                let mut arms = vec![
                    AT::map(remote("SubnetId")),
                    AT::struct_(
                        "Named",
                        vec![
                            CompiledStructField::required("a", remote("VpcId")).build(),
                            CompiledStructField::required(
                                "b",
                                if tie { AT::string() } else { remote("VpcId") },
                            )
                            .build(),
                        ],
                    ),
                ];
                if reverse {
                    arms.reverse();
                }
                let errors = validate_value_with_lookup(
                    &AT::union(arms),
                    &(value)
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .unwrap(),
                    &BTreeMap::new(),
                    FieldAncestry::WRITABLE,
                    &lookup,
                )
                .into_errors();
                assert_eq!(
                    errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    vec![if tie && reverse {
                        "Validation failed: VpcId rejects bad"
                    } else {
                        "Validation failed: SubnetId rejects bad"
                    }],
                    "tie={tie}, reverse={reverse}"
                );
            }
        }
    }

    #[test]
    fn issue_4276_round2_e_deferred_union_does_not_expand_unloaded_arms() {
        let unloaded = |name| AT {
            kind: AttrTypeKind::UnloadedSchemaType(
                crate::parser::UnloadedSchemaType::try_from(TypeIdentity::bare(name)).unwrap(),
            ),
        };
        let members = vec![unloaded("VpcId"), unloaded("SubnetId")];
        let ty = AT::union(members.clone());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let lookup = |_: &AT, _: &Value| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let defs = BTreeMap::new();
        for value in [
            Value::root_resource_ref("source", "id", Vec::new()),
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: Vec::new(),
                source_line: 1.into(),
            }),
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                    binding: crate::resource::RootScope::new()
                        .resolve(crate::resource::RelativeBindingName::new("upstream")),
                },
            )),
        ] {
            let checked = ValueValidator::new(&defs, Some(&lookup)).walk(
                &ty,
                (&(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap())
                    .into(),
                &FieldPath::new(),
                FieldAncestry::WRITABLE,
                &mut Vec::new(),
            );
            let deferred = matches!(
                checked.local,
                LocalOutcome::Accepted(Fit {
                    kind: FitKind::Deferred(_),
                    ..
                })
            );
            assert_eq!(checked.into_findings().into_errors(), Vec::new());
            assert!(deferred);
        }
        for member in members {
            let errors = validate_value_with_lookup(
                &member,
                &(text("bad"))
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &defs,
                FieldAncestry::WRITABLE,
                &lookup,
            )
            .into_errors();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec!["provider custom types are not loaded"]
            );
            assert!(
                matches!(&errors[0], TypeError::CustomTypeValidation { error, .. } if matches!(error.as_ref(), crate::parser::CustomTypeValidationError::ProvidersNotLoaded))
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn rejected_value_records_its_finding_before_it_is_merged() {
        let path = FieldPath::new().push_field("destination").push_field("arn");
        let error = TypeError::ReadOnlyAttribute { name: "arn".into() };
        let checked = CheckedValue::rejected(&path, error.clone());

        assert_eq!(checked.findings.local, vec![(path, error)]);
    }

    #[test]
    fn issue_4276_round2_f_rejection_retains_a_primary_finding_without_manual_push() {
        let ty = AT::bool();
        let path = FieldPath::new().push_field("params").push_index(2);
        let checked = CheckedValue::rejected(
            &path,
            TypeError::MissingRequired {
                name: "size".into(),
            },
        );
        assert_eq!(
            checked
                .into_findings()
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(
                "params[2]".into(),
                "Required attribute 'size' is missing".into()
            ),]
        );
        let pending_value: Value = Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        ));
        let Classification::Pending(pending) = pending_value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap()
        else {
            panic!("pending fixture")
        };
        let mut checked =
            CheckedValue::accepted(&ty, FitExactness::EXACT, FitKind::Deferred(pending.fit()));
        checked.reject(
            &FieldPath::new(),
            TypeError::MissingRequired {
                name: "size".into(),
            },
        );
        let findings = checked.into_findings();
        assert_eq!(
            findings
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(String::new(), "Required attribute 'size' is missing".into()),]
        );
        assert_eq!(
            findings
                .into_errors()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["Required attribute 'size' is missing"]
        );
    }

    #[test]
    fn issue_4276_round1_nested_union_requires_one_complete_arm() {
        let remote = AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None);
        let defs = BTreeMap::from([(
            "Inner".into(),
            AT::union(vec![
                AT::map(remote),
                AT::struct_(
                    "Named",
                    vec![CompiledStructField::required("name", AT::string()).build()],
                ),
            ]),
        )]);
        let ty = AT::union(vec![AT::ref_("Inner"), AT::bool()]);
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "foo".into(),
            text("bad"),
        )])));
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let errors = validate_value_with_lookup(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &defs,
            FieldAncestry::WRITABLE,
            &lookup,
        )
        .into_errors();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Validation failed: VpcId rejects bad"]
        );
    }

    #[test]
    fn issue_4276_round1_union_fit_belongs_to_provider_clean_arm() {
        let map = AT::map(AT::string());
        let named = AT::struct_(
            "Named",
            vec![
                CompiledStructField::required(
                    "name",
                    AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None),
                )
                .build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".into(),
            text("bad"),
        )])));
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        for members in [vec![map.clone(), named.clone()], vec![named, map]] {
            let ty = AT::union(members);
            let checked = ValueValidator::new(empty_defs_for_schema_walks(), Some(&lookup)).walk(
                &ty,
                (&(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap())
                    .into(),
                &FieldPath::new(),
                FieldAncestry::WRITABLE,
                &mut Vec::new(),
            );
            assert!(checked.accepts());
            assert!(
                matches!(checked.local, LocalOutcome::Accepted(Fit { kind: FitKind::Union { member, .. }, .. }) if matches!(member.kind, AttrTypeKind::Map { .. })),
                "the provider-rejected Struct cannot supply an accepting Union's fit"
            );
        }
    }

    #[test]
    fn issue_4276_round1_union_provider_local_tie_keeps_provider_error() {
        let ty = AT::union(vec![
            AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None),
            AT::refined_string(
                None,
                Some(
                    CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                        "^x",
                        crate::schema::PatternMatch::Full,
                    ))
                    .unwrap(),
                ),
                None,
                None,
            ),
        ]);
        let value = text("bad");
        assert!(
            validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                no_lookup()
            )
            .into_errors()
            .is_empty()
        );
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let errors = validate_value_with_lookup(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        )
        .into_errors();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Validation failed: VpcId rejects bad"]
        );
    }

    #[test]
    fn issue_4276_round1_module_if_reports_local_errors_and_guards_provider_dispatch() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(3))),
            None,
        );
        let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    Value::Concrete(ConcreteValue::Bool(true)),
                    "module/main.crn".into(),
                    2,
                ),
                Value::Concrete(ConcreteValue::String("ok".into())),
                Value::Concrete(ConcreteValue::String("too long".into())),
            ),
        )));
        let calls = std::sync::Mutex::new(Vec::new());
        let lookup = |_: &AT, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Ok(())
        };
        let findings = validate_value_with_lookup(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![text("ok")],
            "a locally invalid branch must never reach a provider"
        );
        assert_eq!(
            findings
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(
                String::new(),
                "Invalid Short value 'too long': length 8 is outside allowed range unbounded..=3"
                    .into()
            ),]
        );
        assert_eq!(
            findings
                .into_errors()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["Invalid Short value 'too long': length 8 is outside allowed range unbounded..=3",]
        );
    }

    #[test]
    fn issue_4276_round2_c_module_if_reports_each_branchs_local_and_provider_errors() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("VpcId")),
            None,
            Some((None, Some(5))),
            None,
        );
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        for both_local in [false, true] {
            let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(
                crate::parser::DeferredValueIf::new(
                    crate::parser::DeferredIfCondition::new(
                        Value::Concrete(ConcreteValue::Bool(true)),
                        "child/main.crn".into(),
                        2,
                    ),
                    Value::Concrete(ConcreteValue::String(
                        if both_local { "bad-long" } else { "bad" }.into(),
                    )),
                    Value::Concrete(ConcreteValue::String("vpc-123".into())),
                ),
            )));
            let findings = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            );
            assert_eq!(findings.local.len(), if both_local { 2 } else { 1 });
            assert_eq!(
                findings
                    .into_errors()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                if both_local {
                    vec![
                        "Invalid VpcId value 'bad-long': length 8 is outside allowed range unbounded..=5",
                        "Invalid VpcId value 'vpc-123': length 7 is outside allowed range unbounded..=5",
                    ]
                } else {
                    vec![
                        "Invalid VpcId value 'vpc-123': length 7 is outside allowed range unbounded..=5",
                        "Validation failed: VpcId rejects bad",
                    ]
                }
            );
        }
    }

    #[test]
    fn issue_4276_round1_map_field_group_collector_keeps_map_value_wrapper() {
        let choice = AT::struct_with_field_groups(
            "Choice",
            vec![
                CompiledStructField::optional("first", AT::string()).build(),
                CompiledStructField::optional("second", AT::string()).build(),
            ],
            vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
        );
        let ty = AT::map(AT::struct_(
            "Entry",
            vec![
                CompiledStructField::optional("a", choice).build(),
                CompiledStructField::optional("b", AT::int()).build(),
            ],
        ));
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "entry".into(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "a".into(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                ),
                ("b".into(), text("bad")),
            ]))),
        )])));
        let findings = validate_value_locally(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            None,
        );
        assert_eq!(findings.local.len(), 2);
        assert_eq!(findings.local[0].0.to_string(), "entry.a");
        assert!(
            matches!(&findings.local[1].1, TypeError::MapValueError { key, inner }
            if key == "entry" && matches!(inner.as_ref(), TypeError::FieldGroupViolation { path, .. } if path.to_string() == "a")),
            "{:?}",
            findings.local
        );
        assert_eq!(
            findings.local_error.unwrap().to_string(),
            "entry.a: exactly one of [first, second] must be set"
        );
    }

    #[test]
    fn issue_4276_round1_provider_dispatch_accepts_struct_block_alias() {
        let ty = AT::struct_(
            "Settings",
            vec![
                CompiledStructField::optional(
                    "entries",
                    AT::list(AT::refined_string(
                        Some(TypeIdentity::bare("VpcId")),
                        None,
                        None,
                        None,
                    )),
                )
                .with_block_name("entry")
                .build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "entry".into(),
            Value::Concrete(ConcreteValue::List(vec![text("bad")])),
        )])));
        let calls = std::sync::Mutex::new(Vec::new());
        let lookup = |_: &AT, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let findings = validate_value_with_lookup(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        );
        assert_eq!(*calls.lock().unwrap(), vec![text("bad")]);
        assert!(findings.local_error.is_none());
        assert_eq!(findings.provider.len(), 1);
        assert_eq!(findings.provider[0].0.to_string(), "entry[0]");
    }
    #[test]
    fn combined_walk_native_callbacks_run_once_after_local_checks() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let ty = AT::list(AT::refined_string_with_validator(
            TypeIdentity::bare("Short"),
            None,
            Some((None, Some(3))),
            validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TypeError::ValidationFailed {
                    message: "native rejection".into(),
                })
            }),
            None,
        ));
        let value = Value::Concrete(ConcreteValue::List(vec![text("abcd"), text("bad")]));
        let checked = ValueValidator::new(empty_defs_for_schema_walks(), Some(no_lookup())).walk(
            &ty,
            (&(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap())
                .into(),
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            &mut Vec::new(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(checked.findings.local.len(), 2);
        assert!(checked.findings.provider.is_empty());
    }
}

#[cfg(test)]
mod rootless_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn issue_4276_round2_d_string_list_defaults_stay_outside_authored_validation() {
        let wire = carina_provider_protocol::types::Value::StringList(vec!["bad".into()]);
        let decoded = crate::protocol_convert::protocol_schema_default_to_core(&wire).unwrap();
        assert!(matches!(decoded, Value::Concrete(ConcreteValue::List(_))));
        for default in [
            Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
            decoded,
        ] {
            let schema = PatternResourceSchema::new("test.Sink")
                .attribute(
                    PatternAttributeSchema::optional(
                        "params",
                        PatternAttributeType::list(PatternAttributeType::refined_string(
                            Some(TypeIdentity::bare("VpcId")),
                            None,
                            None,
                            None,
                        )),
                    )
                    .with_default(default)
                    .build(),
                )
                .validate()
                .unwrap();
            let calls = AtomicUsize::new(0);
            let lookup = |_: &CompiledAttributeType, _: &Value| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(TypeError::ValidationFailed {
                    message: "VpcId rejects bad".into(),
                })
            };
            assert_eq!(schema.validate_with_origins_and_lookup::<crate::resource::NoPlaceholder, AbsoluteBindingAddress>(&indexmap::IndexMap::new(), &|_| false, &lookup), Ok(()));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "omitted defaults are not injected"
            );
            let values: [(Value, _); 2] = [
                (
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("bad".into()),
                    )])),
                    Err(vec![
                        "Resource validation failed: VpcId rejects bad".to_string(),
                    ]),
                ),
                (
                    Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
                    Ok(()),
                ),
            ];
            for (value, expected) in values {
                let result = schema
                    .validate_with_origins_and_lookup(
                        &crate::schema::classified_attributes(&HashMap::from([(
                            "params".into(),
                            value,
                        )]))
                        .unwrap(),
                        &|_| false,
                        &lookup,
                    )
                    .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>());
                assert_eq!(result, expected);
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "only ordinary List elements reach provider dispatch"
            );
        }
    }

    #[test]
    fn issue_4276_round5_string_list_keeps_local_guards_and_native_callbacks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let ty = CompiledAttributeType::list(CompiledAttributeType::refined_string_with_validator(
            TypeIdentity::bare("Short"),
            None,
            Some((None, Some(3))),
            validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        ));
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            panic!("StringList items must not reach provider dispatch")
        };
        for (input, valid) in [("ok", true), ("long", false)] {
            let value: Value = Value::Concrete(ConcreteValue::StringList(vec![input.into()]));
            let findings = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            );
            assert_eq!(findings.local_error.is_none(), valid);
            assert!(findings.provider.is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn issue_4276_round1_rootless_returns_only_primary_local_error() {
        let ty = RootlessType::new(CompiledAttributeType::list(
            CompiledAttributeType::refined_string(
                Some(TypeIdentity::bare("Short")),
                None,
                Some((None, Some(3))),
                None,
            ),
        ));
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("abcd".into())),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ]));
        let local = ty.validate(&value).unwrap_err();
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "provider rejected sibling".into(),
            })
        };
        assert_eq!(ty.validate_with_lookup(&value, &lookup), Err(vec![local]));
    }

    #[test]
    fn issue_4276_round1_resource_keeps_unique_list_and_sibling_provider_errors() {
        let schema = PatternResourceSchema::new("test.List")
            .attribute(
                PatternAttributeSchema::optional(
                    "values",
                    PatternAttributeType::refined_list(
                        PatternAttributeType::union(vec![
                            PatternAttributeType::refined_string(
                                Some(TypeIdentity::bare("Remote")),
                                None,
                                None,
                                None,
                            ),
                            PatternAttributeType::refined_string(
                                Some(TypeIdentity::bare("OtherRemote")),
                                None,
                                None,
                                None,
                            ),
                        ]),
                        true,
                        None,
                        ListElements::Unique,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("same".into())),
            Value::Concrete(ConcreteValue::String("same".into())),
        ]));
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "provider rejected sibling".into(),
            })
        };
        let errors = schema
            .validate_with_origins_and_lookup(
                &crate::schema::classified_attributes(&HashMap::from([("values".into(), value)]))
                    .unwrap(),
                &|_| false,
                &lookup,
            )
            .unwrap_err();
        assert_eq!(
            errors.len(),
            3,
            "resource findings must retain both provider failures: {errors:#?}"
        );
        assert!(
            matches!(&errors[0], TypeError::DuplicateListElement { index: 1, first_index: 0, attribute: Some(attribute), .. } if attribute == "values")
        );
        for error in &errors[1..] {
            assert_eq!(error.attribute_name(), Some("values"));
            assert_eq!(
                error.to_string(),
                "Resource validation failed: provider rejected sibling"
            );
        }
    }

    #[test]
    fn combined_walk_keeps_existing_provider_only_union_diagnostics() {
        for list in [false, true] {
            let member = |name| {
                let leaf = CompiledAttributeType::refined_string(
                    Some(TypeIdentity::bare(name)),
                    None,
                    None,
                    None,
                );
                if list {
                    CompiledAttributeType::list(leaf)
                } else {
                    leaf
                }
            };
            let ty = CompiledAttributeType::union(vec![member("a"), member("b")]);
            let text: Value = Value::Concrete(ConcreteValue::String("bad".into()));
            let value = if list {
                Value::Concrete(ConcreteValue::List(vec![text.clone(), text]))
            } else {
                text
            };
            let lookup = |declaration: &CompiledAttributeType, _: &Value| {
                let identity =
                    crate::provider::identified_custom_type_identity(declaration).unwrap();
                Err(TypeError::ValidationFailed {
                    message: format!("{} rejects", identity.kind).into(),
                })
            };
            let errors = RootlessType::new(ty)
                .validate_with_lookup(&value, &lookup)
                .unwrap_err();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec!["Validation failed: a rejects"; if list { 2 } else { 1 }]
            );
        }
    }

    #[test]
    fn combined_walk_rootless_checks_valid_siblings_and_masks_secrets() {
        let ty = CompiledAttributeType::list(CompiledAttributeType::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(3))),
            None,
        ));
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("abcd".into())),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ]));
        for ty in [
            ty.clone(),
            CompiledAttributeType::union(vec![ty, CompiledAttributeType::int()]),
        ] {
            for secret in [false, true] {
                let calls = AtomicUsize::new(0);
                let lookup = |_: &CompiledAttributeType, value: &Value| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(TypeError::ValidationFailed {
                        message: format!("provider rejected {value:?}").into(),
                    })
                };
                let value = if secret {
                    Value::Deferred(DeferredValue::Secret(Box::new(value.clone()).into()))
                } else {
                    value.clone()
                };
                let errors = RootlessType::new(ty.clone())
                    .validate_with_lookup(&value, &lookup)
                    .unwrap_err();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(errors.len(), 1);
                if secret {
                    let rendered = format!("{errors:?}");
                    assert!(!rendered.contains("abcd"), "{rendered}");
                    assert!(!rendered.contains("bad"), "{rendered}");
                }
            }
        }
    }
}

#[cfg(test)]
mod value_knowledge_failure_contract {
    use super::*;

    #[test]
    fn attempted_failure_keeps_known_sibling_and_container_findings() {
        let attr = CompiledAttributeType::list(CompiledAttributeType::int());
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("wrong-kind".into())),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: Vec::new(),
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "already attempted".into(),
                ),
                source_line: 0.into(),
            }),
        ]));
        assert!(matches!(
            crate::resource::knowledge::PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(crate::resource::knowledge::KnowledgeError::Unavailable { .. })
        ));
        let findings = validate_value_locally(
            &attr,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            None,
        );
        assert!(
            findings.local_error().is_some(),
            "the known invalid sibling must still be checked"
        );
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl super::ValidatedResourceSchema {
    /// Transport fixtures must declare their supplied fields too. Existing
    /// destination declarations, callbacks and identity paths are preserved.
    #[doc(hidden)]
    pub fn with_fixture_attributes<
        P: crate::resource::ValueKind,
        B: crate::resource::BindingAddress,
    >(
        &self,
        attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
    ) -> Self {
        let missing = attributes
            .iter()
            .filter(|(key, _)| self.effective_attribute_by_authored_name(key).is_none())
            .map(|(key, value)| (key, crate::test_utils::fixture_value_type(value)))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return self.clone();
        }
        let mut draft = super::PatternResourceSchema::new(self.as_schema().resource_type());
        for (key, ty) in missing {
            draft = draft.attribute(super::PatternAttributeSchema::optional(key, ty).build());
        }
        let extra = draft.validate().expect("fixture field declarations");
        let mut result = self.clone();
        let schema = std::sync::Arc::make_mut(&mut result.schema);
        schema
            .attributes
            .extend(extra.as_schema().attributes.clone());
        result.admission_identity = Default::default();
        result.input_rule_declarations =
            std::sync::Arc::new(crate::input_rules::InputRuleDeclarations::new(schema));
        result
    }
}

#[cfg(test)]
mod round3_ordinary_walk_tests {
    use super::*;
    #[test]
    fn round3_plain_and_authored_walks_have_distinct_consuming_types() {
        let _: fn(OrdinaryCheckedValue<'static>) -> ValueValidationFindings =
            OrdinaryCheckedValue::into_findings;
        let _: fn(AuthoredCheckedValue<'static>, &mut WalkCoverage) -> ValueValidationFindings =
            AuthoredCheckedValue::into_findings_with_coverage;
        assert_eq!(std::mem::size_of::<NoConversions>(), 0);
    }
}

#[cfg(test)]
mod merge_1b_mode_tests {
    use super::*;
    #[test]
    fn no_authored_reference_mode_has_no_claim_or_conversion_storage() {
        let _: <NoAuthored as WalkMode>::Policy<'static, ParserPlaceholder, RelativeBindingName> =
            ();
        let _: <NoAuthored as WalkMode>::Claim<'static, RelativeBindingName> = NoClaim;
        let _: <NoAuthored as WalkMode>::Channel = NoConversions;
        assert_eq!(std::mem::size_of::<NoClaim>(), 0);
        assert_eq!(std::mem::size_of::<NoConversions>(), 0);
    }
}
