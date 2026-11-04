//! Render the sole resource-value report using parser-owned locations.

mod editor;

pub(super) use editor::attribute_findings;

use super::{DiagnosticEngine, Value, render_merged};
use crate::document::Document;
use carina_core::parser::{AnalysisResourceRef, DeclarationLocation, Located};
use carina_core::resource::ConcreteValue;
use carina_core::schema::{
    ResourceValueFinding, ResourceValueTarget, TypeError, ValidatedResourceSchema,
    ValueFindingAnchor,
};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

/// Expanded instances share their parser-authored locations. Keep the report
/// keyed by those locations, rather than by an expanded resource's identity.
/// A location can still carry distinct errors or paths (computed collections
/// can anchor multiple invalid leaves to the same authored parent).
#[derive(Default)]
pub(super) struct ResourceValueReport {
    findings: indexmap::IndexMap<DeclarationLocation, Vec<AuthoredValueFinding>>,
}

struct AuthoredValueFinding {
    core: ResourceValueFinding,
    guidance: Option<String>,
    replaceable: bool,
}

impl ResourceValueReport {
    pub(super) fn collect_located(
        &mut self,
        findings: impl IntoIterator<Item = Located<ResourceValueFinding>>,
    ) {
        for finding in findings {
            let replaceable = matches!(&finding.detail().target,
                ResourceValueTarget::Attribute { anchor: ValueFindingAnchor::BindingToken(token), .. }
                if token.replaceable_location() == Some(finding.location()));
            self.insert(
                finding.location().clone(),
                finding.detail().clone(),
                None,
                replaceable,
            );
        }
    }

    pub(super) fn collect(
        &mut self,
        resource: &AnalysisResourceRef<'_>,
        findings: impl IntoIterator<Item = ResourceValueFinding>,
        values: &std::collections::HashMap<String, Value>,
    ) {
        let mut authored = Self::default();
        for finding in findings {
            authored.collect_finding(resource, finding, values);
        }
        for (location, findings) in authored.findings {
            let reported = self.findings.entry(location).or_default();
            let mut matched = vec![false; reported.len()];
            // Match each occurrence at most once across complete walks. Two
            // coalesce operands can produce equal errors at one fallback
            // anchor, and both findings from that walk must survive.
            for finding in findings {
                if let Some(index) =
                    reported
                        .iter()
                        .zip(&matched)
                        .position(|(existing, matched)| {
                            !matched && existing.core.same_authored_rule(&finding.core)
                        })
                {
                    matched[index] = true;
                } else {
                    reported.push(finding);
                    matched.push(true);
                }
            }
        }
    }

    fn collect_finding(
        &mut self,
        resource: &AnalysisResourceRef<'_>,
        finding: ResourceValueFinding,
        values: &std::collections::HashMap<String, Value>,
    ) {
        if let ResourceValueTarget::Attribute {
            authored_attribute,
            path,
            ..
        } = &finding.target
            && path.is_empty()
            && matches!(finding.error, TypeError::BlockSyntaxNotAllowed { .. })
        {
            let blocks: Vec<_> = resource.attribute_block_keys(authored_attribute).collect();
            if !blocks.is_empty() {
                for location in blocks {
                    self.insert(location.clone(), finding.clone(), None, false);
                }
                return;
            }
        }
        let (location, guidance, replaceable) = match &finding.target {
            ResourceValueTarget::Resource => (resource.type_location(), None, false),
            ResourceValueTarget::Attribute {
                authored_attribute: name,
                path,
                anchor,
            } => {
                let location = match anchor {
                    ValueFindingAnchor::Key => path.key_location(resource, name),
                    ValueFindingAnchor::Value => path.value_location(resource, name),
                    ValueFindingAnchor::BindingToken(token) => token.location(),
                    ValueFindingAnchor::Assignment => {
                        let assignment = resource.attribute_assignment_location(name);
                        let span = assignment.span();
                        if span.start_line() == span.end_line() {
                            assignment
                        } else {
                            resource.attribute_key_location(name)
                        }
                    }
                };
                let guidance = if path.is_empty() {
                    scalar_guidance(&finding.error, values.get(name))
                } else {
                    None
                };
                let replaceable = match anchor {
                    ValueFindingAnchor::BindingToken(token) => {
                        token.replaceable_location() == Some(location)
                    }
                    _ => {
                        path.is_empty()
                            && resource.replaceable_attribute_value(name) == Some(location)
                    }
                };
                (location, guidance, replaceable)
            }
        };
        self.insert(location.clone(), finding, guidance, replaceable);
    }

