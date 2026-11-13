//! The consuming presentation boundary for authored validation findings.

use super::ResourceAdmissionFinding;
use crate::parser::retained::ExpressionReport;
use crate::resource::BindingAddress;

/// Context supplied by the owner of a validation pass, before presentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FindingSubject {
    #[default]
    Root,
    Module(String),
}

impl FindingSubject {
    pub(crate) fn prefix(&self, message: &str) -> String {
        match self {
            Self::Root => message.to_owned(),
            Self::Module(prefix) => message
                .lines()
                .map(|line| format!("{prefix}: {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Distinguishes a stopping ordinary error from a preserved notice.
#[derive(Clone, Copy)]
pub enum ClientFindingKind {
    Error,
    Notice,
}
/// An application's ordinary error type. ConversionError deliberately does
/// not implement this trait; it can enter presentation only through Conversion.
pub trait ValidationClientError: std::fmt::Debug + std::fmt::Display {
    fn kind(&self) -> ClientFindingKind;
}
/// An ordinary error type whose values always stop validation, unlike notices.
pub trait ValidationClientFailure: ValidationClientError {}
impl ValidationClientError for std::convert::Infallible {
    fn kind(&self) -> ClientFindingKind {
        match *self {}
    }
}

/// Every diagnostic consumer receives both alternatives through this enum.
#[derive(Debug)]
pub enum OrdinaryFinding<E: ValidationClientError = std::convert::Infallible> {
    Resource(Box<ResourceAdmissionFinding>),
    Client(Box<E>),
}
impl<E: ValidationClientError> std::fmt::Display for OrdinaryFinding<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resource(finding) => write!(f, "{finding}"),
            Self::Client(finding) => write!(f, "{finding}"),
        }
    }
}

#[derive(Debug)]
pub enum PresentedFinding<E: ValidationClientError = std::convert::Infallible> {
    Ordinary(OrdinaryFinding<E>),
    Conversion(PresentedConversion),
}

/// A conversion after the validation report's consuming exit.
#[derive(Debug, Clone)]
pub struct PresentedConversion {
    error: crate::ConversionError,
    subject: FindingSubject,
    message: String,
    human: String,
}

impl PresentedConversion {
    fn new(error: crate::ConversionError) -> Self {
        let subject = error.subject().clone();
        let message = subject.prefix(&error.diagnostic_message());
        let human = crate::parser::Located::new(error.location().clone(), &message).to_string();
        Self {
            error,
            subject,
            message,
            human,
        }
    }

    pub fn error(&self) -> &crate::ConversionError {
        &self.error
    }
    pub fn subject(&self) -> &FindingSubject {
        &self.subject
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn location(&self) -> &crate::parser::DeclarationLocation {
        self.error.location()
    }
    pub fn code(&self) -> &'static str {
        self.error.kind().code()
    }
}

impl std::fmt::Display for PresentedConversion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.human)
    }
}

impl<E: ValidationClientError> PresentedFinding<E> {
    pub fn resource_id(&self) -> Option<&crate::resource::ResourceId> {
        match self {
            Self::Ordinary(OrdinaryFinding::Resource(finding)) => finding.resource_id(),
            Self::Ordinary(OrdinaryFinding::Client(_)) | Self::Conversion(_) => None,
        }
    }
    pub fn location(&self) -> Option<&crate::parser::DeclarationLocation> {
        match self {
            Self::Ordinary(OrdinaryFinding::Resource(finding)) => finding.location(),
            Self::Ordinary(OrdinaryFinding::Client(_)) => None,
            Self::Conversion(finding) => Some(finding.location()),
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Ordinary(OrdinaryFinding::Resource(finding)) => finding.message(),
            Self::Ordinary(OrdinaryFinding::Client(finding)) => finding.to_string(),
            Self::Conversion(finding) => finding.message().to_owned(),
        }
    }
}

impl<E: ValidationClientError> std::fmt::Display for PresentedFinding<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ordinary(finding) => write!(f, "{finding}"),
            Self::Conversion(finding) => write!(f, "{finding}"),
        }
    }
}

