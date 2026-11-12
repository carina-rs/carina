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
    pub(crate) fn is_empty(&self) -> bool {
        self.findings.findings().is_empty()
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
struct ReadOnlyRun;
enum RunKind {
    Validate,
    ReadOnly(ReadOnlyRun),
}
#[derive(Clone, Copy)]
struct OrdinaryFailure;
enum CompletionAuthority {
    Stopped(OrdinaryFailure),
    ReadOnly(ReadOnlyRun),
    Required,
}

#[derive(Debug, thiserror::Error)]
#[error("internal validation invariant: admission pass did not complete")]
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

/// One owner for all admission coverage and completion in a document set.
pub struct ConversionValidation<E: ValidationClientError = std::convert::Infallible> {
    kind: RunKind,
    entry: AdmissionEntry,
    inventories: Vec<InventoryScope>,
    coverage: crate::schema::value_validation::WalkCoverage,
    ordinary: Vec<(FindingSubject, OrdinaryFinding<E>)>,
}

impl<E: ValidationClientError> Default for ConversionValidation<E> {
    fn default() -> Self {
        Self {
            kind: RunKind::Validate,
            entry: AdmissionEntry::Unstarted,
            inventories: Vec::new(),
            coverage: Default::default(),
            ordinary: Vec::new(),
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
    pub fn is_empty(&self) -> bool {
        self.ordinary.is_empty() && !self.coverage.has_conversion_findings()
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
        run.kind = RunKind::ReadOnly(ReadOnlyRun);
        run
    }

    /// Main's terminal provider-authority failure replaces ordinary errors;
    /// conversion evidence stays owned by this same run.
    pub fn replace_ordinary_error(&mut self, error: E) {
        self.ordinary.clear();
        self.push(error);
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

    #[cfg(test)]
    pub(crate) fn admitted_authored_test_resources<'a, 'file: 'a, B, D, T>(
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
        self.resource_steps(
            tree.resources()
                .filter(|resource| resource.kind() != crate::parser::AnalysisItemKind::Composition),
            judged,
            bindings,
            session,
            FindingSubject::Root,
            AdmissionBoundary::Scope(tree.admission_scope()),
        )
    }

    pub(super) fn resource_steps<'a, 'file: 'a, B, D, T>(
        &'a mut self,
        mut resources: impl Iterator<
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
        let preflight_start = match &boundary {
            AdmissionBoundary::Scope(_) => {
                self.include_in(judged.report(), subject.clone());
                self.ensure_admission_entry();
                self.admission_start()
            }
            AdmissionBoundary::Preflight => self.admission_start(),
        };
        let mut pending = crate::ConversionChannel::default();
        std::iter::from_fn(move || {
            let Some(resource) = resources.next() else {
                match &boundary {
                    AdmissionBoundary::Scope(scope) => self.admission_pass_complete(scope),
                    AdmissionBoundary::Preflight => match preflight_start {
                        AdmissionStart::Ready(_) => self.coverage.commit_channel(
                            AdmissionComplete { _private: () },
                            std::mem::take(&mut pending),
                        ),
                        AdmissionStart::Stopped(_) => {
                            pending = Default::default();
                        }
                    },
                }
                return None;
            };
            let saved = match &boundary {
                AdmissionBoundary::Scope(_) => None,
                AdmissionBoundary::Preflight => {
                    Some(self.coverage.replace_pending(std::mem::take(&mut pending)))
                }
            };
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
                                            .map(|call| call.display_label())
                                            .collect::<Vec<_>>()
                                            .join(" -> "),
                                    )
                                }),
                        };
                        Some(self.coverage.in_subject(resource_subject, |coverage| {
                            super::admission::attributes::resource_attribute_admissions(
                                resource,
                                judged.file(),
                                schema,
                                &classified,
                                resource.quoted_string_attrs(),
                                &inputs,
                                bindings,
                                session,
                                judged.schemas(),
                                coverage,
                            )
                        }))
                    })
                }
            };
            if let Some(saved) = saved {
                pending = self.coverage.replace_pending(saved);
            }
            Some(ResourceAdmissionStep { resource, values })
        })
    }

    /// Low-level unit observers implement their own complete admission loop.
    /// This adapter is unavailable to production callers or external doctests.
    #[cfg(test)]
    pub(crate) fn finish_test_admission_pass(&mut self) {
        let scopes = self
            .inventories
            .iter()
            .map(|scope| scope.scope.clone())
            .collect::<Vec<_>>();
        for scope in scopes {
            self.admission_pass_complete(&scope);
        }
    }

    fn ordinary_failure(&self) -> Option<OrdinaryFailure> {
        self.ordinary.iter().find_map(|(_, finding)| match finding {
            OrdinaryFinding::Resource(_) => Some(OrdinaryFailure),
            OrdinaryFinding::Client(error) => match error.kind() {
                ClientFindingKind::Error => Some(OrdinaryFailure),
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
        admitted: &std::collections::HashSet<crate::resource::ResourceId>,
    ) where
        B: BindingAddress,
        T: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B>,
        D: crate::parser::DeferredForPhaseTraversal<
                crate::resource::ParserPlaceholder,
                crate::resource::ParserPlaceholder,
                B,
            >,
    {
        self.include(tree.admission_report());
        for resource in tree
            .project_resources()
            .filter(|resource| !admitted.contains(resource.id()))
        {
            self.walk_resource(resource, |coverage| {
                super::admission::attributes::unrendered_resource_conversion_admissions(
                    resource, judged, bindings, session, coverage,
                )
            });
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
    pub fn include_in<B: BindingAddress>(
        &mut self,
        report: &ExpressionReport<B>,
        subject: FindingSubject,
    ) {
        self.inventories.push(InventoryScope {
            scope: report.admission_scope(),
            admission: ScopeAdmission::NotAdmitted,
            inventory: report.conversion_inventory(),
            subject,
        });
    }
    pub fn walk<T>(
        &mut self,
        walk: impl FnOnce(&mut crate::schema::value_validation::WalkCoverage) -> T,
    ) -> T {
        self.ensure_admission_entry();
        walk(&mut self.coverage)
    }
    pub fn walk_resource<'a, P, T, D, B, U>(
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
                        .map(|call| call.display_label())
                        .collect::<Vec<_>>()
                        .join(" -> "),
                )
            });
        self.walk_in(subject, walk)
    }
    pub fn walk_in<T>(
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
        use crate::parser::retained::conversion::{
            AdmissionRequirement, complete_inventory, parse_rejections,
        };
        let authority = match self.ordinary_failure() {
            Some(failure) => CompletionAuthority::Stopped(failure),
            None => match self.kind {
                RunKind::Validate => CompletionAuthority::Required,
                RunKind::ReadOnly(proof) => CompletionAuthority::ReadOnly(proof),
            },
        };
        let mut incomplete = AdmissionRequirement::Empty;
        let mut conversions = crate::ConversionFindings::default();
        conversions.extend(self.coverage.take_conversions());
        for scope in self.inventories {
            let errors = match scope.admission {
                ScopeAdmission::Admitted(witness) => {
                    complete_inventory(&scope.inventory, &self.coverage, &witness)
                }
                ScopeAdmission::NotAdmitted => {
                    let (errors, requirement) = parse_rejections(&scope.inventory);
                    match (&authority, requirement) {
                        (CompletionAuthority::Required, AdmissionRequirement::Required) => {
                            incomplete = AdmissionRequirement::Required
                        }
                        (CompletionAuthority::Required, AdmissionRequirement::Empty)
                        | (
                            CompletionAuthority::Stopped(_),
                            AdmissionRequirement::Empty | AdmissionRequirement::Required,
                        )
                        | (
                            CompletionAuthority::ReadOnly(_),
                            AdmissionRequirement::Empty | AdmissionRequirement::Required,
                        ) => {}
                    }
                    errors
                }
            };
            let errors = match scope.subject {
                FindingSubject::Root => errors,
                subject @ FindingSubject::Module(_) => errors.in_subject(subject),
            };
            conversions.extend(errors);
        }
        match incomplete {
            AdmissionRequirement::Empty => {}
            AdmissionRequirement::Required => self.ordinary.push((
                FindingSubject::Root,
                OrdinaryFinding::Resource(Box::new(super::ResourceAdmissionFinding::incomplete(
                    AdmissionIncomplete { _private: () },
                ))),
            )),
        }
        ResourceValidationReport {
            ordinary: self.ordinary,
            conversions,
        }
    }
}
