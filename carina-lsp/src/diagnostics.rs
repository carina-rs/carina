use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use carina_core::parser::{InputParameter, ParseError, ParsedFile, TypeExpr};
use carina_core::provider::ProviderFactory;
use carina_core::resource::Value;
use carina_core::schema::{ResourceSchema, validate_ipv4_cidr};

pub struct DiagnosticEngine {
    schemas: Arc<HashMap<String, ResourceSchema>>,
    provider_names: Vec<String>,
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
}

impl DiagnosticEngine {
    pub fn new(
        schemas: Arc<HashMap<String, ResourceSchema>>,
        provider_names: Vec<String>,
        factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> Self {
        Self {
            schemas,
            provider_names,
            factories,
        }
    }

    pub fn analyze(&self, doc: &Document, base_path: Option<&Path>) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        // Extract defined resource bindings
        let defined_bindings = self.extract_resource_bindings(&text);

        // Parse errors
        if let Some(error) = doc.parse_error() {
            diagnostics.push(parse_error_to_diagnostic(error));
        }

        // Check for undefined resource references in the raw text
        diagnostics.extend(self.check_undefined_references(&text, &defined_bindings));

        // Semantic analysis on parsed file
        if let Some(parsed) = doc.parsed() {
            // Check provider region
            diagnostics.extend(self.check_provider_region(doc, parsed));

            // Check module calls
            if let Some(base) = base_path {
                diagnostics.extend(self.check_module_calls(doc, parsed, base));
            }
            // Build binding_name -> (provider, resource_type) map for ResourceRef type checking
            let mut binding_schema_map: HashMap<String, ResourceSchema> = HashMap::new();
            for res in &parsed.resources {
                if let Some(Value::String(binding_name)) = res.attributes.get("_binding") {
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
                    // Find the line where this resource is defined
                    if let Some((line, col)) =
                        self.find_resource_position(doc, &resource.id.resource_type)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col
                                        + resource.id.resource_type.len() as u32
                                        + provider.len() as u32
                                        + 1, // "provider." prefix
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "Unknown resource type: {}.{}",
                                provider, resource.id.resource_type
                            ),
                            ..Default::default()
                        });
                    }
                }

                // Semantic validation using schema
                let schema = self.schemas.get(&full_resource_type).cloned();
                if let Some(schema) = &schema {
                    // Check data source without `read` keyword
                    if schema.data_source
                        && !resource.read_only
                        && let Some((line, col)) =
                            self.find_resource_position(doc, &resource.id.resource_type)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col
                                        + resource.id.resource_type.len() as u32
                                        + provider.len() as u32
                                        + 1,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                                full_resource_type, full_resource_type
                            ),
                            ..Default::default()
                        });
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
                        if let Some(canon) = bn_map.get(attr_name)
                            && resource.attributes.contains_key(canon)
                        {
                            if let Some((line, col)) = self.find_attribute_position(doc, attr_name)
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col + attr_name.len() as u32,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::ERROR),
                                    source: Some("carina".to_string()),
                                    message: format!(
                                        "Cannot use both '{}' and '{}' (they refer to the same attribute)",
                                        attr_name, canon
                                    ),
                                    ..Default::default()
                                });
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

                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col + attr_name.len() as u32,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("carina".to_string()),
                                    message: format!(
                                        "Unknown attribute '{}' for resource type '{}'{}",
                                        attr_name, resource.id.resource_type, suggestion
                                    ),
                                    ..Default::default()
                                });
                            }
                            continue;
                        }

                        // Type validation
                        if let Some(attr_schema) = schema.attributes.get(canonical_name) {
                            // Check for duplicate struct blocks:
                            // If schema type is Struct (not List(Struct)) but value is a List with multiple items
                            if matches!(
                                &attr_schema.attr_type,
                                carina_core::schema::AttributeType::Struct { .. }
                            ) && let Value::List(items) = attr_value
                                && items.len() > 1
                            {
                                // Find all block positions for this attribute
                                // Use block_name if this attribute was accessed via block_name
                                let search_name =
                                    attr_schema.block_name.as_deref().unwrap_or(attr_name);
                                let block_positions =
                                    self.find_all_block_positions(doc, search_name);
                                // Emit error on the second and subsequent blocks
                                for pos in block_positions.iter().skip(1) {
                                    diagnostics.push(Diagnostic {
                                        range: Range {
                                            start: Position {
                                                line: pos.0,
                                                character: pos.1,
                                            },
                                            end: Position {
                                                line: pos.0,
                                                character: pos.1 + search_name.len() as u32,
                                            },
                                        },
                                        severity: Some(DiagnosticSeverity::ERROR),
                                        source: Some("carina".to_string()),
                                        message: format!(
                                            "'{}' is a single block attribute and cannot be specified more than once",
                                            search_name
                                        ),
                                        ..Default::default()
                                    });
                                }
                            }

                            let type_error = match (&attr_schema.attr_type, attr_value) {
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
                                // ResourceRef type check for Union types
                                (
                                    carina_core::schema::AttributeType::Union(_),
                                    Value::ResourceRef {
                                        binding_name: ref_binding,
                                        attribute_name: ref_attr,
                                        ..
                                    },
                                ) => {
                                    if let Some(ref_schema) =
                                        binding_schema_map.get(ref_binding.as_str())
                                    {
                                        if let Some(ref_attr_schema) =
                                            ref_schema.attributes.get(ref_attr.as_str())
                                        {
                                            let ref_type_name =
                                                ref_attr_schema.attr_type.type_name();
                                            if attr_schema
                                                .attr_type
                                                .accepts_type_name(&ref_type_name)
                                                || ref_type_name == "String"
                                            {
                                                None
                                            } else {
                                                Some(format!(
                                                    "Type mismatch: expected {}, got {} (from {}.{})",
                                                    attr_schema.attr_type.type_name(),
                                                    ref_type_name,
                                                    ref_binding,
                                                    ref_attr
                                                ))
                                            }
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                }
                                // ResourceRef type check for Custom types
                                (
                                    carina_core::schema::AttributeType::Custom {
                                        name: expected_name,
                                        ..
                                    },
                                    Value::ResourceRef {
                                        binding_name: ref_binding,
                                        attribute_name: ref_attr,
                                        ..
                                    },
                                ) => {
                                    if let Some(ref_schema) =
                                        binding_schema_map.get(ref_binding.as_str())
                                    {
                                        if let Some(ref_attr_schema) =
                                            ref_schema.attributes.get(ref_attr.as_str())
                                        {
                                            let ref_type_name =
                                                ref_attr_schema.attr_type.type_name();
                                            if ref_type_name != *expected_name
                                                && ref_type_name != "String"
                                            {
                                                Some(format!(
                                                    "Type mismatch: expected {}, got {} (from {}.{})",
                                                    expected_name,
                                                    ref_type_name,
                                                    ref_binding,
                                                    ref_attr
                                                ))
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                }
                                // Custom type validation (all Custom types use their validate fn)
                                (
                                    carina_core::schema::AttributeType::Custom {
                                        name,
                                        validate,
                                        namespace,
                                        ..
                                    },
                                    value,
                                ) => {
                                    // Handle UnresolvedIdent by expanding to full namespace format
                                    let resolved_value = match value {
                                        Value::UnresolvedIdent(ident, member) => {
                                            let expanded = match (namespace, member) {
                                                // TypeName.value -> namespace.TypeName.value
                                                (Some(ns), Some(m)) if ident == name => {
                                                    format!("{}.{}.{}", ns, ident, m)
                                                }
                                                // SomeOther.value with namespace
                                                (Some(_ns), Some(m)) => {
                                                    format!("{}.{}", ident, m)
                                                }
                                                // value -> namespace.TypeName.value
                                                (Some(ns), None) => {
                                                    format!("{}.{}.{}", ns, name, ident)
                                                }
                                                // No namespace, keep as-is
                                                (None, Some(m)) => format!("{}.{}", ident, m),
                                                (None, None) => ident.clone(),
                                            };
                                            Value::String(expanded)
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
                                _ => None,
                            };

                            if let Some(message) = type_error
                                && let Some((line, col)) =
                                    self.find_attribute_position(doc, attr_name)
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col + attr_name.len() as u32,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("carina".to_string()),
                                    message,
                                    ..Default::default()
                                });
                            }

                            // Struct field validation
                            let struct_fields = match &attr_schema.attr_type {
                                carina_core::schema::AttributeType::Struct { fields, .. } => {
                                    Some(fields)
                                }
                                carina_core::schema::AttributeType::List(inner) => {
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
                    if let Err(errors) = schema.validate(&resource.attributes) {
                        for error in errors {
                            // Skip DuplicateStructBlock errors here; they are already
                            // reported with precise block positions in the attribute-level check above.
                            if matches!(
                                error,
                                carina_core::schema::TypeError::DuplicateStructBlock { .. }
                            ) {
                                continue;
                            }
                            if let Some((line, _col)) =
                                self.find_resource_position(doc, &resource.id.resource_type)
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position { line, character: 0 },
                                        end: Position {
                                            line: line + 1,
                                            character: 0,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::ERROR),
                                    source: Some("carina".to_string()),
                                    message: error.to_string(),
                                    ..Default::default()
                                });
                            }
                        }
                    }

                    // Lint: prefer block syntax for List<Struct> attributes
                    diagnostics.extend(self.check_list_struct_syntax(
                        doc,
                        &resource.attributes,
                        &schema,
                    ));
                }
            }
        }

        diagnostics
    }

    fn find_resource_position(&self, doc: &Document, resource_type: &str) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            for provider_name in &self.provider_names {
                let pattern = format!("{}.{}", provider_name, resource_type);
                if let Some(col) = line.find(pattern.as_str()) {
                    return Some((line_idx as u32, col as u32));
                }
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
            let leading_ws = line.len() - trimmed.len();
            return Some((line_idx as u32, leading_ws as u32));
        }
        None
    }

    /// Validate struct values (fields inside nested blocks)
    fn validate_struct_value(
        &self,
        doc: &Document,
        attr_name: &str,
        value: &Value,
        fields: &[carina_core::schema::StructField],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        let maps: Vec<&std::collections::HashMap<String, Value>> = match value {
            Value::Map(map) => vec![map],
            Value::List(items) => items
                .iter()
                .filter_map(|item| {
                    if let Value::Map(map) = item {
                        Some(map)
                    } else {
                        None
                    }
                })
                .collect(),
            _ => return diagnostics,
        };

        let field_names: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        let field_map: HashMap<&str, &carina_core::schema::StructField> =
            fields.iter().map(|f| (f.name.as_str(), f)).collect();

        for (map_index, map) in maps.iter().enumerate() {
            for (key, val) in *map {
                let all_positions = self.find_all_nested_field_positions(doc, attr_name, key);
                if let Some(Some((line, col))) = all_positions.get(map_index) {
                    let (line, col) = (*line, *col);
                    // Check for unknown fields
                    if !field_names.contains(key.as_str()) {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + key.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("carina".to_string()),
                            message: format!("Unknown field '{}' in '{}'", key, attr_name),
                            ..Default::default()
                        });
                        continue;
                    }

                    // Type validation for known fields
                    if let Some(field) = field_map.get(key.as_str()) {
                        let type_error = match (&field.field_type, val) {
                            (carina_core::schema::AttributeType::Bool, Value::String(s)) => {
                                Some(format!(
                                    "Type mismatch: expected Bool, got String \"{}\". Use true or false.",
                                    s
                                ))
                            }
                            (carina_core::schema::AttributeType::Int, Value::String(s)) => Some(
                                format!("Type mismatch: expected Int, got String \"{}\".", s),
                            ),
                            (carina_core::schema::AttributeType::Float, Value::String(s)) => Some(
                                format!("Type mismatch: expected Float, got String \"{}\".", s),
                            ),
                            // Custom types with Int base (e.g., ranged integers)
                            (
                                carina_core::schema::AttributeType::Custom { base, .. },
                                Value::String(s),
                            ) if matches!(**base, carina_core::schema::AttributeType::Int) => Some(
                                format!("Type mismatch: expected Int, got String \"{}\".", s),
                            ),
                            // Custom types with Float base (e.g., ranged floats)
                            (
                                carina_core::schema::AttributeType::Custom { base, .. },
                                Value::String(s),
                            ) if matches!(**base, carina_core::schema::AttributeType::Float) => {
                                Some(format!(
                                    "Type mismatch: expected Float, got String \"{}\".",
                                    s
                                ))
                            }
                            _ => None,
                        };

                        if let Some(message) = type_error {
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line,
                                        character: col,
                                    },
                                    end: Position {
                                        line,
                                        character: col + key.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Find the start positions of ALL blocks with the given name.
    /// Returns `(line, col)` for each occurrence of `block_name {`.
    fn find_all_block_positions(&self, doc: &Document, block_name: &str) -> Vec<(u32, u32)> {
        let text = doc.text();
        let mut positions = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Look for "block_name {" (without "=")
            if trimmed.starts_with(block_name) && !trimmed.contains('=') {
                let after = trimmed[block_name.len()..].trim();
                if after.starts_with('{') {
                    let leading_ws = line.len() - trimmed.len();
                    positions.push((line_idx as u32, leading_ws as u32));
                }
            }
        }

        positions
    }

    /// Find position of list literal syntax (`attr_name = [`) in source text.
    /// Returns `(line, col)` if found.
    fn find_list_literal_position(&self, doc: &Document, attr_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after = &trimmed[attr_name.len()..];
            // Must be followed by whitespace or '=' (not part of a longer identifier)
            if !after.starts_with(' ') && !after.starts_with('=') {
                continue;
            }
            // Check for `= [` pattern (list literal)
            let after_trimmed = after.trim_start();
            if let Some(rest) = after_trimmed.strip_prefix('=') {
                let rest_trimmed = rest.trim_start();
                if rest_trimmed.starts_with('[') {
                    let leading_ws = line.len() - trimmed.len();
                    return Some((line_idx as u32, leading_ws as u32));
                }
            }
        }
        None
    }

    /// Check List<Struct> attributes for list literal syntax and suggest block syntax.
    fn check_list_struct_syntax(
        &self,
        doc: &Document,
        resource_attrs: &std::collections::HashMap<String, Value>,
        schema: &ResourceSchema,
    ) -> Vec<Diagnostic> {
        use carina_core::schema::AttributeType;

        let mut diagnostics = Vec::new();

        for (attr_name, attr_schema) in &schema.attributes {
            // Only check List<Struct> attributes
            let is_list_struct = matches!(
                &attr_schema.attr_type,
                AttributeType::List(inner) if matches!(inner.as_ref(), AttributeType::Struct { .. })
            );
            if !is_list_struct {
                continue;
            }

            // Only check attributes that actually exist in the resource
            if !resource_attrs.contains_key(attr_name) {
                continue;
            }

            if let Some((line, col)) = self.find_list_literal_position(doc, attr_name) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: col,
                        },
                        end: Position {
                            line,
                            character: col + attr_name.len() as u32,
                        },
                    },
                    severity: Some(DiagnosticSeverity::HINT),
                    source: Some("carina".to_string()),
                    message: format!(
                        "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                        attr_name, attr_name, attr_name
                    ),
                    ..Default::default()
                });
            }
        }

        diagnostics
    }

    /// Find the positions of a field inside ALL matching nested blocks
    /// Returns a Vec with one entry per block occurrence. Each entry is `Some((line, col))`
    /// if the field was found in that block, or `None` if not.
    fn find_all_nested_field_positions(
        &self,
        doc: &Document,
        block_name: &str,
        field_name: &str,
    ) -> Vec<Option<(u32, u32)>> {
        let text = doc.text();
        let mut positions = Vec::new();
        let mut in_block = false;
        let mut brace_depth = 0;
        let mut found_in_current_block: Option<(u32, u32)> = None;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Look for "block_name {" (without "=")
            if !in_block && trimmed.starts_with(block_name) && !trimmed.contains('=') {
                let after = trimmed[block_name.len()..].trim();
                if after.starts_with('{') {
                    in_block = true;
                    brace_depth = 1;
                    found_in_current_block = None;
                    continue;
                }
            }

            if in_block {
                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_block = false;
                            positions.push(found_in_current_block);
                            found_in_current_block = None;
                            break;
                        }
                    }
                }

                if in_block && brace_depth == 1 {
                    // Check if this line starts with the field name
                    if let Some(after) = trimmed.strip_prefix(field_name)
                        && (after.starts_with(' ') || after.starts_with('='))
                    {
                        let leading_ws = line.len() - trimmed.len();
                        found_in_current_block = Some((line_idx as u32, leading_ws as u32));
                    }
                }
            }
        }

        // Handle case where file ends while still in a block
        if in_block {
            positions.push(found_in_current_block);
        }

        positions
    }

    /// Check provider region attribute using factory validate_config
    fn check_provider_region(&self, doc: &Document, parsed: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for provider in &parsed.providers {
            // Find the matching factory for this provider
            if let Some(factory) = self.factories.iter().find(|f| f.name() == provider.name)
                && let Err(e) = factory.validate_config(&provider.attributes)
                && let Some((line, col)) = self.find_provider_region_position(doc, &provider.name)
            {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: col,
                        },
                        end: Position {
                            line,
                            character: col + 6, // "region"
                        },
                    },
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("carina".to_string()),
                    message: format!("provider {}: {}", provider.name, e),
                    ..Default::default()
                });
            }
        }
        diagnostics
    }

    /// Find the position of the region attribute in a provider block
    fn find_provider_region_position(
        &self,
        doc: &Document,
        provider_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_provider = false;
        let provider_pattern = format!("provider {}", provider_name);

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with(&provider_pattern) {
                in_provider = true;
            }

            if in_provider {
                if trimmed.starts_with("region") {
                    let leading_ws = line.len() - trimmed.len();
                    return Some((line_idx as u32, leading_ws as u32));
                }

                if trimmed == "}" {
                    in_provider = false;
                }
            }
        }
        None
    }

    /// Check module calls against imported module definitions
    fn check_module_calls(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        base_path: &Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Build a map of imported modules: alias -> input parameters
        let mut imported_modules: HashMap<String, Vec<InputParameter>> = HashMap::new();

        for import in &parsed.imports {
            let module_path = base_path.join(&import.path);
            if let Some(module_parsed) = self.load_module(&module_path) {
                imported_modules.insert(import.alias.clone(), module_parsed.inputs);
            }
        }

        // Check each module call
        for call in &parsed.module_calls {
            if let Some(module_inputs) = imported_modules.get(&call.module_name) {
                // Check for unknown parameters
                for (arg_name, arg_value) in &call.arguments {
                    let matching_input = module_inputs.iter().find(|input| &input.name == arg_name);

                    if matching_input.is_none() {
                        if let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                        {
                            // Find similar parameter names for suggestion
                            let suggestion = module_inputs
                                .iter()
                                .find(|input| {
                                    input.name.contains(arg_name) || arg_name.contains(&input.name)
                                })
                                .map(|input| format!(". Did you mean '{}'?", input.name))
                                .unwrap_or_default();

                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line,
                                        character: col,
                                    },
                                    end: Position {
                                        line,
                                        character: col + arg_name.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Unknown parameter '{}' for module '{}'{}",
                                    arg_name, call.module_name, suggestion
                                ),
                                ..Default::default()
                            });
                        }
                        continue;
                    }

                    // Type validation for known parameters
                    let input = matching_input.unwrap();
                    if let Some(type_error) =
                        self.validate_module_arg_type(&input.type_expr, arg_value)
                        && let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + arg_name.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("carina".to_string()),
                            message: type_error,
                            ..Default::default()
                        });
                    }
                }

                // Check for missing required parameters
                for input in module_inputs {
                    if input.default.is_none()
                        && !call.arguments.contains_key(&input.name)
                        && let Some((line, col)) =
                            self.find_module_call_position(doc, &call.module_name)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + call.module_name.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "Missing required parameter '{}' for module '{}'",
                                input.name, call.module_name
                            ),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        diagnostics
    }

    /// Validate a module argument value against its expected type
    fn validate_module_arg_type(&self, type_expr: &TypeExpr, value: &Value) -> Option<String> {
        match (type_expr, value) {
            // CIDR type validation
            (TypeExpr::Cidr, Value::String(s)) => validate_ipv4_cidr(s).err(),
            // List of CIDR type validation
            (TypeExpr::List(inner), Value::List(items)) => {
                if let TypeExpr::Cidr = inner.as_ref() {
                    for (i, item) in items.iter().enumerate() {
                        if let Value::String(s) = item {
                            if let Err(e) = validate_ipv4_cidr(s) {
                                return Some(format!("Element {}: {}", i, e));
                            }
                        } else {
                            return Some(format!("Element {}: expected string, got {:?}", i, item));
                        }
                    }
                }
                None
            }
            // Bool type validation
            (TypeExpr::Bool, Value::String(s)) => Some(format!(
                "Type mismatch: expected bool, got string \"{}\". Use true or false.",
                s
            )),
            // Int type validation
            (TypeExpr::Int, Value::String(s)) => Some(format!(
                "Type mismatch: expected int, got string \"{}\".",
                s
            )),
            // Float type validation
            (TypeExpr::Float, Value::String(s)) => Some(format!(
                "Type mismatch: expected float, got string \"{}\".",
                s
            )),
            _ => None,
        }
    }

    /// Find the position of a module call in the document
    fn find_module_call_position(&self, doc: &Document, module_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(col) = line.find(&pattern) {
                return Some((line_idx as u32, col as u32));
            }
        }
        None
    }

    /// Find the position of an argument in a module call
    fn find_module_call_arg_position(
        &self,
        doc: &Document,
        module_name: &str,
        arg_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_module_call = false;
        let module_pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if line.contains(&module_pattern) {
                in_module_call = true;
            }

            if in_module_call {
                let trimmed = line.trim_start();
                if trimmed.starts_with(arg_name)
                    && trimmed[arg_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ' ' || c == '=')
                {
                    let leading_ws = line.len() - trimmed.len();
                    return Some((line_idx as u32, leading_ws as u32));
                }

                if trimmed == "}" {
                    in_module_call = false;
                }
            }
        }
        None
    }

    /// Extract resource binding names from text (variables defined with `let binding_name = aws...` or `let binding_name = read aws...`)
    fn extract_resource_bindings(&self, text: &str) -> HashSet<String> {
        let mut bindings = HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let binding_name = rest[..eq_pos].trim();
                if !binding_name.is_empty()
                    && binding_name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_')
                {
                    bindings.insert(binding_name.to_string());
                }
            }
        }
        bindings
    }

    /// Check for undefined resource references in attribute values
    fn check_undefined_references(
        &self,
        text: &str,
        defined_bindings: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            // Look for patterns like "binding_name.id" or "binding_name.name" after "="
            if let Some(eq_pos) = line.find('=') {
                let after_eq = &line[eq_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                let whitespace_len = after_eq.len() - after_eq_trimmed.len();

                // Skip if it's a string literal
                if after_eq_trimmed.starts_with('"') {
                    continue;
                }

                // Skip if it starts with a provider prefix (enum values like aws.Region.xxx)
                let is_provider_prefix = self
                    .provider_names
                    .iter()
                    .any(|name| after_eq_trimmed.starts_with(&format!("{}.", name)));
                if is_provider_prefix {
                    continue;
                }

                // Check if it looks like a resource reference: identifier.property
                if let Some(dot_pos) = after_eq_trimmed.find('.') {
                    let identifier = &after_eq_trimmed[..dot_pos];
                    let after_dot = &after_eq_trimmed[dot_pos + 1..];

                    // Extract property name
                    let prop_end = after_dot
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(after_dot.len());
                    let property = &after_dot[..prop_end];

                    // Check if this looks like a resource reference (e.g., main_vpc.id)
                    if (property == "id" || property == "name")
                        && !identifier.is_empty()
                        && identifier.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && !identifier.starts_with(|c: char| c.is_uppercase())
                    {
                        // Check if the binding is defined
                        if !defined_bindings.contains(identifier) {
                            let col = (eq_pos + 1 + whitespace_len) as u32;
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line: line_idx as u32,
                                        character: col,
                                    },
                                    end: Position {
                                        line: line_idx as u32,
                                        character: col + identifier.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::ERROR),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Undefined resource: '{}'. Define it with 'let {} = aws...'",
                                    identifier, identifier
                                ),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Load a module from a file or directory
    /// Handles both single-file modules and directory-based modules
    fn load_module(&self, path: &Path) -> Option<ParsedFile> {
        if path.is_dir() {
            // Directory-based module: load main.crn or merge all .crn files
            let main_path = path.join("main.crn");
            if main_path.exists() {
                let content = std::fs::read_to_string(&main_path).ok()?;
                carina_core::parser::parse(&content).ok()
            } else {
                // Merge all .crn files in the directory
                self.load_directory_module(path)
            }
        } else {
            // Single file module
            let content = std::fs::read_to_string(path).ok()?;
            carina_core::parser::parse(&content).ok()
        }
    }

    /// Load all .crn files from a directory and merge them
    fn load_directory_module(&self, dir_path: &Path) -> Option<ParsedFile> {
        let entries = std::fs::read_dir(dir_path).ok()?;
        let mut merged = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            inputs: vec![],
            outputs: vec![],
            backend: None,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "crn")
                && let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(parsed) = carina_core::parser::parse(&content)
            {
                merged.providers.extend(parsed.providers);
                merged.resources.extend(parsed.resources);
                merged.variables.extend(parsed.variables);
                merged.imports.extend(parsed.imports);
                merged.module_calls.extend(parsed.module_calls);
                merged.inputs.extend(parsed.inputs);
                merged.outputs.extend(parsed.outputs);
            }
        }

        if merged.inputs.is_empty() && merged.outputs.is_empty() {
            None
        } else {
            Some(merged)
        }
    }
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            Diagnostic {
                range: Range {
                    start: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: (col.saturating_sub(1)) as u32,
                    },
                    end: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: col as u32,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("carina".to_string()),
                message: format!("{}", pest_error),
                ..Default::default()
            }
        }
        ParseError::InvalidExpression { line, message } => Diagnostic {
            range: Range {
                start: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 0,
                },
                end: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 100,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: message.clone(),
            ..Default::default()
        },
        ParseError::UndefinedVariable(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Undefined variable: {}", name),
            ..Default::default()
        },
        ParseError::EnvVarNotSet(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("carina".to_string()),
            message: format!("Environment variable not set: {}", name),
            ..Default::default()
        },
        ParseError::InvalidResourceType(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Invalid resource type: {}", name),
            ..Default::default()
        },
        ParseError::DuplicateModule(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Duplicate module definition: {}", name),
            ..Default::default()
        },
        ParseError::ModuleNotFound(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Module not found: {}", name),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use carina_core::provider::ProviderFactory;

    fn create_document(content: &str) -> Document {
        Document::new(content.to_string())
    }

    fn test_engine() -> DiagnosticEngine {
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(carina_provider_aws::AwsProviderFactory),
            Box::new(carina_provider_awscc::AwsccProviderFactory),
        ];
        let mut schemas = HashMap::new();
        for factory in &factories {
            for schema in factory.schemas() {
                schemas.insert(schema.resource_type.clone(), schema);
            }
        }
        let schemas = Arc::new(schemas);
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
        let factories = Arc::new(factories);
        DiagnosticEngine::new(schemas, provider_names, factories)
    }

    #[test]
    fn unknown_field_in_struct_block() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        unknown_field = "bad"
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let unknown_field_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("Unknown field 'unknown_field'"));
        assert!(
            unknown_field_diag.is_some(),
            "Should warn about unknown field in struct block. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn type_mismatch_in_struct_field() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = "not_a_number"
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let type_mismatch = diagnostics
            .iter()
            .find(|d| d.message.contains("Type mismatch") && d.message.contains("Int"));
        assert!(
            type_mismatch.is_some(),
            "Should warn about type mismatch for Int field. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resource_ref_type_mismatch() {
        let engine = test_engine();
        // vpc.vpc_id is AwsResourceId, but ipv4_ipam_pool_id expects IpamPoolId
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}

let vpc2 = awscc.ec2.vpc {
    ipv4_ipam_pool_id = vpc.vpc_id
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let type_mismatch = diagnostics
            .iter()
            .find(|d| d.message.contains("Type mismatch") && d.message.contains("IpamPoolId"));
        assert!(
            type_mismatch.is_some(),
            "Should warn about type mismatch for ResourceRef. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resource_ref_compatible_type() {
        let engine = test_engine();
        // ipam_pool.ipam_pool_id is IpamPoolId, and ipv4_ipam_pool_id expects IpamPoolId -> OK
        // Using vpc.vpc_id in a vpc_id field (same type) should not produce a warning
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}

let subnet = awscc.ec2.subnet {
    vpc_id = vpc.vpc_id
    cidr_block = "10.0.1.0/24"
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let type_mismatch = diagnostics
            .iter()
            .find(|d| d.message.contains("Type mismatch") && d.message.contains("AwsResourceId"));
        assert!(
            type_mismatch.is_none(),
            "Should NOT warn about compatible ResourceRef types. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unknown_field_in_second_repeated_block() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
        cidr_ip = "0.0.0.0/0"
    }
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 443
        to_port = 443
        cidr_ip = "0.0.0.0/0"
        bad_field = "oops"
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let bad_field_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("Unknown field 'bad_field'"));
        assert!(
            bad_field_diag.is_some(),
            "Should warn about unknown field in second repeated block. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        // The diagnostic should point to the second block, not the first.
        // LSP uses 0-indexed lines, so line 17 = line 18 in 1-indexed.
        let diag = bad_field_diag.unwrap();
        assert_eq!(
            diag.range.start.line, 17,
            "Diagnostic should point to line 17 (0-indexed, in second block), got line {}",
            diag.range.start.line
        );
    }

    #[test]
    fn duplicate_struct_block_error() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

aws.ec2.subnet {
    name = "my-subnet"
    vpc_id = "vpc-123"
    cidr_block = "10.0.1.0/24"

    private_dns_name_options_on_launch {
        hostname_type = aws.ec2.subnet.HostnameType.resource_name
    }

    private_dns_name_options_on_launch {
        hostname_type = aws.ec2.subnet.HostnameType.ip_name
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let dup_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("single block attribute"));
        assert!(
            dup_diag.is_some(),
            "Should error on duplicate struct block. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        // Error should point to the second block (line 13, 0-indexed)
        let diag = dup_diag.unwrap();
        assert_eq!(
            diag.range.start.line, 13,
            "Diagnostic should point to line 13 (0-indexed, second block), got line {}",
            diag.range.start.line
        );
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));

        // Should only emit one duplicate block diagnostic (not duplicated by resource-level validator)
        let dup_count = diagnostics
            .iter()
            .filter(|d| d.message.contains("single block attribute"))
            .count();
        assert_eq!(
            dup_count,
            1,
            "Should have exactly 1 duplicate block diagnostic, got {}. All diagnostics: {:?}",
            dup_count,
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn single_struct_block_no_error() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

aws.ec2.subnet {
    name = "my-subnet"
    vpc_id = "vpc-123"
    cidr_block = "10.0.1.0/24"

    private_dns_name_options_on_launch {
        hostname_type = aws.ec2.subnet.HostnameType.resource_name
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let dup_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("single block attribute"));
        assert!(
            dup_diag.is_none(),
            "Should NOT error on single struct block. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lint_list_literal_for_list_struct() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
    security_group_ingress = [{
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
        cidr_ip = "0.0.0.0/0"
    }]
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let lint_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("Prefer block syntax"));
        assert!(
            lint_diag.is_some(),
            "Should emit HINT for list literal syntax on List<Struct>. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        let diag = lint_diag.unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
        assert!(diag.message.contains("security_group_ingress"));
    }

    #[test]
    fn lint_block_syntax_no_warning() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
        cidr_ip = "0.0.0.0/0"
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let lint_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("Prefer block syntax"));
        assert!(
            lint_diag.is_none(),
            "Block syntax should NOT produce lint warning. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lint_string_attr_no_warning() {
        let engine = test_engine();
        // group_description is a String attribute — lint should not flag it
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.security_group {
    group_description = "Test security group"
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let lint_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("Prefer block syntax"));
        assert!(
            lint_diag.is_none(),
            "String attributes should NOT produce lint warning. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn data_source_without_read_keyword_errors() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

aws.sts.caller_identity {}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let data_source_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("data source") && d.message.contains("read"));
        assert!(
            data_source_diag.is_some(),
            "Should error when data source is used without `read`. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn data_source_with_read_keyword_no_error() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let identity = read aws.sts.caller_identity {}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let data_source_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("data source") && d.message.contains("read"));
        assert!(
            data_source_diag.is_none(),
            "Should NOT error when data source is used with `read`. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn regular_resource_without_read_no_data_source_error() {
        let engine = test_engine();
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let bucket = aws.s3.bucket {
    name = "my-bucket"
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let data_source_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("data source"));
        assert!(
            data_source_diag.is_none(),
            "Regular resource should NOT trigger data source error. Got diagnostics: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// Create a DiagnosticEngine with reversed factory order (awscc first, aws second)
    fn test_engine_reversed() -> DiagnosticEngine {
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(carina_provider_awscc::AwsccProviderFactory),
            Box::new(carina_provider_aws::AwsProviderFactory),
        ];
        let mut schemas = HashMap::new();
        for factory in &factories {
            for schema in factory.schemas() {
                schemas.insert(schema.resource_type.clone(), schema);
            }
        }
        let schemas = Arc::new(schemas);
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
        let factories = Arc::new(factories);
        DiagnosticEngine::new(schemas, provider_names, factories)
    }

    #[test]
    fn detect_provider_aws_resource_independent_of_factory_order() {
        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let bucket = aws.s3.bucket {
    name = "my-bucket"
}"#,
        );

        let engine = test_engine();
        let engine_rev = test_engine_reversed();

        let diags_normal = engine.analyze(&doc, None);
        let diags_reversed = engine_rev.analyze(&doc, None);

        let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
        let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

        assert_eq!(
            messages_normal, messages_reversed,
            "aws.s3.bucket diagnostics should not depend on factory order.\n\
             Normal: {:?}\n\
             Reversed: {:?}",
            messages_normal, messages_reversed
        );
    }

    #[test]
    fn detect_provider_awscc_resource_independent_of_factory_order() {
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}"#,
        );

        let engine = test_engine();
        let engine_rev = test_engine_reversed();

        let diags_normal = engine.analyze(&doc, None);
        let diags_reversed = engine_rev.analyze(&doc, None);

        let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
        let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

        assert_eq!(
            messages_normal, messages_reversed,
            "awscc.ec2.vpc diagnostics should not depend on factory order.\n\
             Normal: {:?}\n\
             Reversed: {:?}",
            messages_normal, messages_reversed
        );
    }

    #[test]
    fn detect_provider_anonymous_resource_independent_of_factory_order() {
        // Anonymous resource (no let binding) — verify detection works the same
        // regardless of factory order
        let engine = test_engine();
        let engine_rev = test_engine_reversed();

        let doc = create_document(
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
    name = "test-bucket"
}"#,
        );

        let diags_normal = engine.analyze(&doc, None);
        let diags_reversed = engine_rev.analyze(&doc, None);

        let messages_normal: Vec<_> = diags_normal.iter().map(|d| &d.message).collect();
        let messages_reversed: Vec<_> = diags_reversed.iter().map(|d| &d.message).collect();

        assert_eq!(
            messages_normal, messages_reversed,
            "Diagnostics should be identical regardless of factory order.\n\
             Normal: {:?}\n\
             Reversed: {:?}",
            messages_normal, messages_reversed
        );
    }

    #[test]
    fn block_name_not_flagged_as_unknown() {
        let engine = test_engine();
        // Use operating_region (singular block_name) instead of operating_regions
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
    name = "test-ipam"
    operating_region {
        region_name = "ap-northeast-1"
    }
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let unknown = diagnostics
            .iter()
            .find(|d| d.message.contains("Unknown attribute 'operating_region'"));
        assert!(
            unknown.is_none(),
            "block_name 'operating_region' should not be flagged as unknown. Got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn block_name_mixed_syntax_error() {
        let engine = test_engine();
        // Use both operating_region and operating_regions - should error
        let doc = create_document(
            r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
    name = "test-ipam"
    operating_region {
        region_name = "ap-northeast-1"
    }
    operating_regions = [{
        region_name = "us-east-1"
    }]
}"#,
        );

        let diagnostics = engine.analyze(&doc, None);

        let mixed_error = diagnostics.iter().find(|d| {
            d.message.contains("operating_region")
                && d.message.contains("operating_regions")
                && d.message.contains("same attribute")
        });
        assert!(
            mixed_error.is_some(),
            "Should error on mixed block_name and canonical name. Got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