/// Findings which have already crossed the report's one consuming exit.
///
/// ```compile_fail,E0277
/// use carina_core::validation::PresentedFindings;
/// fn ordinary_string(findings: PresentedFindings) -> String { format!("{findings}") }
/// ```
#[derive(Debug)]
pub struct PresentedFindings<E: ValidationClientError = std::convert::Infallible>(
    Vec<PresentedFinding<E>>,
);

impl<E: ValidationClientError> PresentedFindings<E> {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, PresentedFinding<E>> {
        self.0.iter()
    }
}
impl<E: ValidationClientError> IntoIterator for PresentedFindings<E> {
    type Item = PresentedFinding<E>;
    type IntoIter = std::vec::IntoIter<PresentedFinding<E>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
/// An unpresented report cannot expose just its ordinary channel.
///
/// ```compile_fail,E0599
/// use carina_core::validation::ResourceValidationReport;
/// fn omit_conversions(report: ResourceValidationReport) { let _ = report.ordinary(); }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{ConversionError, validation::ResourceAdmissionFinding};
/// fn ordinary(error: ConversionError) -> ResourceAdmissionFinding { error.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{ConversionError, validation::OrdinaryFinding};
/// fn ordinary(error: ConversionError) { let _ = OrdinaryFinding::Client(Box::new(error)); }
/// ```
pub struct ResourceValidationReport<E: ValidationClientError = std::convert::Infallible> {
    ordinary: Vec<(FindingSubject, OrdinaryFinding<E>)>,
    conversions: crate::ConversionFindings,
}

impl<E: ValidationClientError> std::fmt::Debug for ResourceValidationReport<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceValidationReport")
            .field("ordinary_count", &self.ordinary.len())
            .field("conversions", &self.conversions)
            .finish()
    }
}
impl<E: ValidationClientError> ResourceValidationReport<E> {
    /// The only exit for validation findings, including their complete context.
    pub fn into_presented(self) -> PresentedFindings<E> {
        let mut findings = self
            .ordinary
            .into_iter()
            .map(|(subject, finding)| {
                let finding = match (subject, finding) {
                    (FindingSubject::Root, finding) => finding,
                    (FindingSubject::Module(prefix), OrdinaryFinding::Resource(finding)) => {
                        OrdinaryFinding::Resource(Box::new((*finding).with_context(&prefix)))
                    }
                    (FindingSubject::Module(_), OrdinaryFinding::Client(finding)) => {
                        OrdinaryFinding::Client(finding)
                    }
                };
                PresentedFinding::Ordinary(finding)
            })
            .collect::<Vec<_>>();
        findings.extend(
            self.conversions
                .into_findings()
                .into_iter()
                .map(|error| PresentedFinding::Conversion(PresentedConversion::new(error))),
        );
        PresentedFindings(findings)
    }
}
#[derive(Clone, Copy)]
pub struct OrdinaryFailure {
    _private: (),
}

#[derive(Clone)]
pub(super) enum AdmissionBoundary {
    Live,
    Preflight,
}

type ResourceAdmissionParts<'a, 'file, D, B> = (
    &'a crate::parser::AnalysisResourceRef<
        'file,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    Result<
        Option<crate::schema::ResourceValueValidation>,
        crate::parser::retained::MissingEvidence,
    >,
);
pub struct ResourceAdmissionStep<
    'a,
    'file: 'a,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        > + 'file,
    B: BindingAddress + 'file,
> {
    resource: &'a crate::parser::AnalysisResourceRef<
        'file,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    values: Result<
        Option<crate::schema::ResourceValueValidation>,
        crate::parser::retained::MissingEvidence,
    >,
}
impl<
    'a,
    'file: 'a,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        > + 'file,
    B: BindingAddress + 'file,
> ResourceAdmissionStep<'a, 'file, D, B>
{
    pub fn into_parts(self) -> ResourceAdmissionParts<'a, 'file, D, B> {
        (self.resource, self.values)
    }
}

#[derive(PartialEq, Eq, Hash)]
struct AdmissionUse {
    declaration: Option<crate::parser::retained::SlotDeclarationId>,
    resource: crate::resource::ResourceId,
}
fn admission_use<D, B: BindingAddress>(
    resource: &crate::parser::AnalysisResourceRef<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
) -> (
    AdmissionUse,
    Option<crate::parser::retained::ProjectionGeneration>,
)
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    (
        AdmissionUse {
            declaration: resource.authored_declaration_id().cloned(),
            resource: resource.id().clone(),
        },
        resource
            .authored_slots()
            .and_then(|slots| slots.projection_generation()),
    )
}

