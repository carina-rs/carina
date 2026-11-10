//! Provider-dependent evidence recorded by producers and consumed at a terminal boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::parser::{
    DeclarationLocation, DirectoryScoped, Located, ParseWarning, ParseWarningSpan, ProviderContext,
    TypeExpr, UserFunction, WarningKind,
};

/// Authored ownership of a check, retained independently of evaluated values.
/// The parser's location is mandatory for declaration-owned checks.
///
/// ```compile_fail,E0599
/// use carina_core::validation::deferred_types::ProviderTypeCheckSite;
/// let site = ProviderTypeCheckSite::at_line(std::path::Path::new("sibling.crn"), 3, 7, "check");
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::{parser::SourceSpan, validation::deferred_types::ProviderTypeCheckSite};
/// fn fabricate(span: SourceSpan) { let site = ProviderTypeCheckSite::new(span, "check"); }
/// ```
///
/// ```compile_fail,E0061
/// use carina_core::{parser::SourceSpan, validation::deferred_types::ProviderTypeCheckSite};
/// fn fabricate(path: &std::path::Path, span: SourceSpan) {
///     let site = ProviderTypeCheckSite::new(path, span, "check");
/// }
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::validation::deferred_types::ProviderTypeCheckSite;
/// let site = ProviderTypeCheckSite::unlocated("check");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTypeCheckSite {
    owner: ProviderTypeCheckOwner,
    context: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ProviderTypeCheckOwner {
    Declaration(DeclarationLocation),
    Directory,
}

impl ProviderTypeCheckSite {
    pub fn new(location: DeclarationLocation, context: impl Into<String>) -> Self {
        Self {
            owner: ProviderTypeCheckOwner::Declaration(location),
            context: context.into(),
        }
    }

    /// A query without an authored declaration is explicitly directory scoped.
    pub fn directory_scoped(context: impl Into<String>) -> Self {
        Self {
            owner: ProviderTypeCheckOwner::Directory,
            context: context.into(),
        }
    }
}

#[cfg(all(test, unix))]
#[test]
fn deferred_type_diagnostics_preserve_symlink_parent_source_spelling() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("private/nested")).unwrap();
    std::fs::create_dir(temp.path().join("private/bad")).unwrap();
    std::os::unix::fs::symlink("private/nested", temp.path().join("jump")).unwrap();
    let source = temp.path().join("jump/../bad/main.crn");
    let parsed = crate::parser::parse_file(
        "attributes { value: Missing = 'id' }",
        &source,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let site = ProviderTypeCheckSite::new(
        parsed.attribute_params[0].type_check_location().clone(),
        "annotation",
    );
    assert!(
        matches!(site.owner, ProviderTypeCheckOwner::Declaration(location) if location.file().path() == source)
    );
}

/// The evidence belongs exclusively to one run, never to a cloneable catalog.
#[derive(Debug)]
struct DeferredProviderTypeChecks<'s> {
    pending: BTreeMap<ProviderTypeCheckKey, (ProviderTypeCheckSite, TypeExpr)>,
    declarations: BTreeMap<String, Vec<ProviderTypeCheckSite>>,
    blocked: Vec<ProviderTypeCheckFinding>,
    brand: PhantomData<fn(&'s ()) -> &'s ()>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
struct ProviderTypeCheckKey {
    owner: ProviderTypeCheckOwner,
    type_label: String,
}

impl DeferredProviderTypeChecks<'_> {
    fn record(&mut self, site: ProviderTypeCheckSite, expected: &TypeExpr) {
        let key = ProviderTypeCheckKey {
            owner: site.owner.clone(),
            type_label: expected.to_string(),
        };
        self.pending
            .entry(key)
            .or_insert_with(|| (site, expected.clone()));
    }
}

mod sealed {
    pub trait SessionEvidence {}
}

/// A borrowed view of a scoped session. Only the session implements this
/// sealed interface; borrowing it never transfers ownership of evidence.
pub trait ProviderTypeCheckContext:
    sealed::SessionEvidence + std::fmt::Debug + Send + Sync
{
    fn catalog(&self) -> &ProviderContext;
    fn loaded_context(&self) -> Option<&crate::parser::LoadedProviderContext>;
    fn declaration(&self, expected: &TypeExpr, site: ProviderTypeCheckSite);
    /// Declaration diagnostics own failed signatures; their calls must retain
    /// the authored expression while unrelated function checks still run.
    fn fail_function_declaration(&self, source: &Path, name: &str);
    fn has_failed_function_declaration(&self, source: &Path, name: &str) -> bool;
    fn record(&self, expected: &TypeExpr, site: ProviderTypeCheckSite);
    fn record_type(&self, expected: &TypeExpr, context: &str);
    fn publish_checks(
        &self,
        sink: &mut dyn ProviderTypeCheckSink,
    ) -> Result<(), ProviderTypeCheckFailure>;
}

impl dyn ProviderTypeCheckContext + '_ {
    pub(crate) fn function_declaration_failed<B: crate::resource::BindingAddress>(
        &self,
        function: &UserFunction<B>,
    ) -> bool {
        let source = function
            .annotation_source_file
            .as_deref()
            .or(function.body.source_file.as_deref())
            .unwrap_or_else(|| Path::new("<config>"));
        self.has_failed_function_declaration(source, &function.name)
    }

    pub fn pending<T>(&self, output: T) -> PendingProviderTypeChecks<'_, T> {
        PendingProviderTypeChecks {
            session: self,
            output,
            brand: PhantomData,
        }
    }
}

