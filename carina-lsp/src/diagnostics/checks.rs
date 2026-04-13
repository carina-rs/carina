//! Semantic checks: provider region, module calls, unused bindings, undefined references.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::Document;
use crate::position;
use carina_core::builtins;
use carina_core::parser::{ArgumentParameter, ParsedFile, TypeExpr};
use carina_core::resource::Value;
use carina_core::schema::{ResourceSchema, suggest_similar_name};

use super::{DiagnosticEngine, carina_diagnostic};

impl DiagnosticEngine {
    /// Check that provider blocks are not defined inside modules.
    pub(super) fn check_provider_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
        if !is_module || parsed.providers.is_empty() {
            return Vec::new();
        }

        let mut diagnostics = Vec::new();
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("provider ") {
                let col = position::leading_whitespace_chars(line);
                // Highlight "provider <name>" portion
                let end_col = trimmed
                    .find('{')
                    .map(|p| col + p as u32)
                    .unwrap_or(col + trimmed.len() as u32);
                diagnostics.push(carina_diagnostic(
                    line_idx as u32,
                    col,
                    end_col,
                    DiagnosticSeverity::ERROR,
                    "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
                ));
            }
        }

        diagnostics
    }

    /// Check provider block attributes.
    ///
    /// Runs host-side type-level validation using
    /// `ProviderFactory::provider_config_attribute_types`, then delegates to
    /// `validate_config` for any provider-specific semantic checks. Mirrors
    /// the CLI flow in `carina_core::validation::validate_provider_config`
    /// so fixes to generic DSL format validation take effect in LSP without
    /// rebuilding providers.
    pub(super) fn check_provider_region(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for provider in &parsed.providers {
            let Some(factory) = self.factories.iter().find(|f| f.name() == provider.name) else {
                continue;
            };

            // Host-side type-level validation (catches malformed namespace
            // identifiers, invalid enum values, etc.).
            let attr_types = factory.provider_config_attribute_types();
            for (attr_name, value) in &provider.attributes {
                if let Some(attr_type) = attr_types.get(attr_name)
                    && let Err(e) = attr_type.validate(value)
                    && let Some((line, col)) =
                        self.find_provider_attr_position(doc, &provider.name, attr_name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + attr_name.chars().count() as u32,
                        DiagnosticSeverity::WARNING,
                        format!("provider {}: {}: {}", provider.name, attr_name, e),
                    ));
                }
            }

            // Provider-specific validation (semantic checks not expressible
            // in the attribute type schema).
            if let Err(e) = factory.validate_config(&provider.attributes)
                && let Some((line, col)) = self.find_provider_region_position(doc, &provider.name)
            {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + 6, // "region"
                    DiagnosticSeverity::WARNING,
                    format!("provider {}: {}", provider.name, e),
                ));
            }
        }
        diagnostics
    }

    /// Check for providers that failed to load and show info-level diagnostics on the provider block.
    pub(super) fn check_unloaded_providers(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        for provider in &parsed.providers {
            let Some(reason) = self.provider_errors.get(&provider.name) else {
                continue;
            };

            // Find the provider block position
            let provider_pattern = format!("provider {}", provider.name);
            for (line_idx, line) in text.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with(&provider_pattern) {
                    let col = position::leading_whitespace_chars(line);
                    let end_col = col + trimmed.find('{').unwrap_or(trimmed.len()) as u32;
                    diagnostics.push(carina_diagnostic(
                        line_idx as u32,
                        col,
                        end_col,
                        DiagnosticSeverity::INFORMATION,
                        format!("Provider '{}' is not loaded: {}", provider.name, reason),
                    ));
                    break;
                }
            }
        }

        diagnostics
    }

    /// Find the position of the region attribute in a provider block
    pub(super) fn find_provider_region_position(
        &self,
        doc: &Document,
        provider_name: &str,
    ) -> Option<(u32, u32)> {
        self.find_provider_attr_position(doc, provider_name, "region")
    }

    /// Find the position of a named attribute in a provider block.
    pub(super) fn find_provider_attr_position(
        &self,
        doc: &Document,
        provider_name: &str,
        attr_name: &str,
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
                if trimmed.starts_with(attr_name) {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_provider = false;
                }
            }
        }
        None
    }

    /// Check module calls against imported module definitions
    pub(super) fn check_module_calls(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        base_path: &std::path::Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Build a map of imported modules: alias -> argument parameters
        let mut imported_modules: HashMap<String, Vec<ArgumentParameter>> = HashMap::new();

        for import in &parsed.imports {
            let module_path = base_path.join(&import.path);
            if let Some(module_parsed) = carina_core::module_resolver::load_module(&module_path) {
                imported_modules.insert(import.alias.clone(), module_parsed.arguments);
            }
        }

        // Check each module call
        for call in &parsed.module_calls {
            if let Some(module_args) = imported_modules.get(&call.module_name) {
                // Check for unknown parameters
                for (arg_name, arg_value) in &call.arguments {
                    let matching_arg = module_args.iter().find(|arg| &arg.name == arg_name);

                    if matching_arg.is_none() {
                        if let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                        {
                            // Find similar parameter names for suggestion
                            let suggestion = module_args
                                .iter()
                                .find(|arg| {
                                    arg.name.contains(arg_name) || arg_name.contains(&arg.name)
                                })
                                .map(|arg| format!(". Did you mean '{}'?", arg.name))
                                .unwrap_or_default();

                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                col + arg_name.len() as u32,
                                DiagnosticSeverity::WARNING,
                                format!(
                                    "Unknown parameter '{}' for module '{}'{}",
                                    arg_name, call.module_name, suggestion
                                ),
                            ));
                        }
                        continue;
                    }

                    // Type validation for known parameters
                    let arg = matching_arg.unwrap();
                    if let Some(type_error) =
                        self.validate_module_arg_type(&arg.type_expr, arg_value)
                        && let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                    {
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            col + arg_name.len() as u32,
                            DiagnosticSeverity::WARNING,
                            type_error,
                        ));
                    }
                }

                // Check for missing required parameters
                for arg in module_args {
                    if arg.default.is_none()
                        && !call.arguments.contains_key(&arg.name)
                        && let Some((line, col)) =
                            self.find_module_call_position(doc, &call.module_name)
                    {
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            col + call.module_name.len() as u32,
                            DiagnosticSeverity::ERROR,
                            format!(
                                "Missing required parameter '{}' for module '{}'",
                                arg.name, call.module_name
                            ),
                        ));
                    }
                }
            }
        }

        diagnostics
    }

    /// Validate a module argument value against its expected type.
    pub(super) fn validate_module_arg_type(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
    ) -> Option<String> {
        carina_core::validation::validate_type_expr_value(type_expr, value, &self.provider_context)
    }

    /// Find the position of a module call in the document
    pub(super) fn find_module_call_position(
        &self,
        doc: &Document,
        module_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(&pattern) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Find the position of an argument in a module call
    pub(super) fn find_module_call_arg_position(
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
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_module_call = false;
                }
            }
        }
        None
    }

    /// Check for unused `let` bindings and emit warnings.
    pub(super) fn check_unused_bindings(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let unused_bindings = carina_core::validation::check_unused_bindings(parsed);
        if unused_bindings.is_empty() {
            return Vec::new();
        }

        let text = doc.text();
        let mut diagnostics = Vec::new();

        for binding_name in &unused_bindings {
            if let Some((line, col)) = self.find_let_binding_position(&text, binding_name) {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + binding_name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Unused let binding '{}'. Consider using an anonymous resource instead.",
                        binding_name
                    ),
                ));
            }
        }

        diagnostics
    }

    /// Find the position of a `let` binding name in the source text.
    pub(super) fn find_let_binding_position(
        &self,
        text: &str,
        binding_name: &str,
    ) -> Option<(u32, u32)> {
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let name = rest[..eq_pos].trim();
                if name == binding_name {
                    // Find the column of the binding name in the original line
                    let let_byte_pos = line.find("let ").unwrap();
                    let let_char_pos = position::byte_offset_to_char_offset(line, let_byte_pos);
                    let name_col = let_char_pos + 4; // "let " is 4 chars
                    return Some((line_idx as u32, name_col));
                }
            }
        }
        None
    }

    /// Extract resource binding names from text (variables defined with `let binding_name = aws...` or `let binding_name = read aws...`)
    pub(super) fn extract_resource_bindings(&self, text: &str) -> HashSet<String> {
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

    /// Check attributes blocks for type mismatches and undefined binding references.
    pub(super) fn check_attributes_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Collect defined binding names from parsed resources
        let mut defined_bindings: HashSet<String> = HashSet::new();
        for resource in &parsed.resources {
            if let Some(ref binding_name) = resource.binding {
                defined_bindings.insert(binding_name.clone());
            }
        }

        for attr_param in &parsed.attribute_params {
            if let Some(value) = &attr_param.value {
                // Check for undefined binding references
                if let Value::ResourceRef { path } = value
                    && !defined_bindings.contains(path.binding())
                    && let Some((line, col)) =
                        self.find_attributes_value_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + path.binding().len() as u32,
                        DiagnosticSeverity::ERROR,
                        format!(
                            "Undefined resource '{}' in attributes '{}'. Define it with 'let {} = ...'",
                            path.binding(), attr_param.name, path.binding()
                        ),
                    ));
                }

                // Type validation (only when explicit type annotation is present)
                if let Some(ref type_expr) = attr_param.type_expr
                    && let Some(type_error) = self.validate_attributes_type(type_expr, value)
                    && let Some((line, col)) =
                        self.find_attributes_param_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + attr_param.name.len() as u32,
                        DiagnosticSeverity::WARNING,
                        type_error,
                    ));
                }
            }
        }

        diagnostics
    }

    /// Validate an attributes value against its declared type.
    ///
    /// Skips ResourceRef values (type is resolved at runtime), then delegates all
    /// validation to `carina_core::validation::validate_type_expr_value`.
    fn validate_attributes_type(&self, type_expr: &TypeExpr, value: &Value) -> Option<String> {
        // ResourceRef is always allowed (type is resolved at runtime)
        if matches!(value, Value::ResourceRef { .. }) {
            return None;
        }

        carina_core::validation::validate_type_expr_value(type_expr, value, &self.provider_context)
    }

    /// Find the position of an attributes parameter name in the document.
    fn find_attributes_param_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_attributes_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("attributes ") && trimmed.contains('{') {
                in_attributes_block = true;
                continue;
            }

            if in_attributes_block {
                if trimmed == "}" {
                    in_attributes_block = false;
                    continue;
                }

                // Look for "param_name:" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Find the position of the value expression in an attributes parameter line.
    fn find_attributes_value_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_attributes_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("attributes ") && trimmed.contains('{') {
                in_attributes_block = true;
                continue;
            }

            if in_attributes_block {
                if trimmed == "}" {
                    in_attributes_block = false;
                    continue;
                }

                // Look for "param_name: type = value" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    // Find the "=" and return position after it
                    if let Some(eq_byte_pos) = line.find('=') {
                        let after_eq = &line[eq_byte_pos + 1..];
                        let trimmed_after = after_eq.trim_start();
                        // Whitespace after '=' is ASCII, so byte diff == char count
                        let ws_after_eq = after_eq.len() - trimmed_after.len();
                        let value_col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                            + 1
                            + ws_after_eq as u32;
                        return Some((line_idx as u32, value_col));
                    }
                }
            }
        }
        None
    }

    /// Check for undefined resource references in attribute values
    pub(super) fn check_undefined_references(
        &self,
        text: &str,
        defined_bindings: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            // Look for patterns like "binding_name.property" after "="
            if let Some(eq_byte_pos) = line.find('=') {
                let after_eq = &line[eq_byte_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                // Whitespace after '=' is ASCII spaces, so byte diff == char count
                let whitespace_chars = after_eq.len() - after_eq_trimmed.len();

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

                    // Check if this looks like a resource reference (e.g., main_vpc.id, bucket.arn)
                    if !identifier.is_empty()
                        && !property.is_empty()
                        && identifier.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && identifier.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                    {
                        // Check if the binding is defined
                        if !defined_bindings.contains(identifier) {
                            let col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                                + 1
                                + whitespace_chars as u32;
                            diagnostics.push(carina_diagnostic(
                                line_idx as u32,
                                col,
                                col + identifier.len() as u32,
                                DiagnosticSeverity::ERROR,
                                format!(
                                    "Undefined resource: '{}'. Define it with 'let {} = aws...'",
                                    identifier, identifier
                                ),
                            ));
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Check for unknown built-in function calls in parsed resource attributes.
    pub(super) fn check_unknown_functions(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for resource in &parsed.resources {
            for value in resource.attributes.values() {
                self.collect_unknown_function_diagnostics(doc, value, &mut diagnostics);
            }
        }

        diagnostics
    }

    /// Recursively walk a Value tree to find FunctionCall nodes with unknown names.
    fn collect_unknown_function_diagnostics(
        &self,
        doc: &Document,
        value: &Value,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::FunctionCall { name, args } => {
                if !builtins::is_known_builtin(name)
                    && let Some((line, col)) = self.find_function_call_position(doc, name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + name.len() as u32,
                        DiagnosticSeverity::ERROR,
                        format!("Unknown function '{}'", name),
                    ));
                }
                // Also check nested function calls in arguments
                for arg in args {
                    self.collect_unknown_function_diagnostics(doc, arg, diagnostics);
                }
            }
            Value::List(items) => {
                for item in items {
                    self.collect_unknown_function_diagnostics(doc, item, diagnostics);
                }
            }
            Value::Map(map) => {
                for v in map.values() {
                    self.collect_unknown_function_diagnostics(doc, v, diagnostics);
                }
            }
            Value::Interpolation(parts) => {
                for part in parts {
                    if let carina_core::resource::InterpolationPart::Expr(expr) = part {
                        self.collect_unknown_function_diagnostics(doc, expr, diagnostics);
                    }
                }
            }
            _ => {}
        }
    }

    /// Find the position of a function call name in the document text.
    fn find_function_call_position(&self, doc: &Document, func_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{}(", func_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(&pattern) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Check for unknown attributes on resource references (typo detection).
    ///
    /// When a ResourceRef like `igw.internet_gateway_idd` references an attribute
    /// that doesn't exist in the referenced resource's schema, emit a warning
    /// with a "did you mean" suggestion if a similar attribute exists.
    pub(super) fn check_resource_ref_attributes(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        binding_schema_map: &HashMap<String, ResourceSchema>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for resource in &parsed.resources {
            for (attr_name, attr_value) in &resource.attributes {
                if attr_name.starts_with('_') {
                    continue;
                }
                self.collect_ref_attr_diagnostics(
                    doc,
                    attr_value,
                    binding_schema_map,
                    &mut diagnostics,
                );
            }
        }

        // Also check module call arguments
        for call in &parsed.module_calls {
            for value in call.arguments.values() {
                self.collect_ref_attr_diagnostics(doc, value, binding_schema_map, &mut diagnostics);
            }
        }

        // Also check attribute parameter values
        for attr_param in &parsed.attribute_params {
            if let Some(value) = &attr_param.value {
                self.collect_ref_attr_diagnostics(doc, value, binding_schema_map, &mut diagnostics);
            }
        }

        diagnostics
    }

    /// Recursively check ResourceRef values for unknown attributes.
    fn collect_ref_attr_diagnostics(
        &self,
        doc: &Document,
        value: &Value,
        binding_schema_map: &HashMap<String, ResourceSchema>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let Some(ref_schema) = binding_schema_map.get(binding_name) else {
                    return;
                };
                if ref_schema.attributes.contains_key(attribute_name) {
                    return;
                }
                // Attribute not found - build "did you mean" suggestion
                let known_attrs: Vec<&str> =
                    ref_schema.attributes.keys().map(|s| s.as_str()).collect();
                let suggestion = suggest_similar_name(attribute_name, &known_attrs)
                    .map(|s| format!(" Did you mean '{}'?", s))
                    .unwrap_or_default();

                let ref_text = format!("{}.{}", binding_name, attribute_name);
                if let Some((line, col)) = self.find_ref_value_position(doc, &ref_text) {
                    // Highlight just the attribute part (after the dot)
                    let attr_col = col + binding_name.len() as u32 + 1; // +1 for the dot
                    diagnostics.push(carina_diagnostic(
                        line,
                        attr_col,
                        attr_col + attribute_name.len() as u32,
                        DiagnosticSeverity::WARNING,
                        format!(
                            "Unknown attribute '{}' on '{}' (type '{}'){}",
                            attribute_name, binding_name, ref_schema.resource_type, suggestion,
                        ),
                    ));
                }
            }
            Value::List(items) => {
                for item in items {
                    self.collect_ref_attr_diagnostics(doc, item, binding_schema_map, diagnostics);
                }
            }
            Value::Map(map) => {
                for v in map.values() {
                    self.collect_ref_attr_diagnostics(doc, v, binding_schema_map, diagnostics);
                }
            }
            Value::Interpolation(parts) => {
                for part in parts {
                    if let carina_core::resource::InterpolationPart::Expr(expr) = part {
                        self.collect_ref_attr_diagnostics(
                            doc,
                            expr,
                            binding_schema_map,
                            diagnostics,
                        );
                    }
                }
            }
            Value::FunctionCall { args, .. } => {
                for arg in args {
                    self.collect_ref_attr_diagnostics(doc, arg, binding_schema_map, diagnostics);
                }
            }
            _ => {}
        }
    }

    /// Find the position of a resource reference value (e.g., "igw.internet_gateway_id") in the document.
    fn find_ref_value_position(&self, doc: &Document, ref_text: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(ref_text) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }
}