    fn insert(
        &mut self,
        location: DeclarationLocation,
        core: ResourceValueFinding,
        guidance: Option<String>,
        replaceable: bool,
    ) {
        self.findings
            .entry(location)
            .or_default()
            .push(AuthoredValueFinding {
                core,
                guidance,
                replaceable,
            });
    }

    pub(super) fn render(self, doc: &Document) -> Vec<Diagnostic> {
        self.findings
            .into_iter()
            .flat_map(|(location, findings)| {
                findings
                    .into_iter()
                    .filter_map(move |finding| render_value_finding(doc, location.clone(), finding))
            })
            .collect()
    }
}

fn render_value_finding(
    doc: &Document,
    location: DeclarationLocation,
    finding: AuthoredValueFinding,
) -> Option<Diagnostic> {
    let AuthoredValueFinding {
        core: ResourceValueFinding { target, mut error },
        guidance,
        replaceable,
    } = finding;
    let message = match target {
        ResourceValueTarget::Resource => error.to_string(),
        ResourceValueTarget::Attribute {
            authored_attribute: name,
            path,
            anchor,
        } => {
            let binding_token = matches!(anchor, ValueFindingAnchor::BindingToken(_));
            let indexed = path.is_indexed();
            let duplicate = matches!(error, TypeError::DuplicateListElement { .. });
            let attribute =
                if indexed && !duplicate && !matches!(error, TypeError::FieldGroupViolation { .. })
                {
                    format!("{name}{path}")
                } else {
                    name.clone()
                };
            if !binding_token
                && (indexed
                    || path.is_empty()
                    || matches!(error, TypeError::FieldGroupViolation { .. }))
            {
                error = error.with_attribute(&attribute);
            }
            if binding_token {
                error.to_string()
            } else if indexed && error.attribute_name().is_none() {
                format!("{error} for '{attribute}'")
            } else if path.is_empty() {
                guidance.unwrap_or_else(|| error.to_string())
            } else {
                error.to_string()
            }
        }
    };
    let mut diagnostic = render_merged(
        doc,
        &Located::new(location, &error),
        DiagnosticSeverity::ERROR,
        message,
    )?;
    if replaceable && let Some(data) = crate::code_action::enum_diagnostic_data_for_error(&error) {
        diagnostic.data = Some(serde_json::to_value(data).expect("EnumDiagnosticData serialize"));
    }
    Some(diagnostic)
}

fn scalar_guidance(error: &TypeError, value: Option<&Value>) -> Option<String> {
    let TypeError::TypeMismatch { types } = error else {
        return None;
    };
    let value @ Value::Concrete(ConcreteValue::String(_)) = value? else {
        return None;
    };
    match types.expected() {
        "Bool" => Some(format!(
            "Type mismatch: expected Bool, got {}. Use true or false.",
            carina_core::ValueDescription::of(value)
        )),
        "Int" | "Float" => Some(format!(
            "Type mismatch: expected {}, got {}.",
            types.expected(),
            carina_core::ValueDescription::of(value)
        )),
        _ => None,
    }
}

impl DiagnosticEngine {
    pub(super) fn check_list_struct_syntax(
        &self,
        doc: &Document,
        resource: &AnalysisResourceRef<'_>,
        schema: &ValidatedResourceSchema,
    ) -> Vec<Diagnostic> {
        editor::syntax_findings(resource, schema)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    finding.detail().severity(),
                    finding.detail().to_string(),
                )
            })
            .collect()
    }
}