/// Move-only provider-type validation for a single run.
///
/// Sessions can only be borrowed inside a run.
/// ```compile_fail,E0624
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let session = TypeCheckSession::new(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()));
/// ```
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     session.catalog().custom_types_loaded()
/// });
/// assert!(!result.publish(&mut Vec::new()));
/// ```
///
/// ```compile_fail
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let escaped = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| session);
/// ```
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     session.catalog().custom_types_loaded()
/// });
/// assert!(!result.publish(&mut Vec::new()));
/// ```
///
/// ```compile_fail
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let escaped = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| session.pending(42));
/// ```
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     session.pending(42).publish(&mut Vec::new()).unwrap()
/// });
/// assert_eq!(result.publish(&mut Vec::new()), 42);
/// ```
///
/// ```compile_fail
/// use carina_core::{parser::{ProviderContext, TypeExpr}, resource::{AuthoredValue, AuthoredConcreteValue}, validation::{deferred_types::TypeCheckSession, typecheck::check_type_expr_value_providerless}};
/// let expected = TypeExpr::String;
/// let value = AuthoredValue::Concrete(AuthoredConcreteValue::String("value".into()));
/// let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
/// let parsed = carina_core::parser::parse_file("let value = 'value'", std::path::Path::new("test.crn"), &catalog).unwrap();
/// let schemas = carina_core::schema::SchemaRegistry::new();
/// let loaded = carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
/// let report = loaded.judged();
/// let expression = report.report().binding(&carina_core::resource::RelativeBindingName::new("value")).unwrap();
/// let escaped = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     check_type_expr_value_providerless(&expected, &value, session.catalog(), expression)
/// });
/// ```
/// ```
/// use carina_core::{parser::{ProviderContext, TypeExpr}, resource::{AuthoredValue, AuthoredConcreteValue}, validation::{deferred_types::TypeCheckSession, typecheck::check_type_expr_value_providerless}};
/// let expected = TypeExpr::String;
/// let value = AuthoredValue::Concrete(AuthoredConcreteValue::String("value".into()));
/// let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
/// let parsed = carina_core::parser::parse_file("let value = 'value'", std::path::Path::new("test.crn"), &catalog).unwrap();
/// let schemas = carina_core::schema::SchemaRegistry::new();
/// let loaded = carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
/// let report = loaded.judged();
/// let expression = report.report().binding(&carina_core::resource::RelativeBindingName::new("value")).unwrap();
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     check_type_expr_value_providerless(&expected, &value, session.catalog(), expression)
///         .record_deferred(session).unwrap();
/// });
/// result.publish(&mut Vec::new());
/// ```
///
/// ```compile_fail,E0515
/// use carina_core::{parser::{ProviderContext, parse_type_expr_str}, schema::TypeInSchema, validation::{deferred_types::TypeCheckSession, lift_type_expr}};
/// let escaped = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     let expected = parse_type_expr_str("Missing", session.catalog()).unwrap();
///     let source = lift_type_expr(&expected).unwrap();
///     let target = lift_type_expr(&carina_core::parser::TypeExpr::String).unwrap();
///     TypeInSchema::schemaless(&source).assignment_to(target.assignment_sink(&session.catalog().custom_types))
/// });
/// ```
/// ```
/// use carina_core::{parser::{ProviderContext, parse_type_expr_str}, schema::TypeInSchema, validation::{deferred_types::TypeCheckSession, lift_type_expr}};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     let expected = parse_type_expr_str("Missing", session.catalog()).unwrap();
///     let source = lift_type_expr(&expected).unwrap();
///     let target = lift_type_expr(&carina_core::parser::TypeExpr::String).unwrap();
///     TypeInSchema::schemaless(&source).assignment_to(target.assignment_sink(&session.catalog().custom_types))
///         .record_unchecked(session)
///         .require_proven().expect("the plain String sink has no member constraints")
/// });
/// assert!(result.publish(&mut Vec::new()));
/// ```
///
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| 42);
/// let mut diagnostics = Vec::new();
/// assert_eq!(result.publish(&mut diagnostics), 42);
/// assert!(diagnostics.is_empty());
/// ```
///
/// The entry point owns the session and settles its evidence on every normal
/// return, including a producer's `Err`. Pending payloads and checker queries
/// cannot escape the higher-ranked closure. Catalog clones contain no evidence.
///
/// ```compile_fail,E0599
/// use carina_core::validation::deferred_types::TypeCheckSession;
/// let session = TypeCheckSession::default();
/// ```
/// ```
/// use carina_core::validation::deferred_types::TypeCheckSession;
/// let result = TypeCheckSession::run(&carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     session.catalog().custom_types_loaded()
/// });
/// assert!(!result.publish(&mut Vec::new()));
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::validation::deferred_types::TypeCheckSession;
/// use carina_core::parser::ProviderContext;
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     let duplicate: TypeCheckSession<'_> = Clone::clone(session);
/// });
/// ```
/// ```
/// use carina_core::validation::deferred_types::TypeCheckSession;
/// use carina_core::parser::ProviderContext;
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     let duplicate: ProviderContext = Clone::clone(session.catalog());
///     duplicate.custom_types_loaded()
/// });
/// assert!(!result.publish(&mut Vec::new()));
/// ```
#[derive(Debug)]
pub struct TypeCheckSession<'s> {
    catalog: SessionCatalog,
    evidence: Mutex<DeferredProviderTypeChecks<'s>>,
    failed_function_declarations: Mutex<BTreeSet<(PathBuf, String)>>,
    brand: PhantomData<fn(&'s ()) -> &'s ()>,
}

#[derive(Debug)]
enum SessionCatalog {
    Loaded(crate::parser::LoadedProviderContext),
    Incomplete(ProviderContext),
}

impl<'s> TypeCheckSession<'s> {
    /// The sole constructor; test fixtures and production use the same path.
    fn new(catalog: &ProviderContext) -> Self {
        let catalog = if catalog.custom_types_loaded() {
            SessionCatalog::Loaded(crate::parser::LoadedProviderContext::new(catalog.clone()))
        } else {
            SessionCatalog::Incomplete(catalog.clone())
        };
        Self {
            catalog,
            brand: PhantomData,
            failed_function_declarations: Mutex::new(BTreeSet::new()),
            evidence: Mutex::new(DeferredProviderTypeChecks {
                pending: BTreeMap::new(),
                declarations: BTreeMap::new(),
                blocked: Vec::new(),
                brand: PhantomData,
            }),
        }
    }

    pub fn run<T>(
        catalog: &ProviderContext,
        producer: impl for<'scope> FnOnce(&'scope TypeCheckSession<'scope>) -> T,
    ) -> ProviderTypeCheckResult<T> {
        let session = TypeCheckSession::new(catalog);
        let output = producer(&session);
        ProviderTypeCheckResult::settle(&session, output)
    }

