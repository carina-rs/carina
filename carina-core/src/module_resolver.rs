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
use crate::resource::{Resource, ResourceId, Value};

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

        // Check for circular import
        if self.resolving.contains(&full_path) {
            return Err(ModuleError::CircularImport(path.to_string()));
        }

        // Check cache
        if let Some(module) = self.module_cache.get(&full_path) {
            return Ok(module.clone());
        }

        // Mark as resolving
        self.resolving.insert(full_path.clone());

        // Load module: directory or single file
        let parsed = if full_path.is_dir() {
            self.load_directory_module(&full_path)?
        } else {
            let content = fs::read_to_string(&full_path)?;
            crate::parser::parse(&content)?
        };

        // Verify it's a module (has arguments or attributes)
        if parsed.arguments.is_empty() && parsed.attribute_params.is_empty() {
            return Err(ModuleError::NotFound(path.to_string()));
        }

        // Reject provider blocks inside modules
        if !parsed.providers.is_empty() {
            return Err(ModuleError::ProviderInModule);
        }

        // Remove from resolving set
        self.resolving.remove(&full_path);

        // Cache the module
        self.module_cache.insert(full_path, parsed.clone());

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

    /// Get an imported module by alias
    pub fn get_module(&self, alias: &str) -> Option<&ParsedFile> {
        self.imported_modules.get(alias)
    }

    /// Expand a module call into resources
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

            // Prefix the resource name with instance prefix
            let new_name = format!("{}_{}", instance_prefix, new_resource.id.name);
            new_resource.id = ResourceId::with_provider(
                &new_resource.id.provider,
                &new_resource.id.resource_type,
                new_name.clone(),
            );

            // Rewrite _binding with instance prefix
            if let Some(Value::String(binding)) = new_resource.attributes.get("_binding") {
                let prefixed = format!("{}_{}", instance_prefix, binding);
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

        Ok(expanded_resources)
    }
}

/// Substitute arguments references with actual values
fn substitute_arguments(value: &Value, arguments: &HashMap<String, Value>) -> Value {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } if binding_name == "arguments" => arguments
            .get(attribute_name)
            .cloned()
            .unwrap_or_else(|| value.clone()),
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
        _ => value.clone(),
    }
}

/// Rewrite intra-module ResourceRef binding names with instance prefix.
///
/// When a ResourceRef's binding_name matches one of the module's own bindings,
/// prefix it so that each module instance has isolated references.
fn rewrite_intra_module_refs(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
) -> Value {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
        } if intra_module_bindings.contains(binding_name) => Value::ResourceRef {
            binding_name: format!("{}_{}", instance_prefix, binding_name),
            attribute_name: attribute_name.clone(),
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
                            binding_name: "arguments".to_string(),
                            attribute_name: "vpc_id".to_string(),
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
                },
                ArgumentParameter {
                    name: "enable_flag".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                },
            ],
            attribute_params: vec![],
            backend: None,
        }
    }

    #[test]
    fn test_substitute_arguments() {
        let mut inputs = HashMap::new();
        inputs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));

        let value = Value::ResourceRef {
            binding_name: "arguments".to_string(),
            attribute_name: "vpc_id".to_string(),
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
                binding_name: "arguments".to_string(),
                attribute_name: "port".to_string(),
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
        assert_eq!(sg.id.name, "my_instance_sg");
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
                                binding_name: "arguments".to_string(),
                                attribute_name: "cidr".to_string(),
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
            }],
            attribute_params: vec![],
            backend: None,
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

        // _binding must be prefixed so they don't collide
        assert_eq!(
            expanded_a[0].attributes.get("_binding"),
            Some(&Value::String("prod_vpc".to_string())),
            "Instance A vpc _binding should be prefixed"
        );
        assert_eq!(
            expanded_a[1].attributes.get("_binding"),
            Some(&Value::String("prod_subnet".to_string())),
            "Instance A subnet _binding should be prefixed"
        );
        assert_eq!(
            expanded_b[0].attributes.get("_binding"),
            Some(&Value::String("staging_vpc".to_string())),
            "Instance B vpc _binding should be prefixed"
        );
        assert_eq!(
            expanded_b[1].attributes.get("_binding"),
            Some(&Value::String("staging_subnet".to_string())),
            "Instance B subnet _binding should be prefixed"
        );

        // Intra-module ResourceRef must point to the prefixed binding
        assert_eq!(
            expanded_a[1].attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "prod_vpc".to_string(),
                attribute_name: "id".to_string(),
            }),
            "Instance A subnet should reference prod_vpc, not bare vpc"
        );
        assert_eq!(
            expanded_b[1].attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "staging_vpc".to_string(),
                attribute_name: "id".to_string(),
            }),
            "Instance B subnet should reference staging_vpc, not bare vpc"
        );

        // Resource names should also be distinct
        assert_eq!(expanded_a[0].id.name, "prod_main_vpc");
        assert_eq!(expanded_b[0].id.name, "staging_main_vpc");
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
}
