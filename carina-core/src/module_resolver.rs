//! Module Resolver - Resolve module imports and instantiations
//!
//! This module handles:
//! - Resolving import paths to module definitions
//! - Detecting circular dependencies between modules
//! - Validating module argument parameters
//! - Expanding module calls into resources

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::parser::{ImportStatement, ModuleCall, ParseError, ParsedFile};
use crate::resource::{LifecycleConfig, Resource, ResourceId, Value};

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("Module not found: {0}")]
    NotFound(String),

    #[error("Circular import detected: {0}")]
    CircularImport(String),

    #[error("Missing required argument '{argument}' for module '{module}'")]
    MissingArgument { module: String, argument: String },

    #[error("Invalid argument type for '{argument}' in module '{module}': expected {expected}")]
    InvalidArgumentType {
        module: String,
        argument: String,
        expected: String,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("Unknown argument '{argument}' for module '{module}'")]
    UnknownArgument { module: String, argument: String },

    #[error("Unknown module: {0}")]
    UnknownModule(String),

    #[error(
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    )]
    ProviderInModule,
}

/// Context for module resolution
pub struct ModuleResolver {
    /// Base directory for resolving relative imports
    base_dir: PathBuf,
    /// Cache of loaded modules: path -> ParsedFile
    module_cache: HashMap<PathBuf, ParsedFile>,
    /// Currently resolving modules (for cycle detection)
    resolving: HashSet<PathBuf>,
    /// Imported module definitions by alias
    imported_modules: HashMap<String, ParsedFile>,
}

