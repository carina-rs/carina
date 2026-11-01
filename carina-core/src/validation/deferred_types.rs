//! Settlement of provider-dependent checks at a terminal providerless boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::typecheck::{
    TypeExprValueCheck, TypecheckInference, check_type_expr_value_providerless,
};
use crate::parser::{
    DeferredForPhaseTraversal, ExportParamLike, File, ParseWarning, ParseWarningSpan,
    ProviderContext, SourceSpan, TypeExpr, UserFunction, WarningKind,
};
use crate::resource::{
    AuthoredConcreteValue as ConcreteValue, AuthoredDeferredValue as DeferredValue,
    AuthoredInterpolationPart as InterpolationPart, AuthoredValue as Value, FunctionArgument,
};

/// Retains the existing typed missing-provider reason until it is reported.
struct DeferredProviderTypeCheck {
    expected: TypeExpr,
    reason: crate::parser::CustomTypeValidationError,
    context: String,
    source: PathBuf,
    span: ParseWarningSpan,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DeferredTypeCheckSite {
    file: PathBuf,
    line: usize,
    column: usize,
    context: String,
    expected: String,
}

/// A terminal consumer must publish unresolved checks before returning.
/// Intermediate bootstrap parsing and value checking never publish warnings.
#[must_use = "provider type checks must be settled by publishing their diagnostics"]
pub struct DeferredProviderTypeChecks {
    pending: BTreeMap<DeferredTypeCheckSite, DeferredProviderTypeCheck>,
    authored_calls: BTreeSet<(usize, String)>,
}

impl DeferredProviderTypeChecks {
    pub fn collect<E: ExportParamLike, D: DeferredForPhaseTraversal>(
        parsed: &File<E, crate::resource::ParserPlaceholder, crate::resource::ParserPlaceholder, D>,
        config: &ProviderContext,
        fallback_source: &Path,
    ) -> Self {
        let mut checks = Self {
            pending: BTreeMap::new(),
            authored_calls: BTreeSet::new(),
        };
        // An authoritative catalog is settled by the normal strict pipeline,
        // including its unknown-type diagnostics. This seam only closes the
        // providerless branch, without relabeling an unknown type as unloaded.
        if config.custom_types_loaded() {
            return checks;
        }
        for pending in parsed
            .parser_binding_metadata
            .pending_function_type_checks()
        {
            if let Some(function) = parsed.user_functions.get(&pending.function_name) {
                let source = pending.source_file.as_deref().unwrap_or(fallback_source);
                checks
                    .authored_calls
                    .insert((pending.source_line, function.name.clone()));
                checks.function_call(
                    function,
                    &pending.arguments,
                    source,
                    (pending.source_line, pending.source_column.unwrap_or(1)),
                    config,
                );
            }
        }
        for argument in &parsed.arguments {
            let (source, span) = parsed
                .parser_binding_metadata
                .argument_type_source(&argument.name)
                .map(|(source, span)| (source.path(), warning_span(span)))
                .unwrap_or((fallback_source, line_span(1)));
            checks.declaration(
                &argument.static_type,
                argument.default.as_ref(),
                source,
                &span,
                &format!("argument '{}'", argument.name),
                config,
            );
            if let Some(value) = &argument.default {
                checks.value(value, source, config);
            }
        }
        for attribute in &parsed.attribute_params {
            if let Some(expected) = &attribute.static_type {
                checks.declaration(
                    expected,
                    attribute.value.as_ref(),
                    attribute.source_file(),
                    &warning_span(attribute.source_span()),
                    &format!("attribute '{}'", attribute.name),
                    config,
                );
            }
            if let Some(value) = &attribute.value {
                checks.value(value, attribute.source_file(), config);
            }
        }
        for export in &parsed.export_params {
            let (source, span) = export
                .source_location()
                .or_else(|| {
                    parsed
                        .parser_binding_metadata
                        .export_type_source(export.name())
                        .map(|(file, span)| (file.path(), span))
                })
                .map(|(source, span)| (source, warning_span(span)))
                .unwrap_or((fallback_source, line_span(1)));
            if let Some(expected) = export.type_expr_opt() {
                checks.declaration(
                    expected,
                    export.value(),
                    source,
                    &span,
                    &format!("export '{}'", export.name()),
                    config,
                );
            }
            if let Some(value) = export.value() {
                checks.value(value, source, config);
            }
        }
        for value in parsed.variables.values() {
            checks.value(value, fallback_source, config);
        }
        for resource in parsed.iter_all_resources() {
            let source = parsed
                .parser_binding_metadata
                .resource_source_file(resource.id())
                .unwrap_or(fallback_source);
            for value in resource.attributes().values() {
                checks.value(value, source, config);
            }
        }
        for call in &parsed.module_calls {
            for argument in call.arguments_with_source_spans() {
                checks.value(
                    argument.argument().walker_value(),
                    call.source_file(),
                    config,
                );
            }
        }
        for provider in &parsed.providers {
            for value in provider
                .attributes
                .values()
                .chain(provider.default_tags.values())
                .chain(provider.unresolved_attributes.values())
            {
                checks.value(value, fallback_source, config);
            }
        }
        if let Some(backend) = &parsed.backend {
            for value in backend.attributes.values() {
                checks.value(value, fallback_source, config);
            }
        }
        for function in parsed.user_functions.values() {
            let source = function
                .body
                .source_file
                .as_deref()
                .unwrap_or(fallback_source);
            checks.value(&function.body.value, source, config);
            for (_, value, _) in &function.local_lets {
                checks.value(value, source, config);
            }
            for parameter in &function.params {
                if let Some(value) = &parameter.default {
                    checks.value(value, source, config);
                }
            }
        }
        checks
    }

