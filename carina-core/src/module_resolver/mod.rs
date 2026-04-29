//! Module Resolver - Resolve module imports and instantiations
//!
//! This module handles:
//! - Resolving import paths to module definitions
//! - Detecting circular dependencies between modules
//! - Validating module argument parameters
//! - Expanding module calls into resources
//!
//! ## Module structure
//!
//! - `validation`: Expression evaluator for `validate` and `require` blocks

mod validation;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::parser::{
    ModuleCall, ParseError, ParsedFile, ProviderContext, TypeExpr, UseStatement,
    validate_custom_type,
};
use crate::resource::{Expr, LifecycleConfig, Resource, ResourceId, ResourceKind, Value};
use validation::{evaluate_require_expr, evaluate_validate_expr, format_value_for_error};

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

    #[error(
        "Module path '{path}' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory."
    )]
    NotADirectory { path: String },
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

    /// Load and cache a module from a directory path.
    ///
    /// Modules are directory-scoped: a module is a directory containing one or
    /// more `.crn` files. Single-file modules are not supported — pass the
    /// module's directory, not an individual `.crn` file.
    pub fn load_module(&mut self, path: &str) -> Result<ParsedFile, ModuleError> {
        let full_path = self.resolve_path(path);

        // Distinguish "file exists but isn't a directory" from
        // "path doesn't exist / permission denied": only the former is the
        // single-file-module contract violation. Otherwise let canonicalize()
        // surface the underlying IO error (NotFound, PermissionDenied, ...).
        let metadata = full_path.metadata()?;
        if !metadata.is_dir() {
            return Err(ModuleError::NotADirectory {
                path: path.to_string(),
            });
        }

        let canonical = full_path.canonicalize()?;

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

        let mut parsed = match self.load_directory_module(&full_path) {
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
        // The module's directory is used for resolving its relative imports.
        if let Err(e) = self.resolve_nested_modules(&mut parsed, &full_path) {
            self.resolving.remove(&canonical);
            return Err(e);
        }

        // Remove from resolving set
        self.resolving.remove(&canonical);

        // Cache the module
        self.module_cache.insert(canonical, parsed.clone());

        Ok(parsed)
    }

    /// Load all .crn files from a directory and merge them into a single ParsedFile.
    fn load_directory_module(&self, dir_path: &Path) -> Result<ParsedFile, ModuleError> {
        let mut merged = ParsedFile::default();

        for file_path in sorted_crn_paths_in(dir_path)? {
            let content = fs::read_to_string(&file_path)?;
            let parsed = crate::parser::parse(&content, self.config)?;
            crate::config_loader::merge_parsed_file(&mut merged, parsed);
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
    pub fn process_imports(&mut self, imports: &[UseStatement]) -> Result<(), ModuleError> {
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
        if parsed.uses.is_empty() || parsed.module_calls.is_empty() {
            return Ok(());
        }

        // Save and temporarily replace the base_dir and imported_modules
        let original_base_dir = std::mem::replace(&mut self.base_dir, base_dir.to_path_buf());
        let original_imported = std::mem::take(&mut self.imported_modules);

        // Process the module's own imports
        let imports = parsed.uses.clone();
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
            let instance_prefix = instance_prefix_for_call(call);

            match self.expand_module_call(call, &instance_prefix) {
                Ok(expanded) => parsed.resources.extend(expanded), // allow: direct — module expansion, handled separately
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
            let new_name = format!("{}.{}", instance_prefix, new_resource.id.name_str());
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
            // Preserve user-authored attribute order across the
            // module-call expansion (#2222) — `IndexMap`, not `HashMap`.
            let mut substituted_attrs: IndexMap<String, Expr> = IndexMap::new();
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
            let mut virtual_attrs: IndexMap<String, Expr> = IndexMap::new();

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
                dependency_bindings: BTreeSet::new(),
                module_source: None,
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

/// Compute the instance prefix for a module call. Named calls use the
/// binding name; anonymous calls get `<module>_<16hex>` where the hex is a
/// SimHash of the call's module name + flattened arguments.
///
/// SimHash is locality-sensitive, so editing one argument flips only a few
/// bits — `reconcile_anonymous_module_instances` can then find the matching
/// state entry by Hamming distance and preserve the resource address across
/// argument edits.
pub fn instance_prefix_for_call(call: &ModuleCall) -> String {
    use std::collections::BTreeMap;

    if let Some(name) = &call.binding_name {
        return name.clone();
    }

    let mut features: BTreeMap<String, String> = BTreeMap::new();
    features.insert("_module".to_string(), call.module_name.clone());
    for (k, v) in &call.arguments {
        crate::identifier::flatten_value_for_simhash(k, v, &mut features);
    }
    let simhash = crate::identifier::compute_simhash(&features);
    format!("{}_{:016x}", call.module_name, simhash)
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
    resolver.process_imports(&parsed.uses)?;

    // Expand module calls
    for call in &parsed.module_calls {
        let instance_prefix = instance_prefix_for_call(call);
        let expanded = resolver.expand_module_call(call, &instance_prefix)?;
        parsed.resources.extend(expanded); // allow: direct — module expansion, handled separately
    }

    Ok(())
}

/// Split a module-instance prefix into `(module_name, simhash)` when the
/// tail looks like a 16-hex SimHash. Returns `None` for non-synthetic prefixes
/// (user-written binding names, pre-SimHash state formats, etc.).
fn parse_synthetic_instance_prefix(prefix: &str) -> Option<(&str, u64)> {
    let (module, hex) = prefix.rsplit_once('_')?;
    if hex.len() != 16 {
        return None;
    }
    let simhash = u64::from_str_radix(hex, 16).ok()?;
    if module.is_empty() {
        return None;
    }
    Some((module, simhash))
}

/// Split a resource name into `(instance_prefix, rest)` at the first `.`, or
/// return `None` if it has no dot (no module instance prefix at all).
fn split_instance_prefix(name: &str) -> Option<(&str, &str)> {
    name.split_once('.')
}

/// Reconcile anonymous module-instance prefixes with existing state.
///
/// When a user edits an argument of an anonymous module call, its SimHash
/// prefix shifts a few bits. The expanded resources therefore live under a
/// new address (e.g. `thing_ab12….role` → `thing_cd34….role`) and would
/// otherwise look like destroy + create to the differ. This pass detects the
/// case by Hamming-distance matching: for each current DSL instance prefix
/// whose address is absent from state, find a state-only prefix for the same
/// module within `SIMHASH_HAMMING_THRESHOLD` bits; if exactly one candidate
/// qualifies, rewrite the current resources to use the state address.
///
/// `find_state_names_by_type` returns every state resource name for a given
/// `(provider, resource_type)` — the reconciler uses them to discover which
/// instance prefixes already exist in state.
pub fn reconcile_anonymous_module_instances(
    resources: &mut [Resource],
    find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
) {
    use std::collections::{HashMap, HashSet};

    // Collect current (provider, resource_type) pairs that appear in the
    // expanded DSL — we'll query state for matching entries.
    let mut touched_types: HashSet<(String, String)> = HashSet::new();
    for r in resources.iter() {
        if split_instance_prefix(r.id.name_str()).is_none() {
            continue;
        }
        touched_types.insert((r.id.provider.clone(), r.id.resource_type.clone()));
    }

    if touched_types.is_empty() {
        return;
    }

    // Current DSL synthetic prefixes per module — only one entry per
    // distinct prefix (a multi-resource module instance shares one prefix
    // across all of its resources).
    let mut current_synthetic_by_module: HashMap<String, HashSet<u64>> = HashMap::new();
    for r in resources.iter() {
        let Some((prefix, _)) = split_instance_prefix(r.id.name_str()) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        current_synthetic_by_module
            .entry(module.to_string())
            .or_default()
            .insert(simhash);
    }

    // State synthetic prefixes per module. Use a set so a multi-resource
    // module instance — which contributes one state entry per resource
    // type, all under the same prefix — collapses to one candidate. With a
    // Vec the same hash would appear N times and the Hamming-distance
    // search below would mistake duplicates for ambiguous candidates and
    // refuse to remap (#2211).
    let mut state_synthetic_by_module: HashMap<String, HashSet<u64>> = HashMap::new();

    for (provider, resource_type) in &touched_types {
        for name in find_state_names_by_type(provider, resource_type) {
            let Some((prefix, _)) = split_instance_prefix(&name) else {
                continue;
            };
            let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
                continue;
            };
            state_synthetic_by_module
                .entry(module.to_string())
                .or_default()
                .insert(simhash);
        }
    }

    // For each current DSL prefix that has no matching state prefix, find the
    // closest orphan state prefix for the same module. Candidate state hashes
    // exclude any prefix already used by a current DSL instance — without
    // that filter, two distinct anonymous calls could collapse onto the same
    // state entry when only one of them existed before.
    let mut prefix_remap: HashMap<(String, u64), u64> = HashMap::new();
    for (module, current_hashes) in &current_synthetic_by_module {
        let Some(state_hashes) = state_synthetic_by_module.get(module) else {
            continue;
        };
        let orphan_state_hashes: Vec<u64> = state_hashes
            .iter()
            .copied()
            .filter(|h| !current_hashes.contains(h))
            .collect();
        if orphan_state_hashes.is_empty() {
            continue;
        }
        for current_hash in current_hashes {
            if state_hashes.contains(current_hash) {
                continue;
            }
            if let Some(state_hash) = crate::identifier::closest_unique_simhash_match(
                *current_hash,
                orphan_state_hashes.iter().copied(),
                |h| h,
            ) {
                prefix_remap.insert((module.clone(), *current_hash), state_hash);
            }
        }
    }

    if prefix_remap.is_empty() {
        return;
    }

    // Apply remaps: rewrite `id.name` and `binding` for every resource whose
    // instance prefix is in the remap table.
    for r in resources.iter_mut() {
        let Some((prefix, rest)) = split_instance_prefix(r.id.name_str()) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        if let Some(&target) = prefix_remap.get(&(module.to_string(), simhash)) {
            let new_prefix = format!("{}_{:016x}", module, target);
            let new_name = format!("{}.{}", new_prefix, rest);
            r.id = ResourceId::with_provider(&r.id.provider, &r.id.resource_type, new_name.clone());
            if let Some(ref binding) = r.binding
                && let Some((_, binding_rest)) = split_instance_prefix(binding)
            {
                r.binding = Some(format!("{}.{}", new_prefix, binding_rest));
            }
            if let Some(crate::resource::ModuleSource::Module { name, instance: _ }) =
                &r.module_source
            {
                r.module_source = Some(crate::resource::ModuleSource::Module {
                    name: name.clone(),
                    instance: new_prefix.clone(),
                });
            }
        }
    }

    // After remapping resource names, intra-module ResourceRefs also point at
    // bindings with the old prefix. Walk every value and rewrite those.
    for r in resources.iter_mut() {
        let mut replacements = Vec::new();
        for (key, expr) in r.attributes.iter() {
            let rewritten = rewrite_ref_prefixes(&expr.0, &prefix_remap);
            if rewritten != expr.0 {
                replacements.push((key.clone(), rewritten));
            }
        }
        for (key, new_value) in replacements {
            r.set_attr(key, new_value);
        }
    }
}

fn rewrite_ref_prefixes(
    value: &Value,
    remap: &std::collections::HashMap<(String, u64), u64>,
) -> Value {
    match value {
        Value::ResourceRef { path } => {
            let binding = path.binding();
            if let Some((prefix, rest)) = binding.split_once('.')
                && let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix)
                && let Some(&target) = remap.get(&(module.to_string(), simhash))
            {
                let new_binding = format!("{}_{:016x}.{}", module, target, rest);
                return Value::resource_ref(
                    new_binding,
                    path.attribute().to_string(),
                    path.field_path().into_iter().map(String::from).collect(),
                );
            }
            value.clone()
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), rewrite_ref_prefixes(v, remap)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            Value::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        InterpolationPart::Literal(s) => InterpolationPart::Literal(s.clone()),
                        InterpolationPart::Expr(v) => {
                            InterpolationPart::Expr(rewrite_ref_prefixes(v, remap))
                        }
                    })
                    .collect(),
            )
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        },
        _ => value.clone(),
    }
}