impl ModuleResolver {
    /// Create a new resolver with the given base directory
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            module_cache: HashMap::new(),
            resolving: HashSet::new(),
            imported_modules: HashMap::new(),
        }
    }

    /// Load and cache a module from a file or directory path
    pub fn load_module(&mut self, path: &str) -> Result<ParsedFile, ModuleError> {
        let full_path = self.resolve_path(path);

        // Canonicalize for consistent cycle detection and caching.
        // For directories, canonicalize directly. For files, try with .crn extension.
        let canonical = if full_path.exists() {
            full_path.canonicalize()?
        } else if full_path.with_extension("crn").exists() {
            full_path.with_extension("crn").canonicalize()?
        } else {
            full_path.clone()
        };

        // Check for circular import
        if self.resolving.contains(&canonical) {
            return Err(ModuleError::CircularImport(path.to_string()));
        }

        // Check cache
        if let Some(module) = self.module_cache.get(&canonical) {
            return Ok(module.clone());
        }

        // Mark as resolving
        self.resolving.insert(canonical.clone());

        // Load module: directory or single file
        let load_result = if full_path.is_dir() {
            self.load_directory_module(&full_path)
        } else {
            fs::read_to_string(&full_path)
                .map_err(ModuleError::from)
                .and_then(|content| crate::parser::parse(&content).map_err(ModuleError::from))
        };
        let mut parsed = match load_result {
            Ok(parsed) => parsed,
            Err(e) => {
                self.resolving.remove(&canonical);
                return Err(e);
            }
        };

        // Verify it's a module (has arguments or attributes)
        if parsed.arguments.is_empty() && parsed.attribute_params.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::NotFound(path.to_string()));
        }

        // Reject provider blocks inside modules
        if !parsed.providers.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ProviderInModule);
        }

        // Recursively resolve nested module imports within this module.
        // The module's base directory is used for resolving its relative imports.
        let module_base_dir = if full_path.is_dir() {
            full_path.clone()
        } else {
            full_path.parent().unwrap_or(&full_path).to_path_buf()
        };
        if let Err(e) = self.resolve_nested_modules(&mut parsed, &module_base_dir) {
            self.resolving.remove(&canonical);
            return Err(e);
        }

        // Remove from resolving set
        self.resolving.remove(&canonical);

        // Cache the module
        self.module_cache.insert(canonical, parsed.clone());

        Ok(parsed)
    }

    /// Load all .crn files from a directory and merge them into a single ParsedFile
    fn load_directory_module(&self, dir_path: &Path) -> Result<ParsedFile, ModuleError> {
        let mut merged = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
        };

        // Read all .crn files in the directory
        let mut crn_files: Vec<_> = fs::read_dir(dir_path)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "crn"))
            .collect();

        // Sort for consistent ordering
        crn_files.sort_by_key(|e| e.path());

        for entry in crn_files {
            let file_path = entry.path();
            let content = fs::read_to_string(&file_path)?;
            let parsed = crate::parser::parse(&content)?;

            // Merge all fields
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
        }

        Ok(merged)
    }

    /// Resolve a relative path to an absolute path
    fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    /// Process imports and store imported modules
    pub fn process_imports(&mut self, imports: &[ImportStatement]) -> Result<(), ModuleError> {
        for import in imports {
            let module = self.load_module(&import.path)?;
            self.imported_modules.insert(import.alias.clone(), module);
        }
        Ok(())
    }

    /// Resolve nested module imports within a parsed module.
    ///
    /// This processes the module's own imports and module_calls, expanding any
    /// nested modules recursively. Cycle detection is handled by the `resolving`
    /// set in `load_module()`.
    fn resolve_nested_modules(
        &mut self,
        parsed: &mut ParsedFile,
        base_dir: &Path,
    ) -> Result<(), ModuleError> {
        if parsed.imports.is_empty() || parsed.module_calls.is_empty() {
            return Ok(());
        }

        // Save and temporarily replace the base_dir and imported_modules
        let original_base_dir = std::mem::replace(&mut self.base_dir, base_dir.to_path_buf());
        let original_imported = std::mem::take(&mut self.imported_modules);

        // Process the module's own imports
        let imports = parsed.imports.clone();
        let result = self.process_imports(&imports);

        if let Err(e) = result {
            // Restore state on error
            self.base_dir = original_base_dir;
            self.imported_modules = original_imported;
            return Err(e);
        }

        // Expand the module's own module_calls
        let module_calls = parsed.module_calls.clone();
        for call in &module_calls {
            let instance_prefix = call
                .binding_name
                .as_ref()
                .cloned()
                .unwrap_or_else(|| call.module_name.clone());

            match self.expand_module_call(call, &instance_prefix) {
                Ok(expanded) => parsed.resources.extend(expanded),
                Err(e) => {
                    self.base_dir = original_base_dir;
                    self.imported_modules = original_imported;
                    return Err(e);
                }
            }
        }

        // Restore original state
        self.base_dir = original_base_dir;
        self.imported_modules = original_imported;

        Ok(())
    }

    /// Get an imported module by alias
    pub fn get_module(&self, alias: &str) -> Option<&ParsedFile> {
        self.imported_modules.get(alias)
    }

    /// Expand a module call into resources.
    ///
    /// If the module defines `attributes` and the call has a `binding_name`,
    /// a virtual resource is created to expose the module's attribute values.
    /// The virtual resource has `_virtual = "true"` and is skipped by the differ.
    pub fn expand_module_call(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
    ) -> Result<Vec<Resource>, ModuleError> {
        let module = self
            .imported_modules
            .get(&call.module_name)
            .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?;

        // Validate required arguments
        for arg in &module.arguments {
            if arg.default.is_none() && !call.arguments.contains_key(&arg.name) {
                return Err(ModuleError::MissingArgument {
                    module: call.module_name.clone(),
                    argument: arg.name.clone(),
                });
            }
        }

        // Validate no unknown arguments
        let declared_arg_names: HashSet<&str> =
            module.arguments.iter().map(|a| a.name.as_str()).collect();
        for arg_name in call.arguments.keys() {
            if !declared_arg_names.contains(arg_name.as_str()) {
                return Err(ModuleError::UnknownArgument {
                    module: call.module_name.clone(),
                    argument: arg_name.clone(),
                });
            }
        }

        // Build argument value map
        let mut argument_values: HashMap<String, Value> = HashMap::new();
        for arg in &module.arguments {
            let value = call
                .arguments
                .get(&arg.name)
                .cloned()
                .or_else(|| arg.default.clone())
                .unwrap();
            argument_values.insert(arg.name.clone(), value);
        }

        // Collect intra-module binding names so we can rewrite ResourceRefs
        let intra_module_bindings: HashSet<String> = module
            .resources
            .iter()
            .filter_map(|r| {
                r.attributes.get("_binding").and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
            })
            .collect();

        // Expand resources with substituted values
        let mut expanded_resources = Vec::new();
        for resource in &module.resources {
            let mut new_resource = resource.clone();

            // Prefix the resource name with instance path (dot-separated)
            let new_name = format!("{}.{}", instance_prefix, new_resource.id.name);
            new_resource.id = ResourceId::with_provider(
                &new_resource.id.provider,
                &new_resource.id.resource_type,
                new_name.clone(),
            );

            // Rewrite _binding with instance path (dot-separated)
            if let Some(Value::String(binding)) = new_resource.attributes.get("_binding") {
                let prefixed = format!("{}.{}", instance_prefix, binding);
                new_resource
                    .attributes
                    .insert("_binding".to_string(), Value::String(prefixed));
            }

            // Add module source info
            new_resource.attributes.insert(
                "_module".to_string(),
                Value::String(call.module_name.clone()),
            );
            new_resource.attributes.insert(
                "_module_instance".to_string(),
                Value::String(instance_prefix.to_string()),
            );

            // Rewrite intra-module ResourceRefs BEFORE substituting inputs.
            // This ensures that caller-provided ResourceRef values (which may
            // coincidentally share a binding name with a module-internal binding)
            // are not incorrectly prefixed.
            let mut substituted_attrs = HashMap::new();
            for (key, value) in &new_resource.attributes {
                let rewritten =
                    rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                let substituted = substitute_arguments(&rewritten, &argument_values);
                substituted_attrs.insert(key.clone(), substituted);
            }
            new_resource.attributes = substituted_attrs;

            expanded_resources.push(new_resource);
        }

        // Create a virtual resource if the module has attributes and the call has a binding
        if !module.attribute_params.is_empty()
            && let Some(binding_name) = &call.binding_name
        {
            let mut virtual_attrs: HashMap<String, Value> = HashMap::new();
            virtual_attrs.insert("_binding".to_string(), Value::String(binding_name.clone()));
            virtual_attrs.insert("_virtual".to_string(), Value::String("true".to_string()));
            virtual_attrs.insert(
                "_module".to_string(),
                Value::String(call.module_name.clone()),
            );
            virtual_attrs.insert(
                "_module_instance".to_string(),
                Value::String(instance_prefix.to_string()),
            );

            // Copy attribute values from the module definition
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    // Rewrite intra-module refs and substitute arguments
                    let rewritten =
                        rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                    let substituted = substitute_arguments(&rewritten, &argument_values);
                    virtual_attrs.insert(attr_param.name.clone(), substituted);
                }
            }

            let virtual_resource = Resource {
                id: ResourceId::new("_virtual", binding_name),
                attributes: virtual_attrs,
                read_only: false,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
            };
            expanded_resources.push(virtual_resource);
        }

        Ok(expanded_resources)
    }
}

