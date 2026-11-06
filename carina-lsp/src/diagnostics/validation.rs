//! Render schema findings from the sealed resource's parser-owned element locations.

use super::{DiagnosticEngine, render_merged, render_schema_diagnostic};
use crate::document::Document;
use carina_core::parser::{AnalysisResourceRef, Located};
use carina_core::schema::{FieldPath, FieldPathStep, TypeError, ValidatedResourceSchema};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

/// Runtime equality ignores string provenance and equates some enum/string
/// spellings. Cached diagnostics require the same authored representation.
pub(super) fn same_validation_input(left: &super::Value, right: &super::Value) -> bool {
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};

    if std::ptr::eq(left, right) {
        return true;
    }
    match (left, right) {
        (
            Value::Concrete(ConcreteValue::String(left)),
            Value::Concrete(ConcreteValue::String(right)),
        ) => left == right && left.is_diagnostic_sensitive() == right.is_diagnostic_sensitive(),
        (
            Value::Concrete(ConcreteValue::List(left)),
            Value::Concrete(ConcreteValue::List(right)),
        ) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_validation_input(left, right))
        }
        (Value::Concrete(ConcreteValue::Map(left)), Value::Concrete(ConcreteValue::Map(right))) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((left_key, left), (right_key, right))| {
                        left_key == right_key && same_validation_input(left, right)
                    })
        }
        (
            Value::Deferred(DeferredValue::Secret(left)),
            Value::Deferred(DeferredValue::Secret(right)),
        ) => left.visit(|left| right.visit(|right| same_validation_input(left, right))),
        (
            Value::Deferred(DeferredValue::Coalesce {
                value: left,
                default: left_default,
            }),
            Value::Deferred(DeferredValue::Coalesce {
                value: right,
                default: right_default,
            }),
        ) => {
            same_validation_input(left, right) && same_validation_input(left_default, right_default)
        }
        (Value::Concrete(left_value), Value::Concrete(right_value)) => {
            std::mem::discriminant(left_value) == std::mem::discriminant(right_value)
                && left == right
        }
        _ => false,
    }
}

impl DiagnosticEngine {
    /// Attach each cached path-tagged error to the
    /// authored key, list element, or nearest authored computed parent.
    pub(super) fn render_container_findings(
        &self,
        doc: &Document,
        resource: &AnalysisResourceRef<'_>,
        attr_name: &str,
        findings: Vec<(FieldPath, TypeError)>,
    ) -> (Vec<Diagnostic>, bool) {
        let mut diagnostics = Vec::new();
        let mut reported_duplicate = false;
        for (path, error) in findings {
            let is_duplicate = super::is_duplicate_list_error(&error);
            let is_item_error = path
                .steps()
                .iter()
                .any(|step| matches!(step, FieldPathStep::Index(_)));
            let attribute = if is_item_error
                && !is_duplicate
                && !matches!(&error, TypeError::FieldGroupViolation { .. })
            {
                format!("{attr_name}{path}")
            } else {
                attr_name.to_string()
            };
            let error = if is_item_error
                || path.is_empty()
                || matches!(&error, TypeError::FieldGroupViolation { .. })
            {
                error.with_attribute(&attribute)
            } else {
                error
            };
            let message = if is_item_error && error.attribute_name().is_none() {
                format!("{error} for '{attribute}'")
            } else {
                error.to_string()
            };
            let use_value_location = path.is_empty()
                && (is_duplicate
                    || crate::code_action::enum_diagnostic_data_for_error(&error).is_some());
            let location = if is_duplicate || use_value_location {
                resource.attribute_value_location(attr_name, &path)
            } else {
                resource.attribute_path_location(attr_name, &path)
            };
            let finding = Located::new(location.clone(), error);
            let diagnostic = if use_value_location {
                render_schema_diagnostic(
                    doc,
                    &finding,
                    resource.replaceable_attribute_value(attr_name),
                )
            } else {
                render_merged(
                    doc,
                    &finding,
                    severity_for_validation_error(finding.detail()),
                    message,
                )
            };
            if let Some(diagnostic) = diagnostic {
                diagnostics.push(diagnostic);
                reported_duplicate |= is_duplicate;
            }
        }
        (diagnostics, reported_duplicate)
    }

