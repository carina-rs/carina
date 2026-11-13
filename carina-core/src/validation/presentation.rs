//! The consuming presentation boundary for authored validation findings.

use super::{ResourceAdmissionFinding, ResourceRefTypeValidationError};
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
impl<E: ValidationClientError> std::fmt::Display for PresentedFindings<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, finding) in self.0.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(f, "{finding}")?;
        }
        Ok(())
    }
}
impl<E: ValidationClientError> std::error::Error for PresentedFindings<E> {}
impl PresentedFindings {
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.0.iter().flat_map(|finding| match finding {
            PresentedFinding::Ordinary(OrdinaryFinding::Resource(finding)) => finding.human.lines(),
            PresentedFinding::Ordinary(OrdinaryFinding::Client(never)) => match **never {},
            PresentedFinding::Conversion(finding) => finding.human.lines(),
        })
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
impl ResourceValidationReport {
    pub(super) fn into_legacy_result(self) -> Result<(), ResourceRefTypeValidationError> {
        let findings = self.into_presented();
        if findings.is_empty() {
            Ok(())
        } else {
            Err(ResourceRefTypeValidationError::Validation(findings))
        }
    }
}

/// Only core's admission drivers can mint completion evidence.
///
/// ```compile_fail,E0624
/// use carina_core::validation::ConversionValidation;
/// let mut run = ConversionValidation::<std::convert::Infallible>::default();
/// run.admission_pass_complete(unreachable!());
/// ```
#[derive(Debug)]
pub(crate) struct AdmissionComplete {
    _private: (),
}
impl AdmissionComplete {
    pub(crate) fn observed(&self) {}
}
/// A schema-None decision, owned by the admission driver for this run/use.
#[derive(Debug)]
pub(crate) struct DestinationUnknown {
    _private: (),
}
impl DestinationUnknown {
    pub(crate) fn observed(&self) {}
}
#[derive(Clone, Copy)]
pub(crate) enum NotAdmitted {
    Ordinary(OrdinaryFailure),
    ReadOnly(ReadOnlyRun),
}
#[derive(Clone, Copy)]
pub(crate) enum InventoryAdmission<'a> {
    Admitted(&'a AdmissionComplete),
    Stopped(NotAdmitted),
    ReadOnly(NotAdmitted),
    Required,
}
#[derive(Debug, Default)]
pub(crate) struct AdmittedConversions {
    findings: crate::ConversionChannel,
}
impl AdmittedConversions {
    pub(crate) fn commit(
        &mut self,
        _witness: AdmissionComplete,
        findings: crate::ConversionChannel,
    ) {
        self.findings.append(findings);
    }
    pub(crate) fn extend(&mut self, other: Self) {
        self.findings.append(other.findings);
    }
    pub(crate) fn take(&mut self) -> crate::ConversionChannel {
        std::mem::take(&mut self.findings)
    }
}
#[derive(Clone, Copy)]
struct ReadyForAdmission;
enum AdmissionEntry {
    Unstarted,
    Started(AdmissionStart),
}
#[derive(Clone, Copy)]
enum AdmissionStart {
    Ready(ReadyForAdmission),
    Stopped(OrdinaryFailure),
}
enum ScopeAdmission {
    NotAdmitted,
    Admitted(AdmissionComplete),
}
#[derive(Clone, Copy)]
pub(crate) struct ReadOnlyRun {
    _private: (),
}
enum RunKind {
    Validate,
    ReadOnly(ReadOnlyRun),
}
#[derive(Clone, Copy)]
pub struct OrdinaryFailure {
    _private: (),
}
enum CompletionAuthority {
    Stopped(OrdinaryFailure),
    ReadOnly(ReadOnlyRun),
    Required,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "bug in carina: an authored conversion use has no completion outcome (admission pass did not complete)"
)]
pub struct AdmissionIncomplete {
    _private: (),
}