/// Substitute arguments references with actual values.
///
/// Argument parameter names are registered as lexical bindings in the parser,
/// so they appear as `ResourceRef { binding_name: "<param_name>", attribute_name: ... }`.
/// We match when `binding_name` is one of the argument keys.
fn substitute_arguments(value: &Value, arguments: &HashMap<String, Value>) -> Value {
    match value {
        Value::ResourceRef { binding_name, .. } if arguments.contains_key(binding_name) => {
            arguments
                .get(binding_name)
                .cloned()
                .unwrap_or_else(|| value.clone())
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| substitute_arguments(v, arguments))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_arguments(v, arguments)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let substituted_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        InterpolationPart::Expr(substitute_arguments(v, arguments))
                    }
                    other => other.clone(),
                })
                .collect();
            Value::Interpolation(substituted_parts)
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| substitute_arguments(v, arguments))
                .collect(),
        },
        _ => value.clone(),
    }
}

/// Rewrite intra-module ResourceRef binding names with instance path.
///
/// When a ResourceRef's binding_name matches one of the module's own bindings,
/// prefix it with dot notation so that each module instance has isolated references.
fn rewrite_intra_module_refs(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
) -> Value {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
            field_path,
        } if intra_module_bindings.contains(binding_name) => Value::ResourceRef {
            binding_name: format!("{}.{}", instance_prefix, binding_name),
            attribute_name: attribute_name.clone(),
            field_path: field_path.clone(),
        },
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    )
                })
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let rewritten_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => InterpolationPart::Expr(
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    ),
                    other => other.clone(),
                })
                .collect();
            Value::Interpolation(rewritten_parts)
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        },
        _ => value.clone(),
    }
}

/// Resolve all modules in a parsed file
pub fn resolve_modules(parsed: &mut ParsedFile, base_dir: &Path) -> Result<(), ModuleError> {
    let mut resolver = ModuleResolver::new(base_dir);

    // Process imports
    resolver.process_imports(&parsed.imports)?;

    // Expand module calls
    for call in &parsed.module_calls {
        let instance_prefix = call
            .binding_name
            .as_ref()
            .cloned()
            .unwrap_or_else(|| call.module_name.clone());

        let expanded = resolver.expand_module_call(call, &instance_prefix)?;
        parsed.resources.extend(expanded);
    }

    Ok(())
}

/// Get parsed file info for display (supports both module definitions and root configs)
pub fn get_parsed_file(path: &Path) -> Result<ParsedFile, ModuleError> {
    let content = fs::read_to_string(path)?;
    let parsed = crate::parser::parse(&content)?;
    Ok(parsed)
}

/// Load a module from a file or directory path.
///
/// For directories, tries `main.crn` first, then falls back to merging all `.crn` files.
/// Returns `None` if the path cannot be read/parsed, or if the directory contains
/// no module definitions (no inputs or outputs).
pub fn load_module(path: &Path) -> Option<ParsedFile> {
    if path.is_dir() {
        let main_path = path.join("main.crn");
        if main_path.exists() {
            let content = fs::read_to_string(&main_path).ok()?;
            crate::parser::parse(&content).ok()
        } else {
            load_directory_module(path)
        }
    } else {
        let content = fs::read_to_string(path).ok()?;
        crate::parser::parse(&content).ok()
    }
}

