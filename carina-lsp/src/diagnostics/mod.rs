mod checks;
mod validation;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use crate::position;
use carina_core::parser::ParseError;
use carina_core::provider::ProviderFactory;
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;

/// Create a `Diagnostic` on a single line with the standard "carina" source.
pub(crate) fn carina_diagnostic(
    line: u32,
    start_col: u32,
    end_col: u32,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: start_col,
            },
            end: Position {
                line,
                character: end_col,
            },
        },
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

/// Create a `Diagnostic` with an arbitrary `Range` and the standard "carina" source.
pub(crate) fn carina_diagnostic_range(
    range: Range,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

pub struct DiagnosticEngine {
    schemas: Arc<HashMap<String, ResourceSchema>>,
    provider_names: Vec<String>,
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    /// Providers that failed to load: name -> error reason.
    provider_errors: HashMap<String, String>,
    /// Cached provider context with custom type validators from schemas.
    provider_context: carina_core::parser::ProviderContext,
}

impl DiagnosticEngine {
    pub fn new(
        schemas: Arc<HashMap<String, ResourceSchema>>,
        provider_names: Vec<String>,
        factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> Self {
        let factories_clone = Arc::clone(&factories);
        let provider_context = carina_core::parser::ProviderContext {
            decryptor: None,
            validators: carina_core::provider::collect_custom_type_validators(&schemas),
            custom_type_validator: Some(Box::new(move |type_name: &str, value: &str| {
                for factory in factories_clone.iter() {
                    factory.validate_custom_type(type_name, value)?;
                }
                Ok(())
            })),
        };
        Self {
            schemas,
            provider_names,
            factories,
            provider_errors: HashMap::new(),
            provider_context,
        }
    }

    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }

    pub fn with_provider_errors(mut self, errors: HashMap<String, String>) -> Self {
        self.provider_errors = errors;
        self
    }

    pub fn analyze(
        &self,
        doc: &Document,
        base_path: Option<&Path>,
        sibling_bindings: &HashMap<String, String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        // Extract defined resource bindings
        let defined_bindings = self.extract_resource_bindings(&text);

        // Parse errors
        if let Some(error) = doc.parse_error() {
            diagnostics.push(parse_error_to_diagnostic(error));
        }

        // Check for undefined resource references in the raw text
        let undef_diags = self.check_undefined_references(&text, &defined_bindings);
        diagnostics.extend(undef_diags);

        // Semantic analysis on parsed file
        if let Some(parsed) = doc.parsed() {
            // Check provider in module
            diagnostics.extend(self.check_provider_in_module(doc, parsed));

            // Check provider region
            diagnostics.extend(self.check_provider_region(doc, parsed));

            // Check for unloaded providers
            diagnostics.extend(self.check_unloaded_providers(doc, parsed));

            // Check module calls
            if let Some(base) = base_path {
                diagnostics.extend(self.check_module_calls(doc, parsed, base));
            }
            // Build binding_name -> (provider, resource_type) map for ResourceRef type checking
            let mut binding_schema_map: HashMap<String, ResourceSchema> = HashMap::new();
            for res in &parsed.resources {
                if let Some(ref binding_name) = res.binding {
                    let full_type = format!("{}.{}", res.id.provider, res.id.resource_type);
                    if let Some(s) = self.schemas.get(&full_type).cloned() {
                        binding_schema_map.insert(binding_name.clone(), s);
                    }
                }
            }

            // Check resource types
            for resource in &parsed.resources {
                let provider = &resource.id.provider;
                let full_resource_type = format!("{}.{}", provider, resource.id.resource_type);

                if !self.schemas.contains_key(&full_resource_type) {
                    let provider_loaded = self.provider_names.contains(&provider.to_string());

                    if let Some(reason) = self.provider_errors.get(provider) {
                        // Provider failed to load
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource.id.resource_type,
                        ) {
                            let end_col = col
                                + resource.id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::INFORMATION,
                                format!("Provider '{}' is not loaded: {}", provider, reason),
                            ));
                        }
                    } else if !provider_loaded {
                        // Provider not loaded at all
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource.id.resource_type,
                        ) {
                            let end_col = col
                                + resource.id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::ERROR,
                                format!(
                                    "Unknown resource type: {}.{}",
                                    provider, resource.id.resource_type
                                ),
                            ));
                        }
                    } else {
                        // Provider loaded but no schema for this resource type
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource.id.resource_type,
                        ) {
                            let end_col = col
                                + resource.id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::WARNING,
                                format!(
                                    "No schema for {}.{} — attribute validation skipped",
                                    provider, resource.id.resource_type
                                ),
                            ));
                        }
                    }
                }

                // Semantic validation using schema
                let schema = self.schemas.get(&full_resource_type).cloned();
                if let Some(schema) = &schema {
                    // Check data source without `read` keyword
                    if schema.data_source
                        && !resource.is_data_source()
                        && let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource.id.resource_type,
                        )
                    {
                        let end_col = col
                            + resource.id.resource_type.len() as u32
                            + provider.len() as u32
                            + 1;
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            end_col,
                            DiagnosticSeverity::ERROR,
                            format!(
                                "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                                full_resource_type, full_resource_type
                            ),
                        ));
                    }
                }
                if let Some(schema) = schema {
                    // Build block_name -> canonical_name map for this schema
                    let bn_map = schema.block_name_map();

                    for (attr_name, attr_value) in &resource.attributes {
                        if attr_name.starts_with('_') {
                            continue; // Skip internal attributes
                        }

                        // Resolve block_name to canonical attribute name
                        let canonical_name = bn_map
                            .get(attr_name)
                            .map(|s| s.as_str())
                            .unwrap_or(attr_name);

                        // Check for mixed syntax: both block_name and canonical name present
                        // Skip when block_name == canonical name (singular names like "statement")
                        if let Some(canon) = bn_map.get(attr_name)
                            && canon != attr_name
                            && resource.attributes.contains_key(canon)
                        {
                            if let Some((line, col)) = self.find_attribute_position(doc, attr_name)
                            {
                                diagnostics.push(carina_diagnostic(
                                    line,
                                    col,
                                    col + attr_name.len() as u32,
                                    DiagnosticSeverity::ERROR,
                                    format!(
                                        "Cannot use both '{}' and '{}' (they refer to the same attribute)",
                                        attr_name, canon
                                    ),
                                ));
                            }
                            continue;
                        }

                        // Check for unknown attributes
                        if !schema.attributes.contains_key(canonical_name) {
                            if let Some((line, col)) = self.find_attribute_position(doc, attr_name)
                            {
                                // Check if there's a similar attribute (e.g., vpc -> vpc_id)
                                let suggestion =
                                    if schema.attributes.contains_key(&format!("{}_id", attr_name))
                                    {
                                        format!(". Did you mean '{}_id'?", attr_name)
                                    } else {
                                        String::new()
                                    };

                                diagnostics.push(carina_diagnostic(
                                    line,
                                    col,
                                    col + attr_name.len() as u32,
                                    DiagnosticSeverity::WARNING,
                                    format!(
                                        "Unknown attribute '{}' for resource type '{}'{}",
                                        attr_name, resource.id.resource_type, suggestion
                                    ),
                                ));
                            }
                            continue;
                        }

                        // Type validation
                        if let Some(attr_schema) = schema.attributes.get(canonical_name) {
                            // Check for block syntax on bare Struct attributes:
                            // Block syntax produces Value::List, but bare Struct requires
                            // map assignment syntax: attr = { ... }
                            if matches!(
                                &attr_schema.attr_type,
                                carina_core::schema::AttributeType::Struct { .. }
                            ) && matches!(&**attr_value, Value::List(_))
                            {
                                let search_name =
                                    attr_schema.block_name.as_deref().unwrap_or(attr_name);
                                let block_positions =
                                    self.find_all_block_positions(doc, search_name);
                                for pos in &block_positions {
                                    diagnostics.push(carina_diagnostic(
                                        pos.0,
                                        pos.1,
                                        pos.1 + search_name.len() as u32,
                                        DiagnosticSeverity::ERROR,
                                        format!(
                                            "'{}' cannot use block syntax; use map assignment: {} = {{ ... }}",
                                            search_name, search_name
                                        ),
                                    ));
                                }
                            }

                            let type_error = match (&attr_schema.attr_type, &**attr_value) {
                                // Bool type should not receive String
                                (carina_core::schema::AttributeType::Bool, Value::String(s)) => {
                                    Some(format!(
                                        "Type mismatch: expected Bool, got String \"{}\". Use true or false.",
                                        s
                                    ))
                                }
                                // Int type should not receive String
                                (carina_core::schema::AttributeType::Int, Value::String(s)) => {
                                    Some(format!(
                                        "Type mismatch: expected Int, got String \"{}\".",
                                        s
                                    ))
                                }
                                // Float type should not receive String
                                (carina_core::schema::AttributeType::Float, Value::String(s)) => {
                                    Some(format!(
                                        "Type mismatch: expected Float, got String \"{}\".",
                                        s
                                    ))
                                }
                                // ResourceRef type check for Union, StringEnum, and Custom types
                                (
                                    carina_core::schema::AttributeType::Union(_)
                                    | carina_core::schema::AttributeType::StringEnum { .. }
                                    | carina_core::schema::AttributeType::Custom { .. },
                                    Value::ResourceRef { path },
                                ) => check_resource_ref_type_mismatch(
                                    &binding_schema_map,
                                    &attr_schema.attr_type,
                                    path.binding(),
                                    path.attribute(),
                                ),
                                // Custom type validation (all Custom types use their validate fn)
                                (carina_core::schema::AttributeType::StringEnum { .. }, value) => {
                                    attr_schema
                                        .attr_type
                                        .validate(value)
                                        .err()
                                        .map(|e| e.to_string())
                                }
                                (
                                    carina_core::schema::AttributeType::Custom {
                                        name,
                                        validate,
                                        namespace,
                                        ..
                                    },
                                    value,
                                ) => {
                                    // Handle bare/shorthand enum identifiers by expanding to full namespace format.
                                    // These are String values like "dedicated" or "InstanceTenancy.dedicated".
                                    let resolved_value = match value {
                                        Value::String(s) if !s.contains('.') => {
                                            // Bare identifier: "dedicated" -> namespace.TypeName.dedicated
                                            let expanded = match namespace {
                                                Some(ns) => format!("{}.{}.{}", ns, name, s),
                                                None => s.clone(),
                                            };
                                            Value::String(expanded)
                                        }
                                        Value::String(s) if s.split('.').count() == 2 => {
                                            // Two-part: "InstanceTenancy.dedicated" -> namespace.InstanceTenancy.dedicated
                                            if let Some((ident, member)) = s.split_once('.') {
                                                let expanded = match namespace {
                                                    Some(ns) if ident == name => {
                                                        format!("{}.{}.{}", ns, ident, member)
                                                    }
                                                    Some(_ns) => s.clone(),
                                                    None => s.clone(),
                                                };
                                                Value::String(expanded)
                                            } else {
                                                value.clone()
                                            }
                                        }
                                        _ => value.clone(),
                                    };

                                    // Use schema's validate function for all Custom types
                                    validate(&resolved_value).err().map(|e| e.to_string())
                                }
                                // String type - check for bare resource binding
                                (carina_core::schema::AttributeType::String, Value::String(s)) => {
                                    if let Some(binding) =
                                        s.strip_prefix("${").and_then(|s| s.strip_suffix("}"))
                                    {
                                        let suggested_attr = if attr_name.ends_with("_id") {
                                            "id"
                                        } else {
                                            "name"
                                        };
                                        Some(format!(
                                            "Expected string, got resource reference '{}'. Did you mean '{}.{}'?",
                                            binding, binding, suggested_attr
                                        ))
                                    } else {
                                        None
                                    }
                                }
                                // Validate List item types (non-Struct items only;
                                // List<Struct> is handled by validate_struct_value below)
                                (
                                    carina_core::schema::AttributeType::List { inner, .. },
                                    Value::List(_),
                                ) if !matches!(
                                    inner.as_ref(),
                                    carina_core::schema::AttributeType::Struct { .. }
                                ) =>
                                {
                                    attr_schema
                                        .attr_type
                                        .validate(attr_value)
                                        .err()
                                        .map(|e| e.to_string())
                                }
                                // Validate Map value types
                                (carina_core::schema::AttributeType::Map { .. }, Value::Map(_)) => {
                                    attr_schema
                                        .attr_type
                                        .validate(attr_value)
                                        .err()
                                        .map(|e| e.to_string())
                                }
                                // Validate Union static values (non-ResourceRef)
                                (carina_core::schema::AttributeType::Union(_), value)
                                    if !matches!(value, Value::ResourceRef { .. }) =>
                                {
                                    attr_schema
                                        .attr_type
                                        .validate(value)
                                        .err()
                                        .map(|e| e.to_string())
                                }
                                _ => None,
                            };

                            if let Some(message) = type_error
                                && let Some((line, col)) =
                                    self.find_attribute_position(doc, attr_name)
                            {
                                diagnostics.push(carina_diagnostic(
                                    line,
                                    col,
                                    col + attr_name.len() as u32,
                                    DiagnosticSeverity::WARNING,
                                    message,
                                ));
                            }

                            // Struct field validation
                            let struct_fields = match &attr_schema.attr_type {
                                carina_core::schema::AttributeType::Struct { fields, .. } => {
                                    Some(fields)
                                }
                                carina_core::schema::AttributeType::List { inner, .. } => {
                                    match inner.as_ref() {
                                        carina_core::schema::AttributeType::Struct {
                                            fields,
                                            ..
                                        } => Some(fields),
                                        _ => None,
                                    }
                                }
                                _ => None,
                            };

                            if let Some(fields) = struct_fields {
                                diagnostics.extend(
                                    self.validate_struct_value(doc, attr_name, attr_value, fields),
                                );
                            }
                        }
                    }

                    // Run resource-level validator (e.g., mutually exclusive required fields)
                    let resolved_attrs = resource.resolved_attributes();
                    if let Err(errors) = schema.validate(&resolved_attrs) {
                        for error in errors {
                            // Skip errors that are already reported with precise positions
                            // by the attribute-level checks above.
                            if matches!(
                                error,
                                carina_core::schema::TypeError::BlockSyntaxNotAllowed { .. }
                                    | carina_core::schema::TypeError::TypeMismatch { .. }
                                    | carina_core::schema::TypeError::InvalidEnumVariant { .. }
                                    | carina_core::schema::TypeError::ValidationFailed { .. }
                                    | carina_core::schema::TypeError::UnknownStructField { .. }
                                    | carina_core::schema::TypeError::StructFieldError { .. }
                                    | carina_core::schema::TypeError::ListItemError { .. }
                                    | carina_core::schema::TypeError::MapValueError { .. }
                            ) {
                                continue;
                            }
                            // Try attribute-level position first, fall back to resource position
                            let position =
                                if let carina_core::schema::TypeError::ResourceValidationFailed {
                                    attribute: Some(attr),
                                    ..
                                } = &error
                                {
                                    self.find_attribute_position(doc, attr)
                                } else {
                                    None
                                };
                            let position = position.or_else(|| {
                                self.find_resource_type_position(
                                    doc,
                                    provider,
                                    &resource.id.resource_type,
                                )
                            });
                            if let Some((line, col)) = position {
                                diagnostics.push(carina_diagnostic_range(
                                    Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col
                                                + doc
                                                    .text()
                                                    .lines()
                                                    .nth(line as usize)
                                                    .map_or(0, |l| l.trim_end().len() as u32),
                                        },
                                    },
                                    DiagnosticSeverity::ERROR,
                                    error.to_string(),
                                ));
                            }
                        }
                    }

                    // Lint: prefer block syntax for List<Struct> attributes
                    diagnostics.extend(self.check_list_struct_syntax(
                        doc,
                        &resolved_attrs,
                        &schema,
                    ));
                }
            }

            // Check for unknown built-in function calls
            diagnostics.extend(self.check_unknown_functions(doc, parsed));

            // Check attributes blocks
            diagnostics.extend(self.check_attributes_blocks(doc, parsed));

            // Check exports blocks
            diagnostics.extend(self.check_exports_blocks(doc, parsed, None, sibling_bindings));

            // Check for unused let bindings
            diagnostics.extend(self.check_unused_bindings(doc, parsed));

            // Check for unknown attributes on resource references (typo detection)
            diagnostics.extend(self.check_resource_ref_attributes(
                doc,
                parsed,
                &binding_schema_map,
            ));
        }

        // Check for duplicate attribute keys (text-based, works without parsed file)
        diagnostics.extend(self.check_duplicate_attrs(doc));

        // Check for direct calls to pipe-preferred functions
        diagnostics.extend(self.check_pipe_preferred_direct_calls(doc));

        // Check for non-snake_case binding names
        diagnostics.extend(self.check_non_snake_case_bindings(doc));

        diagnostics
    }

    /// Check for duplicate attribute keys within the same block.
    fn check_duplicate_attrs(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let duplicates = carina_core::lint::find_duplicate_attrs(&text);

        duplicates
            .into_iter()
            .filter_map(|dup| {
                // Convert 1-indexed line to 0-indexed
                let line = (dup.line - 1) as u32;
                // Find the column of the attribute name
                let line_text = text.lines().nth(dup.line - 1)?;
                let col = position::leading_whitespace_chars(line_text);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + dup.name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Duplicate attribute '{}' (first defined on line {}). The last value will be used.",
                        dup.name, dup.first_line
                    ),
                ))
            })
            .collect()
    }

    /// Check for direct calls to pipe-preferred functions (info-level).
    fn check_pipe_preferred_direct_calls(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_pipe_preferred_direct_calls(&text);

        warnings
            .into_iter()
            .filter_map(|pw| {
                let line = (pw.line - 1) as u32;
                let line_text = text.lines().nth(pw.line - 1)?;
                let pattern = format!("{}(", pw.name);
                let byte_pos = line_text.find(&pattern)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + pw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Consider using pipe form for '{}': data |> {}(...)",
                        pw.name, pw.name
                    ),
                ))
            })
            .collect()
    }

    /// Check for non-snake_case binding names (info-level).
    fn check_non_snake_case_bindings(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_non_snake_case_bindings(&text);

        warnings
            .into_iter()
            .filter_map(|nw| {
                let line = (nw.line - 1) as u32;
                let line_text = text.lines().nth(nw.line - 1)?;
                // Find the binding name position in the line
                let byte_pos = line_text.find(&nw.name)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + nw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Binding '{}' is not snake_case. Use snake_case for binding names (e.g., 'my_resource').",
                        nw.name
                    ),
                ))
            })
            .collect()
    }

    fn find_resource_type_position(
        &self,
        doc: &Document,
        provider: &str,
        resource_type: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{}.{}", provider, resource_type);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(pattern.as_str()) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    fn find_attribute_position(&self, doc: &Document, attr_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Must start with attr_name followed by whitespace or '='
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after_attr = &trimmed[attr_name.len()..];
            if !after_attr.starts_with(' ') && !after_attr.starts_with('=') {
                continue;
            }
            // Calculate column position (account for leading whitespace)
            return Some((line_idx as u32, position::leading_whitespace_chars(line)));
        }
        None
    }
}

