use std::io::{self, Write};
use std::path::Path;

use carina_core::config_loader::{ConfigurationLoadDiagnostic, load_configuration_report};
use carina_core::parser::{DeclarationLocation, ParseWarning, ProviderContext, SourceSpan};
use carina_core::paths::{DisplayPath, ResolvedPath};
use serde::Serialize;

use crate::error::{AppError, format_error_with_source_chain};
use crate::output_style::{ColorMode, Colorize};
use crate::runtime::CommandRuntime;
use crate::wiring::{
    NonEmptyValidationErrors, ProviderLoad, ProviderLoadEvent, ReportingProviderLoad,
    ValidationReportParts,
};

/// Complete command output. Only the collector can construct an outcome;
/// failure cannot carry an inventory or an empty error collection.
pub struct ValidateReport {
    outcome: Outcome,
    warnings: Vec<Warning>,
    events: Vec<HumanEvent>,
}

enum Outcome {
    Success(Vec<String>),
    Failure(NonEmptyValidationErrors),
}

enum Warning {
    Parse(ParseWarning),
    Input(carina_core::input_rules::InputFinding),
    Dependency(
        carina_core::parser::Located<carina_core::validation::depends_on::DependsOnDiagnostic>,
    ),
    Unused(carina_core::parser::Located<String>),
    Duplicate {
        file: String,
        finding: carina_core::lint::DuplicateAttr,
    },
}

enum HumanEvent {
    Progress,
    Notice(ProviderLoadEvent),
    Warning(usize, WarningStyle),
}

#[derive(Clone, Copy)]
enum WarningStyle {
    Parse,
    Provider,
    Input,
    Dependency,
}

#[derive(Serialize)]
pub(super) struct ValidateOutput {
    pub status: &'static str,
    pub resource_count: usize,
    pub resources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Diagnostic>,
}

/// Only safe diagnostic projections reach Serde, never provider ErrorText,
/// values, paths within values, or declaration provenance.
#[derive(Serialize)]
pub(super) struct Diagnostic {
    pub message: String,
    pub severity: &'static str,
    pub code: Option<String>,
    pub location: Option<Location>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub warning_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Serialize)]
pub(super) struct Location {
    // Complete source label, relative to the process CWD when not absolute.
    // Never an import path to be joined to the project directory again.
    #[serde(serialize_with = "Location::serialize_file")]
    file: DisplayPath,
    range: Option<Range>,
}
#[derive(Serialize)]
struct Range {
    start: Position,
    end: Position,
}
#[derive(Serialize)]
struct Position {
    line: usize,
    column: usize,
}

impl Location {
    fn serialize_file<S: serde::Serializer>(
        file: &DisplayPath,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(file)
    }

    fn authored(location: &DeclarationLocation) -> Self {
        Self::span(location.file().path(), location.span())
    }
    fn span(file: &Path, span: SourceSpan) -> Self {
        Self {
            file: DisplayPath::for_diagnostics(file),
            range: Some(Range {
                start: Position {
                    line: span.start_line(),
                    column: span.start_column(),
                },
                end: Position {
                    line: span.end_line(),
                    column: span.end_column(),
                },
            }),
        }
    }
    fn file(file: &Path) -> Self {
        Self {
            file: DisplayPath::for_diagnostics(file),
            range: None,
        }
    }
}

impl Diagnostic {
    pub(super) fn new(message: String, severity: &'static str) -> Self {
        Self {
            message,
            severity,
            code: None,
            location: None,
            warning_type: None,
            file: None,
        }
    }
    fn located(message: String, location: &DeclarationLocation) -> Self {
        Self {
            location: Some(Location::authored(location)),
            ..Self::new(message, "error")
        }
    }
    fn input(finding: &carina_core::input_rules::InputFinding, severity: &'static str) -> Self {
        Self {
            code: Some(finding.violation().code().to_string()),
            location: finding.authored_location().map(Location::authored),
            warning_type: Some("input_rule"),
            ..Self::new(finding.to_string(), severity)
        }
    }
    fn parse(warning: &ParseWarning) -> Self {
        Self {
            code: match warning.kind {
                carina_core::parser::WarningKind::ProviderSchemasNotLoaded => {
                    Some("providers_not_loaded".into())
                }
                _ => None,
            },
            location: warning.file.as_ref().map(|file| Location {
                file: DisplayPath::for_diagnostics(file),
                range: warning.span.as_ref().map(|span| Range {
                    start: Position {
                        line: span.start_line,
                        column: span.start_column,
                    },
                    end: Position {
                        line: span.end_line,
                        column: span.end_column,
                    },
                }),
            }),
            ..Self::new(warning.message.clone(), "warning")
        }
    }
}