/// Load all `.crn` files from a directory and merge them into a single `ParsedFile`.
///
/// Returns `None` if no module definitions (arguments/attributes) are found.
pub fn load_directory_module(dir_path: &Path) -> Option<ParsedFile> {
    let entries = fs::read_dir(dir_path).ok()?;
    let mut merged = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
            && let Ok(parsed) = crate::parser::parse(&content)
        {
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
        }
    }

    if merged.arguments.is_empty() && merged.attribute_params.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Derive the module name from a file or directory path.
///
/// Examples:
/// - `modules/web_tier/` → `web_tier` (directory)
/// - `modules/web_tier/main.crn` → `web_tier` (directory-based)
/// - `modules/web_tier.crn` → `web_tier` (file-based)
/// - `web_tier.crn` → `web_tier`
pub fn derive_module_name(path: &Path) -> String {
    if path.is_dir() {
        return path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
    }

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // If file is named main.crn, use the parent directory name
    if file_stem == "main"
        && let Some(parent) = path.parent()
        && let Some(parent_name) = parent.file_name()
        && let Some(name) = parent_name.to_str()
    {
        return name.to_string();
    }

    file_stem.to_string()
}

/// Load a module from a directory by reading all `.crn` files.
///
/// Unlike [`load_directory_module`], this returns a `Result` with descriptive error messages
/// and does not check for module definitions (inputs/outputs).
pub fn load_module_from_directory(dir: &Path) -> Result<ParsedFile, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut merged = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
    };

    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "crn") {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            let parsed = crate::parser::parse(&content)
                .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
        }
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ArgumentParameter, TypeExpr};
    use crate::resource::LifecycleConfig;

    fn create_test_module() -> ParsedFile {
        ParsedFile {
            providers: vec![],
            resources: vec![Resource {
                id: ResourceId::new("security_group", "sg"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert("name".to_string(), Value::String("sg".to_string()));
                    attrs.insert(
                        "vpc_id".to_string(),
                        Value::ResourceRef {
                            binding_name: "vpc_id".to_string(),
                            attribute_name: String::new(),
                            field_path: vec![],
                        },
                    );
                    attrs.insert(
                        "_type".to_string(),
                        Value::String("aws.security_group".to_string()),
                    );
                    attrs
                },
                read_only: false,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
            }],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![
                ArgumentParameter {
                    name: "vpc_id".to_string(),
                    type_expr: TypeExpr::String,
                    default: None,
                    description: None,
                },
                ArgumentParameter {
                    name: "enable_flag".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                    description: None,
                },
            ],
            attribute_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
        }
    }

    #[test]
    fn test_substitute_arguments() {
        let mut inputs = HashMap::new();
        inputs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));

        // Argument params are lexically scoped: binding_name is the param name itself
        let value = Value::ResourceRef {
            binding_name: "vpc_id".to_string(),
            attribute_name: String::new(),
            field_path: vec![],
        };
        let result = substitute_arguments(&value, &inputs);

        assert_eq!(result, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_substitute_arguments_nested() {
        let mut inputs = HashMap::new();
        inputs.insert("port".to_string(), Value::Int(8080));

        let value = Value::List(vec![
            Value::ResourceRef {
                binding_name: "port".to_string(),
                attribute_name: String::new(),
                field_path: vec![],
            },
            Value::Int(443),
        ]);
        let result = substitute_arguments(&value, &inputs);

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::Int(8080));
                assert_eq!(items[1], Value::Int(443));
            }
            _ => panic!("Expected list"),
        }
    }

    #[test]
    fn test_expand_module_call() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("test_module".to_string(), create_test_module());
            r
        };

        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("my_instance".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("vpc_id".to_string(), Value::String("vpc-456".to_string()));
                args
            },
        };

        let expanded = resolver.expand_module_call(&call, "my_instance").unwrap();
        assert_eq!(expanded.len(), 1);

        let sg = &expanded[0];
        assert_eq!(sg.id.name, "my_instance.sg");
        assert_eq!(
            sg.attributes.get("vpc_id"),
            Some(&Value::String("vpc-456".to_string()))
        );
        assert_eq!(
            sg.attributes.get("_module"),
            Some(&Value::String("test_module".to_string()))
        );
    }

    /// Module with two resources where one references the other via _binding / ResourceRef.
    fn create_module_with_intra_refs() -> ParsedFile {
        ParsedFile {
            providers: vec![],
            resources: vec![
                Resource {
                    id: ResourceId::new("ec2.vpc", "main_vpc"),
                    attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert("_binding".to_string(), Value::String("vpc".to_string()));
                        attrs.insert(
                            "cidr_block".to_string(),
                            Value::ResourceRef {
                                binding_name: "cidr".to_string(),
                                attribute_name: String::new(),
                                field_path: vec![],
                            },
                        );
                        attrs
                    },
                    read_only: false,
                    lifecycle: LifecycleConfig::default(),
                    prefixes: HashMap::new(),
                },
                Resource {
                    id: ResourceId::new("ec2.subnet", "sub"),
                    attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert("_binding".to_string(), Value::String("subnet".to_string()));
                        attrs.insert(
                            "vpc_id".to_string(),
                            Value::ResourceRef {
                                binding_name: "vpc".to_string(),
                                attribute_name: "id".to_string(),
                                field_path: vec![],
                            },
                        );
                        attrs
                    },
                    read_only: false,
                    lifecycle: LifecycleConfig::default(),
                    prefixes: HashMap::new(),
                },
            ],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![ArgumentParameter {
                name: "cidr".to_string(),
                type_expr: TypeExpr::String,
                default: None,
                description: None,
            }],
            attribute_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
        }
    }

    #[test]
    fn test_multiple_module_instances_no_collision() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("net".to_string(), create_module_with_intra_refs());
            r
        };

        let call_a = ModuleCall {
            module_name: "net".to_string(),
            binding_name: Some("prod".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("cidr".to_string(), Value::String("10.0.0.0/16".to_string()));
                args
            },
        };
        let call_b = ModuleCall {
            module_name: "net".to_string(),
            binding_name: Some("staging".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("cidr".to_string(), Value::String("10.1.0.0/16".to_string()));
                args
            },
        };

        let expanded_a = resolver.expand_module_call(&call_a, "prod").unwrap();
        let expanded_b = resolver.expand_module_call(&call_b, "staging").unwrap();

        // _binding must be prefixed so they don't collide (using dot notation)
        assert_eq!(
            expanded_a[0].attributes.get("_binding"),
            Some(&Value::String("prod.vpc".to_string())),
            "Instance A vpc _binding should use dot path"
        );
        assert_eq!(
            expanded_a[1].attributes.get("_binding"),
            Some(&Value::String("prod.subnet".to_string())),
            "Instance A subnet _binding should use dot path"
        );
        assert_eq!(
            expanded_b[0].attributes.get("_binding"),
            Some(&Value::String("staging.vpc".to_string())),
            "Instance B vpc _binding should use dot path"
        );
        assert_eq!(
            expanded_b[1].attributes.get("_binding"),
            Some(&Value::String("staging.subnet".to_string())),
            "Instance B subnet _binding should use dot path"
        );

        // Intra-module ResourceRef must point to the dot-path binding
        assert_eq!(
            expanded_a[1].attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "prod.vpc".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            }),
            "Instance A subnet should reference prod.vpc, not bare vpc"
        );
        assert_eq!(
            expanded_b[1].attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "staging.vpc".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            }),
            "Instance B subnet should reference staging.vpc, not bare vpc"
        );

        // Resource names should also be distinct (dot notation)
        assert_eq!(expanded_a[0].id.name, "prod.main_vpc");
        assert_eq!(expanded_b[0].id.name, "staging.main_vpc");
    }

    /// Module with an attributes block that exposes a security_group binding.
    fn create_module_with_attributes() -> ParsedFile {
        use crate::parser::AttributeParameter;

        ParsedFile {
            providers: vec![],
            resources: vec![Resource {
                id: ResourceId::new("security_group", "sg"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert("name".to_string(), Value::String("sg".to_string()));
                    attrs.insert("_binding".to_string(), Value::String("sg".to_string()));
                    attrs.insert(
                        "_type".to_string(),
                        Value::String("aws.security_group".to_string()),
                    );
                    attrs
                },
                read_only: false,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
            }],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![AttributeParameter {
                name: "security_group".to_string(),
                type_expr: None,
                value: Some(Value::ResourceRef {
                    binding_name: "sg".to_string(),
                    attribute_name: "id".to_string(),
                    field_path: vec![],
                }),
            }],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
        }
    }

    #[test]
    fn test_expand_module_call_creates_virtual_resource() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("web_tier".to_string(), create_module_with_attributes());
            r
        };

        let call = ModuleCall {
            module_name: "web_tier".to_string(),
            binding_name: Some("web".to_string()),
            arguments: HashMap::new(),
        };

        let expanded = resolver.expand_module_call(&call, "web").unwrap();
        // 1 real resource + 1 virtual resource
        assert_eq!(expanded.len(), 2);

        // Find the virtual resource
        let virtual_res = expanded
            .iter()
            .find(|r| {
                r.attributes
                    .get("_virtual")
                    .is_some_and(|v| matches!(v, Value::String(s) if s == "true"))
            })
            .expect("Virtual resource should exist");

        assert_eq!(
            virtual_res.attributes.get("_binding"),
            Some(&Value::String("web".to_string()))
        );
        // The security_group attribute should be a rewritten ResourceRef
        // pointing to the dot-path binding (web.sg)
        assert_eq!(
            virtual_res.attributes.get("security_group"),
            Some(&Value::ResourceRef {
                binding_name: "web.sg".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            })
        );
    }

    #[test]
    fn test_expand_module_call_without_binding_no_virtual() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("web_tier".to_string(), create_module_with_attributes());
            r
        };

        // Module call without binding_name
        let call = ModuleCall {
            module_name: "web_tier".to_string(),
            binding_name: None,
            arguments: HashMap::new(),
        };

        let expanded = resolver.expand_module_call(&call, "web_tier").unwrap();
        // Only real resources, no virtual
        let virtual_count = expanded
            .iter()
            .filter(|r| {
                r.attributes
                    .get("_virtual")
                    .is_some_and(|v| matches!(v, Value::String(s) if s == "true"))
            })
            .count();
        assert_eq!(virtual_count, 0);
    }

    #[test]
    fn test_missing_required_argument() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("test_module".to_string(), create_test_module());
            r
        };

        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("my_instance".to_string()),
            arguments: HashMap::new(), // Missing vpc_id
        };

        let result = resolver.expand_module_call(&call, "my_instance");
        assert!(matches!(result, Err(ModuleError::MissingArgument { .. })));
    }

    #[test]
    fn test_expand_module_call_uses_dot_path_addressing() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("test_module".to_string(), create_test_module());
            r
        };

        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("my_instance".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("vpc_id".to_string(), Value::String("vpc-456".to_string()));
                args
            },
        };

        let expanded = resolver.expand_module_call(&call, "my_instance").unwrap();
        assert_eq!(expanded.len(), 1);

        let sg = &expanded[0];
        // Resource name should use dot notation, not underscore
        assert_eq!(sg.id.name, "my_instance.sg");
    }

    #[test]
    fn test_module_dot_path_bindings_and_refs() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("net".to_string(), create_module_with_intra_refs());
            r
        };

        let call = ModuleCall {
            module_name: "net".to_string(),
            binding_name: Some("prod".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("cidr".to_string(), Value::String("10.0.0.0/16".to_string()));
                args
            },
        };

        let expanded = resolver.expand_module_call(&call, "prod").unwrap();

        // Resource names should use dot notation
        assert_eq!(expanded[0].id.name, "prod.main_vpc");
        assert_eq!(expanded[1].id.name, "prod.sub");

        // _binding should use dot notation
        assert_eq!(
            expanded[0].attributes.get("_binding"),
            Some(&Value::String("prod.vpc".to_string())),
        );
        assert_eq!(
            expanded[1].attributes.get("_binding"),
            Some(&Value::String("prod.subnet".to_string())),
        );

        // Intra-module ResourceRef should use dot notation
        assert_eq!(
            expanded[1].attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "prod.vpc".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            }),
        );
    }

    #[test]
    fn test_module_virtual_resource_dot_path_refs() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("web_tier".to_string(), create_module_with_attributes());
            r
        };

        let call = ModuleCall {
            module_name: "web_tier".to_string(),
            binding_name: Some("web".to_string()),
            arguments: HashMap::new(),
        };

        let expanded = resolver.expand_module_call(&call, "web").unwrap();

        let virtual_res = expanded
            .iter()
            .find(|r| {
                r.attributes
                    .get("_virtual")
                    .is_some_and(|v| matches!(v, Value::String(s) if s == "true"))
            })
            .expect("Virtual resource should exist");

        // The security_group attribute should reference dot-notation binding
        assert_eq!(
            virtual_res.attributes.get("security_group"),
            Some(&Value::ResourceRef {
                binding_name: "web.sg".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            })
        );
    }

    #[test]
    fn test_substitute_arguments_interpolation() {
        use crate::resource::InterpolationPart;

        let mut inputs = HashMap::new();
        inputs.insert("env_name".to_string(), Value::String("dev".to_string()));

        // Interpolation like "prefix-${env_name}-suffix" where env_name is a module argument
        let value = Value::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(Value::ResourceRef {
                binding_name: "env_name".to_string(),
                attribute_name: String::new(),
                field_path: vec![],
            }),
            InterpolationPart::Literal("-suffix".to_string()),
        ]);
        let result = substitute_arguments(&value, &inputs);

        // After substitution, the ResourceRef should be replaced with the argument value
        assert_eq!(
            result,
            Value::Interpolation(vec![
                InterpolationPart::Literal("prefix-".to_string()),
                InterpolationPart::Expr(Value::String("dev".to_string())),
                InterpolationPart::Literal("-suffix".to_string()),
            ])
        );
    }

    #[test]
    fn test_unknown_argument_rejected() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("test_module".to_string(), create_test_module());
            r
        };

        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("my_instance".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("vpc_id".to_string(), Value::String("vpc-456".to_string()));
                // Unknown argument: not declared in the module
                args.insert(
                    "unknown_arg".to_string(),
                    Value::String("should-fail".to_string()),
                );
                args
            },
        };

        let result = resolver.expand_module_call(&call, "my_instance");
        assert!(
            matches!(result, Err(ModuleError::UnknownArgument { .. })),
            "Expected UnknownArgument error, got {:?}",
            result
        );
    }

    #[test]
    fn test_substitute_arguments_function_call() {
        let mut inputs = HashMap::new();
        inputs.insert("cidr".to_string(), Value::String("10.0.0.0/16".to_string()));

        // FunctionCall like cidr_subnet(cidr, 8, 0) where cidr is a module argument
        let value = Value::FunctionCall {
            name: "cidr_subnet".to_string(),
            args: vec![
                Value::ResourceRef {
                    binding_name: "cidr".to_string(),
                    attribute_name: String::new(),
                    field_path: vec![],
                },
                Value::Int(8),
                Value::Int(0),
            ],
        };
        let result = substitute_arguments(&value, &inputs);

        assert_eq!(
            result,
            Value::FunctionCall {
                name: "cidr_subnet".to_string(),
                args: vec![
                    Value::String("10.0.0.0/16".to_string()),
                    Value::Int(8),
                    Value::Int(0),
                ],
            }
        );
    }

    /// Module with interpolation in resource attributes to test argument substitution
    fn create_module_with_interpolation() -> ParsedFile {
        use crate::resource::InterpolationPart;

        ParsedFile {
            providers: vec![],
            resources: vec![Resource {
                id: ResourceId::new("ec2.vpc", "vpc"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert("_binding".to_string(), Value::String("vpc".to_string()));
                    attrs.insert(
                        "cidr_block".to_string(),
                        Value::ResourceRef {
                            binding_name: "cidr_block".to_string(),
                            attribute_name: String::new(),
                            field_path: vec![],
                        },
                    );
                    attrs.insert(
                        "name".to_string(),
                        Value::Interpolation(vec![
                            InterpolationPart::Literal("test-".to_string()),
                            InterpolationPart::Expr(Value::ResourceRef {
                                binding_name: "env_name".to_string(),
                                attribute_name: String::new(),
                                field_path: vec![],
                            }),
                        ]),
                    );
                    attrs.insert(
                        "env".to_string(),
                        Value::ResourceRef {
                            binding_name: "env_name".to_string(),
                            attribute_name: String::new(),
                            field_path: vec![],
                        },
                    );
                    attrs
                },
                read_only: false,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
            }],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![
                ArgumentParameter {
                    name: "cidr_block".to_string(),
                    type_expr: TypeExpr::String,
                    default: None,
                    description: None,
                },
                ArgumentParameter {
                    name: "env_name".to_string(),
                    type_expr: TypeExpr::String,
                    default: None,
                    description: None,
                },
            ],
            attribute_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
        }
    }

    #[test]
    fn test_expand_module_call_with_interpolation() {
        use crate::resource::InterpolationPart;

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("vpc_mod".to_string(), create_module_with_interpolation());
            r
        };

        let call = ModuleCall {
            module_name: "vpc_mod".to_string(),
            binding_name: Some("dev_vpc".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "cidr_block".to_string(),
                    Value::String("10.0.0.0/16".to_string()),
                );
                args.insert("env_name".to_string(), Value::String("dev".to_string()));
                args
            },
        };

        let expanded = resolver.expand_module_call(&call, "dev_vpc").unwrap();
        assert_eq!(expanded.len(), 1);

        let vpc = &expanded[0];

        // Simple argument substitution should work
        assert_eq!(
            vpc.attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            vpc.attributes.get("env"),
            Some(&Value::String("dev".to_string()))
        );

        // Interpolation with argument should have the argument value substituted
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("test-".to_string()),
                InterpolationPart::Expr(Value::String("dev".to_string())),
            ]))
        );
    }

    #[test]
    fn test_nested_module_two_level() {
        // outer_module imports inner_module
        // resolve_modules on root.crn should expand both levels
        let fixtures_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
        let content = fs::read_to_string(fixtures_dir.join("root.crn")).unwrap();
        let mut parsed = crate::parser::parse(&content).unwrap();

        resolve_modules(&mut parsed, &fixtures_dir).unwrap();

        // Should have resources from both inner_module (vpc) and outer_module (sg)
        let resource_types: Vec<&str> = parsed
            .resources
            .iter()
            .filter_map(|r| {
                r.attributes.get("_type").and_then(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
            })
            .collect();

        assert!(
            resource_types.iter().any(|t| t.contains("vpc")),
            "Should contain VPC resource from inner module, got: {:?}",
            resource_types
        );
        assert!(
            resource_types.iter().any(|t| t.contains("security_group")),
            "Should contain security group from outer module, got: {:?}",
            resource_types
        );
    }

    #[test]
    fn test_nested_module_three_level() {
        // root -> middle_module -> inner_module
        let fixtures_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
        let content = fs::read_to_string(fixtures_dir.join("root_three_level.crn")).unwrap();
        let mut parsed = crate::parser::parse(&content).unwrap();

        resolve_modules(&mut parsed, &fixtures_dir).unwrap();

        // Should have the VPC resource from inner_module (through middle_module)
        let resource_types: Vec<&str> = parsed
            .resources
            .iter()
            .filter_map(|r| {
                r.attributes.get("_type").and_then(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
            })
            .collect();

        assert!(
            resource_types.iter().any(|t| t.contains("vpc")),
            "Should contain VPC resource from inner module (3 levels deep), got: {:?}",
            resource_types
        );
    }

    #[test]
    fn test_nested_module_cycle_detection() {
        // cycle_a imports cycle_b, cycle_b imports cycle_a
        let fixtures_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
        let content = fs::read_to_string(fixtures_dir.join("root_cycle.crn")).unwrap();
        let mut parsed = crate::parser::parse(&content).unwrap();

        let result = resolve_modules(&mut parsed, &fixtures_dir);
        assert!(
            result.is_err(),
            "Should detect circular import, but got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, ModuleError::CircularImport(_)),
            "Expected CircularImport error, got: {:?}",
            err
        );
    }

    #[test]
    fn test_expand_module_call_with_function_call_argument() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules
                .insert("vpc_mod".to_string(), create_module_with_interpolation());
            r
        };

        // Pass a FunctionCall as an argument value
        let call = ModuleCall {
            module_name: "vpc_mod".to_string(),
            binding_name: Some("dev_vpc".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "cidr_block".to_string(),
                    Value::FunctionCall {
                        name: "cidr_subnet".to_string(),
                        args: vec![
                            Value::String("10.0.0.0/16".to_string()),
                            Value::Int(8),
                            Value::Int(0),
                        ],
                    },
                );
                args.insert("env_name".to_string(), Value::String("dev".to_string()));
                args
            },
        };

        let expanded = resolver.expand_module_call(&call, "dev_vpc").unwrap();
        assert_eq!(expanded.len(), 1);

        let vpc = &expanded[0];

        // FunctionCall argument should be substituted as-is (resolved at apply time)
        assert_eq!(
            vpc.attributes.get("cidr_block"),
            Some(&Value::FunctionCall {
                name: "cidr_subnet".to_string(),
                args: vec![
                    Value::String("10.0.0.0/16".to_string()),
                    Value::Int(8),
                    Value::Int(0),
                ],
            })
        );
    }

    #[test]
    fn test_load_module_io_error_cleans_resolving_set() {
        let tmp_dir = std::env::temp_dir().join("carina_test_io_error_cleanup");
        let _ = fs::create_dir_all(&tmp_dir);

        let mut resolver = ModuleResolver::new(&tmp_dir);

        // First attempt: load a non-existent file -> IO error
        let result = resolver.load_module("nonexistent");
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), ModuleError::Io(_)),
            "expected IO error on first attempt"
        );

        // Second attempt: should get the same IO error, not a circular import error.
        // Before the fix, the path stayed in `resolving` and this would return CircularImport.
        let result = resolver.load_module("nonexistent");
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), ModuleError::Io(_)),
            "expected IO error on second attempt, not CircularImport"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_load_module_parse_error_cleans_resolving_set() {
        let tmp_dir = std::env::temp_dir().join("carina_test_parse_error_cleanup");
        let _ = fs::create_dir_all(&tmp_dir);
        let bad_file = tmp_dir.join("bad_module.crn");
        fs::write(&bad_file, "this is not valid carina syntax {{{{").unwrap();

        let mut resolver = ModuleResolver::new(&tmp_dir);

        // First attempt: parse error (use .crn extension since load_module reads full_path directly)
        let result = resolver.load_module("bad_module.crn");
        assert!(
            result.is_err(),
            "expected error but got: {:?}",
            result.unwrap()
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err, ModuleError::Parse(_)),
            "expected Parse error on first attempt, got: {err:?}"
        );

        // Second attempt: should still get parse error, not circular import
        let result = resolver.load_module("bad_module.crn");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, ModuleError::Parse(_)),
            "expected Parse error on second attempt, not CircularImport, got: {err:?}"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