#[derive(Clone)]
pub(super) enum AdmissionBoundary {
    Scope(crate::parser::retained::AuthoredScopeId),
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

struct InventoryScope {
    scope: crate::parser::retained::AuthoredScopeId,
    admission: ScopeAdmission,
    inventory: std::sync::Arc<crate::parser::retained::conversion::ConversionInventory>,
    subject: FindingSubject,
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

/// One owner for all admission coverage and completion in a document set.
pub struct ConversionValidation<E: ValidationClientError = std::convert::Infallible> {
    kind: RunKind,
    entry: AdmissionEntry,
    inventories: Vec<InventoryScope>,
    coverage: crate::schema::value_validation::WalkCoverage,
    ordinary: Vec<(FindingSubject, OrdinaryFinding<E>)>,
    admitted_uses: std::collections::HashMap<
        AdmissionUse,
        Option<crate::parser::retained::ProjectionGeneration>,
    >,
}

impl<E: ValidationClientError> Default for ConversionValidation<E> {
    fn default() -> Self {
        Self {
            kind: RunKind::Validate,
            entry: AdmissionEntry::Unstarted,
            inventories: Vec::new(),
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

    /// Register the parsed root inventory before any validation phase can
    /// return early. This reads existing AST inventory without another walk.
    pub fn for_directory<T>(parsed: &crate::config_loader::RootDirectoryParse<T>) -> Self {
        Self::new(parsed.parsed().expression_report())
    }

    /// Destroy/state deliberately perform no resource admission, as on main.
    pub fn for_read_only_directory<T>(
        parsed: &crate::config_loader::RootDirectoryParse<T>,
    ) -> Self {
        let mut run = Self::for_directory(parsed);
        run.kind = RunKind::ReadOnly(ReadOnlyRun { _private: () });
        run
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

    /// Each step performs admission before yielding. Only exhausting this
    /// core-owned pass grants completeness authority; dropping it cannot.
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
            AdmissionBoundary::Scope(tree.admission_scope()),
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
        let start = match &boundary {
            AdmissionBoundary::Scope(_) => {
                self.include_in(judged.report(), subject.clone());
                self.ensure_admission_entry()
            }
            AdmissionBoundary::Preflight => self.admission_start(),
        };
        std::iter::from_fn(move || {
            let Some(resource) = resources.next() else {
                match &boundary {
                    AdmissionBoundary::Scope(scope) => self.admission_pass_complete(scope),
                    AdmissionBoundary::Preflight => {}
                }
                return None;
            };
            let values = match resource.schema(judged.schemas()) {
                None => {
                    self.destination_unknown(resource);
                    Ok(None)
                }
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
                                AdmissionBoundary::Scope(_) if matches!(start, AdmissionStart::Stopped(_)) || resource.authored_slots().is_some_and(|slots| slots.is_unexpanded_template()) =>
                                    super::admission::attributes::resource_template_preflight_admissions(request, coverage),
                                AdmissionBoundary::Scope(_) => super::admission::attributes::resource_attribute_admissions(
                                    request.resource, request.parsed, request.schema, request.attributes,
                                    request.quoted, request.inputs, request.bindings, request.session, request.schemas, coverage,
                                ),
                                AdmissionBoundary::Preflight => super::admission::attributes::resource_template_preflight_admissions(request, coverage),
                            }
                        }))
                    })
                }
            };
            if let AdmissionBoundary::Scope(_) = &boundary {
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
    fn admission_start(&self) -> AdmissionStart {
        match self.ordinary_failure() {
            Some(failure) => AdmissionStart::Stopped(failure),
            None => AdmissionStart::Ready(ReadyForAdmission),
        }
    }
    fn ensure_admission_entry(&mut self) -> AdmissionStart {
        match self.entry {
            AdmissionEntry::Started(start) => start,
            AdmissionEntry::Unstarted => {
                let start = self.admission_start();
                self.entry = AdmissionEntry::Started(start);
                start
            }
        }
    }
    pub(super) fn admission_pass_complete(
        &mut self,
        admitted_scope: &crate::parser::retained::AuthoredScopeId,
    ) {
        match self.ensure_admission_entry() {
            AdmissionStart::Ready(_) => {
                self.coverage
                    .commit_pending(AdmissionComplete { _private: () });
                for scope in &mut self.inventories {
                    if scope.scope == *admitted_scope {
                        scope.admission =
                            ScopeAdmission::Admitted(AdmissionComplete { _private: () });
                    }
                }
            }
            AdmissionStart::Stopped(_) => {
                self.coverage.replace_pending(Default::default());
            }
        }
        self.entry = AdmissionEntry::Unstarted;
    }

    /// Drive the remaining document resources, then seal the scope. Callers
    /// cannot mint the completion witness by forgetting this admission pass.
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
        let start = self.ensure_admission_entry();
        for resource in tree.project_resources() {
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
            if resource.schema(judged.schemas()).is_none() {
                self.destination_unknown(resource);
            }
            self.walk_resource(resource, |coverage| {
                if matches!(start, AdmissionStart::Stopped(_))
                    || resource
                        .authored_slots()
                        .is_some_and(|slots| slots.is_unexpanded_template())
                {
                    super::admission::attributes::unrendered_resource_template_admissions(
                        resource, judged, bindings, session, coverage,
                    )
                } else {
                    super::admission::attributes::unrendered_resource_conversion_admissions(
                        resource, judged, bindings, session, coverage,
                    )
                }
            });
            self.record_admitted_resource(resource);
        }
        self.admission_pass_complete(&tree.admission_scope());
    }