/// Get parsed file info for display (supports both module definitions and root configs)
pub fn get_parsed_file(path: &Path) -> Result<ParsedFile, ModuleError> {
    let content = fs::read_to_string(path)?;
    let parsed = crate::parser::parse(&content, &ProviderContext::default())?;
    Ok(parsed)
}

/// Load a module from a directory path.
///
/// Modules are directory-scoped: all `.crn` files in the directory are merged
/// uniformly, with no file name (including `main.crn`) treated as privileged.
///
/// Returns `None` if `path` is not a directory, cannot be read/parsed, or
/// contains no module definitions (no inputs or outputs).
pub fn load_module(path: &Path) -> Option<ParsedFile> {
    if !path.is_dir() {
        return None;
    }
    load_directory_module(path)
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
        TypeExpr::Struct { fields } => {
            let Value::Map(entries) = value else {
                return TypeCheckResult::Mismatch;
            };
            if crate::validation::struct_field_shape_errors(fields, entries).is_some() {
                return TypeCheckResult::Mismatch;
            }
            for (name, ty) in fields {
                if let Some(v) = entries.get(name) {
                    match check_type_match(ty, v, config) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
            }
            TypeCheckResult::Ok
        }
    }
}

/// Collect `.crn` file paths directly inside `dir_path`, sorted by path.
///
/// Sorting is load-bearing: merged `ParsedFile` vectors inherit this order,
/// and downstream consumers (LSP first-match-wins lookups, CLI diagnostic
/// ordering) must not depend on filesystem iteration order, which varies
/// across ext4/APFS/tmpfs.
fn sorted_crn_paths_in(dir_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut crn_files: Vec<PathBuf> = fs::read_dir(dir_path)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "crn"))
        .collect();
    crn_files.sort();
    Ok(crn_files)
}