    pub(super) fn check_list_struct_syntax(
        &self,
        doc: &Document,
        resource: &AnalysisResourceRef<'_>,
        schema: &ValidatedResourceSchema,
    ) -> Vec<Diagnostic> {
        use carina_core::schema::Shape;
        let mut diagnostics = Vec::new();
        for attribute in schema.effective_attributes() {
            let name = attribute.name();
            if attribute.is_read_only() || !resource.is_list_literal(name) {
                continue;
            }
            if !matches!(schema.as_schema().shape_of(attribute.field_type()), Shape::List { element_type, .. } if matches!(schema.as_schema().shape_of(element_type), Shape::Struct { .. }))
            {
                continue;
            }
            let finding = Located::new(
                resource.attribute_key_location(name).clone(),
                format!(
                    "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                    name, name, name,
                ),
            );
            diagnostics.extend(render_merged(
                doc,
                &finding,
                DiagnosticSeverity::HINT,
                finding.detail().clone(),
            ));
        }
        diagnostics
    }
}

fn severity_for_validation_error(error: &TypeError) -> DiagnosticSeverity {
    match error {
        TypeError::ReadOnlyAttribute { name: _ }
        | TypeError::FieldGroupViolation { path: _, group: _ }
        | TypeError::DuplicateListElement {
            value: _,
            index: _,
            first_index: _,
            attribute: _,
        } => DiagnosticSeverity::ERROR,
        TypeError::ListItemError { inner, .. }
        | TypeError::MapKeyError { inner, .. }
        | TypeError::MapValueError { inner, .. }
        | TypeError::StructFieldError { inner, .. } => severity_for_validation_error(inner),
        TypeError::UnionAlternativesMismatch { failures } => {
            if failures
                .iter()
                .all(|failure| severity_for_validation_error(failure) == DiagnosticSeverity::ERROR)
            {
                DiagnosticSeverity::ERROR
            } else {
                DiagnosticSeverity::WARNING
            }
        }
        _ => DiagnosticSeverity::WARNING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, NoPlaceholder, Value};
    use carina_core::schema::{
        AttributeType, RootlessType, TypeIdentity, ValidatedConfigSchema, validator,
    };
    use std::collections::BTreeMap;

    fn pathless_test_type(attr_type: AttributeType) -> RootlessType {
        ValidatedConfigSchema::validate(BTreeMap::from([("value".to_string(), attr_type)]))
            .expect("test config type must be valid")
            .rootless_type("value")
            .expect("test config type must exist")
    }

    fn tied_string_error(first: TypeError, second: TypeError) -> TypeError {
        let rejecting = |error: TypeError| {
            AttributeType::refined_string_with_validator(
                TypeIdentity::bare("TestValidator"),
                None,
                None,
                validator(move |_| Err(error.clone())),
                None,
            )
        };
        pathless_test_type(AttributeType::union(vec![
            rejecting(first),
            rejecting(second),
        ]))
        .validate(&Value::<NoPlaceholder>::Concrete(ConcreteValue::String(
            "invalid".into(),
        )))
        .expect_err("both Union arms must reject the value")
    }

    #[test]
    fn tied_union_is_warning_when_only_one_arm_is_an_error() {
        let error = tied_string_error(
            TypeError::ReadOnlyAttribute {
                name: "provider_value".to_string(),
            },
            TypeError::ValidationFailed {
                message: "not accepted".into(),
            },
        );

        assert_eq!(
            severity_for_validation_error(&error),
            DiagnosticSeverity::WARNING
        );
    }

    #[test]
    fn tied_union_is_error_when_every_arm_is_an_error() {
        let error = tied_string_error(
            TypeError::ReadOnlyAttribute {
                name: "first".to_string(),
            },
            TypeError::ReadOnlyAttribute {
                name: "second".to_string(),
            },
        );

        assert_eq!(
            severity_for_validation_error(&error),
            DiagnosticSeverity::ERROR
        );
    }
}