    pub fn new<B: BindingAddress>(report: &ExpressionReport<B>) -> Self {
        let mut run = Self::default();
        run.include(report);
        run
    }
    pub fn include<B: BindingAddress>(&mut self, report: &ExpressionReport<B>) {
        self.include_in(report, FindingSubject::Root);
    }
    /// Retain the resolver's own template/instance inventory even for a
    /// read-only entry that deliberately builds no admission analysis tree.
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
        if let Some(scope) = self
            .inventories
            .iter_mut()
            .find(|scope| scope.scope == report.admission_scope())
        {
            scope.inventory = report.conversion_inventory();
            scope.subject = subject;
            return;
        }
        self.inventories.push(InventoryScope {
            scope: report.admission_scope(),
            admission: ScopeAdmission::NotAdmitted,
            inventory: report.conversion_inventory(),
            subject,
        });
    }
    pub fn observe_destination<D, B: BindingAddress>(
        &mut self,
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
            B,
        >,
        schemas: &crate::schema::SchemaRegistry,
    ) where
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            >,
    {
        if resource.schema(schemas).is_none() {
            self.destination_unknown(resource);
        }
    }
    fn destination_unknown<D, B: BindingAddress>(
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
        if let Some(slots) = resource.authored_slots() {
            for root in slots.admission_roots() {
                self.coverage
                    .destination_unknown(root, DestinationUnknown { _private: () });
            }
        }
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
        let start = self.ensure_admission_entry();
        let findings = self.walk_resource(resource, |coverage| {
            if matches!(start, AdmissionStart::Stopped(_))
                || resource
                    .authored_slots()
                    .is_some_and(|slots| slots.is_unexpanded_template())
            {
                super::admission::attributes::resource_template_preflight_admissions(
                    request, coverage,
                )
            } else {
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
            }
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
    pub(crate) fn work_for<B: BindingAddress>(&self, report: &ExpressionReport<B>) -> usize {
        self.coverage.work() + report.completeness_report(&self.coverage).work()
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
        self.ensure_admission_entry();
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
        use crate::parser::retained::conversion::{InventoryCompletion, complete_inventory};
        let authority = match self.ordinary_failure() {
            Some(failure) => CompletionAuthority::Stopped(failure),
            None => match self.kind {
                RunKind::Validate => CompletionAuthority::Required,
                RunKind::ReadOnly(proof) => CompletionAuthority::ReadOnly(proof),
            },
        };
        let mut incomplete = false;
        let mut conversions = crate::ConversionFindings::default();
        conversions.extend(self.coverage.take_conversions());
        for scope in self.inventories {
            let admission = match &scope.admission {
                ScopeAdmission::Admitted(witness) => InventoryAdmission::Admitted(witness),
                ScopeAdmission::NotAdmitted => match authority {
                    CompletionAuthority::Stopped(witness) => {
                        InventoryAdmission::Stopped(NotAdmitted::Ordinary(witness))
                    }
                    CompletionAuthority::ReadOnly(witness) => {
                        InventoryAdmission::ReadOnly(NotAdmitted::ReadOnly(witness))
                    }
                    CompletionAuthority::Required => InventoryAdmission::Required,
                },
            };
            let (errors, completion) =
                complete_inventory(&scope.inventory, &self.coverage, admission);
            match completion {
                InventoryCompletion::Complete => {}
                InventoryCompletion::MissingOutcome => incomplete = true,
            }
            let errors = match scope.subject {
                FindingSubject::Root => errors,
                subject @ FindingSubject::Module(_) => errors.in_subject(subject),
            };
            conversions.extend(errors);
        }
        if incomplete {
            self.ordinary.push((
                FindingSubject::Root,
                OrdinaryFinding::Resource(Box::new(super::ResourceAdmissionFinding::incomplete(
                    AdmissionIncomplete { _private: () },
                ))),
            ));
        }
        ResourceValidationReport {
            ordinary: self.ordinary,
            conversions,
        }
    }
}
