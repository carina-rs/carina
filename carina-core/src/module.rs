//! Module - Module definitions and dependency analysis
//!
//! This module provides types for analyzing module structure and dependencies.

use std::collections::{HashMap, HashSet};

use crate::parser::{ResourceTypePath, TypeExpr};
use crate::resource::Value;

/// Dependency between resources
#[derive(Debug, Clone)]
pub struct Dependency {
    /// Target resource binding name
    pub target: String,
    /// Referenced attribute (e.g., "id")
    pub attribute: String,
    /// Where this reference is used (e.g., "security_group_id")
    pub used_in: String,
}

/// Dependency graph for resources within a module
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    /// Resource binding name -> list of dependencies
    pub edges: HashMap<String, Vec<Dependency>>,
    /// Reverse edges: target -> list of resources that depend on it
    pub reverse_edges: HashMap<String, Vec<String>>,
}

impl DependencyGraph {
    /// Create a new empty dependency graph
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a dependency edge
    pub fn add_edge(&mut self, from: String, dependency: Dependency) {
        let target = dependency.target.clone();
        self.edges.entry(from.clone()).or_default().push(dependency);
        self.reverse_edges.entry(target).or_default().push(from);
    }

    /// Get resources that have no dependencies (root resources)
    pub fn root_resources(&self) -> Vec<String> {
        let all_sources: HashSet<_> = self.edges.keys().cloned().collect();
        let all_targets: HashSet<_> = self.reverse_edges.keys().cloned().collect();

        // Resources that depend on others but nothing depends on them
        all_sources.difference(&all_targets).cloned().collect()
    }

    /// Get resources that nothing depends on (leaf resources)
    pub fn leaf_resources(&self) -> Vec<String> {
        let all_with_deps: HashSet<_> = self.edges.keys().cloned().collect();
        let all_depended_on: HashSet<_> = self.reverse_edges.keys().cloned().collect();

        // Resources that are depended on but don't depend on others
        all_depended_on
            .difference(&all_with_deps)
            .cloned()
            .collect()
    }

    /// Get direct dependencies of a resource
    pub fn dependencies_of(&self, resource: &str) -> &[Dependency] {
        self.edges.get(resource).map_or(&[], |v| v.as_slice())
    }

    /// Get resources that depend on this resource
    pub fn dependents_of(&self, resource: &str) -> &[String] {
        self.reverse_edges
            .get(resource)
            .map_or(&[], |v| v.as_slice())
    }

    /// Check if the graph has any cycles
    pub fn has_cycle(&self) -> bool {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node in self.edges.keys() {
            if self.has_cycle_util(node, &mut visited, &mut rec_stack) {
                return true;
            }
        }
        false
    }

    fn has_cycle_util(
        &self,
        node: &str,
        visited: &mut HashSet<String>,
        rec_stack: &mut HashSet<String>,
    ) -> bool {
        if rec_stack.contains(node) {
            return true;
        }
        if visited.contains(node) {
            return false;
        }

        visited.insert(node.to_string());
        rec_stack.insert(node.to_string());

        if let Some(deps) = self.edges.get(node) {
            for dep in deps {
                if self.has_cycle_util(&dep.target, visited, rec_stack) {
                    return true;
                }
            }
        }

        rec_stack.remove(node);
        false
    }
}

/// Format a Value for display
fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => {
            if s.len() > 50 {
                format!("\"{}...\"", &s[..47])
            } else {
                format!("\"{}\"", s)
            }
        }
        Value::Int(n) => n.to_string(),
        Value::Float(f) => {
            let s = f.to_string();
            if s.contains('.') {
                s
            } else {
                format!("{}.0", s)
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::List(items) => {
            if items.is_empty() {
                "[]".to_string()
            } else if items.len() <= 3 {
                let strs: Vec<_> = items.iter().map(format_value).collect();
                format!("[{}]", strs.join(", "))
            } else {
                format!("[{} items]", items.len())
            }
        }
        Value::StringList(items) => {
            if items.is_empty() {
                "[]".to_string()
            } else if items.len() <= 3 {
                let strs: Vec<_> = items.iter().map(|s| format!("\"{}\"", s)).collect();
                format!("[{}]", strs.join(", "))
            } else {
                format!("[{} items]", items.len())
            }
        }
        Value::Map(map) => {
            if map.is_empty() {
                "{}".to_string()
            } else {
                format!("{{...{} keys}}", map.len())
            }
        }
        Value::ResourceRef { path } => {
            if path.attribute().is_empty() {
                path.binding().to_string()
            } else {
                format!("{}.{}", path.binding(), path.attribute())
            }
        }
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let inner: String = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => s.clone(),
                    InterpolationPart::Expr(v) => format!("${{{}}}", format_value(v)),
                })
                .collect();
            format!("\"{}\"", inner)
        }
        Value::FunctionCall { name, args } => {
            let arg_strs: Vec<_> = args.iter().map(format_value).collect();
            format!("{}({})", name, arg_strs.join(", "))
        }
        Value::Secret(_) => "(secret)".to_string(),
        Value::Unknown(reason) => crate::value::render_unknown(reason),
    }
}

