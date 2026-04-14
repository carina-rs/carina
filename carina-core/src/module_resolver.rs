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

use crate::parser::{
    CompareOp, ImportStatement, ModuleCall, ParseError, ParsedFile, ProviderContext, TypeExpr,
    ValidateExpr, validate_custom_type,
};
use crate::resource::{Expr, LifecycleConfig, Resource, ResourceId, ResourceKind, Value};

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

    #[error(
        "Validation failed for argument '{argument}' in module '{module}': {message} (got {actual})"
    )]
    ArgumentValidationFailed {
        module: String,
        argument: String,
        message: String,
        actual: String,
    },

    #[error("Require constraint failed in module '{module}': {message}")]
    RequireConstraintFailed { module: String, message: String },
}

/// Context for module resolution
pub struct ModuleResolver<'cfg> {
    /// Base directory for resolving relative imports
    base_dir: PathBuf,
    /// Cache of loaded modules: path -> ParsedFile
    module_cache: HashMap<PathBuf, ParsedFile>,
    /// Currently resolving modules (for cycle detection)
    resolving: HashSet<PathBuf>,
    /// Imported module definitions by alias
    imported_modules: HashMap<String, ParsedFile>,
    /// Parser configuration (decryptor, custom validators)
    config: &'cfg ProviderContext,
}

impl<'cfg> ModuleResolver<'cfg> {
    /// Create a new resolver with the given base directory and default config
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        static DEFAULT_CONFIG: std::sync::LazyLock<ProviderContext> =
            std::sync::LazyLock::new(ProviderContext::default);
        Self::with_config(base_dir, &DEFAULT_CONFIG)
    }

    /// Create a new resolver with the given base directory and parser configuration
    pub fn with_config(base_dir: impl AsRef<Path>, config: &'cfg ProviderContext) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            module_cache: HashMap::new(),
            resolving: HashSet::new(),
            imported_modules: HashMap::new(),
            config,
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
                .and_then(|content| {
                    crate::parser::parse(&content, self.config).map_err(ModuleError::from)
                })
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
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
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
            let parsed = crate::parser::parse(&content, self.config)?;

            // Merge all fields
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
            merged.export_params.extend(parsed.export_params);
            merged.user_functions.extend(parsed.user_functions);
            merged.remote_states.extend(parsed.remote_states);
            merged.requires.extend(parsed.requires);
            merged
                .structural_bindings
                .extend(parsed.structural_bindings);
            merged.warnings.extend(parsed.warnings);
            merged
                .deferred_for_expressions
                .extend(parsed.deferred_for_expressions);
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
    /// The virtual resource has `ResourceKind::Virtual` and is skipped by the differ.
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

        // Type-check argument values against declared types
        for arg in &module.arguments {
            let value = argument_values.get(&arg.name).unwrap();
            check_module_arg_type(
                &call.module_name,
                &arg.name,
                &arg.type_expr,
                value,
                self.config,
            )?;
        }

        // Validate argument values against validate blocks
        for arg in &module.arguments {
            let value = argument_values.get(&arg.name).unwrap();
            for validation_block in &arg.validations {
                match evaluate_validate_expr(&validation_block.condition, &arg.name, value) {
                    Ok(true) => {} // Validation passed
                    Ok(false) => {
                        let message = validation_block.error_message.clone().unwrap_or_else(|| {
                            format!("validation failed for argument '{}'", arg.name)
                        });
                        return Err(ModuleError::ArgumentValidationFailed {
                            module: call.module_name.clone(),
                            argument: arg.name.clone(),
                            message,
                            actual: format_value_for_error(value),
                        });
                    }
                    Err(e) => {
                        return Err(ModuleError::ArgumentValidationFailed {
                            module: call.module_name.clone(),
                            argument: arg.name.clone(),
                            message: format!("error evaluating validate expression: {}", e),
                            actual: format_value_for_error(value),
                        });
                    }
                }
            }
        }

        // Evaluate require blocks (cross-argument constraints)
        for require in &module.requires {
            match evaluate_require_expr(&require.condition, &argument_values) {
                Ok(true) => {} // Constraint satisfied
                Ok(false) => {
                    return Err(ModuleError::RequireConstraintFailed {
                        module: call.module_name.clone(),
                        message: require.error_message.clone(),
                    });
                }
                Err(e) => {
                    return Err(ModuleError::RequireConstraintFailed {
                        module: call.module_name.clone(),
                        message: format!("error evaluating require expression: {}", e),
                    });
                }
            }
        }

        // Collect intra-module binding names so we can rewrite ResourceRefs
        let intra_module_bindings: HashSet<String> = module
            .resources
            .iter()
            .filter_map(|r| r.binding.clone())
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

            // Rewrite binding with instance path (dot-separated)
            if let Some(ref binding) = new_resource.binding {
                let prefixed = format!("{}.{}", instance_prefix, binding);
                new_resource.binding = Some(prefixed);
            }

            // Set typed module source info
            new_resource.module_source = Some(crate::resource::ModuleSource::Module {
                name: call.module_name.clone(),
                instance: instance_prefix.to_string(),
            });

            // Rewrite intra-module ResourceRefs BEFORE substituting inputs.
            // This ensures that caller-provided ResourceRef values (which may
            // coincidentally share a binding name with a module-internal binding)
            // are not incorrectly prefixed.
            let mut substituted_attrs = HashMap::new();
            for (key, expr) in &new_resource.attributes {
                let rewritten =
                    rewrite_intra_module_refs(expr, instance_prefix, &intra_module_bindings);
                let substituted = substitute_arguments(&rewritten, &argument_values);
                substituted_attrs.insert(key.clone(), Expr(substituted));
            }
            new_resource.attributes = substituted_attrs;

            expanded_resources.push(new_resource);
        }

        // Create a virtual resource if the module has attributes and the call has a binding
        if !module.attribute_params.is_empty()
            && let Some(binding_name) = &call.binding_name
        {
            let mut virtual_attrs: HashMap<String, Expr> = HashMap::new();

            // Copy attribute values from the module definition
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    // Rewrite intra-module refs and substitute arguments
                    let rewritten =
                        rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                    let substituted = substitute_arguments(&rewritten, &argument_values);
                    virtual_attrs.insert(attr_param.name.clone(), Expr(substituted));
                }
            }

            let virtual_resource = Resource {
                id: ResourceId::new("_virtual", binding_name),
                attributes: virtual_attrs,
                kind: ResourceKind::Virtual {
                    module_name: call.module_name.clone(),
                    instance: instance_prefix.to_string(),
                },
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
                binding: Some(binding_name.clone()),
                dependency_bindings: Vec::new(),
                module_source: None,
            };
            expanded_resources.push(virtual_resource);
        }

        Ok(expanded_resources)
    }
}