    pub async fn run_async<T>(
        catalog: &ProviderContext,
        producer: impl for<'scope> AsyncFnOnce(&'scope TypeCheckSession<'scope>) -> T,
    ) -> ProviderTypeCheckResult<T> {
        let session = TypeCheckSession::new(catalog);
        // Provider preparation and execution embed large async state machines.
        // Keep them off each caller's stack, including while polling nested sessions.
        let output = Box::pin(producer(&session)).await;
        ProviderTypeCheckResult::settle(&session, output)
    }

    pub fn as_provider_context(&self) -> &ProviderContext {
        self.catalog()
    }

    pub fn catalog(&self) -> &ProviderContext {
        match &self.catalog {
            SessionCatalog::Loaded(catalog) => catalog.as_provider_context(),
            SessionCatalog::Incomplete(catalog) => catalog,
        }
    }

    pub fn loaded_context(&self) -> Option<&crate::parser::LoadedProviderContext> {
        match &self.catalog {
            SessionCatalog::Loaded(catalog) => Some(catalog),
            SessionCatalog::Incomplete(_) => None,
        }
    }

    pub fn declaration(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        let label = expected.to_string();
        let sites = evidence.declarations.entry(label.clone()).or_default();
        if !sites.contains(&site) {
            sites.push(site.clone());
        }
        // A later declaration replaces synthetic ownership rather than adding
        // a second warning at <config>:1:1.
        let unlocated = evidence
            .pending
            .iter()
            .filter(|(key, _)| {
                key.owner == ProviderTypeCheckOwner::Directory && key.type_label == label
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in unlocated {
            if let Some((_old_site, expected)) = evidence.pending.remove(&key) {
                evidence.record(site.clone(), &expected);
            }
        }
    }

    pub fn record(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        self.evidence
            .lock()
            .expect("provider type evidence lock")
            .record(site, expected);
    }

    pub fn record_type(&self, expected: &TypeExpr, context: &str) {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        let sites = evidence
            .declarations
            .get(&expected.to_string())
            .cloned()
            .unwrap_or_else(|| vec![ProviderTypeCheckSite::directory_scoped(context)]);
        for site in sites {
            evidence.record(site, expected);
        }
    }

    pub fn pending<T>(&'s self, output: T) -> PendingProviderTypeChecks<'s, T> {
        PendingProviderTypeChecks {
            session: self,
            output,
            brand: PhantomData,
        }
    }

    fn settle_pending<T>(&self, output: T) -> ProviderTypeCheckResult<T> {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        let checks = DeferredProviderTypeChecks {
            pending: std::mem::take(&mut evidence.pending),
            declarations: BTreeMap::new(),
            blocked: Vec::new(),
            brand: PhantomData,
        };
        let mut result = ProviderTypeCheckResult::from_checks(
            self.catalog().custom_types_loaded(),
            checks,
            output,
        );
        if self.catalog().custom_types_loaded() {
            for diagnostic in &result.diagnostics {
                if let ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) = diagnostic {
                    evidence.blocked.push(error.clone());
                }
            }
            if result.diagnostics.is_empty() {
                result.diagnostics = evidence
                    .blocked
                    .iter()
                    .cloned()
                    .map(ProviderTypeCheckDiagnostic::UnsettledLoadedCheck)
                    .collect();
            }
        }
        result
    }
}

impl sealed::SessionEvidence for TypeCheckSession<'_> {}

impl ProviderTypeCheckContext for TypeCheckSession<'_> {
    fn catalog(&self) -> &ProviderContext {
        self.catalog()
    }
    fn loaded_context(&self) -> Option<&crate::parser::LoadedProviderContext> {
        self.loaded_context()
    }
    fn declaration(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        self.declaration(expected, site);
    }
    fn fail_function_declaration(&self, source: &Path, name: &str) {
        self.failed_function_declarations
            .lock()
            .expect("failed function declaration lock")
            .insert((
                crate::utils::normalize_source_path(source),
                name.to_string(),
            ));
    }
    fn has_failed_function_declaration(&self, source: &Path, name: &str) -> bool {
        self.failed_function_declarations
            .lock()
            .expect("failed function declaration lock")
            .contains(&(
                crate::utils::normalize_source_path(source),
                name.to_string(),
            ))
    }
    fn record(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        self.record(expected, site);
    }
    fn record_type(&self, expected: &TypeExpr, context: &str) {
        self.record_type(expected, context);
    }
    fn publish_checks(
        &self,
        sink: &mut dyn ProviderTypeCheckSink,
    ) -> Result<(), ProviderTypeCheckFailure> {
        let checks = self.settle_pending(());
        let errors = checks
            .diagnostics
            .iter()
            .filter_map(|diagnostic| match diagnostic {
                ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => {
                    Some(error.message.clone())
                }
                ProviderTypeCheckDiagnostic::Deferred(_) => None,
            })
            .collect::<Vec<_>>();
        checks.publish(sink);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ProviderTypeCheckFailure {
                message: errors.join("\n"),
            })
        }
    }
}

/// A provider payload stays private until its checks have been published.
pub struct PendingProviderTypeChecks<'s, T> {
    session: &'s dyn ProviderTypeCheckContext,
    output: T,
    brand: PhantomData<fn(&'s ()) -> &'s ()>,
}

impl<T> PendingProviderTypeChecks<'_, T> {
    pub fn publish(
        self,
        sink: &mut impl ProviderTypeCheckSink,
    ) -> Result<T, ProviderTypeCheckFailure> {
        self.session.publish_checks(sink)?;
        Ok(self.output)
    }
}

pub trait ProviderTypeCheckSink {
    fn record(&mut self, diagnostic: ProviderTypeCheckDiagnostic);
}