/// Typed argument parameter for module signature
#[derive(Debug, Clone)]
pub struct TypedArgument {
    /// Argument parameter name
    pub name: String,
    /// Type expression (including ref types)
    pub type_expr: TypeExpr,
    /// Whether this argument is required (no default)
    pub required: bool,
    /// Default value as a display string
    pub default: Option<String>,
    /// Optional description (from block form)
    pub description: Option<String>,
}

/// Resource creation entry in module signature
#[derive(Debug, Clone)]
pub struct ResourceCreation {
    /// Binding name for this resource
    pub binding_name: String,
    /// Full resource type path (e.g., aws.security_group)
    pub resource_type: ResourceTypePath,
    /// Dependencies on other resources or arguments
    pub dependencies: Vec<TypedDependency>,
}

/// Typed attribute parameter for module signature
#[derive(Debug, Clone)]
pub struct TypedAttributeParam {
    /// Attribute parameter name
    pub name: String,
    /// Type expression (including ref types), None if inferred
    pub type_expr: Option<TypeExpr>,
    /// Source binding name (if the output comes from a resource)
    pub source_binding: Option<String>,
}

/// Typed dependency representing a reference from one resource to another
#[derive(Debug, Clone)]
pub struct TypedDependency {
    /// Target binding name (e.g., "vpc", "cidr_block")
    pub target: String,
    /// Target resource type (if known)
    pub target_type: Option<ResourceTypePath>,
    /// Attribute being referenced (e.g., "id")
    pub attribute: String,
    /// Where this reference is used (e.g., "vpc_id")
    pub used_in: String,
}

/// Typed dependency graph with resource type information
#[derive(Debug, Clone, Default)]
pub struct TypedDependencyGraph {
    /// Resource binding name -> list of typed dependencies
    pub edges: HashMap<String, Vec<TypedDependency>>,
}

impl TypedDependencyGraph {
    /// Create a new empty typed dependency graph
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a typed dependency edge
    pub fn add_edge(&mut self, from: String, dependency: TypedDependency) {
        self.edges.entry(from).or_default().push(dependency);
    }

    /// Get dependencies for a resource
    pub fn dependencies_of(&self, resource: &str) -> &[TypedDependency] {
        self.edges.get(resource).map_or(&[], |v| v.as_slice())
    }
}

/// ANSI color codes for terminal output
/// Shared state for recursive dependency tree drawing.
///
/// Groups the output buffer, visited set, and color configuration that are
/// threaded through every recursive call of `display_creates_tree_colored`.
struct TreeDrawState<'a> {
    output: &'a mut String,
    visited: &'a mut HashSet<String>,
    colors: &'a Colors,
}

struct Colors {
    bold: &'static str,
    reset: &'static str,
    dim: &'static str,
    green: &'static str,
    yellow: &'static str,
    blue: &'static str,
    cyan: &'static str,
    white: &'static str,
}

impl Colors {
    fn new(use_color: bool) -> Self {
        if use_color {
            Self {
                bold: "\x1b[1m",
                reset: "\x1b[0m",
                dim: "\x1b[2m",
                green: "\x1b[32m",
                yellow: "\x1b[33m",
                blue: "\x1b[34m",
                cyan: "\x1b[36m",
                white: "\x1b[97m",
            }
        } else {
            Self {
                bold: "",
                reset: "",
                dim: "",
                green: "",
                yellow: "",
                blue: "",
                cyan: "",
                white: "",
            }
        }
    }
}

/// Signature for a root configuration file (not a module definition)
#[derive(Debug, Clone)]
pub struct RootConfigSignature {
    /// File name
    pub name: String,
    /// Imported modules
    pub imports: Vec<ImportInfo>,
    /// Resources created directly
    pub resources: Vec<ResourceCreation>,
    /// Module instantiations
    pub module_calls: Vec<ModuleCallInfo>,
    /// Typed dependency graph
    pub dependency_graph: TypedDependencyGraph,
}

/// Information about an imported module
#[derive(Debug, Clone)]
pub struct ImportInfo {
    pub path: String,
    pub alias: String,
}

/// Information about a module instantiation
#[derive(Debug, Clone)]
pub struct ModuleCallInfo {
    pub module_name: String,
    pub binding_name: Option<String>,
    pub arguments: Vec<String>,
}

impl RootConfigSignature {
    /// Build a root config signature from a parsed file
    pub fn from_parsed_file<E>(parsed: &crate::parser::File<E>, file_name: &str) -> Self {
        // Build imports
        let imports: Vec<ImportInfo> = parsed
            .uses
            .iter()
            .map(|i| ImportInfo {
                path: i.path.clone(),
                alias: i.alias.clone(),
            })
            .collect();

        // Build resource creations
        let mut creates: Vec<ResourceCreation> = Vec::new();
        let mut binding_types: HashMap<String, ResourceTypePath> = HashMap::new();

        for resource in &parsed.resources {
            let binding_name = resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.name_str().to_string());

            let resource_type_str = resource
                .attributes
                .get("_type")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.resource_type.clone());

