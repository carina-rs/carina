//! Reference obligations left after the retained judge reports a loop selector.

use super::*;
use crate::parser::retained::{AuthoredCursor, AuthoredUse, CursorKind, ExprKind, MissingEvidence};
use crate::resource::{ReferencePath, ReferencePathRef};
use std::collections::HashSet;

pub(super) struct ReportedSelector<'s, B: BindingAddress> {
    root: AuthoredUse<'s, B>,
    references: HashSet<ReferencePath<B>>,
}

impl<'s, B: BindingAddress> ReportedSelector<'s, B> {
    pub(super) fn from_cursor(
        cursor: &AuthoredCursor<'s, B>,
    ) -> Result<Option<Self>, MissingEvidence> {
        let root = match &cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage)
                if usage.node().judgement().reported_loop_selector_failure() =>
            {
                usage.clone()
            }
            _ => return Ok(None),
        };
        let mut references = HashSet::new();
        let mut pending = vec![root.node().clone()];
        while let Some(node) = pending.pop() {
            if !node.judgement().loop_selector_owns_failure() {
                continue;
            }
            if let ExprKind::Reference {
                binding,
                attribute,
                segments,
                ..
            } = node.kind()
            {
                // Coalesce can retain only this failed reference's bare head.
                references.insert(ReferencePath::Binding(binding.clone()));
                references.insert(ReferencePath::Access(
                    crate::resource::AccessPath::from_binding(
                        binding.clone(),
                        attribute.clone(),
                        segments.clone(),
                    ),
                ));
            } else {
                pending.extend(node.children().collect::<Result<Vec<_>, _>>()?);
            }
        }
        Ok(Some(Self { root, references }))
    }

    pub(super) fn record(&self, coverage: &mut WalkCoverage) {
        coverage.record(&self.root);
    }

    pub(super) fn errors<P: PlaceholderKind>(
        &self,
        value: &Value<P, B>,
        id: &ResourceId,
        name: &str,
        bindings: &BindingIndex<'_, B>,
    ) -> Vec<TypeError> {
        let interpolation_sink =
            crate::validation::lift_type_expr(&crate::validation::INTERPOLATION_REF_SINK)
                .expect("built-in interpolation sink");
        crate::validation::collect_ref_diagnostics_with_sink(
            value,
            None,
            &mut |reference: ReferencePathRef<'_, B>, sink, usage, _, errors| {
                if self.references.contains(&reference.to_owned()) {
                    return;
                }
                let location = crate::validation::ReferenceFindingLocation::Resource {
                    location: id.to_string(),
                    attribute: name.into(),
                };
                let context = crate::validation::ResourceReferenceContext {
                    location,
                    bindings,
                    declared_source: None,
                    interpolation_sink: &interpolation_sink,
                };
                if let Some(finding) =
                    crate::validation::check_resource_reference(&context, reference, sink, usage)
                {
                    if !matches!(sink, Some(crate::validation::RefSink::Interpolation)) {
                        let reference = ReferenceCandidate {
                            reference: reference.to_owned(),
                        };
                        if let Err(error) = reference.source_in(bindings) {
                            errors.push(error);
                            return;
                        }
                    }
                    errors.push(assignment::reference_error(
                        AdmissionReferenceError::Authored(Box::new(finding)),
                    ));
                }
            },
        )
    }
}

/// Root clients can stop on the expression finding before ordinary resource
/// admission. Discharge only the independent references of those failed slots.
pub fn sibling_findings<E, D, B: BindingAddress>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
) -> Result<Vec<crate::validation::ResourceAdmissionFinding>, MissingEvidence>
where
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file())
        .map_err(|error| MissingEvidence::new(error.to_string()))?;
    let mut findings = Vec::new();
    for resource in tree.resources() {
        for (name, value) in resource.attributes().iter() {
            let location =
                resource.attribute_value_location(name, &crate::schema::FieldPath::new());
            if !judged
                .report()
                .expression(location)
                .is_ok_and(|expression| expression.reported_loop_selector_failure())
            {
                continue;
            }
            let slot = resource.authored_slot(name)?;
            let cursor = AuthoredCursor::authored(slot.cursor()?);
            let Some(selector) = ReportedSelector::from_cursor(&cursor)? else {
                continue;
            };
            for error in selector.errors(value, resource.id(), name, bindings) {
                let diagnostic = crate::schema::SchemaDiagnostic::unlocated(error);
                let mut finding = crate::validation::ResourceAdmissionFinding::schema(
                    resource.id(),
                    &diagnostic,
                    Some(location.clone()),
                );
                finding.human = finding.message();
                findings.push(finding);
            }
        }
    }
    Ok(findings)
}
