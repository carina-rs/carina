//! Module Resolver - Resolve module imports and instantiations
//!
//! This module handles:
//! - Resolving import paths to module definitions
//! - Detecting circular dependencies between modules
//! - Validating module input parameters
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

    #[error("Missing required input '{input}' for module '{module}'")]
    MissingInput { module: String, input: String },

    #[error("Invalid input type for '{input}' in module '{module}': expected {expected}")]
    InvalidInputType {
        module: String,
        input: String,
        expected: String,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("Unknown module: {0}")]
    UnknownModule(String),
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

        // Verify it's a module (has inputs or outputs)
        if parsed.inputs.is_empty() && parsed.outputs.is_empty() {
            return Err(ModuleError::NotFound(path.to_string()));
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
            inputs: vec![],
            outputs: vec![],
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
            merged.inputs.extend(parsed.inputs);
            merged.outputs.extend(parsed.outputs);
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

        // Validate required inputs
        for input in &module.inputs {
            if input.default.is_none() && !call.arguments.contains_key(&input.name) {
                return Err(ModuleError::MissingInput {
                    module: call.module_name.clone(),
                    input: input.name.clone(),
                });
            }
        }

        // Build input value map
        let mut input_values: HashMap<String, Value> = HashMap::new();
        for input in &module.inputs {
            let value = call
                .arguments
                .get(&input.name)
                .cloned()
                .or_else(|| input.default.clone())
                .unwrap();
            input_values.insert(input.name.clone(), value);
        }

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

            // Add module source info
            new_resource.attributes.insert(
                "_module".to_string(),
                Value::String(call.module_name.clone()),
            );
            new_resource.attributes.insert(
                "_module_instance".to_string(),
                Value::String(instance_prefix.to_string()),
            );

            // Substitute input references
            let mut substituted_attrs = HashMap::new();
            for (key, value) in &new_resource.attributes {
                substituted_attrs.insert(key.clone(), substitute_inputs(value, &input_values));
            }
            new_resource.attributes = substituted_attrs;

            expanded_resources.push(new_resource);
        }

        Ok(expanded_resources)
    }
}

/// Substitute input references with actual values
fn substitute_inputs(value: &Value, inputs: &HashMap<String, Value>) -> Value {
    match value {
        Value::ResourceRef(binding, attr) if binding == "input" => {
            inputs.get(attr).cloned().unwrap_or_else(|| value.clone())
        }
        Value::TypedResourceRef {
            binding_name,
            attribute_name,
            ..
        } if binding_name == "input" => inputs
            .get(attribute_name)
            .cloned()
            .unwrap_or_else(|| value.clone()),
        Value::List(items) => {
            Value::List(items.iter().map(|v| substitute_inputs(v, inputs)).collect())
        }
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_inputs(v, inputs)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{InputParameter, TypeExpr};

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
                        Value::ResourceRef("input".to_string(), "vpc_id".to_string()),
                    );
                    attrs.insert(
                        "_type".to_string(),
                        Value::String("aws.security_group".to_string()),
                    );
                    attrs
                },
                read_only: false,
            }],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            inputs: vec![
                InputParameter {
                    name: "vpc_id".to_string(),
                    type_expr: TypeExpr::String,
                    default: None,
                },
                InputParameter {
                    name: "enable_flag".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                },
            ],
            outputs: vec![],
            backend: None,
        }
    }

    #[test]
    fn test_substitute_inputs() {
        let mut inputs = HashMap::new();
        inputs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));

        let value = Value::ResourceRef("input".to_string(), "vpc_id".to_string());
        let result = substitute_inputs(&value, &inputs);

        assert_eq!(result, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_substitute_inputs_nested() {
        let mut inputs = HashMap::new();
        inputs.insert("port".to_string(), Value::Int(8080));

        let value = Value::List(vec![
            Value::ResourceRef("input".to_string(), "port".to_string()),
            Value::Int(443),
        ]);
        let result = substitute_inputs(&value, &inputs);

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

    #[test]
    fn test_missing_required_input() {
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
        assert!(matches!(result, Err(ModuleError::MissingInput { .. })));
    }
}