impl ProviderTypeCheckSink for Vec<ProviderTypeCheckDiagnostic> {
    fn record(&mut self, diagnostic: ProviderTypeCheckDiagnostic) {
        self.push(diagnostic);
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderTypeCheckFailure {
    message: String,
}

/// Execute test producers using the production constructor and terminal consumer.
#[cfg(any(test, feature = "test-utils"))]
pub fn with_session_for_test<T>(
    catalog: &ProviderContext,
    producer: impl for<'s> FnOnce(&'s TypeCheckSession<'s>) -> T,
) -> T {
    TypeCheckSession::run(catalog, producer).publish(&mut TestTypeCheckSink)
}

#[cfg(any(test, feature = "test-utils"))]
pub async fn with_session_for_test_async<T>(
    catalog: &ProviderContext,
    producer: impl for<'s> AsyncFnOnce(&'s TypeCheckSession<'s>) -> T,
) -> T {
    TypeCheckSession::run_async(catalog, producer)
        .await
        .publish(&mut TestTypeCheckSink)
}

#[cfg(any(test, feature = "test-utils"))]
struct TestTypeCheckSink;

#[cfg(any(test, feature = "test-utils"))]
impl ProviderTypeCheckSink for TestTypeCheckSink {
    fn record(&mut self, diagnostic: ProviderTypeCheckDiagnostic) {
        match diagnostic {
            ProviderTypeCheckDiagnostic::Deferred(warning) => eprintln!(
                "{}:{}: {}",
                warning.file.as_deref().unwrap_or("<config>"),
                warning.line,
                warning.message
            ),
            ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => panic!("{}", error.message),
        }
    }
}

/// A terminal output can only be constructed by consuming its unsettled evidence.
/// No `Default`, public fields, or constructor without the sink is provided.
///
/// ```compile_fail,E0599
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| 42);
/// let (output, _) = result.into_parts();
/// ```
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| 42);
/// assert_eq!(result.publish(&mut Vec::new()), 42);
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| 42);
/// let output = *result.output();
/// ```
/// ```
/// use carina_core::{parser::ProviderContext, validation::deferred_types::TypeCheckSession};
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| 42);
/// assert_eq!(result.publish(&mut Vec::new()), 42);
/// ```
///
/// ```compile_fail,E0451
/// use carina_core::validation::deferred_types::ProviderTypeCheckResult;
/// let result = ProviderTypeCheckResult { output: Vec::<String>::new(), diagnostics: vec![] };
/// ```
/// ```
/// use carina_core::validation::deferred_types::ProviderTypeCheckResult;
/// let result: ProviderTypeCheckResult<Vec<String>> = carina_core::validation::deferred_types::TypeCheckSession::run(
///     &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| Vec::<String>::new(),
/// );
/// let mut diagnostics = vec![];
/// assert!(result.publish(&mut diagnostics).is_empty());
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::validation::deferred_types::{TypeCheckSession, ProviderTypeCheckResult};
/// use carina_core::parser::ProviderContext;
/// let result = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |session| {
///     ProviderTypeCheckResult::settle(session, Vec::<String>::new())
/// });
/// ```
/// ```
/// use carina_core::validation::deferred_types::{TypeCheckSession, ProviderTypeCheckResult};
/// use carina_core::parser::ProviderContext;
/// let result: ProviderTypeCheckResult<Vec<String>> = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| {
///     Vec::<String>::new()
/// });
/// assert!(result.publish(&mut Vec::new()).is_empty());
/// ```
///
/// ```compile_fail,E0382
/// use carina_core::validation::deferred_types::{TypeCheckSession, ProviderTypeCheckResult};
/// use carina_core::parser::ProviderContext;
/// let result: ProviderTypeCheckResult<Vec<String>> = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| Vec::new());
/// drop(result);
/// let output = result.publish(&mut Vec::new());
/// ```
/// ```
/// use carina_core::validation::deferred_types::{TypeCheckSession, ProviderTypeCheckResult};
/// use carina_core::parser::ProviderContext;
/// let result: ProviderTypeCheckResult<Vec<String>> = TypeCheckSession::run(&ProviderContext::new(carina_core::EnvironmentSnapshot::empty()), |_session| Vec::new());
/// let output = result.publish(&mut Vec::new());
/// drop(output);
/// ```
pub struct ProviderTypeCheckResult<T> {
    output: T,
    diagnostics: Vec<ProviderTypeCheckDiagnostic>,
}

#[derive(Debug)]
pub enum ProviderTypeCheckDiagnostic {
    Deferred(ProviderTypeCheckFinding),
    UnsettledLoadedCheck(ProviderTypeCheckFinding),
}

/// A declaration finding has parser-owned provenance; a source-free query is
/// explicitly directory scoped. Legacy CLI warnings are produced only at output.
#[derive(Debug, Clone)]
pub enum ProviderTypeCheckFinding {
    Declaration(Located<ParseWarning>),
    Directory(DirectoryScoped<ParseWarning>),
}
impl ProviderTypeCheckFinding {
    pub fn into_warning(self) -> ParseWarning {
        match self {
            Self::Declaration(finding) => finding.into_parts().1,
            Self::Directory(finding) => finding.into_detail(),
        }
    }
}
impl std::ops::Deref for ProviderTypeCheckFinding {
    type Target = ParseWarning;
    fn deref(&self) -> &ParseWarning {
        match self {
            Self::Declaration(finding) => finding.detail(),
            Self::Directory(finding) => finding.detail(),
        }
    }
}

impl<T> ProviderTypeCheckResult<T> {
    /// The sole constructor of terminal provider-type results.
    fn settle(session: &TypeCheckSession<'_>, output: T) -> Self {
        let loaded = session.catalog().custom_types_loaded();
        let mut evidence = session
            .evidence
            .lock()
            .expect("provider type evidence lock");
        let checks = DeferredProviderTypeChecks {
            pending: std::mem::take(&mut evidence.pending),
            declarations: BTreeMap::new(),
            blocked: Vec::new(),
            brand: PhantomData,
        };
        let mut result = Self::from_checks(loaded, checks, output);
        result.diagnostics.extend(
            evidence
                .blocked
                .iter()
                .cloned()
                .map(ProviderTypeCheckDiagnostic::UnsettledLoadedCheck),
        );
        result
    }

