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

/// An application's ordinary error type. ConversionError deliberately does
/// not implement this trait; it can enter presentation only through Conversion.
pub trait ValidationClientError: std::fmt::Debug + std::fmt::Display {}
impl ValidationClientError for std::convert::Infallible {}

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

struct InventoryScope {
    inventory: std::sync::Arc<crate::parser::retained::conversion::ConversionInventory>,
    subject: FindingSubject,
}

/// One owner for all admission coverage and completion in a document set.
pub struct ConversionValidation<E: ValidationClientError = std::convert::Infallible> {
    inventories: Vec<InventoryScope>,
    coverage: crate::schema::value_validation::WalkCoverage,
    ordinary: Vec<(FindingSubject, OrdinaryFinding<E>)>,
}

impl<E: ValidationClientError> Default for ConversionValidation<E> {
    fn default() -> Self {
        Self {
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
            inventory: report.conversion_inventory(),
            subject,
        });
    }
    pub fn walk<T>(
        &mut self,
        walk: impl FnOnce(&mut crate::schema::value_validation::WalkCoverage) -> T,
    ) -> T {
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
        let mut conversions = crate::ConversionFindings::default();
        conversions.extend(self.coverage.take_conversions());
        for scope in self.inventories {
            let errors = crate::parser::retained::conversion::complete_inventory(
                &scope.inventory,
                &self.coverage,
            )
            .in_subject(scope.subject);
            conversions.extend(errors);
        }
        ResourceValidationReport {
            ordinary: self.ordinary,
            conversions,
        }
    }
}