/// Format a Value for use in error messages.
fn format_value_for_error(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => format!("[...] (length {})", items.len()),
        Value::Map(map) => format!("{{...}} (length {})", map.len()),
        _ => format!("{:?}", value),
    }
}

/// Evaluate a validate expression with the given argument name and value.
/// Returns Ok(true) if validation passes, Ok(false) if it fails.
fn evaluate_validate_expr(
    expr: &ValidateExpr,
    arg_name: &str,
    arg_value: &Value,
) -> Result<bool, String> {
    let result = eval_validate(expr, arg_name, arg_value)?;
    match result {
        ValidateValue::Bool(b) => Ok(b),
        other => Err(format!(
            "validate expression must return a boolean, got {:?}",
            other
        )),
    }
}

/// Internal value type for validate expression evaluation
#[derive(Debug, Clone)]
enum ValidateValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

/// Evaluate a validate expression node, returning a ValidateValue
fn eval_validate(
    expr: &ValidateExpr,
    arg_name: &str,
    arg_value: &Value,
) -> Result<ValidateValue, String> {
    match expr {
        ValidateExpr::Bool(b) => Ok(ValidateValue::Bool(*b)),
        ValidateExpr::Int(n) => Ok(ValidateValue::Int(*n)),
        ValidateExpr::Float(f) => Ok(ValidateValue::Float(*f)),
        ValidateExpr::String(s) => Ok(ValidateValue::String(s.clone())),
        ValidateExpr::Null => {
            Err("null is not supported in per-argument validation expressions".to_string())
        }
        ValidateExpr::Var(name) => {
            if name == arg_name {
                match arg_value {
                    Value::Int(n) => Ok(ValidateValue::Int(*n)),
                    Value::Float(f) => Ok(ValidateValue::Float(*f)),
                    Value::Bool(b) => Ok(ValidateValue::Bool(*b)),
                    Value::String(s) => Ok(ValidateValue::String(s.clone())),
                    other => Err(format!(
                        "unsupported value type for validation: {:?}",
                        other
                    )),
                }
            } else {
                Err(format!(
                    "unknown variable '{}' in validate expression (expected '{}')",
                    name, arg_name
                ))
            }
        }
        ValidateExpr::Compare { lhs, op, rhs } => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            let right = eval_validate(rhs, arg_name, arg_value)?;
            let result = compare_validate_values(&left, op, &right)?;
            Ok(ValidateValue::Bool(result))
        }
        ValidateExpr::And(lhs, rhs) => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            match left {
                ValidateValue::Bool(false) => Ok(ValidateValue::Bool(false)),
                ValidateValue::Bool(true) => {
                    let right = eval_validate(rhs, arg_name, arg_value)?;
                    match right {
                        ValidateValue::Bool(b) => Ok(ValidateValue::Bool(b)),
                        _ => Err("right operand of && must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of && must be boolean".to_string()),
            }
        }
        ValidateExpr::Or(lhs, rhs) => {
            let left = eval_validate(lhs, arg_name, arg_value)?;
            match left {
                ValidateValue::Bool(true) => Ok(ValidateValue::Bool(true)),
                ValidateValue::Bool(false) => {
                    let right = eval_validate(rhs, arg_name, arg_value)?;
                    match right {
                        ValidateValue::Bool(b) => Ok(ValidateValue::Bool(b)),
                        _ => Err("right operand of || must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of || must be boolean".to_string()),
            }
        }
        ValidateExpr::Not(inner) => {
            let val = eval_validate(inner, arg_name, arg_value)?;
            match val {
                ValidateValue::Bool(b) => Ok(ValidateValue::Bool(!b)),
                _ => Err("operand of ! must be boolean".to_string()),
            }
        }
        ValidateExpr::FunctionCall { name, args } => {
            eval_validate_function(name, args, arg_name, arg_value)
        }
    }
}

/// Compare two ValidateValues with the given operator
fn compare_validate_values(
    left: &ValidateValue,
    op: &CompareOp,
    right: &ValidateValue,
) -> Result<bool, String> {
    match (left, right) {
        (ValidateValue::Int(a), ValidateValue::Int(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (ValidateValue::Float(a), ValidateValue::Float(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (ValidateValue::Int(a), ValidateValue::Float(b)) => {
            let a = *a as f64;
            Ok(match op {
                CompareOp::Gte => a >= *b,
                CompareOp::Lte => a <= *b,
                CompareOp::Gt => a > *b,
                CompareOp::Lt => a < *b,
                CompareOp::Eq => a == *b,
                CompareOp::Ne => a != *b,
            })
        }
        (ValidateValue::Float(a), ValidateValue::Int(b)) => {
            let b = *b as f64;
            Ok(match op {
                CompareOp::Gte => *a >= b,
                CompareOp::Lte => *a <= b,
                CompareOp::Gt => *a > b,
                CompareOp::Lt => *a < b,
                CompareOp::Eq => *a == b,
                CompareOp::Ne => *a != b,
            })
        }
        (ValidateValue::String(a), ValidateValue::String(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("strings only support == and != comparisons".to_string()),
        }),
        (ValidateValue::Bool(a), ValidateValue::Bool(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("booleans only support == and != comparisons".to_string()),
        }),
        _ => Err(format!("cannot compare {:?} with {:?}", left, right)),
    }
}

/// Evaluate a function call in a validate expression
fn eval_validate_function(
    name: &str,
    args: &[ValidateExpr],
    arg_name: &str,
    arg_value: &Value,
) -> Result<ValidateValue, String> {
    match name {
        "len" | "length" => {
            if args.len() != 1 {
                return Err(format!("{}() expects 1 argument, got {}", name, args.len()));
            }
            // For Var references, access the original Value directly to support
            // List and Map types (which can't be represented as ValidateValue).
            if let ValidateExpr::Var(var_name) = &args[0]
                && var_name == arg_name
            {
                return match arg_value {
                    Value::String(s) => Ok(ValidateValue::Int(s.len() as i64)),
                    Value::List(items) => Ok(ValidateValue::Int(items.len() as i64)),
                    Value::Map(map) => Ok(ValidateValue::Int(map.len() as i64)),
                    _ => Err(format!(
                        "{}() argument must be a string, list, or map",
                        name
                    )),
                };
            }
            // For non-Var expressions (e.g., string literals), evaluate normally
            let val = eval_validate(&args[0], arg_name, arg_value)?;
            match val {
                ValidateValue::String(s) => Ok(ValidateValue::Int(s.len() as i64)),
                _ => Err(format!(
                    "{}() argument must be a string, list, or map",
                    name
                )),
            }
        }
        _ => Err(format!(
            "unknown function '{}' in validate expression",
            name
        )),
    }
}

/// Evaluate a require expression with access to all argument values.
/// Returns Ok(true) if the constraint is satisfied, Ok(false) if it fails.
fn evaluate_require_expr(
    expr: &ValidateExpr,
    args: &HashMap<String, Value>,
) -> Result<bool, String> {
    let result = eval_require(expr, args)?;
    match result {
        RequireValue::Bool(b) => Ok(b),
        other => Err(format!(
            "require expression must return a boolean, got {:?}",
            other
        )),
    }
}

/// Internal value type for require expression evaluation
#[derive(Debug, Clone)]
enum RequireValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Null,
}

/// Evaluate a require expression node with access to all argument values
fn eval_require(
    expr: &ValidateExpr,
    args: &HashMap<String, Value>,
) -> Result<RequireValue, String> {
    match expr {
        ValidateExpr::Bool(b) => Ok(RequireValue::Bool(*b)),
        ValidateExpr::Int(n) => Ok(RequireValue::Int(*n)),
        ValidateExpr::Float(f) => Ok(RequireValue::Float(*f)),
        ValidateExpr::String(s) => Ok(RequireValue::String(s.clone())),
        ValidateExpr::Null => Ok(RequireValue::Null),
        ValidateExpr::Var(name) => {
            if let Some(value) = args.get(name) {
                match value {
                    Value::Int(n) => Ok(RequireValue::Int(*n)),
                    Value::Float(f) => Ok(RequireValue::Float(*f)),
                    Value::Bool(b) => Ok(RequireValue::Bool(*b)),
                    Value::String(s) => Ok(RequireValue::String(s.clone())),
                    other => Err(format!(
                        "unsupported value type for require expression: {:?}",
                        other
                    )),
                }
            } else {
                Err(format!("unknown variable '{}' in require expression", name))
            }
        }
        ValidateExpr::Compare { lhs, op, rhs } => {
            let left = eval_require(lhs, args)?;
            let right = eval_require(rhs, args)?;
            let result = compare_require_values(&left, op, &right)?;
            Ok(RequireValue::Bool(result))
        }
        ValidateExpr::And(lhs, rhs) => {
            let left = eval_require(lhs, args)?;
            match left {
                RequireValue::Bool(false) => Ok(RequireValue::Bool(false)),
                RequireValue::Bool(true) => {
                    let right = eval_require(rhs, args)?;
                    match right {
                        RequireValue::Bool(b) => Ok(RequireValue::Bool(b)),
                        _ => Err("right operand of && must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of && must be boolean".to_string()),
            }
        }
        ValidateExpr::Or(lhs, rhs) => {
            let left = eval_require(lhs, args)?;
            match left {
                RequireValue::Bool(true) => Ok(RequireValue::Bool(true)),
                RequireValue::Bool(false) => {
                    let right = eval_require(rhs, args)?;
                    match right {
                        RequireValue::Bool(b) => Ok(RequireValue::Bool(b)),
                        _ => Err("right operand of || must be boolean".to_string()),
                    }
                }
                _ => Err("left operand of || must be boolean".to_string()),
            }
        }
        ValidateExpr::Not(inner) => {
            let val = eval_require(inner, args)?;
            match val {
                RequireValue::Bool(b) => Ok(RequireValue::Bool(!b)),
                _ => Err("operand of ! must be boolean".to_string()),
            }
        }
        ValidateExpr::FunctionCall {
            name,
            args: fn_args,
        } => eval_require_function(name, fn_args, args),
    }
}

/// Compare two RequireValues with the given operator
fn compare_require_values(
    left: &RequireValue,
    op: &CompareOp,
    right: &RequireValue,
) -> Result<bool, String> {
    // Handle null comparisons
    match (left, right) {
        (RequireValue::Null, RequireValue::Null) => {
            return Ok(matches!(op, CompareOp::Eq));
        }
        (RequireValue::Null, _) | (_, RequireValue::Null) => {
            return Ok(matches!(op, CompareOp::Ne));
        }
        _ => {}
    }

    match (left, right) {
        (RequireValue::Int(a), RequireValue::Int(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (RequireValue::Float(a), RequireValue::Float(b)) => Ok(match op {
            CompareOp::Gte => a >= b,
            CompareOp::Lte => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Lt => a < b,
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
        }),
        (RequireValue::Int(a), RequireValue::Float(b)) => {
            let a = *a as f64;
            Ok(match op {
                CompareOp::Gte => a >= *b,
                CompareOp::Lte => a <= *b,
                CompareOp::Gt => a > *b,
                CompareOp::Lt => a < *b,
                CompareOp::Eq => a == *b,
                CompareOp::Ne => a != *b,
            })
        }
        (RequireValue::Float(a), RequireValue::Int(b)) => {
            let b = *b as f64;
            Ok(match op {
                CompareOp::Gte => *a >= b,
                CompareOp::Lte => *a <= b,
                CompareOp::Gt => *a > b,
                CompareOp::Lt => *a < b,
                CompareOp::Eq => *a == b,
                CompareOp::Ne => *a != b,
            })
        }
        (RequireValue::String(a), RequireValue::String(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("strings only support == and != comparisons".to_string()),
        }),
        (RequireValue::Bool(a), RequireValue::Bool(b)) => Ok(match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            _ => return Err("booleans only support == and != comparisons".to_string()),
        }),
        _ => Err(format!("cannot compare {:?} with {:?}", left, right)),
    }
}

/// Evaluate a function call in a require expression
fn eval_require_function(
    name: &str,
    fn_args: &[ValidateExpr],
    args: &HashMap<String, Value>,
) -> Result<RequireValue, String> {
    match name {
        "len" | "length" => {
            if fn_args.len() != 1 {
                return Err(format!(
                    "{}() expects 1 argument, got {}",
                    name,
                    fn_args.len()
                ));
            }
            // For Var references, access the original Value directly to support
            // List and Map types (which can't be represented as RequireValue).
            if let ValidateExpr::Var(var_name) = &fn_args[0]
                && let Some(value) = args.get(var_name)
            {
                return match value {
                    Value::String(s) => Ok(RequireValue::Int(s.len() as i64)),
                    Value::List(items) => Ok(RequireValue::Int(items.len() as i64)),
                    Value::Map(map) => Ok(RequireValue::Int(map.len() as i64)),
                    _ => Err(format!(
                        "{}() argument must be a string, list, or map",
                        name
                    )),
                };
            }
            // For non-Var expressions, evaluate normally
            let val = eval_require(&fn_args[0], args)?;
            match val {
                RequireValue::String(s) => Ok(RequireValue::Int(s.len() as i64)),
                _ => Err(format!(
                    "{}() argument must be a string, list, or map",
                    name
                )),
            }
        }
        _ => Err(format!("unknown function '{}' in require expression", name)),
    }
}

/// Substitute arguments references with actual values.
///
/// Argument parameter names are registered as lexical bindings in the parser,
/// so they appear as `ResourceRef { binding_name: "<param_name>", attribute_name: ... }`.
/// We match when `binding_name` is one of the argument keys.
fn substitute_arguments(value: &Value, arguments: &HashMap<String, Value>) -> Value {
    match value {
        Value::ResourceRef { path } if arguments.contains_key(path.binding()) => arguments
            .get(path.binding())
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
        Value::ResourceRef { path } if intra_module_bindings.contains(path.binding()) => {
            Value::resource_ref(
                format!("{}.{}", instance_prefix, path.binding()),
                path.attribute().to_string(),
                path.field_path().iter().map(|s| s.to_string()).collect(),
            )
        }
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
    resolve_modules_with_config(parsed, base_dir, &ProviderContext::default())
}

/// Resolve all modules in a parsed file with the given parser configuration.
pub fn resolve_modules_with_config(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    config: &ProviderContext,
) -> Result<(), ModuleError> {
    let mut resolver = ModuleResolver::with_config(base_dir, config);

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
    let parsed = crate::parser::parse(&content, &ProviderContext::default())?;
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
            crate::parser::parse(&content, &ProviderContext::default()).ok()
        } else {
            load_directory_module(path)
        }
    } else {
        let content = fs::read_to_string(path).ok()?;
        crate::parser::parse(&content, &ProviderContext::default()).ok()
    }
}

/// Check that a module argument value matches the declared type.
///
/// Similar to parser's `check_fn_arg_type` for user-defined functions,
/// this validates module call arguments against their declared `TypeExpr`.
fn check_module_arg_type(
    module_name: &str,
    arg_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), ModuleError> {
    match check_type_match(type_expr, value, config) {
        TypeCheckResult::Ok => Ok(()),
        TypeCheckResult::Mismatch => Err(ModuleError::InvalidArgumentType {
            module: module_name.to_string(),
            argument: arg_name.to_string(),
            expected: type_expr.to_string(),
        }),
        TypeCheckResult::ValidationError(e) => Err(ModuleError::InvalidArgumentType {
            module: module_name.to_string(),
            argument: arg_name.to_string(),
            expected: format!("{} ({})", type_expr, e),
        }),
    }
}

enum TypeCheckResult {
    Ok,
    Mismatch,
    ValidationError(String),
}

fn check_type_match(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> TypeCheckResult {
    match type_expr {
        // FunctionCall results are not known statically; defer validation
        _ if matches!(value, Value::FunctionCall { .. }) => TypeCheckResult::Ok,
        TypeExpr::String => {
            if matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Int => {
            if matches!(value, Value::Int(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Float => {
            if matches!(value, Value::Float(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Bool => {
            if matches!(value, Value::Bool(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::List(inner) => {
            if let Value::List(items) = value {
                for item in items {
                    match check_type_match(inner, item, config) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Map(inner) => {
            if let Value::Map(entries) = value {
                for v in entries.values() {
                    match check_type_match(inner, v, config) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        // Simple types (cidr, arn, iam_policy_arn, etc.) are string subtypes
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                TypeCheckResult::Mismatch
            } else if let Err(e) = validate_custom_type(name, value, config) {
                TypeCheckResult::ValidationError(e)
            } else {
                TypeCheckResult::Ok
            }
        }
        // Resource type refs and schema types: accept strings (validated elsewhere)
        TypeExpr::Ref(_) | TypeExpr::SchemaType { .. } => {
            if matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
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
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        remote_states: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
            && let Ok(parsed) = crate::parser::parse(&content, &ProviderContext::default())
        {
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
            merged.export_params.extend(parsed.export_params);
            merged.user_functions.extend(parsed.user_functions);
            merged.remote_states.extend(parsed.remote_states);
            merged.requires.extend(parsed.requires);
            merged
                .structural_bindings
                .extend(parsed.structural_bindings);
            merged.warnings.extend(parsed.warnings);
            merged
                .deferred_for_expressions
                .extend(parsed.deferred_for_expressions);
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
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        remote_states: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "crn") {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            let parsed = crate::parser::parse(&content, &ProviderContext::default())
                .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.arguments.extend(parsed.arguments);
            merged.attribute_params.extend(parsed.attribute_params);
            merged.export_params.extend(parsed.export_params);
            merged.user_functions.extend(parsed.user_functions);
            merged.remote_states.extend(parsed.remote_states);
            merged.requires.extend(parsed.requires);
            merged
                .structural_bindings
                .extend(parsed.structural_bindings);
            merged.warnings.extend(parsed.warnings);
            merged
                .deferred_for_expressions
                .extend(parsed.deferred_for_expressions);
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
                        Value::resource_ref("vpc_id".to_string(), String::new(), vec![]),
                    );
                    attrs.insert(
                        "_type".to_string(),
                        Value::String("aws.security_group".to_string()),
                    );
                    Expr::wrap_map(attrs)
                },
                kind: ResourceKind::Real,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
                binding: None,
                dependency_bindings: Vec::new(),
                module_source: None,
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
                    validations: Vec::new(),
                },
                ArgumentParameter {
                    name: "enable_flag".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                    description: None,
                    validations: Vec::new(),
                },
            ],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        }
    }

    #[test]
    fn test_substitute_arguments() {
        let mut inputs = HashMap::new();
        inputs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));

        // Argument params are lexically scoped: binding_name is the param name itself
        let value = Value::resource_ref("vpc_id".to_string(), String::new(), vec![]);
        let result = substitute_arguments(&value, &inputs);

        assert_eq!(result, Value::String("vpc-123".to_string()));
    }

    #[test]
    fn test_substitute_arguments_nested() {
        let mut inputs = HashMap::new();
        inputs.insert("port".to_string(), Value::Int(8080));

        let value = Value::List(vec![
            Value::resource_ref("port".to_string(), String::new(), vec![]),
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
            sg.get_attr("vpc_id"),
            Some(&Value::String("vpc-456".to_string()))
        );
        assert_eq!(
            sg.module_source,
            Some(crate::resource::ModuleSource::Module {
                name: "test_module".to_string(),
                instance: "my_instance".to_string(),
            })
        );
        // Module info should NOT be in attributes
        assert!(!sg.attributes.contains_key("_module"));
        assert!(!sg.attributes.contains_key("_module_instance"));
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
                        attrs.insert(
                            "cidr_block".to_string(),
                            Value::resource_ref("cidr".to_string(), String::new(), vec![]),
                        );
                        Expr::wrap_map(attrs)
                    },
                    kind: ResourceKind::Real,
                    lifecycle: LifecycleConfig::default(),
                    prefixes: HashMap::new(),
                    binding: Some("vpc".to_string()),
                    dependency_bindings: Vec::new(),
                    module_source: None,
                },
                Resource {
                    id: ResourceId::new("ec2.subnet", "sub"),
                    attributes: {
                        let mut attrs = HashMap::new();
                        attrs.insert(
                            "vpc_id".to_string(),
                            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                        );
                        Expr::wrap_map(attrs)
                    },
                    kind: ResourceKind::Real,
                    lifecycle: LifecycleConfig::default(),
                    prefixes: HashMap::new(),
                    binding: Some("subnet".to_string()),
                    dependency_bindings: Vec::new(),
                    module_source: None,
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
                validations: Vec::new(),
            }],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
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

        // binding must be prefixed so they don't collide (using dot notation)
        assert_eq!(
            expanded_a[0].binding,
            Some("prod.vpc".to_string()),
            "Instance A vpc binding should use dot path"
        );
        assert_eq!(
            expanded_a[1].binding,
            Some("prod.subnet".to_string()),
            "Instance A subnet binding should use dot path"
        );
        assert_eq!(
            expanded_b[0].binding,
            Some("staging.vpc".to_string()),
            "Instance B vpc binding should use dot path"
        );
        assert_eq!(
            expanded_b[1].binding,
            Some("staging.subnet".to_string()),
            "Instance B subnet binding should use dot path"
        );

        // Intra-module ResourceRef must point to the dot-path binding
        assert_eq!(
            expanded_a[1].get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "prod.vpc".to_string(),
                "id".to_string(),
                vec![]
            )),
            "Instance A subnet should reference prod.vpc, not bare vpc"
        );
        assert_eq!(
            expanded_b[1].get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "staging.vpc".to_string(),
                "id".to_string(),
                vec![]
            )),
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
                    attrs.insert(
                        "_type".to_string(),
                        Value::String("aws.security_group".to_string()),
                    );
                    Expr::wrap_map(attrs)
                },
                kind: ResourceKind::Real,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
                binding: Some("sg".to_string()),
                dependency_bindings: Vec::new(),
                module_source: None,
            }],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![AttributeParameter {
                name: "security_group".to_string(),
                type_expr: None,
                value: Some(Value::resource_ref(
                    "sg".to_string(),
                    "id".to_string(),
                    vec![],
                )),
            }],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
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
            .find(|r| r.is_virtual())
            .expect("Virtual resource should exist");

        assert_eq!(virtual_res.binding, Some("web".to_string()));
        // Module info should be in the kind, not in attributes
        assert_eq!(
            virtual_res.kind,
            ResourceKind::Virtual {
                module_name: "web_tier".to_string(),
                instance: "web".to_string(),
            }
        );
        assert!(!virtual_res.attributes.contains_key("_module"));
        assert!(!virtual_res.attributes.contains_key("_module_instance"));
        // The security_group attribute should be a rewritten ResourceRef
        // pointing to the dot-path binding (web.sg)
        assert_eq!(
            virtual_res.get_attr("security_group"),
            Some(&Value::resource_ref(
                "web.sg".to_string(),
                "id".to_string(),
                vec![]
            ))
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
        let virtual_count = expanded.iter().filter(|r| r.is_virtual()).count();
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

        // binding should use dot notation
        assert_eq!(expanded[0].binding, Some("prod.vpc".to_string()));
        assert_eq!(expanded[1].binding, Some("prod.subnet".to_string()));

        // Intra-module ResourceRef should use dot notation
        assert_eq!(
            expanded[1].get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "prod.vpc".to_string(),
                "id".to_string(),
                vec![]
            )),
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
            .find(|r| r.is_virtual())
            .expect("Virtual resource should exist");

        // The security_group attribute should reference dot-notation binding
        assert_eq!(
            virtual_res.get_attr("security_group"),
            Some(&Value::resource_ref(
                "web.sg".to_string(),
                "id".to_string(),
                vec![]
            ))
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
            InterpolationPart::Expr(Value::resource_ref(
                "env_name".to_string(),
                String::new(),
                vec![],
            )),
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
                Value::resource_ref("cidr".to_string(), String::new(), vec![]),
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
                    attrs.insert(
                        "cidr_block".to_string(),
                        Value::resource_ref("cidr_block".to_string(), String::new(), vec![]),
                    );
                    attrs.insert(
                        "name".to_string(),
                        Value::Interpolation(vec![
                            InterpolationPart::Literal("test-".to_string()),
                            InterpolationPart::Expr(Value::resource_ref(
                                "env_name".to_string(),
                                String::new(),
                                vec![],
                            )),
                        ]),
                    );
                    attrs.insert(
                        "env".to_string(),
                        Value::resource_ref("env_name".to_string(), String::new(), vec![]),
                    );
                    Expr::wrap_map(attrs)
                },
                kind: ResourceKind::Real,
                lifecycle: LifecycleConfig::default(),
                prefixes: HashMap::new(),
                binding: Some("vpc".to_string()),
                dependency_bindings: Vec::new(),
                module_source: None,
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
                    validations: Vec::new(),
                },
                ArgumentParameter {
                    name: "env_name".to_string(),
                    type_expr: TypeExpr::String,
                    default: None,
                    description: None,
                    validations: Vec::new(),
                },
            ],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
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
            vpc.get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(vpc.get_attr("env"), Some(&Value::String("dev".to_string())));

        // Interpolation with argument should have the argument value substituted
        assert_eq!(
            vpc.get_attr("name"),
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
        let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

        resolve_modules(&mut parsed, &fixtures_dir).unwrap();

        // Should have resources from both inner_module (vpc) and outer_module (sg)
        let resource_types: Vec<&str> = parsed
            .resources
            .iter()
            .filter_map(|r| {
                r.get_attr("_type").and_then(|v| match v {
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
        let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

        resolve_modules(&mut parsed, &fixtures_dir).unwrap();

        // Should have the VPC resource from inner_module (through middle_module)
        let resource_types: Vec<&str> = parsed
            .resources
            .iter()
            .filter_map(|r| {
                r.get_attr("_type").and_then(|v| match v {
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
        let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

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
            vpc.get_attr("cidr_block"),
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

    /// Helper to create a module with a validated port argument
    fn create_module_with_port_validation() -> ParsedFile {
        use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
        ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![ArgumentParameter {
                name: "port".to_string(),
                type_expr: TypeExpr::Int,
                default: Some(Value::Int(8080)),
                description: Some("Web server port".to_string()),
                validations: vec![ValidationBlock {
                    condition: ValidateExpr::And(
                        Box::new(ValidateExpr::Compare {
                            lhs: Box::new(ValidateExpr::Var("port".to_string())),
                            op: CompareOp::Gte,
                            rhs: Box::new(ValidateExpr::Int(1)),
                        }),
                        Box::new(ValidateExpr::Compare {
                            lhs: Box::new(ValidateExpr::Var("port".to_string())),
                            op: CompareOp::Lte,
                            rhs: Box::new(ValidateExpr::Int(65535)),
                        }),
                    ),
                    error_message: Some("Port must be between 1 and 65535".to_string()),
                }],
            }],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        }
    }

    #[test]
    fn test_argument_validation_passes_with_valid_value() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert(
                "web_server".to_string(),
                create_module_with_port_validation(),
            );
            r
        };

        let call = ModuleCall {
            module_name: "web_server".to_string(),
            binding_name: Some("web".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("port".to_string(), Value::Int(443));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "web");
        assert!(result.is_ok());
    }

    #[test]
    fn test_argument_validation_passes_with_default_value() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert(
                "web_server".to_string(),
                create_module_with_port_validation(),
            );
            r
        };

        let call = ModuleCall {
            module_name: "web_server".to_string(),
            binding_name: Some("web".to_string()),
            arguments: HashMap::new(), // Uses default 8080
        };

        let result = resolver.expand_module_call(&call, "web");
        assert!(result.is_ok());
    }

    #[test]
    fn test_argument_validation_fails_with_invalid_value() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert(
                "web_server".to_string(),
                create_module_with_port_validation(),
            );
            r
        };

        let call = ModuleCall {
            module_name: "web_server".to_string(),
            binding_name: Some("web".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("port".to_string(), Value::Int(0));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "web");
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ModuleError::ArgumentValidationFailed {
                module,
                argument,
                message,
                actual,
            } => {
                assert_eq!(module, "web_server");
                assert_eq!(argument, "port");
                assert_eq!(message, "Port must be between 1 and 65535");
                assert_eq!(actual, "0");
            }
            other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_argument_validation_fails_with_negative_value() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert(
                "web_server".to_string(),
                create_module_with_port_validation(),
            );
            r
        };

        let call = ModuleCall {
            module_name: "web_server".to_string(),
            binding_name: Some("web".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("port".to_string(), Value::Int(-1));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "web");
        assert!(result.is_err());
    }

    #[test]
    fn test_argument_validation_fails_too_large() {
        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert(
                "web_server".to_string(),
                create_module_with_port_validation(),
            );
            r
        };

        let call = ModuleCall {
            module_name: "web_server".to_string(),
            binding_name: Some("web".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("port".to_string(), Value::Int(70000));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "web");
        assert!(result.is_err());
    }

    #[test]
    fn test_argument_validation_no_message_uses_default() {
        use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![ArgumentParameter {
                name: "count".to_string(),
                type_expr: TypeExpr::Int,
                default: None,
                description: None,
                validations: vec![ValidationBlock {
                    condition: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("count".to_string())),
                        op: CompareOp::Gt,
                        rhs: Box::new(ValidateExpr::Int(0)),
                    },
                    error_message: None,
                }],
            }],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("counter".to_string(), module);
            r
        };

        let call = ModuleCall {
            module_name: "counter".to_string(),
            binding_name: Some("c".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("count".to_string(), Value::Int(0));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "c");
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ModuleError::ArgumentValidationFailed { message, .. } => {
                assert_eq!(message, "validation failed for argument 'count'");
            }
            other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_argument_validation_len_with_list() {
        use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![ArgumentParameter {
                name: "tags".to_string(),
                type_expr: TypeExpr::List(Box::new(TypeExpr::String)),
                default: None,
                description: None,
                validations: vec![ValidationBlock {
                    condition: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::FunctionCall {
                            name: "len".to_string(),
                            args: vec![ValidateExpr::Var("tags".to_string())],
                        }),
                        op: CompareOp::Gte,
                        rhs: Box::new(ValidateExpr::Int(1)),
                    },
                    error_message: Some("At least one tag is required".to_string()),
                }],
            }],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("tagged".to_string(), module);
            r
        };

        // Valid: non-empty list
        let call = ModuleCall {
            module_name: "tagged".to_string(),
            binding_name: Some("t".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "tags".to_string(),
                    Value::List(vec![Value::String("env:prod".to_string())]),
                );
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "t").is_ok());

        // Invalid: empty list
        let call = ModuleCall {
            module_name: "tagged".to_string(),
            binding_name: Some("t".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("tags".to_string(), Value::List(vec![]));
                args
            },
        };
        let result = resolver.expand_module_call(&call, "t");
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::ArgumentValidationFailed { message, .. } => {
                assert_eq!(message, "At least one tag is required");
            }
            other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_require_block_passes() {
        use crate::parser::{RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![
                ArgumentParameter {
                    name: "enable_https".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                    description: None,
                    validations: Vec::new(),
                },
                ArgumentParameter {
                    name: "cert_arn".to_string(),
                    type_expr: TypeExpr::String,
                    default: Some(Value::String(
                        "arn:aws:acm:us-east-1:123:cert/abc".to_string(),
                    )),
                    description: None,
                    validations: Vec::new(),
                },
            ],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![RequireBlock {
                // !enable_https || cert_arn != null
                condition: ValidateExpr::Or(
                    Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                        "enable_https".to_string(),
                    )))),
                    Box::new(ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("cert_arn".to_string())),
                        op: CompareOp::Ne,
                        rhs: Box::new(ValidateExpr::Null),
                    }),
                ),
                error_message: "cert_arn is required when HTTPS is enabled".to_string(),
            }],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("web".to_string(), module);
            r
        };

        // HTTPS enabled with cert_arn provided: should pass
        let call = ModuleCall {
            module_name: "web".to_string(),
            binding_name: Some("w".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("enable_https".to_string(), Value::Bool(true));
                args.insert(
                    "cert_arn".to_string(),
                    Value::String("arn:aws:acm:us-east-1:123:cert/abc".to_string()),
                );
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "w").is_ok());
    }

    #[test]
    fn test_require_block_fails_with_not_expr() {
        use crate::parser::{RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![
                ArgumentParameter {
                    name: "enable_https".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(true)),
                    description: None,
                    validations: Vec::new(),
                },
                ArgumentParameter {
                    name: "has_cert".to_string(),
                    type_expr: TypeExpr::Bool,
                    default: Some(Value::Bool(false)),
                    description: None,
                    validations: Vec::new(),
                },
            ],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![RequireBlock {
                // !enable_https || has_cert
                condition: ValidateExpr::Or(
                    Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                        "enable_https".to_string(),
                    )))),
                    Box::new(ValidateExpr::Var("has_cert".to_string())),
                ),
                error_message: "cert is required when HTTPS is enabled".to_string(),
            }],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("web".to_string(), module);
            r
        };

        // HTTPS enabled but has_cert is false: should fail
        let call = ModuleCall {
            module_name: "web".to_string(),
            binding_name: Some("w".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("enable_https".to_string(), Value::Bool(true));
                args.insert("has_cert".to_string(), Value::Bool(false));
                args
            },
        };
        let result = resolver.expand_module_call(&call, "w");
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::RequireConstraintFailed { message, .. } => {
                assert_eq!(message, "cert is required when HTTPS is enabled");
            }
            other => panic!("Expected RequireConstraintFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_require_block_len_function() {
        use crate::parser::{RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![ArgumentParameter {
                name: "subnet_ids".to_string(),
                type_expr: TypeExpr::List(Box::new(TypeExpr::String)),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![RequireBlock {
                // len(subnet_ids) >= 2
                condition: ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::FunctionCall {
                        name: "len".to_string(),
                        args: vec![ValidateExpr::Var("subnet_ids".to_string())],
                    }),
                    op: CompareOp::Gte,
                    rhs: Box::new(ValidateExpr::Int(2)),
                },
                error_message: "ALB requires at least two subnets".to_string(),
            }],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("alb".to_string(), module);
            r
        };

        // Two subnets: should pass
        let call = ModuleCall {
            module_name: "alb".to_string(),
            binding_name: Some("lb".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "subnet_ids".to_string(),
                    Value::List(vec![
                        Value::String("subnet-a".to_string()),
                        Value::String("subnet-b".to_string()),
                    ]),
                );
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "lb").is_ok());

        // One subnet: should fail
        let call = ModuleCall {
            module_name: "alb".to_string(),
            binding_name: Some("lb".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "subnet_ids".to_string(),
                    Value::List(vec![Value::String("subnet-a".to_string())]),
                );
                args
            },
        };
        let result = resolver.expand_module_call(&call, "lb");
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::RequireConstraintFailed { message, .. } => {
                assert_eq!(message, "ALB requires at least two subnets");
            }
            other => panic!("Expected RequireConstraintFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_require_block_multiple_constraints() {
        use crate::parser::{RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![
                ArgumentParameter {
                    name: "min_size".to_string(),
                    type_expr: TypeExpr::Int,
                    default: None,
                    description: None,
                    validations: Vec::new(),
                },
                ArgumentParameter {
                    name: "max_size".to_string(),
                    type_expr: TypeExpr::Int,
                    default: None,
                    description: None,
                    validations: Vec::new(),
                },
            ],
            attribute_params: vec![],
            export_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![RequireBlock {
                // min_size <= max_size
                condition: ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("min_size".to_string())),
                    op: CompareOp::Lte,
                    rhs: Box::new(ValidateExpr::Var("max_size".to_string())),
                },
                error_message: "min_size must be <= max_size".to_string(),
            }],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
        };

        let resolver = {
            let mut r = ModuleResolver::new(".");
            r.imported_modules.insert("asg".to_string(), module);
            r
        };

        // min_size < max_size: should pass
        let call = ModuleCall {
            module_name: "asg".to_string(),
            binding_name: Some("a".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("min_size".to_string(), Value::Int(1));
                args.insert("max_size".to_string(), Value::Int(5));
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "a").is_ok());

        // min_size > max_size: should fail
        let call = ModuleCall {
            module_name: "asg".to_string(),
            binding_name: Some("a".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert("min_size".to_string(), Value::Int(10));
                args.insert("max_size".to_string(), Value::Int(5));
                args
            },
        };
        let result = resolver.expand_module_call(&call, "a");
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::RequireConstraintFailed { message, .. } => {
                assert_eq!(message, "min_size must be <= max_size");
            }
            other => panic!("Expected RequireConstraintFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_argument_type_mismatch_int_for_string() {
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
                // vpc_id expects string, pass int
                args.insert("vpc_id".to_string(), Value::Int(42));
                args
            },
        };

        let result = resolver.expand_module_call(&call, "my_instance");
        assert!(
            matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
            "Expected InvalidArgumentType error, got {:?}",
            result
        );
    }

    #[test]
    fn test_argument_type_mismatch_string_for_bool() {
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
                args.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
                // enable_flag expects bool, pass string
                args.insert(
                    "enable_flag".to_string(),
                    Value::String("not-a-bool".to_string()),
                );
                args
            },
        };

        let result = resolver.expand_module_call(&call, "my_instance");
        assert!(
            matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
            "Expected InvalidArgumentType error, got {:?}",
            result
        );
    }

    #[test]
    fn test_argument_type_custom_validator() {
        use crate::parser::ValidatorFn;

        // Create a ProviderContext with a custom "arn" validator
        let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
        validators.insert(
            "arn".to_string(),
            Box::new(|s: &str| {
                if s.starts_with("arn:") {
                    Ok(())
                } else {
                    Err(format!("expected ARN format, got '{}'", s))
                }
            }),
        );
        let config = ProviderContext {
            decryptor: None,
            validators,
            custom_type_validator: None,
        };

        let mut module = create_test_module();
        module.arguments = vec![ArgumentParameter {
            name: "policy_arn".to_string(),
            type_expr: TypeExpr::Simple("arn".to_string()),
            default: None,
            description: None,
            validations: Vec::new(),
        }];

        let resolver = {
            let mut r = ModuleResolver::with_config(".", &config);
            r.imported_modules.insert("test_module".to_string(), module);
            r
        };

        // Valid ARN passes
        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("a".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "policy_arn".to_string(),
                    Value::String("arn:aws:iam::123456789012:policy/MyPolicy".to_string()),
                );
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "a").is_ok());

        // Invalid ARN fails
        let call_bad = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("b".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "policy_arn".to_string(),
                    Value::String("not-an-arn".to_string()),
                );
                args
            },
        };
        let result = resolver.expand_module_call(&call_bad, "b");
        assert!(
            matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
            "Expected InvalidArgumentType error for invalid ARN, got {:?}",
            result
        );
    }

    #[test]
    fn test_argument_type_list_of_custom_type() {
        use crate::parser::ValidatorFn;

        let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
        validators.insert(
            "arn".to_string(),
            Box::new(|s: &str| {
                if s.starts_with("arn:") {
                    Ok(())
                } else {
                    Err(format!("expected ARN format, got '{}'", s))
                }
            }),
        );
        let config = ProviderContext {
            decryptor: None,
            validators,
            custom_type_validator: None,
        };

        let mut module = create_test_module();
        module.arguments = vec![ArgumentParameter {
            name: "policy_arns".to_string(),
            type_expr: TypeExpr::List(Box::new(TypeExpr::Simple("arn".to_string()))),
            default: None,
            description: None,
            validations: Vec::new(),
        }];

        let resolver = {
            let mut r = ModuleResolver::with_config(".", &config);
            r.imported_modules.insert("test_module".to_string(), module);
            r
        };

        // Valid list of ARNs
        let call = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("a".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "policy_arns".to_string(),
                    Value::List(vec![
                        Value::String("arn:aws:iam::123:policy/A".to_string()),
                        Value::String("arn:aws:iam::123:policy/B".to_string()),
                    ]),
                );
                args
            },
        };
        assert!(resolver.expand_module_call(&call, "a").is_ok());

        // List with invalid ARN fails
        let call_bad = ModuleCall {
            module_name: "test_module".to_string(),
            binding_name: Some("b".to_string()),
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "policy_arns".to_string(),
                    Value::List(vec![
                        Value::String("arn:aws:iam::123:policy/A".to_string()),
                        Value::String("not-an-arn".to_string()),
                    ]),
                );
                args
            },
        };
        let result = resolver.expand_module_call(&call_bad, "b");
        assert!(
            matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
            "Expected InvalidArgumentType for list with invalid ARN, got {:?}",
            result
        );
    }
}