    fn declaration(
        &mut self,
        expected: &TypeExpr,
        value: Option<&Value>,
        source: &Path,
        span: &ParseWarningSpan,
        context: &str,
        config: &ProviderContext,
    ) {
        // A proven mismatch has its ordinary diagnostic; it is not deferred.
        if value.is_some_and(|value| {
            matches!(
                check_type_expr_value_providerless(
                    expected,
                    value,
                    config,
                    TypecheckInference::Unavailable
                ),
                TypeExprValueCheck::Mismatch(_)
            )
        }) {
            return;
        }
        match expected {
            TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::DottedUnresolved(_) => {
                let normalized_source = crate::utils::normalize_source_path(source);
                let key = DeferredTypeCheckSite {
                    file: normalized_source.clone(),
                    line: span.start_line,
                    column: span.start_column,
                    context: context.to_string(),
                    expected: expected.to_string(),
                };
                self.pending
                    .entry(key)
                    .or_insert_with(|| DeferredProviderTypeCheck {
                        expected: expected.clone(),
                        reason: crate::parser::CustomTypeValidationError::ProvidersNotLoaded,
                        context: context.to_string(),
                        source: normalized_source,
                        span: span.clone(),
                    });
            }
            TypeExpr::List(inner) => match value {
                Some(Value::Concrete(ConcreteValue::List(items))) => {
                    for (index, item) in items.iter().enumerate() {
                        self.declaration(
                            inner,
                            Some(item),
                            source,
                            span,
                            &format!("{context}, list item {index}"),
                            config,
                        );
                    }
                }
                _ => self.declaration(inner, None, source, span, context, config),
            },
            TypeExpr::Map(inner) => match value {
                Some(Value::Concrete(ConcreteValue::Map(items))) => {
                    for (key, item) in items {
                        self.declaration(
                            inner,
                            Some(item),
                            source,
                            span,
                            &format!("{context}, map key '{key}'"),
                            config,
                        );
                    }
                }
                _ => self.declaration(inner, None, source, span, context, config),
            },
            TypeExpr::Struct { fields } => {
                for (name, field) in fields {
                    let item = match value {
                        Some(Value::Concrete(ConcreteValue::Map(items))) => items.get(name),
                        _ => None,
                    };
                    self.declaration(
                        field,
                        item,
                        source,
                        span,
                        &format!("{context}, field '{name}'"),
                        config,
                    );
                }
            }
            TypeExpr::Union(members) => {
                if value.is_some_and(|value| {
                    matches!(
                        check_type_expr_value_providerless(
                            expected,
                            value,
                            config,
                            TypecheckInference::Unavailable
                        ),
                        TypeExprValueCheck::Assignable
                    )
                }) {
                    return;
                }
                for member in members {
                    self.declaration(member, value, source, span, context, config);
                }
            }
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::StringLiteral(_) => {}
        }
    }