            let resource_type =
                ResourceTypePath::parse(&resource_type_str).unwrap_or_else(|| ResourceTypePath {
                    provider: "unknown".to_string(),
                    resource_type: resource_type_str.clone(),
                });

            binding_types.insert(binding_name.clone(), resource_type.clone());

            creates.push(ResourceCreation {
                binding_name,
                resource_type,
                dependencies: Vec::new(),
            });
        }

        // Build module calls info
        let module_calls: Vec<ModuleCallInfo> = parsed
            .module_calls
            .iter()
            .map(|call| ModuleCallInfo {
                module_name: call.module_name.clone(),
                binding_name: call.binding_name.clone(),
                arguments: call.arguments.keys().cloned().collect(),
            })
            .collect();

        // Build typed dependency graph
        let mut typed_graph = TypedDependencyGraph::new();

        for resource in &parsed.resources {
            let binding_name = resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.name_str().to_string());

            for (attr_key, value) in &resource.attributes {
                if attr_key.starts_with('_') {
                    continue;
                }
                Self::collect_typed_dependencies(
                    &binding_name,
                    attr_key,
                    value,
                    &mut typed_graph,
                    &binding_types,
                );
            }
        }

        // Update creates with dependencies
        for creation in &mut creates {
            if let Some(deps) = typed_graph.edges.get(&creation.binding_name) {
                creation.dependencies = deps.clone();
            }
        }

        RootConfigSignature {
            name: file_name.to_string(),
            imports,
            resources: creates,
            module_calls,
            dependency_graph: typed_graph,
        }
    }

    fn collect_typed_dependencies(
        from: &str,
        attr_key: &str,
        value: &Value,
        graph: &mut TypedDependencyGraph,
        binding_types: &HashMap<String, ResourceTypePath>,
    ) {
        match value {
            Value::ResourceRef { path } => {
                let target_type = binding_types.get(path.binding()).cloned();
                graph.add_edge(
                    from.to_string(),
                    TypedDependency {
                        target: path.binding().to_string(),
                        target_type,
                        attribute: path.attribute().to_string(),
                        used_in: attr_key.to_string(),
                    },
                );
            }
            Value::List(items) => {
                for item in items {
                    Self::collect_typed_dependencies(from, attr_key, item, graph, binding_types);
                }
            }
            Value::Map(map) => {
                for (k, v) in map {
                    Self::collect_typed_dependencies(from, k, v, graph, binding_types);
                }
            }
            Value::Interpolation(parts) => {
                use crate::resource::InterpolationPart;
                for part in parts {
                    if let InterpolationPart::Expr(v) = part {
                        Self::collect_typed_dependencies(from, attr_key, v, graph, binding_types);
                    }
                }
            }
            Value::FunctionCall { args, .. } => {
                for arg in args {
                    Self::collect_typed_dependencies(from, attr_key, arg, graph, binding_types);
                }
            }
            _ => {}
        }
    }

    /// Display the root config signature as a formatted string
    pub fn display(&self) -> String {
        self.display_with_color(true)
    }

    /// Display with optional color support
    pub fn display_with_color(&self, use_color: bool) -> String {
        let c = Colors::new(use_color);
        let mut output = String::new();

        output.push_str(&format!(
            "{}File:{} {}{}{}\n\n",
            c.bold, c.reset, c.cyan, self.name, c.reset
        ));

        // IMPORTS section
        output.push_str(&format!("{}=== IMPORTS ==={}\n\n", c.bold, c.reset));
        if self.imports.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            for import in &self.imports {
                output.push_str(&format!(
                    "  {}\"{}\" as {}{}{}\n",
                    c.dim, import.path, c.cyan, import.alias, c.reset
                ));
            }
        }
        output.push('\n');

        // CREATES section
        output.push_str(&format!("{}=== CREATES ==={}\n\n", c.bold, c.reset));
        if self.resources.is_empty() && self.module_calls.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            // Show resources with dependency tree
            let roots = self.find_root_nodes();
            if roots.is_empty() {
                // No dependencies, just list resources
                for creation in &self.resources {
                    output.push_str(&format!(
                        "  {}{}{}: {}{}{}\n",
                        c.white,
                        creation.binding_name,
                        c.reset,
                        c.yellow,
                        creation.resource_type,
                        c.reset
                    ));
                }
            } else {
                let mut visited = HashSet::new();
                let mut draw = TreeDrawState {
                    output: &mut output,
                    visited: &mut visited,
                    colors: &c,
                };
                for root in roots {
                    self.display_creates_tree_colored(&mut draw, &root, "  ", true, true);
                }
            }

            // Show module instantiations
            if !self.module_calls.is_empty() {
                output.push('\n');
                output.push_str(&format!("  {}Module instantiations:{}\n", c.dim, c.reset));
                for call in &self.module_calls {
                    let binding = call
                        .binding_name
                        .as_ref()
                        .map(|b| format!("{} = ", b))
                        .unwrap_or_default();
                    output.push_str(&format!(
                        "    {}{}{}{}{}\n",
                        c.white, binding, c.blue, call.module_name, c.reset
                    ));
                }
            }
        }

        output
    }

    fn find_root_nodes(&self) -> Vec<String> {
        let mut all_targets: HashSet<String> = HashSet::new();
        let mut all_sources: HashSet<String> = HashSet::new();

        for (source, deps) in &self.dependency_graph.edges {
            all_sources.insert(source.clone());
            for dep in deps {
                all_targets.insert(dep.target.clone());
            }
        }

        let mut roots: Vec<String> = all_targets.difference(&all_sources).cloned().collect();

        roots.sort();
        roots
    }

    fn display_creates_tree_colored(
        &self,
        state: &mut TreeDrawState<'_>,
        node: &str,
        prefix: &str,
        is_last: bool,
        is_root: bool,
    ) {
        if state.visited.contains(node) {
            return;
        }
        state.visited.insert(node.to_string());

        let c = state.colors;
        let connector = if is_root {
            ""
        } else if is_last {
            &format!("{}└── {}", c.dim, c.reset)
        } else {
            &format!("{}├── {}", c.dim, c.reset)
        };

        // Format the node with its type
        let node_display = self
            .resources
            .iter()
            .find(|r| r.binding_name == node)
            .map(|r| {
                format!(
                    "{}{}{}: {}{}{}",
                    c.white, r.binding_name, c.reset, c.yellow, r.resource_type, c.reset
                )
            })
            .unwrap_or_else(|| node.to_string());

        state
            .output
            .push_str(&format!("{}{}{}\n", prefix, connector, node_display));

        // Find children (nodes that depend on this node)
        // Filter out nodes that have a more specific path through another resource
        let direct_dependents: Vec<String> = self
            .dependency_graph
            .edges
            .iter()
            .filter(|(_, deps)| deps.iter().any(|d| d.target == node))
            .map(|(source, _)| source.clone())
            .collect();

        // Filter to only show "direct" children - nodes that don't have a path through another dependent
        let mut children: Vec<String> = direct_dependents
            .iter()
            .filter(|child| {
                // Check if this child has a dependency on another node that also depends on `node`
                // If so, skip it (it should be shown under that other node instead)
                let child_deps = self.dependency_graph.dependencies_of(child);
                !child_deps.iter().any(|dep| {
                    // dep.target is something this child depends on
                    // Check if dep.target also depends on `node`
                    dep.target != node && direct_dependents.contains(&dep.target)
                })
            })
            .cloned()
            .collect();
        children.sort();

        let dim_pipe = format!("{}│{}", c.dim, c.reset);
        let new_prefix = if is_root {
            format!("{}  ", prefix)
        } else {
            format!("{}{}  ", prefix, if is_last { " " } else { &dim_pipe })
        };

        for (i, child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            self.display_creates_tree_colored(state, child, &new_prefix, child_is_last, false);
        }
    }
}