/// Check whether a ResourceRef value is type-compatible with the expected attribute type.
/// Returns `Some(message)` on mismatch, `None` when compatible or when the binding/attribute
/// cannot be resolved (unknown bindings are not flagged here).
fn check_resource_ref_type_mismatch(
    binding_schema_map: &HashMap<String, ResourceSchema>,
    expected_type: &carina_core::schema::AttributeType,
    ref_binding: &str,
    ref_attr: &str,
) -> Option<String> {
    let ref_schema = binding_schema_map.get(ref_binding)?;
    let ref_attr_schema = ref_schema.attributes.get(ref_attr)?;
    let ref_type_name = ref_attr_schema.attr_type.type_name();

    let expected_type_name = expected_type.type_name();
    if expected_type.accepts_type_name(&ref_type_name)
        || expected_type_name == "String"
        || ref_type_name == "String"
        || (expected_type.is_string_based_custom()
            && ref_attr_schema.attr_type.is_string_based_custom())
    {
        None
    } else {
        Some(format!(
            "Type mismatch: expected {}, got {} (from {}.{})",
            expected_type.type_name(),
            ref_type_name,
            ref_binding,
            ref_attr
        ))
    }
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            carina_diagnostic(
                (line.saturating_sub(1)) as u32,
                (col.saturating_sub(1)) as u32,
                col as u32,
                DiagnosticSeverity::ERROR,
                format!("{}", pest_error),
            )
        }
        ParseError::InvalidExpression { line, message } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            message.clone(),
        ),
        ParseError::UndefinedVariable(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Undefined variable: {}", name),
        ),
        ParseError::InvalidResourceType(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Invalid resource type: {}", name),
        ),
        ParseError::DuplicateModule(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Duplicate module definition: {}", name),
        ),
        ParseError::DuplicateBinding { name, line } => carina_diagnostic(
            (line - 1) as u32,
            0,
            0,
            DiagnosticSeverity::ERROR,
            format!("Duplicate binding: {}", name),
        ),
        ParseError::ModuleNotFound(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Module not found: {}", name),
        ),
        ParseError::InternalError { expected, context } => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!(
                "Internal parser error: expected {} in {}",
                expected, context
            ),
        ),
        ParseError::RecursiveFunction(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Recursive function call detected: {}", name),
        ),
        ParseError::UserFunctionError(msg) => {
            carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, msg.to_string())
        }
    }
}