    fn function_call(
        &mut self,
        function: &UserFunction,
        arguments: &[FunctionArgument<
            crate::resource::ParserPlaceholder,
            crate::resource::RelativeBindingName,
        >],
        source: &Path,
        position: (usize, usize),
        config: &ProviderContext,
    ) {
        let mut span = line_span(position.0);
        span.start_column = position.1;
        span.end_column = position.1 + 1;
        for (parameter, argument) in function.params.iter().zip(arguments) {
            if let Some(expected) = &parameter.param_type {
                self.declaration(
                    expected,
                    argument.value(),
                    source,
                    &span,
                    &format!(
                        "function '{}' parameter '{}'",
                        function.name, parameter.name
                    ),
                    config,
                );
            }
        }
        if let Some(expected) = &function.return_type {
            self.declaration(
                expected,
                None,
                source,
                &span,
                &format!("function '{}' return value", function.name),
                config,
            );
        }
    }

    fn value(&mut self, value: &Value, source: &Path, config: &ProviderContext) {
        match value {
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                args,
                source_line,
                ..
            }) => {
                let line = source_line.get().unwrap_or(function.body.source_line);
                if !self.authored_calls.contains(&(line, function.name.clone())) {
                    self.function_call(function, args, source, (line, 1), config);
                }
                for argument in args {
                    self.value(argument.walker_value(), source, config);
                }
            }
            Value::Deferred(
                DeferredValue::FunctionCall { args, .. } | DeferredValue::Unavailable { args, .. },
            ) => {
                for argument in args {
                    self.value(argument.walker_value(), source, config);
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => self.value(inner, source, config),
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                self.value(value, source, config);
                self.value(default, source, config);
            }
            Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
                for value in deferred.values() {
                    self.value(value, source, config);
                }
            }
            Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
                self.value(expression.lhs(), source, config);
                self.value(expression.rhs(), source, config);
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    if let InterpolationPart::Expr(value) = part {
                        self.value(value, source, config);
                    }
                }
            }
            Value::Deferred(DeferredValue::Access(access)) => {
                self.value(access.base(), source, config)
            }
            Value::Concrete(ConcreteValue::List(items)) => {
                for value in items {
                    self.value(value, source, config);
                }
            }
            Value::Concrete(ConcreteValue::Map(items)) => {
                for value in items.values() {
                    self.value(value, source, config);
                }
            }
            Value::Concrete(_)
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::RedactedSecret { .. }
                | DeferredValue::Unknown(_)
                | DeferredValue::Placeholder(_),
            ) => {}
        }
    }

    /// The sole conversion from unresolved provider evidence to a source-owned
    /// warning. The existing ProvidersNotLoaded reason is retained until here.
    pub fn report(self) -> Vec<ParseWarning> {
        self.pending.into_values().map(|check| {
            let crate::parser::CustomTypeValidationError::ProvidersNotLoaded = check.reason else {
                unreachable!("providerless settlement only retains unavailable schemas")
            };
            ParseWarning {
                kind: WarningKind::ProviderSchemasNotLoaded,
                file: Some(check.source.display().to_string()), line: check.span.start_line,
                span: Some(check.span),
                message: format!("{}: type check for {} was deferred because provider schemas were not loaded", check.context, check.expected),
            }
        }).collect()
    }
}

fn line_span(line: usize) -> ParseWarningSpan {
    ParseWarningSpan {
        start_line: line.max(1),
        start_column: 1,
        end_line: line.max(1),
        end_column: 2,
    }
}

fn warning_span(span: SourceSpan) -> ParseWarningSpan {
    ParseWarningSpan {
        start_line: span.start_line(),
        start_column: span.start_column(),
        end_line: span.end_line(),
        end_column: span.end_column(),
    }
}

