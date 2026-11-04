//! Editor checks have a closed result type. Concrete schema errors can only
//! enter diagnostics through the resource-value report.

use super::super::Value;
use carina_core::parser::{AnalysisResourceRef, Located};
use carina_core::resource::{DeferredValue, ParserPlaceholder};
use carina_core::schema::ValidatedResourceSchema;
use carina_core::validation::{DeferredAccessTypeError, IntegerArithmeticTypeFinding};
use std::fmt;
use tower_lsp::lsp_types::DiagnosticSeverity;

pub(crate) enum EditorFinding {
    MixedAttributeSyntax { authored: String, canonical: String },
    PreferBlockSyntax { attribute: String },
    DeferredAccess(DeferredAccessTypeError),
    IntegerArithmetic(IntegerArithmeticTypeFinding),
}

impl EditorFinding {
    pub(in crate::diagnostics) fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::PreferBlockSyntax { .. } => DiagnosticSeverity::HINT,
            Self::DeferredAccess(DeferredAccessTypeError::TypeMismatch { .. }) => {
                DiagnosticSeverity::WARNING
            }
            Self::MixedAttributeSyntax { .. }
            | Self::DeferredAccess(DeferredAccessTypeError::InvalidAccess { .. })
            | Self::IntegerArithmetic(_) => DiagnosticSeverity::ERROR,
        }
    }
}

impl fmt::Display for EditorFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MixedAttributeSyntax {
                authored,
                canonical,
            } => write!(
                f,
                "Cannot use both '{authored}' and '{canonical}' (they refer to the same attribute)"
            ),
            Self::PreferBlockSyntax { attribute } => write!(
                f,
                "Prefer block syntax for '{attribute}'. Use `{attribute} {{ ... }}` instead of `{attribute} = [{{ ... }}]`."
            ),
            Self::DeferredAccess(error) => error.fmt(f),
            Self::IntegerArithmetic(error) => error.fmt(f),
        }
    }
}

pub(in crate::diagnostics) fn attribute_findings(
    resource: &AnalysisResourceRef<'_>,
    name: &str,
    value: &Value,
    arithmetic: Vec<Located<IntegerArithmeticTypeFinding>>,
    mut access_check: impl FnMut(&Value, bool) -> Result<(), DeferredAccessTypeError>,
) -> Vec<Located<EditorFinding>> {
    if !arithmetic.is_empty() {
        return arithmetic
            .into_iter()
            .map(|finding| {
                Located::new(
                    finding.location().clone(),
                    EditorFinding::IntegerArithmetic(finding.detail().clone()),
                )
            })
            .collect();
    }
    let mut findings = Vec::new();
    deferred_findings(
        resource,
        name,
        value,
        true,
        &mut access_check,
        &mut findings,
    );
    findings
}

fn deferred_findings(
    resource: &AnalysisResourceRef<'_>,
    name: &str,
    value: &Value,
    top_level: bool,
    check: &mut impl FnMut(&Value, bool) -> Result<(), DeferredAccessTypeError>,
    findings: &mut Vec<Located<EditorFinding>>,
) {
    match value {
        // Concrete operands belong to the core value walk. Only deferred
        // operands need editor inference, so a bad concrete default cannot
        // be re-emitted here as a second type mismatch.
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            deferred_findings(resource, name, value, false, check, findings);
            deferred_findings(resource, name, default, false, check, findings);
        }
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::Access(_)
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument { .. }),
        ) => {
            if let Err(error) = check(value, top_level) {
                findings.push(Located::new(
                    resource.attribute_key_location(name).clone(),
                    EditorFinding::DeferredAccess(error),
                ));
            }
        }
        _ => {}
    }
}

pub(super) fn syntax_findings(
    resource: &AnalysisResourceRef<'_>,
    schema: &ValidatedResourceSchema,
) -> Vec<Located<EditorFinding>> {
    use carina_core::schema::Shape;
    let attributes = resource.attributes();
    let mut findings: Vec<_> = schema
        .as_schema()
        .block_name_map()
        .into_iter()
        .filter(|(authored, canonical)| {
            authored != canonical
                && attributes.contains_key(authored)
                && attributes.contains_key(canonical)
        })
        .map(|(authored, canonical)| {
            Located::new(
                resource.attribute_key_location(&authored).clone(),
                EditorFinding::MixedAttributeSyntax {
                    authored,
                    canonical,
                },
            )
        })
        .collect();
    findings.extend(schema.effective_attributes().filter_map(|attribute| {
        let name = attribute.name();
        if attribute.is_read_only() || !resource.is_list_literal(name) { return None; }
        if !matches!(schema.as_schema().shape_of(attribute.field_type()), Shape::List { element_type, .. }
            if matches!(schema.as_schema().shape_of(element_type), Shape::Struct { .. })) { return None; }
        Some(Located::new(resource.attribute_key_location(name).clone(),
            EditorFinding::PreferBlockSyntax { attribute: name.into() }))
    }));
    findings
}