/// Enum to represent either a module or a root configuration file signature
#[derive(Debug, Clone)]
pub enum FileSignature {
    Module(ModuleSignature),
    RootConfig(RootConfigSignature),
}

impl FileSignature {
    /// Create from a parsed file
    /// For directory-based modules (files with top-level arguments/attributes blocks),
    /// the module name is derived from the directory name or file name.
    pub fn from_parsed_file<E>(parsed: &crate::parser::File<E>, file_name: &str) -> Self {
        // Check for directory-based module (has top-level arguments or attribute_params)
        if !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty() {
            return FileSignature::Module(ModuleSignature::from_directory_module(
                parsed, file_name,
            ));
        }

        // Otherwise, treat as a root configuration file
        FileSignature::RootConfig(RootConfigSignature::from_parsed_file(parsed, file_name))
    }

    /// Create from a parsed file with a specific module name
    /// Use this when you know the module name (e.g., from directory structure)
    pub fn from_parsed_file_with_name<E>(
        parsed: &crate::parser::File<E>,
        module_name: &str,
    ) -> Self {
        // Check for directory-based module (has top-level arguments or attribute_params)
        if !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty() {
            return FileSignature::Module(ModuleSignature::from_directory_module(
                parsed,
                module_name,
            ));
        }

        // Otherwise, treat as a root configuration file
        FileSignature::RootConfig(RootConfigSignature::from_parsed_file(parsed, module_name))
    }

    /// Display the signature
    pub fn display(&self) -> String {
        match self {
            FileSignature::Module(sig) => sig.display(),
            FileSignature::RootConfig(sig) => sig.display(),
        }
    }
}