/// One owner for placement and sink findings in a document set.
pub struct ConversionValidation<E: ValidationClientError = std::convert::Infallible> {
    conversions: crate::ConversionFindings,
    coverage: crate::schema::value_validation::WalkCoverage,
    ordinary: Vec<(FindingSubject, OrdinaryFinding<E>)>,
    // Work memo: NodeId diagnostic dedup cannot undo repeated provider callbacks.
    admitted_uses: std::collections::HashMap<
        AdmissionUse,
        Option<crate::parser::retained::ProjectionGeneration>,
    >,
}

impl<E: ValidationClientError> Default for ConversionValidation<E> {
    fn default() -> Self {
        Self {
            conversions: Default::default(),
            coverage: Default::default(),
            ordinary: Vec::new(),
            admitted_uses: Default::default(),
        }
    }
}
impl<E: ValidationClientError> Extend<E> for ConversionValidation<E> {
    fn extend<T: IntoIterator<Item = E>>(&mut self, iter: T) {
        for error in iter {
            self.push(error);
        }
    }
}
impl<E: ValidationClientError> ConversionValidation<E> {
    pub fn push(&mut self, error: E) {
        self.ordinary.push((
            FindingSubject::Root,
            OrdinaryFinding::Client(Box::new(error)),
        ));
    }
    pub fn fail(&mut self, error: E) -> OrdinaryFailure
    where
        E: ValidationClientFailure,
    {
        self.push(error);
        OrdinaryFailure { _private: () }
    }

    /// Collect placement findings already produced by the root Judge pass.
    pub fn for_directory<T>(parsed: &crate::config_loader::RootDirectoryParse<T>) -> Self {
        Self::new(parsed.parsed().expression_report())
    }

    /// Main's terminal provider-authority failure replaces ordinary errors;
    /// conversion evidence stays owned by this same run.
    pub fn replace_ordinary_error(&mut self, error: E) -> OrdinaryFailure
    where
        E: ValidationClientFailure,
    {
        self.ordinary.clear();
        self.fail(error)
    }