    fn from_checks(loaded: bool, checks: DeferredProviderTypeChecks<'_>, output: T) -> Self {
        let diagnostics = checks.pending.into_values().map(|(site, expected)| {
            let message = if loaded {
                format!("internal error: {}: type check for {expected} remained deferred in a fully loaded provider catalog", site.context)
            } else {
                format!("{}: type check for {expected} was deferred because provider schemas were not loaded", site.context)
            };
            let warning = match site.owner {
                ProviderTypeCheckOwner::Declaration(location) => {
                    let span = location.span();
                    let warning = ParseWarning {
                        kind: WarningKind::ProviderSchemasNotLoaded,
                        file: Some(location.file().path().display().to_string()),
                        line: span.start_line(),
                        span: Some(ParseWarningSpan {
                            start_line: span.start_line(), start_column: span.start_column(),
                            end_line: span.end_line(), end_column: span.end_column(),
                        }),
                        message,
                    };
                    ProviderTypeCheckFinding::Declaration(Located::new(location, warning))
                }
                ProviderTypeCheckOwner::Directory => ProviderTypeCheckFinding::Directory(
                    DirectoryScoped::provider_type_check(ParseWarning {
                        kind: WarningKind::ProviderSchemasNotLoaded,
                        file: None, line: 0, span: None, message,
                    })
                ),
            };
            if loaded { ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(warning) }
            else { ProviderTypeCheckDiagnostic::Deferred(warning) }
        }).collect();
        Self {
            output,
            diagnostics,
        }
    }

    pub fn diagnostics(&self) -> &[ProviderTypeCheckDiagnostic] {
        &self.diagnostics
    }

    pub fn publish(self, sink: &mut (impl ProviderTypeCheckSink + ?Sized)) -> T {
        for diagnostic in self.diagnostics {
            sink.record(diagnostic);
        }
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_settles_evidence_when_a_producer_returns_early_error() {
        let checks = TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                session.record(
                    &TypeExpr::String,
                    ProviderTypeCheckSite::new(
                        crate::parser::parse_file(
                            "\n\n\n attributes { value: Missing = 'id' }",
                            Path::new("/project/main.crn"),
                            &crate::parser::ProviderContext::new(
                                crate::EnvironmentSnapshot::empty(),
                            ),
                        )
                        .unwrap()
                        .attribute_params[0]
                            .type_check_location()
                            .clone(),
                        "early check",
                    ),
                );
                Err::<(), _>("producer failed")?;
                Ok(())
            },
        );
        let mut diagnostics = Vec::new();
        assert_eq!(checks.publish(&mut diagnostics), Err("producer failed"));
        assert!(
            matches!(&diagnostics[..], [ProviderTypeCheckDiagnostic::Deferred(warning)] if warning.line == 4)
        );
    }

    #[test]
    fn settlement_value_producer_retains_all_provider_leaves() {
        let result = TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |context| {
                let expected = TypeExpr::Struct {
                    fields: vec![
                        (
                            "first".into(),
                            crate::parser::parse_type_expr_str("FirstId", context.catalog())
                                .unwrap(),
                        ),
                        (
                            "second".into(),
                            crate::parser::parse_type_expr_str("SecondId", context.catalog())
                                .unwrap(),
                        ),
                    ],
                };
                let value = crate::resource::AuthoredValue::Concrete(
                    crate::resource::AuthoredConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "first".into(),
                            crate::resource::AuthoredValue::Concrete(
                                crate::resource::AuthoredConcreteValue::String("a".into()),
                            ),
                        ),
                        (
                            "second".into(),
                            crate::resource::AuthoredValue::Concrete(
                                crate::resource::AuthoredConcreteValue::String("b".into()),
                            ),
                        ),
                    ])),
                );
                super::super::typecheck::check_type_expr_value_providerless(
                    &expected,
                    &value,
                    context.catalog(),
                    &crate::parser::retained::value_dependent_judgement_for_test(),
                )
                .record_at(
                    context,
                    ProviderTypeCheckSite::new(
                        crate::parser::parse_file(
                            "attributes { value: Missing = 'id' }",
                            Path::new("/project/main.crn"),
                            &crate::parser::ProviderContext::new(
                                crate::EnvironmentSnapshot::empty(),
                            ),
                        )
                        .unwrap()
                        .attribute_params[0]
                            .type_check_location()
                            .clone(),
                        "argument 'ids'",
                    ),
                )
                .unwrap();
            },
        );
        assert_eq!(result.diagnostics().len(), 2, "{:?}", result.diagnostics());
    }

    #[test]
    fn loaded_settlement_rejects_artificially_pending_check() {
        let context = crate::parser::LoadedProviderContext::empty_for_test();

        let result = TypeCheckSession::run(&context, |sink| {
            sink.record(
                &crate::parser::parse_type_expr_str(
                    "Typo",
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap(),
                ProviderTypeCheckSite::new(
                    crate::parser::parse_file(
                        "\n\n      attributes { value: Missing = 'id' }",
                        Path::new("/project/main.crn"),
                        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    )
                    .unwrap()
                    .attribute_params[0]
                        .type_check_location()
                        .clone(),
                    "attribute 'x'",
                ),
            );
        });
        assert!(
            matches!(result.diagnostics(), [ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error)] if error.message.contains("fully loaded"))
        );
    }

    #[test]
    fn sink_deduplicates_authored_sites_without_an_ast_rewalk() {
        let result = TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |sink| {
                let site = ProviderTypeCheckSite::new(
                    crate::parser::parse_file(
                        "\n\n      attributes { value: Missing = 'id' }",
                        Path::new("/project/main.crn"),
                        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    )
                    .unwrap()
                    .attribute_params[0]
                        .type_check_location()
                        .clone(),
                    "attribute 'x'",
                );
                let expected = crate::parser::parse_type_expr_str(
                    "Arn",
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                sink.record(&expected, site.clone());
                sink.record(&expected, site);
            },
        );
        assert_eq!(result.diagnostics().len(), 1);
    }
}

#[cfg(test)]
mod partial_catalog_tests {
    use super::*;
    use crate::parser::{OwnedTypeIdentity, ProviderCustomTypes, parse_file};
    use crate::schema::{RefinedScalarKind, TypeIdentity};

    fn partial() -> ProviderContext {
        let loaded = ProviderCustomTypes::loaded(
            std::collections::HashSet::from([
                OwnedTypeIdentity::provider(
                    "ready",
                    TypeIdentity::new(Some("ready"), Vec::<String>::new(), "ReadyId"),
                    RefinedScalarKind::String,
                ),
                OwnedTypeIdentity::provider(
                    "ready",
                    TypeIdentity::from_dotted("ready.Value.Id"),
                    RefinedScalarKind::String,
                ),
            ]),
            Box::new(|_, _| Ok(())),
        );
        ProviderContext {
            custom_types: loaded
                .with_failed_providers(std::collections::BTreeSet::from(["broken".into()])),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        }
    }