/// Central conversion for residual application errors. Typed branches retain
/// their own message authorities; the fallback uses the terminal's safe chain.
fn error_diagnostics(error: &AppError) -> Vec<Diagnostic> {
    let diagnostic = match error {
        AppError::ProviderConfigInvariant(error) => {
            return error
                .findings()
                .iter()
                .map(|finding| Diagnostic::new(format_error_with_source_chain(finding), "error"))
                .collect();
        }
        AppError::FinalProviderInput(error) => {
            return error
                .findings()
                .iter()
                .map(|finding| Diagnostic::new(format_error_with_source_chain(finding), "error"))
                .collect();
        }
        AppError::LockConstraint(carina_provider_resolver::LockConstraintError::LockFile(
            carina_provider_resolver::LockFileError::Parse { path, .. },
        )) => Diagnostic {
            // TOML's Display and even its parser message may contain source
            // values. Keep the typed file without serializing either payload.
            location: Some(Location::file(path)),
            ..Diagnostic::new("Failed to parse provider lock file".to_string(), "error")
        },
        AppError::ProviderTypeCheck(error) => Diagnostic {
            severity: "error",
            code: None,
            ..Diagnostic::parse(error)
        },
        AppError::UpstreamReference(error) => {
            let (location, message) = error.diagnostic();
            Diagnostic::located(message, location)
        }
        AppError::ExportBoundary(error) => Diagnostic {
            location: Some(Location::span(error.source_file(), error.source_span())),
            ..Diagnostic::new(error.to_string(), "error")
        },
        AppError::ModuleResolution(failure) => {
            return module_error_diagnostics(failure.individual_errors());
        }
        AppError::UpstreamResolution(finding) => {
            if let Some(error) = finding.detail().parse_failure() {
                if let Some(element) = error.element_diagnostic() {
                    return vec![Diagnostic::located(
                        element.detail().diagnostic_message(),
                        element.location(),
                    )];
                }
                if let Some(parse) = error.parse_error() {
                    return vec![parse_diagnostic(parse, error.source_file())];
                }
            }
            if let Some((_, error)) = finding.detail().module_failure() {
                return module_error_diagnostics(error.individual_errors());
            }
            let boundaries = finding.detail().boundary_errors();
            if !boundaries.is_empty() {
                return boundaries
                    .iter()
                    .map(|error| Diagnostic {
                        location: Some(Location::span(error.source_file(), error.source_span())),
                        ..Diagnostic::new(error.to_string(), "error")
                    })
                    .collect();
            }
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::AttributePrefix(error) => Diagnostic {
            location: error.location().map(Location::authored),
            ..Diagnostic::new(error.to_string(), "error")
        },
        AppError::ParameterReference { finding, .. } => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::ParameterValue { finding, .. } => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::UpstreamSource(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::InputFinding(finding) => Diagnostic::input(finding, "error"),
        AppError::InputPreparation(error) => Diagnostic {
            location: error.authored_location().map(Location::authored),
            ..Diagnostic::new(format_error_with_source_chain(error.as_ref()), "error")
        },
        AppError::ModuleCall(finding) => {
            Diagnostic::located(finding.message().to_string(), finding.anchor().location())
        }
        AppError::TypeResolution(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::BindingUse(finding) => Diagnostic::located(
            format_error_with_source_chain(&finding.error),
            finding.use_site.location(),
        ),
        AppError::BindingUseReport(report) => {
            return report
                .findings()
                .iter()
                .map(|finding| {
                    Diagnostic::located(
                        format_error_with_source_chain(&finding.error),
                        finding.use_site.location(),
                    )
                })
                .collect();
        }
        AppError::ExpressionType(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::DependsOn(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::Wait(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::DeferredPopulate(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::ProviderRouting(finding) => {
            Diagnostic::located(finding.detail().to_string(), finding.location())
        }
        AppError::ResourceAdmission(finding) => Diagnostic {
            location: finding.location().map(Location::authored),
            ..Diagnostic::new(finding.message(), "error")
        },
        AppError::ConfigurationLoad(failure) => {
            return failure
                .diagnostics()
                .iter()
                .map(|diagnostic| match diagnostic {
                    ConfigurationLoadDiagnostic::Parse { file, error } => {
                        parse_diagnostic(error, file.as_deref())
                    }
                    ConfigurationLoadDiagnostic::Message { file, message } => Diagnostic {
                        location: file.as_deref().map(Location::file),
                        ..Diagnostic::new(message.clone(), "error")
                    },
                })
                .collect();
        }
        AppError::Parse(error) => parse_diagnostic(error, error.source_file()),
        AppError::ValidationFileRead { file, .. } => Diagnostic {
            location: Some(Location::file(file)),
            ..Diagnostic::new(format_error_with_source_chain(error), "error")
        },
        AppError::AnalysisResolution(error) => match error.as_ref() {
            carina_core::parser::AnalysisResolutionError::Diagnostic(finding) => {
                Diagnostic::located(finding.detail().diagnostic_message(), finding.location())
            }
            _ => Diagnostic::new(format_error_with_source_chain(error.as_ref()), "error"),
        },
        _ => Diagnostic::new(format_error_with_source_chain(error), "error"),
    };
    vec![diagnostic]
}

fn module_error_diagnostics(
    errors: Vec<carina_core::module_resolver::ModuleErrorDiagnostic<'_>>,
) -> Vec<Diagnostic> {
    use carina_core::module_resolver::ModuleError;
    errors
        .into_iter()
        .flat_map(|finding| {
            use carina_core::module_resolver::ModuleErrorCallSource;
            let anchor = match finding.call_source {
                ModuleErrorCallSource::Failing(call) => Some(call.module_name_location()),
                ModuleErrorCallSource::Owners(calls) => {
                    calls.first().map(|call| call.module_name_location())
                }
                ModuleErrorCallSource::Unavailable => None,
            };
            let mut diagnostics = match finding.error {
                ModuleError::DirectoryConstraint(finding) => {
                    vec![Diagnostic::located(
                        finding.detail().to_string(),
                        finding.location(),
                    )]
                }
                ModuleError::Parse(error) => vec![parse_diagnostic(error, error.source_file())],
                ModuleError::ParseInFile { path, error } => {
                    vec![parse_diagnostic(error, Some(path))]
                }
                ModuleError::ReferenceType(errors) => errors
                    .iter()
                    .map(|error| Diagnostic::new(error.to_string(), "error"))
                    .collect(),
                error => vec![Diagnostic::new(error.to_string(), "error")],
            };
            for diagnostic in &mut diagnostics {
                if diagnostic.location.is_none() {
                    diagnostic.location = anchor.map(Location::authored);
                }
            }
            diagnostics
        })
        .collect()
}

fn parse_diagnostic(error: &carina_core::parser::ParseError, file: Option<&Path>) -> Diagnostic {
    use carina_core::parser::ParseError;
    if let ParseError::Located { path, error, .. } = error {
        return parse_diagnostic(error, Some(path));
    }
    if let Some(finding) = error.element_diagnostic() {
        return Diagnostic::located(finding.detail().diagnostic_message(), finding.location());
    }
    Diagnostic {
        location: file.map(|file| Location {
            file: DisplayPath::for_diagnostics(file),
            range: error.diagnostic_range().map(|span| Range {
                start: Position {
                    line: span.start_line,
                    column: span.start_column,
                },
                end: Position {
                    line: span.end_line,
                    column: span.end_column,
                },
            }),
        }),
        ..Diagnostic::new(error.diagnostic_message(), "error")
    }
}

impl Warning {
    fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::Parse(warning) => Diagnostic::parse(warning),
            Self::Input(finding) => Diagnostic::input(finding, "warning"),
            Self::Dependency(finding) => Diagnostic {
                severity: "warning",
                ..Diagnostic::located(finding.detail().to_string(), finding.location())
            },
            Self::Unused(finding) => Diagnostic {
                severity: "warning",
                warning_type: Some("unused_binding"),
                ..Diagnostic::located(
                    format!("Unused let binding '{}'", finding.detail()),
                    finding.location(),
                )
            },
            Self::Duplicate { file, finding } => Diagnostic {
                warning_type: Some("duplicate_attribute"),
                file: Some(file.clone()),
                location: Some(Location {
                    file: DisplayPath::for_diagnostics(file),
                    range: Some(Range {
                        start: Position {
                            line: finding.line,
                            column: finding.column,
                        },
                        end: Position {
                            line: finding.line,
                            column: finding.end_column,
                        },
                    }),
                }),
                ..Diagnostic::new(finding.message_with_location(), "warning")
            },
        }
    }
}

impl ValidateReport {
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn into_result(self, exit_code: i32) -> Result<(), AppError> {
        match self.outcome {
            Outcome::Failure(errors) => Err(crate::commands::collapse_errors(errors.into_vec())),
            Outcome::Success(_) if exit_code == 0 => Ok(()),
            Outcome::Success(_) => {
                Err(AppError::Config("Failed to write validation output".into()))
            }
        }
    }
    fn failure(error: AppError) -> Self {
        Self::finish(Vec::new(), vec![error], Vec::new(), Vec::new())
    }
    fn finish(
        resources: Vec<String>,
        errors: Vec<AppError>,
        warnings: Vec<Warning>,
        events: Vec<HumanEvent>,
    ) -> Self {
        let outcome = match NonEmptyValidationErrors::try_from(errors) {
            Ok(errors) => Outcome::Failure(errors),
            Err(_) => Outcome::Success(resources),
        };
        Self {
            outcome,
            warnings,
            events,
        }
    }
    fn output(&self) -> ValidateOutput {
        let (status, resources, errors) = match &self.outcome {
            Outcome::Success(resources) => ("ok", resources.clone(), Vec::new()),
            Outcome::Failure(errors) => (
                "error",
                Vec::new(),
                errors
                    .as_slice()
                    .iter()
                    .flat_map(error_diagnostics)
                    .collect(),
            ),
        };
        ValidateOutput {
            status,
            resource_count: resources.len(),
            resources,
            errors,
            warnings: self.warnings.iter().map(Warning::diagnostic).collect(),
        }
    }
    pub fn exit_code(&self) -> i32 {
        match &self.outcome {
            Outcome::Success(_) => 0,
            Outcome::Failure(errors)
                if errors
                    .as_slice()
                    .iter()
                    .any(|error| matches!(error, AppError::Interrupted)) =>
            {
                130
            }
            Outcome::Failure(_) => 1,
        }
    }
    fn render_json(&self, stdout: &mut impl Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *stdout, &self.output())?;
        writeln!(stdout)
    }
    fn render_human(
        &self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
        color: ColorMode,
    ) -> io::Result<()> {
        for event in &self.events {
            match event {
                HumanEvent::Progress => writeln!(stdout, "{}", "Validating...".cyan(color))?,
                HumanEvent::Notice(notice) => {
                    if matches!(
                        notice,
                        crate::wiring::ProviderLoadEvent::Cache(
                            carina_plugin_host::WasmLoadNotice::ValidationRebuilt(_)
                        )
                    ) {
                        log::warn!(target: "carina_plugin_host::wasm_factory", "{}", notice.render_human(color));
                    } else {
                        writeln!(stderr, "{}", notice.render_human(color))?;
                    }
                }
                HumanEvent::Warning(index, style) => match (&self.warnings[*index], style) {
                    (Warning::Parse(warning), WarningStyle::Provider) => writeln!(
                        stderr,
                        "  ⚠ {}:{}:{}: {}",
                        warning.file.as_deref().unwrap_or("<config>"),
                        warning.line,
                        warning.span.as_ref().map_or(1, |span| span.start_column),
                        warning.message
                    )?,
                    (Warning::Parse(warning), _) => {
                        let location = match (&warning.file, &warning.span) {
                            (Some(file), Some(span)) => {
                                format!("{file}:{}:{}", span.start_line, span.start_column)
                            }
                            (Some(file), None) => format!("{file}:{}", warning.line),
                            (None, Some(span)) => {
                                format!("line {}:{}", span.start_line, span.start_column)
                            }
                            (None, None) => format!("line {}", warning.line),
                        };
                        writeln!(stderr, "  ⚠ {location}: {}", warning.message)?;
                    }
                    (Warning::Input(finding), _) => writeln!(stderr, "  ⚠ {finding}")?,
                    (Warning::Dependency(finding), _) => writeln!(stderr, "warning: {finding}")?,
                    _ => unreachable!("lint warnings follow the success inventory"),
                },
            }
        }
        match &self.outcome {
            Outcome::Failure(errors) => {
                let prefix = "Error:".red(color).bold(color).to_string();
                for error in errors.as_slice() {
                    if matches!(error, AppError::Interrupted) {
                        continue;
                    }
                    for line in format_error_with_source_chain(error).lines() {
                        writeln!(stderr, "{prefix} {line}")?;
                    }
                }
            }
            Outcome::Success(resources) => {
                writeln!(
                    stdout,
                    "{}",
                    format!("✓ {} resources validated successfully.", resources.len())
                        .green(color)
                        .bold(color)
                )?;
                for resource in resources {
                    writeln!(stdout, "  • {resource}")?;
                }
                for warning in &self.warnings {
                    match warning {
                        Warning::Unused(finding) => writeln!(
                            stdout,
                            "{}",
                            format!(
                                "⚠ {}",
                                carina_core::validation::unused_binding_message(finding.detail())
                            )
                            .yellow(color)
                        )?,
                        Warning::Duplicate { file, finding } => writeln!(
                            stdout,
                            "{}",
                            format!("⚠ {file}:{}", finding.message_with_location()).yellow(color)
                        )?,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn render(&self, json: bool, color: ColorMode) -> i32 {
        let result = if json {
            self.render_json(&mut io::stdout().lock())
        } else {
            self.render_human(&mut io::stdout().lock(), &mut io::stderr().lock(), color)
        };
        match result {
            Ok(()) => self.exit_code(),
            Err(error) => {
                eprintln!("Error: {error}");
                1
            }
        }
    }
}

type ProviderConfigs =
    [carina_core::parser::ProviderConfig<carina_core::resource::ParserPlaceholder>];

pub(super) fn collect(
    runtime: Result<&CommandRuntime, AppError>,
    path: &Path,
    provider_context: Option<&ProviderContext>,
    loader: impl FnOnce(
        &CommandRuntime,
        &ProviderConfigs,
        &ResolvedPath,
        &mut crate::wiring::ProviderNoticeSink,
    ) -> Result<(ReportingProviderLoad, Option<AppError>), AppError>,
) -> ValidateReport {
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => return ValidateReport::failure(error),
    };
    let owned_context = ProviderContext::new(runtime.environment().clone());
    let provider_context = provider_context.unwrap_or(&owned_context);
    let project = match ResolvedPath::new(path) {
        Ok(project) => project,
        Err(error) => return ValidateReport::failure(AppError::Config(error.to_string())),
    };
    let mut report = collect_configuration(runtime, &project, provider_context, loader);
    // Lints are independent of semantic validation. Preserve the legacy human
    // failure presentation while retaining every readable source's lints in JSON.
    let files = match carina_core::config_loader::find_crn_files_in_dir(&project) {
        Ok(files) => files,
        Err(error) => {
            report.fail_if_success(AppError::Config(error));
            return report;
        }
    };
    for file in files {
        match file.read_to_string() {
            Ok(source) => report.warnings.extend(
                carina_core::lint::find_duplicate_attrs(&source)
                    .into_iter()
                    .map(|finding| Warning::Duplicate {
                        file: file.source_label().display().to_string(),
                        finding,
                    }),
            ),
            Err(error) => report.fail_if_success(AppError::ValidationFileRead {
                file: file.source_label(),
                error,
            }),
        }
    }
    report
}

impl ValidateReport {
    fn fail_if_success(&mut self, error: AppError) {
        if matches!(self.outcome, Outcome::Success(_)) {
            self.outcome = Outcome::Failure(
                NonEmptyValidationErrors::try_from(vec![error])
                    .unwrap_or_else(|_| unreachable!("one failure")),
            );
        }
    }
}

fn collect_configuration(
    runtime: &CommandRuntime,
    project: &ResolvedPath,
    provider_context: &ProviderContext,
    loader: impl FnOnce(
        &CommandRuntime,
        &ProviderConfigs,
        &ResolvedPath,
        &mut crate::wiring::ProviderNoticeSink,
    ) -> Result<(ReportingProviderLoad, Option<AppError>), AppError>,
) -> ValidateReport {
    let loaded = match load_configuration_report(project, provider_context, &Default::default()) {
        Ok(loaded) => loaded,
        Err(error) => {
            let (errors, provider_warnings, parser_warnings) =
                crate::wiring::failed_validate_configuration(
                    project,
                    AppError::ConfigurationLoad(Box::new(error)),
                    runtime.environment().clone(),
                );
            let mut warnings = Vec::new();
            let mut events = Vec::new();
            for warning in provider_warnings {
                add_warning(
                    &mut warnings,
                    &mut events,
                    Warning::Parse(warning),
                    WarningStyle::Provider,
                );
            }
            warnings.extend(parser_warnings.into_iter().map(Warning::Parse));
            return ValidateReport::finish(Vec::new(), errors, warnings, events);
        }
    };
    let parsed = &loaded.parsed;
    let mut errors = Vec::new();
    match super::upstream_source_findings(project, parsed) {
        Ok(findings) => errors.extend(
            findings
                .into_iter()
                .map(|finding| AppError::UpstreamSource(Box::new(finding))),
        ),
        Err(error) => errors.push(AppError::AnalysisInvariant(error)),
    }
    let mut warnings = Vec::new();
    let mut events = Vec::new();
    for warning in &parsed.warnings {
        add_warning(
            &mut warnings,
            &mut events,
            Warning::Parse(warning.clone()),
            WarningStyle::Parse,
        );
    }
    events.push(HumanEvent::Progress);
    let notices = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = notices.clone();
    let mut sink = crate::wiring::ProviderNoticeSink::new(move |notice| {
        collected
            .lock()
            .expect("notice collection poisoned")
            .push(notice)
    });
    let providers = loader(runtime, &parsed.providers, project, &mut sink);
    events.extend(
        std::mem::take(&mut *notices.lock().expect("notice collection poisoned"))
            .into_iter()
            .map(HumanEvent::Notice),
    );
    let (providers, load_error) = match providers {
        Ok(providers) => providers,
        Err(error) => {
            errors.push(error);
            return ValidateReport::finish(Vec::new(), errors, warnings, events);
        }
    };
    errors.extend(load_error);
    let parts = providers
        .validation_report(parsed, project, false, &loaded.duplicate_declarations)
        .check_authored_inputs()
        .into_diagnostic_parts();
    events.extend(
        std::mem::take(&mut *notices.lock().expect("notice collection poisoned"))
            .into_iter()
            .map(HumanEvent::Notice),
    );
    let (result, schemas) = collect_parts(parts, &mut errors, &mut warnings, &mut events);
    match carina_core::validation::unused_binding_findings(&loaded.authored_bindings, schemas) {
        Ok(findings) => warnings.extend(findings.into_iter().map(Warning::Unused)),
        Err(error) => errors.push(AppError::AnalysisInvariant(error)),
    }
    let parsed = match result {
        Some(parsed) => parsed,
        None => return ValidateReport::finish(Vec::new(), errors, warnings, events),
    };
    if !errors.is_empty() {
        return ValidateReport::finish(Vec::new(), errors, warnings, events);
    }
    ValidateReport::finish(
        super::validated_entries(&parsed)
            .iter()
            .map(ToString::to_string)
            .collect(),
        errors,
        warnings,
        events,
    )
}

fn add_warning(
    warnings: &mut Vec<Warning>,
    events: &mut Vec<HumanEvent>,
    warning: Warning,
    style: WarningStyle,
) {
    events.push(HumanEvent::Warning(warnings.len(), style));
    warnings.push(warning);
}

fn collect_parts<'a, T>(
    parts: ValidationReportParts<'a, T>,
    errors: &mut Vec<AppError>,
    warnings: &mut Vec<Warning>,
    events: &mut Vec<HumanEvent>,
) -> (Option<T>, &'a carina_core::schema::SchemaRegistry) {
    for warning in parts.early_warnings {
        match warning {
            crate::commands::ConfigurationWarning::Dependency(warning) => add_warning(
                warnings,
                events,
                Warning::Dependency(warning),
                WarningStyle::Dependency,
            ),
            crate::commands::ConfigurationWarning::Provider(warning) => add_warning(
                warnings,
                events,
                Warning::Parse(warning),
                WarningStyle::Provider,
            ),
            crate::commands::ConfigurationWarning::Parser(_) => {
                unreachable!("expansion warnings are rendered after settlement")
            }
        }
    }
    for warning in parts.warnings {
        add_warning(
            warnings,
            events,
            Warning::Parse(warning),
            WarningStyle::Provider,
        );
    }
    for warning in parts.input_warnings {
        add_warning(
            warnings,
            events,
            Warning::Input(warning),
            WarningStyle::Input,
        );
    }
    for warning in parts.parser_warnings {
        // The former human path printed expansion warnings only when the
        // pipeline returned a file. JSON still retains them on failure.
        if parts.result.is_ok() {
            add_warning(
                warnings,
                events,
                Warning::Parse(warning),
                WarningStyle::Parse,
            );
        } else {
            warnings.push(Warning::Parse(warning));
        }
    }
    let result = match parts.result {
        Ok(parsed) => Some(parsed),
        Err(failures) => {
            errors.extend(failures.into_vec());
            None
        }
    };
    (result, parts.schemas)
}

pub(super) fn collect_production(
    runtime: Result<&CommandRuntime, AppError>,
    path: &Path,
    context: Option<&ProviderContext>,
) -> ValidateReport {
    collect(runtime, path, context, ProviderLoad::load_reporting)
}

#[cfg(any(test, feature = "test-utils"))]
pub(super) fn test_diagnostics(report: ValidateReport) -> super::ValidateDiagnostics {
    let json = serde_json::to_value(report.output()).expect("safe validation output");
    let errors = match report.outcome {
        Outcome::Success(_) => Vec::new(),
        Outcome::Failure(errors) => errors.into_vec(),
    };
    let mut diagnostics = super::ValidateDiagnostics {
        errors,
        warnings: Vec::new(),
        input_warnings: Vec::new(),
        json,
    };
    for warning in report.warnings {
        match warning {
            Warning::Parse(warning) => diagnostics.warnings.push(warning),
            Warning::Input(finding) => diagnostics.input_warnings.push(finding),
            _ => {}
        }
    }
    diagnostics
}

#[cfg(any(test, feature = "test-utils"))]
pub(super) fn test_inventory(report: ValidateReport) -> Vec<String> {
    match report.outcome {
        Outcome::Success(resources) => resources,
        Outcome::Failure(errors) => panic!(
            "fixture failed validation: {}",
            errors
                .as_slice()
                .iter()
                .map(|error| format_error_with_source_chain(error))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_4304_3a_merge_provider_config_findings_are_individual_json_diagnostics() {
        use carina_core::parser::{
            ProviderConfig, ProviderConfigInvariantError, ResolvedProviderConfigs,
        };
        use carina_core::resource::{NoPlaceholder, RelativeBindingName, Value};
        let reference = Value::<NoPlaceholder, RelativeBindingName>::resource_ref(
            RelativeBindingName::new("management"),
            "id",
            vec![],
        )
        .resolve_root_for_test();
        let config = ProviderConfig {
            name: "mock".into(),
            attributes: Default::default(),
            default_tags: [("Owner", reference.clone()), ("Team", reference)]
                .into_iter()
                .collect(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let errors = ResolvedProviderConfigs::validate_known(&[config]).unwrap_err();
        let cases = [
            (errors, vec!["default_tags.Owner", "default_tags.Team"]),
            (
                ProviderConfigInvariantError::ExpectedMap {
                    provider: "mock".into(),
                    attribute: carina_core::parser::DEFAULT_TAGS_ATTRIBUTE.into(),
                },
                vec!["default_tags"],
            ),
        ];
        for (error, slots) in cases {
            let report =
                ValidateReport::failure(AppError::ProviderConfigInvariant(Box::new(error)));
            let output = json(&report);
            let diagnostics = output["errors"].as_array().unwrap();
            assert_eq!(diagnostics.len(), slots.len(), "{output}");
            assert_eq!(report.exit_code(), 1);
            assert_eq!(output["status"], "error");
            assert_eq!(output["resource_count"], 0);
            assert_eq!(output["resources"], serde_json::json!([]));
            for (diagnostic, slot) in diagnostics.iter().zip(slots) {
                let message = diagnostic["message"].as_str().unwrap();
                assert!(message.contains("provider mock"), "{output}");
                assert!(message.contains(slot), "{output}");
                assert!(
                    !message.contains('\n'),
                    "each finding must be a separate diagnostic: {output}"
                );
                assert_eq!(diagnostic["severity"], "error");
            }
        }
    }

    #[test]
    fn recovered_type_resolution_errors_keep_the_original_located_display() {
        let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let mut parsed = carina_core::parser::parse_file(
            "attributes { x: NoSuchType = 1 }\n",
            Path::new("b.crn"),
            &catalog,
        )
        .unwrap();
        let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
            std::sync::Arc::new(Vec::new()),
            Default::default(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let findings = carina_core::validation::deferred_types::TypeCheckSession::run(
            snapshot.provider_context(),
            |session| {
                carina_core::validation::resolve_file_type_exprs(&mut parsed, session).unwrap()
            },
        )
        .publish(&mut Vec::new());
        let finding = findings
            .into_iter()
            .next()
            .expect("loaded catalog rejects the unknown type");
        let expected = format!("Error: {finding}\n");
        let report = ValidateReport::failure(AppError::TypeResolution(Box::new(finding)));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report
            .render_human(&mut stdout, &mut stderr, ColorMode::Plain)
            .unwrap();
        assert!(stdout.is_empty());
        assert_eq!(String::from_utf8(stderr).unwrap(), expected);
    }

    fn json(report: &ValidateReport) -> serde_json::Value {
        let mut bytes = Vec::new();
        report.render_json(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_file_uses_terminal_display_for_non_utf8_source_labels() {
        use std::os::unix::ffi::OsStringExt;
        let file =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"infra/\xff.crn".to_vec()));
        let output =
            serde_json::to_value(Location::file(&file)).expect("diagnostic files must serialize");
        assert_eq!(output["file"], file.display().to_string());
        assert!(output["range"].is_null());
    }

    #[test]
    fn opaque_app_error_keeps_one_safe_locationless_diagnostic_at_early_and_late_exits() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "exports { value = 'ok' }\n",
        )
        .unwrap();
        let runtime = crate::test_support::TestCommandRuntime::new();
        let text = "opaque failure\nnot-a-file.crn:42:19: explanation";
        let reports = [
            collect_production(Err(AppError::Config(text.into())), fixture.path(), None),
            collect(Ok(&runtime), fixture.path(), None, |_, _, _, _| {
                Err(AppError::Config(text.into()))
            }),
        ];
        for report in reports {
            let output = json(&report);
            assert_eq!(report.exit_code(), 1);
            assert_eq!(output["resource_count"], 0);
            assert_eq!(output["resources"], serde_json::json!([]));
            assert_eq!(output["errors"].as_array().unwrap().len(), 1);
            assert_eq!(
                output["errors"][0],
                serde_json::json!({
                    "message": text, "severity": "error", "code": null, "location": null,
                })
            );
        }
    }

    #[test]
    fn late_source_read_failure_retains_an_input_warning_and_file_only_location() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "provider mock {}\nlet tagged = mock.test.input_constrained { name = '192.168.1.1' }\nexports { name = tagged.name }\n").unwrap();
        let runtime = crate::test_support::TestCommandRuntime::new();
        let report = collect(
            Ok(&runtime),
            fixture.path(),
            None,
            |runtime, providers, _, _| {
                // Added after initial loading, this file fails only the late lint
                // source reread, after the input findings have been collected.
                std::fs::write(fixture.path().join("late.crn"), [0xff]).unwrap();
                Ok((
                    ProviderLoad::for_test_with_load_errors(
                        runtime,
                        Vec::new(),
                        providers,
                        Default::default(),
                    )
                    .into_reporting(),
                    None,
                ))
            },
        );
        let output = json(&report);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(output["resource_count"], 0);
        assert_eq!(output["resources"], serde_json::json!([]));
        assert_eq!(output["errors"].as_array().unwrap().len(), 1);
        assert_eq!(output["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(output["warnings"][0]["code"], "mock.name.not_ipv4");
        assert!(
            output["errors"][0]["location"]["file"]
                .as_str()
                .unwrap()
                .ends_with("late.crn")
        );
        assert!(output["errors"][0]["location"]["range"].is_null());
    }

    #[test]
    fn residual_provider_error_uses_bound_diagnostic_display_including_source_chain() {
        use carina_core::test_utils::provider_text::{BindProviderText, context};
        let error = carina_core::provider::ProviderError::api_error_caused_by(
            "provider rejected request\u{1b}[31m",
            io::Error::other("provider-private-payload"),
        )
        .with_secrets_for_test(&context(&["provider-private-payload"]));
        let error = AppError::Provider(error);
        let safe = format_error_with_source_chain(&error);
        let report = ValidateReport::failure(error);
        let output = json(&report);
        assert_eq!(output["errors"][0]["message"], safe);
        assert!(!output.to_string().contains("provider-private-payload"));
        assert!(
            !output["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains('\u{1b}')
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report
            .render_human(&mut stdout, &mut stderr, ColorMode::Plain)
            .unwrap();
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            safe.lines()
                .map(|line| format!("Error: {line}\n"))
                .collect::<String>()
        );
    }

    #[test]
    fn output_failure_returns_without_a_second_renderer() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let report = ValidateReport::failure(AppError::Config("opaque".into()));
        assert!(report.render_json(&mut BrokenWriter).is_err());
    }

    #[test]
    fn interruption_retains_exit_130() {
        assert_eq!(
            ValidateReport::failure(AppError::Interrupted).exit_code(),
            130
        );
    }
}

#[cfg(test)]
mod provider_configuration_tests;
