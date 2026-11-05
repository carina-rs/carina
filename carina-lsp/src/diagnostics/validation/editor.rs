//! Editor checks have a closed result type. Concrete schema errors can only
//! enter diagnostics through the resource-value report.

use super::super::Value;
use carina_core::parser::{AnalysisResourceRef, Located};
use carina_core::resource::DeferredValue;
use carina_core::schema::ValidatedResourceSchema;
use carina_core::validation::{
    AccessTypeFinding, IntegerArithmeticTypeFinding, UnresolvedExpression,
};
use std::fmt;
use tower_lsp::lsp_types::DiagnosticSeverity;

pub(crate) enum EditorFinding {
    MixedAttributeSyntax { authored: String, canonical: String },
    PreferBlockSyntax { attribute: String },
    DeferredAccess(AccessTypeFinding),
    IntegerArithmetic(IntegerArithmeticTypeFinding),
}

impl EditorFinding {
    pub(in crate::diagnostics) fn severity(&self) -> DiagnosticSeverity {
        match self {
            Self::PreferBlockSyntax { .. } => DiagnosticSeverity::HINT,
            Self::DeferredAccess(error) => {
                if error.is_type_mismatch() {
                    DiagnosticSeverity::WARNING
                } else {
                    DiagnosticSeverity::ERROR
                }
            }
            Self::MixedAttributeSyntax { .. } | Self::IntegerArithmetic(_) => {
                DiagnosticSeverity::ERROR
            }
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
    judged: &carina_core::parser::retained::JudgedExpr,
    mut access_check: impl FnMut(
        UnresolvedExpression<'_, carina_core::resource::RelativeBindingName>,
        bool,
        &carina_core::parser::retained::JudgedExpr,
    ) -> Result<(), AccessTypeFinding>,
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
    if !judged.editor_owns_check(value) {
        return findings;
    }
    if let Some(expression) = UnresolvedExpression::new(value) {
        deferred_findings(
            resource,
            name,
            expression,
            true,
            judged,
            &mut access_check,
            &mut findings,
        );
    }
    findings
}

#[allow(clippy::too_many_arguments)]
fn deferred_findings(
    resource: &AnalysisResourceRef<'_>,
    name: &str,
    expression: UnresolvedExpression<'_, carina_core::resource::RelativeBindingName>,
    top_level: bool,
    judged: &carina_core::parser::retained::JudgedExpr,
    check: &mut impl FnMut(
        UnresolvedExpression<'_, carina_core::resource::RelativeBindingName>,
        bool,
        &carina_core::parser::retained::JudgedExpr,
    ) -> Result<(), AccessTypeFinding>,
    findings: &mut Vec<Located<EditorFinding>>,
) {
    match expression.value() {
        // Concrete operands belong to the core value walk. Only deferred
        // operands need editor inference, so a bad concrete default cannot
        // be re-emitted here as a second type mismatch.
        Value::Deferred(DeferredValue::Coalesce { .. } | DeferredValue::ModuleIf(_)) => {
            let before = findings.len();
            for (index, operand) in expression.operands() {
                match judged
                    .coalesce_operand(index)
                    .or_else(|_| judged.branch(index))
                {
                    Ok(evidence) => {
                        deferred_findings(resource, name, operand, false, evidence, check, findings)
                    }
                    Err(error) => findings.push(Located::new(
                        resource.attribute_key_location(name).clone(),
                        EditorFinding::DeferredAccess(AccessTypeFinding::missing_evidence(error)),
                    )),
                }
            }
            if matches!(
                expression.value(),
                Value::Deferred(DeferredValue::Coalesce { .. })
            ) && findings.len() == before
                && let Err(error) = check(expression, top_level, judged)
            {
                findings.push(Located::new(
                    resource.attribute_key_location(name).clone(),
                    EditorFinding::DeferredAccess(error),
                ));
            }
        }
        Value::Deferred(_) => {
            if let Err(error) = check(expression, top_level, judged) {
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