    #[test]
    fn empty_loaded_catalog_stays_strict_without_failed_providers() {
        let catalog = ProviderContext {
            custom_types: ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(|_, _| panic!("unknown declarations must not call validators")),
            )
            .with_failed_providers(Default::default()),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };
        assert!(catalog.custom_types_loaded());
        assert!(!catalog.custom_types.may_defer(None));
        assert!(!catalog.custom_types.may_defer(Some("aws")));
        let result = TypeCheckSession::run(&catalog, |session| {
            let mut parsed = parse_file(
                "attributes { x: AwsAccountId = '123' }",
                Path::new("/project/main.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            let errors = crate::validation::resolve_file_type_exprs(&mut parsed, session).unwrap();
            assert_eq!(errors.len(), 1, "{errors:#?}");
            assert!(
                errors[0]
                    .to_string()
                    .contains("unknown custom type 'AwsAccountId'")
            );
        });
        assert!(result.diagnostics().is_empty());
    }

    #[test]
    fn failed_function_declaration_keeps_unrelated_pending_call_checks() {
        for failed_call in ["", "exports { failed: String = bad('id') }\n"] {
            let result = TypeCheckSession::run(&partial(), |session| {
                let mut parsed = parse_file(
                    &format!("fn bad(x: ready.Missing) {{ x }}\nfn echo(x: Arn): String {{ 'ok' }}\n{failed_call}exports {{ x = echo('id') }}\n"),
                    Path::new("/project/main.crn"),
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                let errors =
                    crate::validation::resolve_file_type_exprs(&mut parsed, session).unwrap();
                assert_eq!(errors.len(), 1, "{errors:#?}");
                assert!(errors[0].to_string().contains("ready.Missing"));
                crate::parser::resolve_resource_refs_in_session(&mut parsed, session)
                    .expect("only the failed declaration owns its error");
            });
            assert_eq!(result.diagnostics().len(), 2);
            assert!(result.diagnostics().iter().all(|diagnostic| matches!(
                diagnostic,
                ProviderTypeCheckDiagnostic::Deferred(warning)
                    if warning.message.contains("type check for Arn was deferred")
            )));
        }
    }

    #[test]
    fn failed_function_declaration_does_not_block_same_name_in_another_file() {
        let result = TypeCheckSession::run(&partial(), |session| {
            let mut failed = parse_file(
                "fn echo(x: ready.Missing) { x }",
                Path::new("/project/bad.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            assert_eq!(
                crate::validation::resolve_file_type_exprs(&mut failed, session)
                    .unwrap()
                    .len(),
                1
            );
            let mut other = parse_file(
                "fn echo(x: Arn): String { 'ok' }\nexports { x = echo('id') }",
                Path::new("/project/other.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            assert!(
                crate::validation::resolve_file_type_exprs(&mut other, session)
                    .unwrap()
                    .is_empty()
            );
            crate::parser::resolve_resource_refs_in_session(&mut other, session).unwrap();
        });
        assert_eq!(result.diagnostics().len(), 2);
    }

    #[test]
    fn settlement_partial_catalog_loaded_owner_cannot_defer() {
        let catalog = ProviderCustomTypes::loaded(
            std::collections::HashSet::from([OwnedTypeIdentity::provider(
                "ready",
                TypeIdentity::new(Some("ready"), Vec::<String>::new(), "ReadyId"),
                RefinedScalarKind::String,
            )]),
            Box::new(|_, _| Err(crate::parser::CustomTypeValidationError::ProvidersNotLoaded)),
        );
        let context = ProviderContext {
            custom_types: catalog
                .with_failed_providers(std::collections::BTreeSet::from(["broken".into()])),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };

        assert!(
            TypeCheckSession::run(&context, |context| {
                let mut parsed = parse_file(
                    "attributes { x: ready.ReadyId = 'ok' }",
                    Path::new("/project/main.crn"),
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                assert!(
                    crate::validation::resolve_file_type_exprs(&mut parsed, context)
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    crate::validation::validate_attribute_params_with_context(
                        &parsed.attribute_params,
                        context
                    )
                    .is_err(),
                    "a loaded owner's missing evidence must be an error, not a deferred warning"
                );
            })
            .diagnostics()
            .is_empty()
        );
    }

    #[test]
    fn settlement_partial_catalog_preserves_owner_authority_and_unknown_bare_names() {
        let catalog = partial();
        for (source, expected_errors, expected_warnings) in [
            ("attributes { x: ready.ReadyId = 'ok' }", 0, 0),
            ("attributes { x: ready.Value.Id = 'ok' }", 0, 0),
            ("attributes { x: ready.Value.Typo = 'ok' }", 1, 0),
            ("attributes { x: broken.Value.Id = 'ok' }", 0, 1),
            ("attributes { x: broken.Count = 42 }", 0, 1),
            ("attributes { x: BareTypo = 'ok' }", 0, 1),
            ("fn f(x: Arnn) { x }\nfn g(): Bogus { 'x' }", 0, 2),
        ] {
            let result = TypeCheckSession::run(&catalog, |context| {
                let mut parsed = parse_file(
                    source,
                    Path::new("/project/main.crn"),
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                let errors =
                    crate::validation::resolve_file_type_exprs(&mut parsed, context).unwrap();
                assert_eq!(errors.len(), expected_errors, "{source}: {errors:#?}");
                if errors.is_empty() {
                    crate::validation::validate_attribute_params_with_context(
                        &parsed.attribute_params,
                        context,
                    )
                    .unwrap();
                }
                errors
            });
            assert_eq!(
                result.diagnostics().len(),
                expected_warnings,
                "{source}: {:?}",
                result.diagnostics()
            );
        }
    }
}

#[cfg(test)]
mod producer_tests {
    use super::*;
    use crate::parser::parse_file;

    #[test]
    fn sessionless_function_handle_annotation_stays_in_the_deferred_call() {
        let mut parsed = parse_file(
            "let source = mock.test.resource { name = 'one' }\nfn ignore(x: missing.Widget) { 'ok' }\nattributes { value: String = ignore(source) }\n",
            Path::new("/project/main.crn"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        ).unwrap();
        crate::parser::resolve_resource_refs_with_config(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        assert!(
            matches!(
                parsed.attribute_params[0]
                    .value
                    .as_ref()
                    .and_then(crate::parser::BoundaryExpression::value),
                Some(crate::resource::Value::Deferred(
                    crate::resource::DeferredValue::ResolvedFunctionCall { .. }
                ))
            ),
            "a sessionless pass must preserve the annotation and its handle argument"
        );
        let checks = TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                resolve(&mut parsed, session);
            },
        );
        let (_, warnings) = warnings(checks);
        assert_eq!(
            warnings.len(),
            2,
            "the parameter declaration and call each defer: {warnings:#?}"
        );
    }

    fn warnings<T>(result: ProviderTypeCheckResult<T>) -> (T, Vec<ParseWarning>) {
        let mut diagnostics = Vec::new();
        let output = result.publish(&mut diagnostics);
        (
            output,
            diagnostics
                .into_iter()
                .map(|diagnostic| match diagnostic {
                    ProviderTypeCheckDiagnostic::Deferred(warning) => warning.into_warning(),
                    other => panic!("unexpected settlement: {other:?}"),
                })
                .collect(),
        )
    }

    fn resolve<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        context: &TypeCheckSession<'_>,
    ) {
        assert!(
            crate::validation::resolve_file_type_exprs(parsed, context)
                .unwrap()
                .is_empty()
        );
        crate::parser::resolve_resource_refs_in_session(parsed, context).unwrap();
    }

    #[test]
    fn settlement_module_failure_preserves_root_evidence() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("main.crn"), "let child = use { source = './missing' }\nchild {}\nattributes { id: AwsAccountId = '123' }\n").unwrap();

        let (resolution, warnings) = warnings(TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |context| {
                let schemas = crate::schema::SchemaRegistry::new();
                let loaded = crate::config_loader::load_configuration_with_config(
                    &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                    context.catalog(),
                    &schemas,
                )
                .unwrap();
                let resolution =
                    crate::module_resolver::resolve_module_template_with_providerless_config(
                        loaded.parsed,
                        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                        context,
                        &schemas,
                    );
                resolution.map_err(Box::new)
            },
        ));
        assert!(resolution.is_err());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].message.contains("AwsAccountId"));
        assert_eq!(
            warnings[0].file.as_deref(),
            directory.path().join("main.crn").to_str()
        );
    }