/// Load all `.crn` files from a directory and merge them into a single `ParsedFile`.
///
/// Returns `None` if the directory cannot be read or contains no module
/// definitions (no arguments/attributes).
pub fn load_directory_module(dir_path: &Path) -> Option<ParsedFile> {
    let mut merged = ParsedFile::default();

    for path in sorted_crn_paths_in(dir_path).ok()? {
        if let Ok(content) = fs::read_to_string(&path)
            && let Ok(parsed) = crate::parser::parse(&content, &ProviderContext::default())
        {
            crate::config_loader::merge_parsed_file(&mut merged, parsed);
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
/// Unlike [`load_directory_module`], this returns a `Result` with descriptive
/// error messages and does not check for module definitions (inputs/outputs).
pub fn load_module_from_directory(dir: &Path) -> Result<ParsedFile, String> {
    let paths = sorted_crn_paths_in(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut merged = ParsedFile::default();
    for path in paths {
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let parsed = crate::parser::parse(&content, &ProviderContext::default())
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
        crate::config_loader::merge_parsed_file(&mut merged, parsed);
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
                dependency_bindings: BTreeSet::new(),
                module_source: None,
            }],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
        assert_eq!(sg.id.name_str(), "my_instance.sg");
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
                    id: ResourceId::new("ec2.Vpc", "main_vpc"),
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
                    dependency_bindings: BTreeSet::new(),
                    module_source: None,
                },
                Resource {
                    id: ResourceId::new("ec2.Subnet", "sub"),
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
                    dependency_bindings: BTreeSet::new(),
                    module_source: None,
                },
            ],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
        assert_eq!(expanded_a[0].id.name_str(), "prod.main_vpc");
        assert_eq!(expanded_b[0].id.name_str(), "staging.main_vpc");
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
                dependency_bindings: BTreeSet::new(),
                module_source: None,
            }],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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

    /// Regression fixtures for #2197. Writes a minimal `modules/thing` module
    /// (one `awscc.iam.Role` whose `role_name` comes from a `name` argument)
    /// and a `root/main.crn` with the caller-supplied body; returns the parsed
    /// root with modules already resolved.
    fn resolve_thing_fixture(root_body: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().expect("tempdir");
        let module_dir = tmp.path().join("modules/thing");
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(
            module_dir.join("main.crn"),
            r#"
arguments {
  name: String
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
        )
        .unwrap();

        let root_dir = tmp.path().join("root");
        fs::create_dir_all(&root_dir).unwrap();
        fs::write(root_dir.join("main.crn"), root_body).unwrap();

        let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
        let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
        resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");
        parsed
    }

    fn role_names(parsed: &ParsedFile) -> HashSet<String> {
        parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .filter_map(|r| match r.get_attr("role_name")? {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_anonymous_module_calls_get_distinct_prefixes() {
        let call_a = ModuleCall {
            module_name: "github".to_string(),
            binding_name: None,
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "github_repo".to_string(),
                    Value::String("carina-rs/infra".to_string()),
                );
                args
            },
        };
        let call_b = ModuleCall {
            module_name: "github".to_string(),
            binding_name: None,
            arguments: {
                let mut args = HashMap::new();
                args.insert(
                    "github_repo".to_string(),
                    Value::String("carina-rs/other".to_string()),
                );
                args
            },
        };

        let a = instance_prefix_for_call(&call_a);
        let b = instance_prefix_for_call(&call_b);
        assert_ne!(a, b);
        assert!(
            a.starts_with("github_"),
            "expected `github_<16hex>`, got {a}"
        );
        assert_eq!(
            a.len(),
            "github_".len() + 16,
            "expected 16 hex chars in {a}"
        );
    }

    // SimHash is locality-sensitive: editing one argument must flip only a few
    // bits so reconciliation can find the state entry. Assert the Hamming
    // distance is below the reconciliation threshold.
    #[test]
    fn test_anonymous_module_call_prefix_is_locality_sensitive() {
        let make = |repo: &str| ModuleCall {
            module_name: "github".to_string(),
            binding_name: None,
            arguments: {
                let mut args = HashMap::new();
                args.insert("github_repo".to_string(), Value::String(repo.to_string()));
                args.insert(
                    "role_name".to_string(),
                    Value::String("github-actions".to_string()),
                );
                args.insert(
                    "managed_policy_arns".to_string(),
                    Value::List(vec![Value::String(
                        "arn:aws:iam::aws:policy/AdministratorAccess".to_string(),
                    )]),
                );
                args
            },
        };

        let a = instance_prefix_for_call(&make("carina-rs/infra"));
        let b = instance_prefix_for_call(&make("carina-rs/other"));
        let parse = |p: &str| parse_synthetic_instance_prefix(p).unwrap().1;
        let distance = (parse(&a) ^ parse(&b)).count_ones();
        assert!(
            distance < crate::identifier::SIMHASH_HAMMING_THRESHOLD,
            "small edit should stay inside the reconciliation threshold, got distance {distance}",
        );
    }

    #[test]
    fn test_named_module_call_uses_binding_name() {
        let call = ModuleCall {
            module_name: "github".to_string(),
            binding_name: Some("prod".to_string()),
            arguments: HashMap::new(),
        };
        assert_eq!(instance_prefix_for_call(&call), "prod");
    }

    #[test]
    fn test_anonymous_module_calls_expand_into_distinct_instances() {
        let parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'alpha' }
thing { name = 'beta'  }
"#,
        );

        let role_addresses: HashSet<String> = parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .map(|r| r.id.name_str().to_string())
            .collect();
        assert_eq!(role_addresses.len(), 2, "got {:?}", role_addresses);

        assert_eq!(
            role_names(&parsed),
            ["alpha".to_string(), "beta".to_string()]
                .into_iter()
                .collect::<HashSet<_>>(),
        );
    }

    #[test]
    fn test_mixed_named_and_anonymous_module_calls_coexist() {
        let parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

let named = thing { name = 'named-call' }
thing              { name = 'anon-call'  }
"#,
        );

        assert_eq!(
            role_names(&parsed),
            ["named-call".to_string(), "anon-call".to_string()]
                .into_iter()
                .collect::<HashSet<_>>(),
        );

        let addrs: Vec<&str> = parsed
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert!(addrs.iter().any(|n| n.starts_with("named.")), "{:?}", addrs);
        assert!(addrs.iter().any(|n| n.starts_with("thing_")), "{:?}", addrs);
    }

    // Reconciliation: an argument edit moves the SimHash prefix by a few bits;
    // if the old prefix is in state and the new one is not, the reconciler
    // must rewrite the expanded resources to use the state address.
    #[test]
    fn test_reconcile_anonymous_module_instances_remaps_close_prefix() {
        let mut parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
        );

        let before: Vec<String> = parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .map(|r| r.id.name_str().to_string())
            .collect();
        assert_eq!(before.len(), 1);
        let (new_prefix, _) = before[0].split_once('.').unwrap();
        let (module, new_hash) = parse_synthetic_instance_prefix(new_prefix).unwrap();
        assert_eq!(module, "thing");

        // Fabricate a state entry whose SimHash is within threshold of the
        // current one (flip one bit).
        let state_hash = new_hash ^ 1;
        let state_name = format!("thing_{:016x}.role", state_hash);
        let state_lookup = |_: &str, _: &str| vec![state_name.clone()];

        reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

        let after: Vec<String> = parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .map(|r| r.id.name_str().to_string())
            .collect();
        assert_eq!(
            after,
            vec![state_name.clone()],
            "expected prefix to be remapped to state's",
        );
        let role = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap();
        assert_eq!(
            role.binding.as_deref(),
            Some(format!("thing_{:016x}.role", state_hash).as_str()),
            "binding should be remapped too",
        );
    }

    // Reconciliation must not cross module names: a `foo_<hash>` state entry
    // has nothing to do with a current `bar_<hash>` DSL instance.
    #[test]
    fn test_reconcile_anonymous_module_instances_ignores_other_modules() {
        let mut parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
        );

        let before_name: String = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .name_str()
            .to_string();

        // State entry uses a different module name.
        let state_lookup = |_: &str, _: &str| vec!["other_0000000000000001.role".to_string()];
        reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

        let after_name = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .name_str()
            .to_string();
        assert_eq!(before_name, after_name);
    }

    // Regression for #2211: a single anonymous module instance whose module
    // expands to multiple resource types means the same state prefix shows up
    // once per resource type when `find_state_names_by_type` is queried per
    // (provider, type). The reconciler must treat repeated identical hashes
    // as the same candidate, not as multiple ambiguous candidates.
    #[test]
    fn test_reconcile_anonymous_module_instances_dedups_state_prefixes_across_types() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let module_dir = tmp.path().join("modules/thing");
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(
            module_dir.join("main.crn"),
            r#"
arguments {
  name: String
}

let provider_res = awscc.iam.OidcProvider {
  url             = 'https://example.com'
  client_id_list  = ['x']
  thumbprint_list = ['y']
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
        )
        .unwrap();

        let root_dir = tmp.path().join("root");
        fs::create_dir_all(&root_dir).unwrap();
        fs::write(
            root_dir.join("main.crn"),
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
        )
        .unwrap();

        let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
        let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
        resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");

        // Discover the new prefix from the parsed Role.
        let role_name_before = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .name_str()
            .to_string();
        let (new_prefix, _) = role_name_before.split_once('.').unwrap();
        let (_, new_hash) = parse_synthetic_instance_prefix(new_prefix).unwrap();

        // State holds the *same* instance prefix at two resource types, one
        // bit away from the current SimHash — i.e. a small argument edit.
        let state_hash = new_hash ^ 1;
        let state_lookup = move |_: &str, resource_type: &str| match resource_type {
            "iam.OidcProvider" => vec![format!("thing_{:016x}.provider_res", state_hash)],
            "iam.Role" => vec![format!("thing_{:016x}.role", state_hash)],
            _ => vec![],
        };

        reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

        let role_after = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap();
        assert_eq!(
            role_after.id.name_str(),
            format!("thing_{:016x}.role", state_hash),
            "Role address must be remapped to the state prefix",
        );
        let provider_after = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.OidcProvider")
            .unwrap();
        assert_eq!(
            provider_after.id.name_str(),
            format!("thing_{:016x}.provider_res", state_hash),
            "OidcProvider address must be remapped to the state prefix",
        );
    }

    // Reconciliation must not run when there are multiple candidate state
    // prefixes within threshold — ambiguity means we can't tell which is the
    // "same instance."
    #[test]
    fn test_reconcile_anonymous_module_instances_skips_ambiguous() {
        let mut parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
        );

        let before_name = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .name_str()
            .to_string();
        let (prefix, _) = before_name.split_once('.').unwrap();
        let (_, cur_hash) = parse_synthetic_instance_prefix(prefix).unwrap();

        // Two state entries at the same Hamming distance — ambiguous.
        let state_lookup = move |_: &str, _: &str| {
            vec![
                format!("thing_{:016x}.role", cur_hash ^ 0b1),
                format!("thing_{:016x}.role", cur_hash ^ 0b10),
            ]
        };
        reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

        let after_name = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "iam.Role")
            .unwrap()
            .id
            .name_str()
            .to_string();
        assert_eq!(before_name, after_name, "ambiguous match must not remap");
    }

    // When state holds prefix A and the DSL has both A (unchanged) and a new
    // A' (a new anonymous call with similar args), A' must not be remapped
    // onto A — they are two distinct instances even though their SimHashes
    // are close. State prefixes already in use by current DSL must not serve
    // as remap candidates.
    #[test]
    fn test_reconcile_anonymous_module_instances_does_not_steal_in_use_prefix() {
        let mut parsed = resolve_thing_fixture(
            r#"
let thing = use { source = '../modules/thing' }

thing { name = 'unchanged' }
thing { name = 'unchanged-but-different' }
"#,
        );

        let prefixes_before: HashSet<String> = parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .map(|r| r.id.name_str().split_once('.').unwrap().0.to_string())
            .collect();
        assert_eq!(prefixes_before.len(), 2);
        let mut iter = prefixes_before.iter();
        let first = iter.next().unwrap().clone();
        let _second = iter.next().unwrap().clone();

        // State only holds the *first* prefix. The reconciler must not
        // remap the second instance onto it.
        let first_clone = first.clone();
        let state_lookup = move |_: &str, _: &str| vec![format!("{}.role", first_clone)];
        reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

        let prefixes_after: HashSet<String> = parsed
            .resources
            .iter()
            .filter(|r| r.id.resource_type == "iam.Role")
            .map(|r| r.id.name_str().split_once('.').unwrap().0.to_string())
            .collect();
        assert_eq!(
            prefixes_after, prefixes_before,
            "in-use state prefix must not be reassigned to a different DSL instance",
        );
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
        assert_eq!(sg.id.name_str(), "my_instance.sg");
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
        assert_eq!(expanded[0].id.name_str(), "prod.main_vpc");
        assert_eq!(expanded[1].id.name_str(), "prod.sub");

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
                id: ResourceId::new("ec2.Vpc", "vpc"),
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
                dependency_bindings: BTreeSet::new(),
                module_source: None,
            }],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
            resource_types.iter().any(|t| t.ends_with(".Vpc")),
            "Should contain VPC resource from inner module, got: {:?}",
            resource_types
        );
        assert!(
            resource_types.iter().any(|t| t.ends_with(".SecurityGroup")),
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
            resource_types.iter().any(|t| t.ends_with(".Vpc")),
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
    fn test_load_module_missing_path_cleans_resolving_set() {
        // Nonexistent import path => descriptive IO error (NotFound), not the
        // single-file-module contract error. The resolving set must be cleaned
        // up so a retry does not masquerade as a circular import.
        let tmp_dir = std::env::temp_dir().join("carina_test_missing_path_cleanup");
        let _ = fs::create_dir_all(&tmp_dir);

        let mut resolver = ModuleResolver::new(&tmp_dir);

        let err = resolver
            .load_module("nonexistent")
            .expect_err("expected error");
        assert!(
            matches!(&err, ModuleError::Io(_)),
            "expected Io error for a nonexistent path, got: {err:?}"
        );

        let err = resolver
            .load_module("nonexistent")
            .expect_err("expected error");
        assert!(
            matches!(&err, ModuleError::Io(_)),
            "expected Io error on second attempt, not CircularImport, got: {err:?}"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_load_module_parse_error_cleans_resolving_set() {
        let tmp_root = std::env::temp_dir().join("carina_test_parse_error_cleanup");
        let _ = fs::remove_dir_all(&tmp_root);
        let bad_module_dir = tmp_root.join("bad_module");
        fs::create_dir_all(&bad_module_dir).unwrap();
        fs::write(
            bad_module_dir.join("main.crn"),
            "this is not valid carina syntax {{{{",
        )
        .unwrap();

        let mut resolver = ModuleResolver::new(&tmp_root);

        // First attempt: parse error on a directory module with a bad .crn file.
        let result = resolver.load_module("bad_module");
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
        // (the resolving set must have been cleaned up).
        let result = resolver.load_module("bad_module");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, ModuleError::Parse(_)),
            "expected Parse error on second attempt, not CircularImport, got: {err:?}"
        );

        let _ = fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn test_load_module_rejects_file_path() {
        // Issue #1997: Modules must be directories. A single `.crn` file as a
        // module target should be rejected with NotADirectory instead of being
        // parsed as a one-file module.
        let tmp_root = std::env::temp_dir().join("carina_test_module_rejects_file");
        let _ = fs::remove_dir_all(&tmp_root);
        fs::create_dir_all(&tmp_root).unwrap();
        fs::write(tmp_root.join("single.crn"), "arguments {\n  x: String\n}\n").unwrap();

        let mut resolver = ModuleResolver::new(&tmp_root);
        let err = resolver
            .load_module("single.crn")
            .expect_err("a single .crn file must not be loadable as a module");
        assert!(
            matches!(&err, ModuleError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}"
        );

        let _ = fs::remove_dir_all(&tmp_root);
    }

    /// Helper to create a module with a validated port argument
    fn create_module_with_port_validation() -> ParsedFile {
        use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
        ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
            warnings: vec![],
            deferred_for_expressions: vec![],
            string_literal_paths: HashSet::new(),
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
        use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
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
            string_literal_paths: HashSet::new(),
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
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
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
            string_literal_paths: HashSet::new(),
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
        use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
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
            string_literal_paths: HashSet::new(),
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
        use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
        let module = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: IndexMap::new(),
            uses: vec![],
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
            upstream_states: vec![],
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
            string_literal_paths: HashSet::new(),
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
            schema_types: Default::default(),
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
            schema_types: Default::default(),
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

    #[test]
    fn test_load_module_directory_merges_sibling_files_with_main() {
        // A directory-based module that splits definitions across main.crn and
        // sibling files (arguments.crn, exports.crn, resources.crn) must be
        // parsed as a whole. The previous behavior returned only main.crn's
        // contents when main.crn existed, silently dropping siblings.
        let tmp_dir = std::env::temp_dir().join("carina_test_load_module_sibling_merge");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        fs::write(tmp_dir.join("main.crn"), "# main module file\n").unwrap();
        fs::write(
            tmp_dir.join("arguments.crn"),
            "arguments {\n  env: String\n}\n",
        )
        .unwrap();
        fs::write(
            tmp_dir.join("exports.crn"),
            "exports {\n  region = \"ap-northeast-1\"\n}\n",
        )
        .unwrap();

        let parsed = load_module(&tmp_dir)
            .expect("expected module to load because arguments.crn declares an argument");

        assert_eq!(
            parsed.arguments.len(),
            1,
            "arguments declared in arguments.crn must be preserved when main.crn exists"
        );
        assert_eq!(parsed.arguments[0].name, "env");
        assert_eq!(
            parsed.export_params.len(),
            1,
            "exports declared in exports.crn must be preserved when main.crn exists"
        );
        assert_eq!(parsed.export_params[0].name, "region");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_load_module_directory_merge_order_is_deterministic() {
        // Merged vectors must be ordered by file path so that downstream
        // first-match-wins lookups (hover, completion, diagnostics) do not
        // depend on filesystem iteration order.
        let tmp_dir = std::env::temp_dir().join("carina_test_load_module_merge_order");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create files out of lexicographic order to make the sort observable.
        fs::write(tmp_dir.join("z_last.crn"), "arguments {\n  c: String\n}\n").unwrap();
        fs::write(tmp_dir.join("a_first.crn"), "arguments {\n  a: String\n}\n").unwrap();
        fs::write(
            tmp_dir.join("m_middle.crn"),
            "arguments {\n  b: String\n}\n",
        )
        .unwrap();

        let parsed = load_module(&tmp_dir).expect("module should load");
        let names: Vec<&str> = parsed.arguments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "b", "c"],
            "arguments must be merged in sorted filename order"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