/// Finish a providerless file while preserving ordinary intermediate checks.
/// Repeated graph walks deduplicate by authored site; an enriched re-run
/// removes provisional warnings before strict validation takes ownership.
pub fn finalize_provider_type_checks<E: ExportParamLike, D: DeferredForPhaseTraversal>(
    parsed: &mut File<E, crate::resource::ParserPlaceholder, crate::resource::ParserPlaceholder, D>,
    config: &ProviderContext,
    fallback_source: &Path,
) {
    let warnings = DeferredProviderTypeChecks::collect(parsed, config, fallback_source).report();
    parsed
        .warnings
        .retain(|warning| warning.kind != WarningKind::ProviderSchemasNotLoaded);
    parsed.warnings.extend(warnings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{WarningKind, parse_file};

    #[test]
    fn providerless_module_failure_still_reports_deferred_type_sites() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "let child = use { source = './missing' }\nchild {}\nattributes { id: AwsAccountId = '123' }\n").unwrap();
        let schemas = crate::schema::SchemaRegistry::new();
        let loaded = crate::config_loader::load_configuration_with_config(
            fixture.path(),
            ProviderContext::unloaded(),
            &schemas,
        )
        .unwrap();
        let error = crate::module_resolver::resolve_module_template_with_providerless_config(
            loaded.parsed,
            fixture.path(),
            ProviderContext::unloaded(),
            &schemas,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("AwsAccountId")
                && error
                    .to_string()
                    .contains("provider schemas were not loaded"),
            "{error}"
        );
        let warnings = error.provider_type_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, WarningKind::ProviderSchemasNotLoaded);
    }

    #[test]
    fn providerless_module_graph_reports_uncalled_and_repeated_import_sites_once() {
        for calls in ["", "let a = child {}\nlet b = child {}\n"] {
            let fixture = tempfile::tempdir().unwrap();
            let child = fixture.path().join("child");
            std::fs::create_dir(&child).unwrap();
            let source = child.join("main.crn");
            std::fs::write(&source, "attributes { id: AwsAccountId = '123' }\n").unwrap();
            std::fs::write(
                fixture.path().join("main.crn"),
                format!("let child = use {{ source = './child' }}\n{calls}"),
            )
            .unwrap();
            let schemas = crate::schema::SchemaRegistry::new();
            let loaded = crate::config_loader::load_configuration_with_config(
                fixture.path(),
                ProviderContext::unloaded(),
                &schemas,
            )
            .unwrap();
            let resolved =
                crate::module_resolver::resolve_module_template_with_providerless_config(
                    loaded.parsed,
                    fixture.path(),
                    ProviderContext::unloaded(),
                    &schemas,
                )
                .unwrap();
            let warnings = resolved
                .warnings
                .iter()
                .filter(|warning| warning.kind == WarningKind::ProviderSchemasNotLoaded)
                .collect::<Vec<_>>();
            assert_eq!(
                warnings.len(),
                1,
                "one warning per authored site: {warnings:#?}"
            );
            assert_eq!(warnings[0].file.as_deref(), source.to_str());
        }
    }

    #[test]
    fn settlement_preserves_calling_file_without_repeating_substituted_values() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("functions.crn"), "fn f(x: Arn) { x }\n").unwrap();
        let path = fixture.path().join("calls.crn");
        std::fs::write(
            &path,
            "let a = f('arn:aws:s3:::bucket')\nattributes { x: AwsAccountId = '123' }\n",
        )
        .unwrap();
        let loaded = crate::config_loader::load_configuration_with_config(
            fixture.path(),
            ProviderContext::unloaded(),
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let warnings = DeferredProviderTypeChecks::collect(
            loaded.parsed.parsed(),
            ProviderContext::unloaded(),
            &fixture.path().join("main.crn"),
        )
        .report();
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(
            warnings
                .iter()
                .all(|warning| warning.file.as_deref() == path.to_str()),
            "{warnings:#?}"
        );
    }

    #[test]
    fn settlement_reports_unchecked_assignment_source_declaration() {
        let path = Path::new("/project/main.crn");
        let parsed = parse_file(
            "arguments { id: AwsAccountId }\nattributes { value: String = id }\n",
            path,
            ProviderContext::unloaded(),
        )
        .unwrap();
        let schemas = crate::schema::SchemaRegistry::new();
        let index = crate::binding_index::BindingIndex::from_parsed(
            &parsed,
            &schemas,
            ProviderContext::unloaded(),
        );
        let crate::binding_index::RefType::Typed(source) =
            index.ref_type(&crate::resource::RelativeBindingName::new("id"))
        else {
            panic!("typed argument")
        };
        assert!(
            source
                .is_assignable_to_sink(
                    crate::schema::TypeInSchema::schemaless(
                        &crate::schema::CompiledAttributeType::string()
                    ),
                    &index
                )
                .is_ok()
        );
        let warnings =
            DeferredProviderTypeChecks::collect(&parsed, ProviderContext::unloaded(), path)
                .report();
        assert_eq!(
            warnings.len(),
            1,
            "unchecked assignment must still be settled: {warnings:#?}"
        );
        assert!(warnings[0].message.contains("AwsAccountId"));
        assert_eq!(warnings[0].line, 1);
    }

    #[test]
    fn settlement_retains_each_nested_leaf_and_clears_provisional_warnings_when_loaded() {
        let path = Path::new("/project/outputs.crn");
        let mut parsed = parse_file(
            "exports {\n  ids: list(AwsAccountId) = ['123', '456']\n}\n",
            path,
            ProviderContext::unloaded(),
        )
        .unwrap();
        finalize_provider_type_checks(&mut parsed, ProviderContext::unloaded(), path);
        assert_eq!(parsed.warnings.len(), 2);
        assert!(parsed.warnings.iter().all(|warning| warning.file.as_deref()
            == Some("/project/outputs.crn")
            && warning.line == 2));
        let loaded = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                Default::default(),
                Box::new(|_, _| Ok(())),
            ),
            ..Default::default()
        };
        finalize_provider_type_checks(&mut parsed, &loaded, path);
        assert!(
            parsed.warnings.is_empty(),
            "loaded checking owns regular unknown-type errors"
        );
        assert!(!crate::validation::resolve_file_type_exprs(&mut parsed, &loaded).is_empty());
    }

    #[test]
    fn settlement_keeps_distinct_function_calls_on_the_same_line() {
        let path = Path::new("/project/main.crn");
        let parsed = parse_file(
            "fn f(x: Arn) { x }\nlet pair = [f('one'), f('two')]\n",
            path,
            ProviderContext::unloaded(),
        )
        .unwrap();
        let warnings =
            DeferredProviderTypeChecks::collect(&parsed, ProviderContext::unloaded(), path)
                .report();
        assert_eq!(
            warnings.len(),
            2,
            "each call must be settled: {warnings:#?}"
        );
        assert_ne!(
            warnings[0].span.as_ref().unwrap().start_column,
            warnings[1].span.as_ref().unwrap().start_column
        );
    }

    #[test]
    fn providerless_module_resolution_returns_settled_type_warnings() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "fn f(x: Arn) { x }\nlet a = f('arn:aws:s3:::bucket')\nattributes { x: AwsAccountId = '123' }\n").unwrap();
        let schemas = crate::schema::SchemaRegistry::new();
        let loaded = crate::config_loader::load_configuration_with_config(
            fixture.path(),
            ProviderContext::unloaded(),
            &schemas,
        )
        .unwrap();
        let resolved = crate::module_resolver::resolve_module_template_with_providerless_config(
            loaded.parsed,
            fixture.path(),
            ProviderContext::unloaded(),
            &schemas,
        )
        .unwrap();
        let warnings = resolved
            .warnings
            .iter()
            .filter(|warning| warning.kind == WarningKind::ProviderSchemasNotLoaded)
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
    }

    #[test]
    fn settlement_reports_each_unloaded_function_and_attribute_site_once() {
        let path = Path::new("/project/main.crn");
        let parsed = parse_file(
            "fn f(x: Arn) { x }\nlet a = f('arn:aws:s3:::bucket')\nattributes {\n  x: AwsAccountId = '123'\n}\n",
            path, ProviderContext::unloaded(),
        ).unwrap();
        let warnings =
            DeferredProviderTypeChecks::collect(&parsed, ProviderContext::unloaded(), path)
                .report();
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        for (warning, name, line) in [(&warnings[0], "Arn", 2), (&warnings[1], "AwsAccountId", 4)] {
            assert_eq!(warning.kind, WarningKind::ProviderSchemasNotLoaded);
            assert_eq!(warning.file.as_deref(), Some("/project/main.crn"));
            assert_eq!(warning.line, line);
            assert!(warning.span.is_some());
            assert!(warning.message.contains(name), "{warning:?}");
            assert!(
                warning.message.contains("provider schemas were not loaded"),
                "{warning:?}"
            );
        }
    }
}