    #[test]
    fn settlement_uncalled_and_repeated_module_imports_record_once() {
        for calls in ["", "let a = child {}\nlet b = child {}\n"] {
            let directory = tempfile::tempdir().unwrap();
            let child = directory.path().join("child");
            std::fs::create_dir(&child).unwrap();
            std::fs::write(
                child.join("main.crn"),
                "attributes { id: AwsAccountId = '123' }\n",
            )
            .unwrap();
            std::fs::write(
                directory.path().join("main.crn"),
                format!("let child = use {{ source = './child' }}\n{calls}"),
            )
            .unwrap();

            let (resolution, warnings) = warnings(TypeCheckSession::run(
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                |context| {
                    let schemas = crate::schema::SchemaRegistry::new();
                    let loaded = crate::config_loader::load_configuration_with_config(
                        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                        context.catalog(),
                        &schemas,
                    )
                    .unwrap();
                    let resolution =
                        crate::module_resolver::resolve_module_template_with_providerless_config(
                            loaded.parsed,
                            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                            context,
                            &schemas,
                        );
                    resolution.map_err(Box::new)
                },
            ));
            resolution.unwrap();
            assert_eq!(warnings.len(), 1, "{warnings:#?}");
            assert_eq!(warnings[0].file.as_deref(), child.join("main.crn").to_str());
        }
    }

    #[test]
    fn settlement_calls_and_annotations_keep_their_own_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("functions.crn"),
            "fn f(x: Arn) { x }\n",
        )
        .unwrap();
        let path = directory.path().join("calls.crn");
        std::fs::write(
            &path,
            "let a = f('one')\nattributes { x: AwsAccountId = '123' }\n",
        )
        .unwrap();

        let (_, warnings) = warnings(TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |context| {
                let loaded = crate::config_loader::load_configuration_with_config(
                    &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                    context.catalog(),
                    &crate::schema::SchemaRegistry::new(),
                )
                .unwrap();
                let mut parsed = loaded.parsed.parsed().clone();
                resolve(&mut parsed, context);
                parsed
            },
        ));
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.file.as_deref() == path.to_str())
                .count(),
            2,
            "{warnings:#?}"
        );
    }

    #[test]
    fn settlement_records_unchecked_assignment_at_its_declaration() {
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let (_, warnings) = warnings(TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |context| {
                let mut parsed = parse_file(
                    "arguments { id: AwsAccountId }\nattributes { value: String = id }\n",
                    Path::new("/project/main.crn"),
                    context.catalog(),
                )
                .unwrap();
                resolve(&mut parsed, context);
                let schemas = crate::schema::SchemaRegistry::new();
                let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
                let bindings = crate::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(&parsed, &schemas, None, &signatures, context);
                let crate::binding_index::RefType::Typed(source) =
                    bindings.ref_type(&crate::resource::RelativeBindingName::new("id"))
                else {
                    panic!("typed argument")
                };
                let assignment = source
                    .is_assignable_to_sink(
                        crate::schema::TypeInSchema::schemaless(
                            &crate::schema::CompiledAttributeType::string(),
                        )
                        .with_authority(
                            &context.catalog().custom_types,
                            crate::schema::SinkAuthority::Owner(&owner),
                        ),
                        &bindings,
                    )
                    .unwrap();
                match assignment {
                    crate::schema::AdmittedTypeAssignment::Assignable => {}
                    crate::schema::AdmittedTypeAssignment::NeedsLocal(obligation) => {
                        panic!("plain String has no member obligation: {obligation:?}")
                    }
                }
            },
        ));
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].message.contains("AwsAccountId"));
        assert_eq!(warnings[0].line, 1);
    }

    #[test]
    fn settlement_retains_nested_annotation_leaves_and_loaded_retry_is_strict() {
        let (mut parsed, warnings) = warnings(TypeCheckSession::run(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |context| {
                let path = Path::new("/project/main.crn");
                let mut parsed = parse_file(
                    "exports { ids: list(AwsAccountId) = ['123', '456'] }",
                    path,
                    context.catalog(),
                )
                .unwrap();
                resolve(&mut parsed, context);
                parsed
            },
        ));
        assert_eq!(warnings.len(), 1, "one authored annotation: {warnings:#?}");
        assert_eq!(warnings[0].file.as_deref(), Some("/project/main.crn"));

        let result = TypeCheckSession::run(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |loaded| {
                assert_eq!(
                    crate::validation::resolve_file_type_exprs(&mut parsed, loaded)
                        .unwrap()
                        .len(),
                    1
                );
            },
        );
        assert!(result.diagnostics().is_empty());
    }

    #[test]
    fn settlement_keeps_distinct_calls_on_the_same_line() {
        for annotation in ["Arn", "missing.Id"] {
            let (_, warnings) = warnings(TypeCheckSession::run(
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                |context| {
                    let mut parsed = parse_file(
                        &format!(
                            "fn f(x: {annotation}) {{ x }}\nlet pair = [f('one'), f('two')]\n"
                        ),
                        Path::new("/project/main.crn"),
                        context.catalog(),
                    )
                    .unwrap();
                    resolve(&mut parsed, context);
                    parsed
                },
            ));
            assert_eq!(
                warnings.len(),
                3,
                "one annotation and two calls: {warnings:#?}"
            );
            let calls = warnings
                .iter()
                .filter(|warning| warning.line == 2)
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 2);
            assert_ne!(
                calls[0].span.as_ref().unwrap().start_column,
                calls[1].span.as_ref().unwrap().start_column
            );
        }
    }
}