/// Module signature containing typed information about inputs, outputs, and resources
#[derive(Debug, Clone)]
pub struct ModuleSignature {
    /// Module name
    pub name: String,
    /// Required arguments with types
    pub requires: Vec<TypedArgument>,
    /// Resources created by this module
    pub creates: Vec<ResourceCreation>,
    /// Exposed attribute params with types
    pub exposes: Vec<TypedAttributeParam>,
    /// Typed dependency graph
    pub dependency_graph: TypedDependencyGraph,
}

impl ModuleSignature {
    /// Build a module signature from a directory-based module (ParsedFile with top-level arguments/attributes)
    pub fn from_directory_module<E>(parsed: &crate::parser::File<E>, module_name: &str) -> Self {
        // Build requires (typed inputs)
        let requires: Vec<TypedArgument> = parsed
            .arguments
            .iter()
            .map(|input| TypedArgument {
                name: input.name.clone(),
                type_expr: input.type_expr.clone(),
                required: input.default.is_none(),
                default: input.default.as_ref().map(format_value),
                description: input.description.clone(),
            })
            .collect();

        // Build input type map for dependency type inference
        let argument_types: HashMap<String, TypeExpr> = parsed
            .arguments
            .iter()
            .map(|i| (i.name.clone(), i.type_expr.clone()))
            .collect();

        // Build creates (resource creations with dependencies)
        let mut creates: Vec<ResourceCreation> = Vec::new();
        let mut binding_types: HashMap<String, ResourceTypePath> = HashMap::new();

        for resource in &parsed.resources {
            let binding_name = resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.name_str().to_string());

            let resource_type_str = resource
                .attributes
                .get("_type")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.resource_type.clone());

            let resource_type =
                ResourceTypePath::parse(&resource_type_str).unwrap_or_else(|| ResourceTypePath {
                    provider: "unknown".to_string(),
                    resource_type: resource_type_str.clone(),
                });

            binding_types.insert(binding_name.clone(), resource_type.clone());