    /// Each step performs admission before yielding and records its use in
    /// this driver. Dropping the iterator produces no judgement for later slots.
    pub fn admitted_resources<'a, 'file: 'a, B, D, T>(
        &'a mut self,
        tree: &'a crate::parser::AnalysisTree<
            'file,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
            B,
            T,
        >,
        judged: &'a crate::parser::retained::JudgedFile<'_, T, D, B>,
        bindings: &'a crate::binding_index::BindingIndex<'_, B>,
        session: &'a dyn super::deferred_types::ProviderTypeCheckContext,
        subject: FindingSubject,
    ) -> impl Iterator<Item = ResourceAdmissionStep<'a, 'file, D, B>> + 'a
    where
        B: BindingAddress + 'file,
        T: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B> + 'file,
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            > + 'file,
    {
        self.include_in(tree.admission_report(), subject.clone());
        self.resource_steps(
            tree.project_resources(),
            judged,
            bindings,
            session,
            subject,
            AdmissionBoundary::Live,
        )
    }

    pub(super) fn resource_steps<'a, 'file: 'a, B, D, T>(
        &'a mut self,
        resources: impl Iterator<
            Item = &'a crate::parser::AnalysisResourceRef<
                'file,
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                D,
                B,
            >,
        > + 'a,
        judged: &'a crate::parser::retained::JudgedFile<'_, T, D, B>,
        bindings: &'a crate::binding_index::BindingIndex<'_, B>,
        session: &'a dyn super::deferred_types::ProviderTypeCheckContext,
        subject: FindingSubject,
        boundary: AdmissionBoundary,
    ) -> impl Iterator<Item = ResourceAdmissionStep<'a, 'file, D, B>> + 'a
    where
        B: BindingAddress + 'file,
        T: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B> + 'file,
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            > + 'file,
    {
        let mut resources = resources.filter(|resource| match resource.kind() {
            crate::parser::AnalysisItemKind::Composition => false,
            crate::parser::AnalysisItemKind::Resource
            | crate::parser::AnalysisItemKind::DataSource
            | crate::parser::AnalysisItemKind::DeferredResource
            | crate::parser::AnalysisItemKind::Wait => true,
        });
        match &boundary {
            AdmissionBoundary::Live => self.include_in(judged.report(), subject.clone()),
            AdmissionBoundary::Preflight => {}
        }
        std::iter::from_fn(move || {
            let resource = resources.next()?;
            let values = match resource.schema(judged.schemas()) {
                None => Ok(None),
                Some(schema) => {
                    super::admission::attributes::ResourceAttributeInputs::for_resource(
                        resource,
                        judged.report(),
                    )
                    .map(|inputs| {
                        let classified = inputs.classify(
                            &crate::resource::knowledge::KnowledgeContext::validation(),
                            |_, _| {},
                        );
                        let resource_subject = match &subject {
                            FindingSubject::Module(_) => subject.clone(),
                            FindingSubject::Root => resource
                                .module_source()
                                .and_then(crate::resource::ModuleSource::module_call)
                                .map_or(FindingSubject::Root, |module| {
                                    FindingSubject::Module(
                                        module
                                            .module_call_path()
                                            .calls()
                                            .map(|call| format!("{} ({})", call.module_name, call.display_label()))
                                            .collect::<Vec<_>>()
                                            .join(" -> "),
                                    )
                                }),
                        };
                        Some(self.coverage.in_subject(resource_subject, |coverage| {
                            let request = super::admission::attributes::ResourceAdmissionRequest {
                                resource, parsed: judged.file(), schema, attributes: &classified,
                                quoted: resource.quoted_string_attrs(), inputs: &inputs,
                                bindings, session, schemas: judged.schemas(),
                            };
                            match &boundary {
                                AdmissionBoundary::Live => super::admission::attributes::resource_attribute_admissions(
                                    request.resource, request.parsed, request.schema, request.attributes,
                                    request.quoted, request.inputs, request.bindings, request.session, request.schemas, coverage,
                                ),
                                AdmissionBoundary::Preflight => super::admission::attributes::resource_template_preflight_admissions(request, coverage),
                            }
                        }))
                    })
                }
            };
            if let AdmissionBoundary::Live = &boundary {
                self.record_admitted_resource(resource);
            }
            Some(ResourceAdmissionStep { resource, values })
        })
    }

    pub fn ordinary_failure(&self) -> Option<OrdinaryFailure> {
        self.ordinary.iter().find_map(|(_, finding)| match finding {
            OrdinaryFinding::Resource(_) => Some(OrdinaryFailure { _private: () }),
            OrdinaryFinding::Client(error) => match error.kind() {
                ClientFindingKind::Error => Some(OrdinaryFailure { _private: () }),
                ClientFindingKind::Notice => None,
            },
        })
    }
    /// Check sibling uses whose ordinary findings are outside this document.
    /// This is needed to report an alias rejection at its defining document;
    /// it neither seals a scope nor supplies completion evidence.
    pub fn admit_remaining_resources<'a, B, D, T>(
        &mut self,
        tree: &crate::parser::AnalysisTree<
            'a,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
            B,
            T,
        >,
        judged: &crate::parser::retained::JudgedFile<'_, T, D, B>,
        bindings: &crate::binding_index::BindingIndex<'_, B>,
        session: &dyn super::deferred_types::ProviderTypeCheckContext,
    ) where
        B: BindingAddress,
        T: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B>,
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            >,
    {
        for resource in tree.project_resources() {
            if !resource
                .authored_slots()
                .is_some_and(|slots| slots.carries_conversion(judged.report()))
            {
                continue;
            }
            let (identity, revision) = admission_use(resource);
            if self.admitted_uses.get(&identity).is_some_and(|admitted| {
                match (*admitted, revision) {
                    (Some(previous), Some(current)) => previous.latest(current) == previous,
                    (None, None) => true,
                    (Some(_), None) | (None, Some(_)) => false,
                }
            }) {
                continue;
            }
            if self.ordinary_failure().is_some() {
                continue;
            }
            self.walk_resource(resource, |coverage| {
                super::admission::attributes::unrendered_resource_conversion_admissions(
                    resource, judged, bindings, session, coverage,
                )
            });
            self.record_admitted_resource(resource);
        }
    }

    pub fn new<B: BindingAddress>(report: &ExpressionReport<B>) -> Self {
        let mut run = Self::default();
        run.include(report);
        run
    }
    pub fn include<B: BindingAddress>(&mut self, report: &ExpressionReport<B>) {
        self.include_in(report, FindingSubject::Root);
    }
    /// Collect produced placement findings even when an entry skips admission.
    pub fn include_file<X, D, B: BindingAddress>(
        &mut self,
        file: &crate::parser::File<
            X,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
            B,
        >,
    ) {
        self.include(file.expression_report());
    }
    pub fn include_in<B: BindingAddress>(
        &mut self,
        report: &ExpressionReport<B>,
        subject: FindingSubject,
    ) {
        self.conversions
            .record_batch(report.placements.clone(), subject);
    }

    pub(crate) fn produced_placements(&mut self, batch: std::sync::Arc<crate::ConversionChannel>) {
        self.conversions.record_batch(batch, FindingSubject::Root);
    }

    /// Admit one resource use with the run's shared coverage. Only this driver
    /// records admitted use identities; presentation clients supply no visited set.
    pub fn admit_resource<X, D, B: BindingAddress>(
        &mut self,
        request: super::admission::attributes::ResourceAdmissionRequest<'_, '_, X, D, B>,
    ) -> crate::schema::ResourceValueValidation
    where
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            >,
    {
        let resource = request.resource;
        let findings = self.walk_resource(resource, |coverage| {
            super::admission::attributes::resource_attribute_admissions(
                request.resource,
                request.parsed,
                request.schema,
                request.attributes,
                request.quoted,
                request.inputs,
                request.bindings,
                request.session,
                request.schemas,
                coverage,
            )
        });
        self.record_admitted_resource(resource);
        findings
    }

    fn record_admitted_resource<D, B: BindingAddress>(
        &mut self,
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
            B,
        >,
    ) where
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            >,
    {
        let (identity, revision) = admission_use(resource);
        self.admitted_uses
            .entry(identity)
            .and_modify(|previous| {
                *previous = match (*previous, revision) {
                    (Some(previous), Some(current)) => Some(previous.latest(current)),
                    (None, current) => current,
                    (previous @ Some(_), None) => previous,
                };
            })
            .or_insert(revision);
    }

    #[cfg(test)]
    pub(crate) fn work_for<B: BindingAddress>(&self, _report: &ExpressionReport<B>) -> usize {
        self.coverage.work()
    }
    pub(crate) fn walk_resource<'a, P, T, D, B, U>(
        &mut self,
        resource: &crate::parser::AnalysisResourceRef<'a, P, T, D, B>,
        walk: impl FnOnce(&mut crate::schema::value_validation::WalkCoverage) -> U,
    ) -> U
    where
        P: crate::resource::ValueKind,
        T: crate::resource::ValueKind,
        B: BindingAddress,
        D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    {
        let subject = resource
            .module_source()
            .and_then(crate::resource::ModuleSource::module_call)
            .map_or(FindingSubject::Root, |module| {
                FindingSubject::Module(
                    module
                        .module_call_path()
                        .calls()
                        .map(|call| format!("{} ({})", call.module_name, call.display_label()))
                        .collect::<Vec<_>>()
                        .join(" -> "),
                )
            });
        self.walk_in(subject, walk)
    }
    pub(crate) fn walk_in<T>(
        &mut self,
        subject: FindingSubject,
        walk: impl FnOnce(&mut crate::schema::value_validation::WalkCoverage) -> T,
    ) -> T {
        self.coverage.in_subject(subject, walk)
    }
    pub(super) fn ordinary(
        &mut self,
        findings: impl IntoIterator<Item = ResourceAdmissionFinding>,
        subject: FindingSubject,
    ) {
        self.ordinary.extend(findings.into_iter().map(|finding| {
            (
                subject.clone(),
                OrdinaryFinding::Resource(Box::new(finding)),
            )
        }));
    }
    pub fn complete(mut self) -> ResourceValidationReport<E> {
        self.conversions.extend(self.coverage.take_conversions());
        ResourceValidationReport {
            ordinary: self.ordinary,
            conversions: self.conversions,
        }
    }
}