#[cfg(test)]
mod session_isolation_tests {
    use super::*;
    use crate::parser::{OwnedTypeIdentity, ProviderCustomTypes};
    use crate::schema::{RefinedScalarKind, TypeIdentity};

    #[test]
    fn partial_bare_match_retains_loaded_candidates_without_claiming_authority() {
        let owned = OwnedTypeIdentity::provider(
            "awscc",
            TypeIdentity::bare("SharedId"),
            RefinedScalarKind::String,
        );
        let catalog = ProviderContext {
            custom_types: ProviderCustomTypes::loaded(
                std::collections::HashSet::from([owned.clone()]),
                Box::new(|_, _| {
                    panic!("partial bare match must not validate only the loaded subset")
                }),
            )
            .with_failed_providers(std::collections::BTreeSet::from(["aws".into()])),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };

        let terminal = TypeCheckSession::run(&catalog, |session| {
            let mut parsed = crate::parser::parse_file(
                "fn f(x: SharedId) { x }",
                Path::new("/project/main.crn"),
                &catalog,
            )
            .unwrap();
            assert!(
                crate::validation::resolve_file_type_exprs(&mut parsed, session)
                    .unwrap()
                    .is_empty()
            );
            let TypeExpr::UnloadedSchemaType(unloaded) = parsed.user_functions["f"].params[0]
                .param_type
                .as_ref()
                .unwrap()
            else {
                panic!("bare match must remain deferred")
            };
            assert_eq!(unloaded.identity(), &TypeIdentity::bare("SharedId"));
            assert_eq!(unloaded.loaded_candidates(), &[owned]);
            parsed
        });
        assert!(
            matches!(terminal.diagnostics(), [ProviderTypeCheckDiagnostic::Deferred(warning)] if warning.line == 1 && warning.message.contains("SharedId"))
        );
    }

    #[test]
    fn unloaded_sessions_are_independent_even_when_the_catalog_is_shared() {
        let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());

        assert_eq!(
            TypeCheckSession::run(&catalog, |first| {
                let expected = crate::parser::parse_type_expr_str("Missing", &catalog).unwrap();
                first.record(
                    &expected,
                    ProviderTypeCheckSite::new(
                        crate::parser::parse_file(
                            "\n\n    attributes { value: Missing = 'id' }",
                            Path::new("/project/a.crn"),
                            &crate::parser::ProviderContext::new(
                                crate::EnvironmentSnapshot::empty(),
                            ),
                        )
                        .unwrap()
                        .attribute_params[0]
                            .type_check_location()
                            .clone(),
                        "first run",
                    ),
                );
            })
            .diagnostics()
            .len(),
            1
        );
        assert!(
            TypeCheckSession::run(&catalog, |_session| {})
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn recorded_queries_keep_distinct_sites_and_replace_synthetic_ownership() {
        let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());

        let terminal = TypeCheckSession::run(&catalog, |session| {
            let expected = crate::parser::parse_type_expr_str("Missing", &catalog).unwrap();
            session.record_type(&expected, "initial query");
            session.declaration(
                &expected,
                ProviderTypeCheckSite::new(
                    crate::parser::parse_file(
                        "\n   attributes { value: Missing = 'id' }",
                        Path::new("/project/main.crn"),
                        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    )
                    .unwrap()
                    .attribute_params[0]
                        .type_check_location()
                        .clone(),
                    "annotation",
                ),
            );
            session.record(
                &expected,
                ProviderTypeCheckSite::new(
                    crate::parser::parse_file(
                        "\n\n      attributes { value: Missing = 'id' }",
                        Path::new("/project/main.crn"),
                        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    )
                    .unwrap()
                    .attribute_params[0]
                        .type_check_location()
                        .clone(),
                    "query one",
                ),
            );
            session.record(
                &expected,
                ProviderTypeCheckSite::new(
                    crate::parser::parse_file(
                        "\n\n           attributes { value: Missing = 'id' }",
                        Path::new("/project/main.crn"),
                        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                    )
                    .unwrap()
                    .attribute_params[0]
                        .type_check_location()
                        .clone(),
                    "query two",
                ),
            );
        });
        assert_eq!(terminal.diagnostics().len(), 3);
        assert!(terminal.diagnostics().iter().all(|diagnostic| matches!(diagnostic, ProviderTypeCheckDiagnostic::Deferred(warning) if warning.file.as_deref()==Some("/project/main.crn"))));
    }
}