            creates.push(ResourceCreation {
                binding_name,
                resource_type,
                dependencies: Vec::new(),
            });
        }

        // Build typed dependency graph from resource attributes
        let mut typed_graph = TypedDependencyGraph::new();

        for resource in &parsed.resources {
            let binding_name = resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.name_str().to_string());

            for (attr_key, value) in &resource.attributes {
                if attr_key.starts_with('_') {
                    continue;
                }
                Self::collect_typed_dependencies(
                    &binding_name,
                    attr_key,
                    value,
                    &mut typed_graph,
                    &binding_types,
                    &argument_types,
                );
            }
        }

        // Update creates with dependencies
        for creation in &mut creates {
            if let Some(deps) = typed_graph.edges.get(&creation.binding_name) {
                creation.dependencies = deps.clone();
            }
        }

        // Build exposes (typed attribute params)
        let exposes: Vec<TypedAttributeParam> = parsed
            .attribute_params
            .iter()
            .map(|attr_param| {
                let source_binding = attr_param.value.as_ref().and_then(|v| match v {
                    Value::ResourceRef { path } => Some(path.binding().to_string()),
                    _ => None,
                });

                TypedAttributeParam {
                    name: attr_param.name.clone(),
                    type_expr: attr_param.type_expr.clone(),
                    source_binding,
                }
            })
            .collect();

        ModuleSignature {
            name: module_name.to_string(),
            requires,
            creates,
            exposes,
            dependency_graph: typed_graph,
        }
    }

    fn collect_typed_dependencies(
        from: &str,
        attr_key: &str,
        value: &Value,
        graph: &mut TypedDependencyGraph,
        binding_types: &HashMap<String, ResourceTypePath>,
        argument_types: &HashMap<String, TypeExpr>,
    ) {
        match value {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let target_type = if let Some(arg_type) = argument_types.get(binding_name) {
                    // Argument parameter reference (lexically scoped)
                    if let TypeExpr::Ref(p) = arg_type {
                        Some(p.clone())
                    } else {
                        None
                    }
                } else {
                    binding_types.get(binding_name).cloned()
                };

                graph.add_edge(
                    from.to_string(),
                    TypedDependency {
                        target: binding_name.to_string(),
                        target_type,
                        attribute: path.attribute().to_string(),
                        used_in: attr_key.to_string(),
                    },
                );
            }
            Value::List(items) => {
                for item in items {
                    Self::collect_typed_dependencies(
                        from,
                        attr_key,
                        item,
                        graph,
                        binding_types,
                        argument_types,
                    );
                }
            }
            Value::Map(map) => {
                for (k, v) in map {
                    Self::collect_typed_dependencies(
                        from,
                        k,
                        v,
                        graph,
                        binding_types,
                        argument_types,
                    );
                }
            }
            Value::Interpolation(parts) => {
                use crate::resource::InterpolationPart;
                for part in parts {
                    if let InterpolationPart::Expr(v) = part {
                        Self::collect_typed_dependencies(
                            from,
                            attr_key,
                            v,
                            graph,
                            binding_types,
                            argument_types,
                        );
                    }
                }
            }
            Value::FunctionCall { args, .. } => {
                for arg in args {
                    Self::collect_typed_dependencies(
                        from,
                        attr_key,
                        arg,
                        graph,
                        binding_types,
                        argument_types,
                    );
                }
            }
            _ => {}
        }
    }

    /// Display the module signature as a formatted string
    pub fn display(&self) -> String {
        self.display_with_color(true)
    }

    /// Display the module signature with optional color support
    pub fn display_with_color(&self, use_color: bool) -> String {
        let c = Colors::new(use_color);
        let mut output = String::new();

        output.push_str(&format!(
            "{}Module:{} {}{}{}\n\n",
            c.bold, c.reset, c.cyan, self.name, c.reset
        ));

        // ARGUMENTS section
        output.push_str(&format!("{}=== ARGUMENTS ==={}\n\n", c.bold, c.reset));
        if self.requires.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            for input in &self.requires {
                let required_str = if input.required {
                    format!("{}(required){}", c.yellow, c.reset)
                } else {
                    String::new()
                };
                let default_str = input
                    .default
                    .as_ref()
                    .map(|d| format!(" {}={} {}{}{}", c.dim, c.reset, c.green, d, c.reset))
                    .unwrap_or_default();
                let type_str = self.format_type_expr(&input.type_expr, &c);
                output.push_str(&format!(
                    "  {}{}{}: {}{}  {}\n",
                    c.white, input.name, c.reset, type_str, default_str, required_str
                ));
                if let Some(desc) = &input.description {
                    output.push_str(&format!("    {}{}{}\n", c.dim, desc, c.reset));
                }
            }
        }
        output.push('\n');

        // CREATES section (with dependency tree)
        output.push_str(&format!("{}=== CREATES ==={}\n\n", c.bold, c.reset));
        if self.creates.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            let roots = self.find_root_nodes();
            if roots.is_empty() {
                // No dependencies, just list resources
                for creation in &self.creates {
                    output.push_str(&format!(
                        "  {}{}{}: {}{}{}\n",
                        c.white,
                        creation.binding_name,
                        c.reset,
                        c.yellow,
                        creation.resource_type,
                        c.reset
                    ));
                }
            } else {
                let mut visited = HashSet::new();
                let mut draw = TreeDrawState {
                    output: &mut output,
                    visited: &mut visited,
                    colors: &c,
                };
                for root in roots {
                    self.display_creates_tree_colored(&mut draw, &root, "  ", true, true);
                }
            }
        }
        output.push('\n');

        // ATTRIBUTES section
        output.push_str(&format!("{}=== ATTRIBUTES ==={}\n\n", c.bold, c.reset));
        if self.exposes.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            for attr_param in &self.exposes {
                if let Some(type_expr) = &attr_param.type_expr {
                    let type_str = self.format_type_expr(type_expr, &c);
                    output.push_str(&format!(
                        "  {}{}{}: {}\n",
                        c.white, attr_param.name, c.reset, type_str
                    ));
                } else {
                    output.push_str(&format!("  {}{}{}\n", c.white, attr_param.name, c.reset));
                }
            }
        }

        output
    }

    fn format_type_expr(&self, type_expr: &TypeExpr, c: &Colors) -> String {
        match type_expr {
            TypeExpr::Ref(path) => {
                format!("{}{}{}", c.yellow, path, c.reset)
            }
            TypeExpr::List(inner) => {
                format!(
                    "{}list({}{})",
                    c.green,
                    self.format_type_expr(inner, c),
                    c.reset
                )
            }
            TypeExpr::Map(inner) => {
                format!(
                    "{}map({}{})",
                    c.green,
                    self.format_type_expr(inner, c),
                    c.reset
                )
            }
            TypeExpr::Struct { fields } => {
                if fields.is_empty() {
                    format!("{}struct {{}}{}", c.green, c.reset)
                } else {
                    let mut out = format!("{}struct {{ ", c.green);
                    for (i, (name, ty)) in fields.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(name);
                        out.push_str(": ");
                        out.push_str(&self.format_type_expr(ty, c));
                        out.push_str(c.green);
                    }
                    out.push_str(" }");
                    out.push_str(c.reset);
                    out
                }
            }
            _ => format!("{}{}{}", c.green, type_expr, c.reset),
        }
    }

    fn display_creates_tree_colored(
        &self,
        state: &mut TreeDrawState<'_>,
        node: &str,
        prefix: &str,
        is_last: bool,
        is_root: bool,
    ) {
        if state.visited.contains(node) {
            return;
        }
        state.visited.insert(node.to_string());

        let c = state.colors;
        let connector = if is_root {
            ""
        } else if is_last {
            &format!("{}└── {}", c.dim, c.reset)
        } else {
            &format!("{}├── {}", c.dim, c.reset)
        };

        // Format the node with its type
        let node_display = if let Some(cr) = self.creates.iter().find(|cr| cr.binding_name == node)
        {
            // Show resource with its type
            format!(
                "{}{}{}: {}{}{}",
                c.white, cr.binding_name, c.reset, c.yellow, cr.resource_type, c.reset
            )
        } else {
            // Argument parameter name (lexically scoped)
            node.to_string()
        };

        state
            .output
            .push_str(&format!("{}{}{}\n", prefix, connector, node_display));

        // Find children (nodes that depend on this node)
        // Filter out nodes that have a more specific path through another resource
        let direct_dependents: Vec<String> = self
            .dependency_graph
            .edges
            .iter()
            .filter(|(_, deps)| deps.iter().any(|d| d.target == node))
            .map(|(source, _)| source.clone())
            .collect();

        // Filter to only show "direct" children - nodes that don't have a path through another dependent
        let mut children: Vec<String> = direct_dependents
            .iter()
            .filter(|child| {
                // Check if this child has a dependency on another node that also depends on `node`
                // If so, skip it (it should be shown under that other node instead)
                let child_deps = self.dependency_graph.dependencies_of(child);
                !child_deps.iter().any(|dep| {
                    // dep.target is something this child depends on
                    // Check if dep.target also depends on `node`
                    dep.target != node && direct_dependents.contains(&dep.target)
                })
            })
            .cloned()
            .collect();
        children.sort();

        let dim_pipe = format!("{}│{}", c.dim, c.reset);
        let new_prefix = if is_root {
            format!("{}  ", prefix)
        } else {
            format!("{}{}  ", prefix, if is_last { " " } else { &dim_pipe })
        };

        for (i, child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            self.display_creates_tree_colored(state, child, &new_prefix, child_is_last, false);
        }
    }

    fn find_root_nodes(&self) -> Vec<String> {
        // Collect all resource binding names
        let resource_names: HashSet<String> = self
            .creates
            .iter()
            .map(|c| c.binding_name.clone())
            .collect();

        // Find resources whose only dependencies are on arguments (not other resources)
        // These become root nodes
        let mut roots: Vec<String> = resource_names
            .iter()
            .filter(|name| {
                let deps = self.dependency_graph.dependencies_of(name);
                // A resource is a root if it has no resource dependencies
                // (it either has no deps, or all deps are on arguments)
                deps.iter().all(|dep| !resource_names.contains(&dep.target))
            })
            .cloned()
            .collect();

        // If no roots found (no dependencies at all), return empty
        // (the caller will fall back to listing resources without a tree)
        if roots.iter().all(|r| {
            self.dependency_graph.dependencies_of(r).is_empty()
                && !self
                    .dependency_graph
                    .edges
                    .values()
                    .any(|deps| deps.iter().any(|d| d.target == *r))
        }) {
            return Vec::new();
        }

        roots.sort();
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::TypeExpr;

    #[test]
    fn test_cycle_detection() {
        let mut graph = DependencyGraph::new();

        // Create a cycle: a -> b -> c -> a
        graph.add_edge(
            "a".to_string(),
            Dependency {
                target: "b".to_string(),
                attribute: "id".to_string(),
                used_in: "b_id".to_string(),
            },
        );
        graph.add_edge(
            "b".to_string(),
            Dependency {
                target: "c".to_string(),
                attribute: "id".to_string(),
                used_in: "c_id".to_string(),
            },
        );
        graph.add_edge(
            "c".to_string(),
            Dependency {
                target: "a".to_string(),
                attribute: "id".to_string(),
                used_in: "a_id".to_string(),
            },
        );

        assert!(graph.has_cycle());
    }

    #[test]
    fn test_no_cycle() {
        let mut graph = DependencyGraph::new();

        // Create a DAG: a -> b -> c
        graph.add_edge(
            "a".to_string(),
            Dependency {
                target: "b".to_string(),
                attribute: "id".to_string(),
                used_in: "b_id".to_string(),
            },
        );
        graph.add_edge(
            "b".to_string(),
            Dependency {
                target: "c".to_string(),
                attribute: "id".to_string(),
                used_in: "c_id".to_string(),
            },
        );

        assert!(!graph.has_cycle());
    }

    #[test]
    fn test_module_signature_display() {
        use crate::parser::{ProviderContext, parse};

        let input = r#"
            arguments {
                vpc: aws.vpc
                enable_https: Bool = true
            }

            attributes {
                security_group: aws.security_group = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc
            }

            let http_rule = aws.security_group.ingress_rule {
                name              = "http"
                security_group_id = web_sg.id
                from_port         = 80
                to_port           = 80
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let signature = ModuleSignature::from_directory_module(&parsed, "web_tier");
        let display = signature.display_with_color(false);

        // Check sections are present
        assert!(display.contains("Module: web_tier"));
        assert!(display.contains("=== ARGUMENTS ==="));
        assert!(display.contains("=== CREATES ==="));
        assert!(display.contains("=== ATTRIBUTES ==="));

        // Check ref types are displayed correctly
        assert!(display.contains("aws.vpc"));
        assert!(display.contains("aws.security_group"));

        // Check tree structure shows resources
        assert!(display.contains("web_sg: aws.security_group"));
        assert!(display.contains("http_rule: aws.security_group.ingress_rule"));
    }

    #[test]
    fn test_module_signature_display_with_descriptions() {
        use crate::parser::{ProviderContext, parse};

        let input = r#"
            arguments {
                enable_https: Bool = true

                vpc_id: String {
                    description = "The VPC to deploy into"
                }

                port: Int {
                    description = "Web server port"
                    default     = 8080
                }
            }

            attributes {
                sg_id: String = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc_id
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let signature = ModuleSignature::from_directory_module(&parsed, "web_tier");
        let display = signature.display_with_color(false);

        // Simple form has no description line
        assert!(display.contains("enable_https: Bool"));
        // The line after enable_https should not contain a description
        let lines: Vec<&str> = display.lines().collect();
        let enable_idx = lines
            .iter()
            .position(|l| l.contains("enable_https"))
            .expect("enable_https should be in display");
        // Next line should be vpc_id (not a description for enable_https)
        assert!(
            lines[enable_idx + 1].contains("vpc_id"),
            "Simple-form argument should not have a description line, got: {}",
            lines[enable_idx + 1]
        );

        // Block form descriptions are shown on the line after the argument
        assert!(display.contains("vpc_id: String"));
        let vpc_idx = lines
            .iter()
            .position(|l| l.contains("vpc_id"))
            .expect("vpc_id should be in display");
        assert!(
            lines[vpc_idx + 1].contains("The VPC to deploy into"),
            "Description should appear on line after vpc_id, got: {}",
            lines[vpc_idx + 1]
        );

        assert!(display.contains("port: Int"));
        let port_idx = lines
            .iter()
            .position(|l| l.contains("port:"))
            .expect("port should be in display");
        assert!(
            lines[port_idx + 1].contains("Web server port"),
            "Description should appear on line after port, got: {}",
            lines[port_idx + 1]
        );

        // Required/optional indicators
        assert!(display.contains("(required)")); // vpc_id has no default
    }

    #[test]
    fn test_typed_dependency_graph() {
        use crate::parser::ResourceTypePath;

        let mut graph = TypedDependencyGraph::new();

        graph.add_edge(
            "web_sg".to_string(),
            TypedDependency {
                target: "vpc".to_string(),
                target_type: Some(ResourceTypePath::new("aws", "vpc")),
                attribute: String::new(),
                used_in: "vpc_id".to_string(),
            },
        );

        let deps = graph.dependencies_of("web_sg");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target, "vpc");
        assert_eq!(deps[0].attribute, "");
        assert!(deps[0].target_type.is_some());
    }

    #[test]
    fn test_directory_based_module() {
        use crate::parser::{ProviderContext, parse};

        // Parse a directory-based module (no module {} wrapper)
        let input = r#"
            arguments {
                vpc: aws.vpc
                enable_https: Bool = true
            }

            attributes {
                security_group: aws.security_group = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();

        // Verify parsed file has top-level inputs and outputs
        assert_eq!(parsed.arguments.len(), 2);
        assert_eq!(parsed.attribute_params.len(), 1);

        // Create signature from directory-based module
        let signature = ModuleSignature::from_directory_module(&parsed, "web_tier");

        // Check module name
        assert_eq!(signature.name, "web_tier");

        // Check requires
        assert_eq!(signature.requires.len(), 2);
        assert_eq!(signature.requires[0].name, "vpc");
        assert!(matches!(signature.requires[0].type_expr, TypeExpr::Ref(_)));

        // Check creates
        assert_eq!(signature.creates.len(), 1);
        assert_eq!(signature.creates[0].binding_name, "web_sg");

        // Check exposes
        assert_eq!(signature.exposes.len(), 1);
        assert_eq!(signature.exposes[0].name, "security_group");
    }

    #[test]
    fn test_file_signature_from_directory_module() {
        use crate::parser::{ProviderContext, parse};

        // Directory-based module (top-level arguments/attributes)
        let input = r#"
            arguments {
                vpc: aws.vpc
            }
            attributes {
                sg: aws.security_group = web_sg.id
            }

            let web_sg = aws.security_group {
                name = "web-sg"
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let signature = FileSignature::from_parsed_file(&parsed, "web_tier");

        // Should be detected as a Module, not a RootConfig
        assert!(matches!(signature, FileSignature::Module(_)));

        if let FileSignature::Module(sig) = signature {
            assert_eq!(sig.name, "web_tier");
            assert_eq!(sig.requires.len(), 1);
            assert_eq!(sig.exposes.len(), 1);
        }
    }

    #[test]
    fn test_file_signature_from_root_config() {
        use crate::parser::{ProviderContext, parse};

        // Root config (no arguments/attributes, no module wrapper)
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            let main_vpc = aws.vpc {
                name = "main-vpc"
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let signature = FileSignature::from_parsed_file(&parsed, "main");

        // Should be detected as a RootConfig
        assert!(matches!(signature, FileSignature::RootConfig(_)));

        if let FileSignature::RootConfig(sig) = signature {
            assert_eq!(sig.name, "main");
            assert_eq!(sig.resources.len(), 1);
        }
    }
}
