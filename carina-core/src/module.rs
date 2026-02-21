//! Module - Module definitions and dependency analysis
//!
//! This module provides types for analyzing module structure and dependencies.

use std::collections::{HashMap, HashSet};

use crate::parser::{ParsedFile, ResourceTypePath, TypeExpr};
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
        Value::Map(map) => {
            if map.is_empty() {
                "{}".to_string()
            } else {
                format!("{{...{} keys}}", map.len())
            }
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } => format!("{}.{}", binding_name, attribute_name),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => format!("{}.{}", name, m),
            None => name.clone(),
        },
    }
}

/// Typed input parameter for module signature
#[derive(Debug, Clone)]
pub struct TypedInput {
    /// Input parameter name
    pub name: String,
    /// Type expression (including ref types)
    pub type_expr: TypeExpr,
    /// Whether this input is required (no default)
    pub required: bool,
    /// Default value as a display string
    pub default: Option<String>,
}

/// Resource creation entry in module signature
#[derive(Debug, Clone)]
pub struct ResourceCreation {
    /// Binding name for this resource
    pub binding_name: String,
    /// Full resource type path (e.g., aws.security_group)
    pub resource_type: ResourceTypePath,
    /// Dependencies on other resources or inputs
    pub dependencies: Vec<TypedDependency>,
}

/// Typed output parameter for module signature
#[derive(Debug, Clone)]
pub struct TypedOutput {
    /// Output parameter name
    pub name: String,
    /// Type expression (including ref types)
    pub type_expr: TypeExpr,
    /// Source binding name (if the output comes from a resource)
    pub source_binding: Option<String>,
}

/// Typed dependency representing a reference from one resource to another
#[derive(Debug, Clone)]
pub struct TypedDependency {
    /// Target binding name (e.g., "vpc", "input")
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
    pub fn from_parsed_file(parsed: &ParsedFile, file_name: &str) -> Self {
        // Build imports
        let imports: Vec<ImportInfo> = parsed
            .imports
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
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.name.clone());

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
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.name.clone());

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
            Value::ResourceRef {
                binding_name,
                attribute_name,
            } => {
                let target_type = binding_types.get(binding_name).cloned();
                graph.add_edge(
                    from.to_string(),
                    TypedDependency {
                        target: binding_name.clone(),
                        target_type,
                        attribute: attribute_name.clone(),
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
                for root in roots {
                    self.display_creates_tree_colored(
                        &mut output,
                        &root,
                        "  ",
                        true,
                        &mut visited,
                        true,
                        &c,
                    );
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

    #[allow(clippy::too_many_arguments)]
    fn display_creates_tree_colored(
        &self,
        output: &mut String,
        node: &str,
        prefix: &str,
        is_last: bool,
        visited: &mut HashSet<String>,
        is_root: bool,
        c: &Colors,
    ) {
        if visited.contains(node) {
            return;
        }
        visited.insert(node.to_string());

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

        output.push_str(&format!("{}{}{}\n", prefix, connector, node_display));

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
            self.display_creates_tree_colored(
                output,
                child,
                &new_prefix,
                child_is_last,
                visited,
                false,
                c,
            );
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
    /// For directory-based modules (files with top-level input/output blocks),
    /// the module name is derived from the directory name or file name.
    pub fn from_parsed_file(parsed: &ParsedFile, file_name: &str) -> Self {
        // Check for directory-based module (has top-level inputs or outputs)
        if !parsed.inputs.is_empty() || !parsed.outputs.is_empty() {
            return FileSignature::Module(ModuleSignature::from_directory_module(
                parsed, file_name,
            ));
        }

        // Otherwise, treat as a root configuration file
        FileSignature::RootConfig(RootConfigSignature::from_parsed_file(parsed, file_name))
    }

    /// Create from a parsed file with a specific module name
    /// Use this when you know the module name (e.g., from directory structure)
    pub fn from_parsed_file_with_name(parsed: &ParsedFile, module_name: &str) -> Self {
        // Check for directory-based module (has top-level inputs or outputs)
        if !parsed.inputs.is_empty() || !parsed.outputs.is_empty() {
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
    /// Required inputs with types
    pub requires: Vec<TypedInput>,
    /// Resources created by this module
    pub creates: Vec<ResourceCreation>,
    /// Exposed outputs with types
    pub exposes: Vec<TypedOutput>,
    /// Typed dependency graph
    pub dependency_graph: TypedDependencyGraph,
}

impl ModuleSignature {
    /// Build a module signature from a directory-based module (ParsedFile with top-level inputs/outputs)
    pub fn from_directory_module(parsed: &ParsedFile, module_name: &str) -> Self {
        // Build requires (typed inputs)
        let requires: Vec<TypedInput> = parsed
            .inputs
            .iter()
            .map(|input| TypedInput {
                name: input.name.clone(),
                type_expr: input.type_expr.clone(),
                required: input.default.is_none(),
                default: input.default.as_ref().map(format_value),
            })
            .collect();

        // Build input type map for dependency type inference
        let input_types: HashMap<String, TypeExpr> = parsed
            .inputs
            .iter()
            .map(|i| (i.name.clone(), i.type_expr.clone()))
            .collect();

        // Build creates (resource creations with dependencies)
        let mut creates: Vec<ResourceCreation> = Vec::new();
        let mut binding_types: HashMap<String, ResourceTypePath> = HashMap::new();

        for resource in &parsed.resources {
            let binding_name = resource
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.name.clone());

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
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| resource.id.name.clone());

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
                    &input_types,
                );
            }
        }

        // Update creates with dependencies
        for creation in &mut creates {
            if let Some(deps) = typed_graph.edges.get(&creation.binding_name) {
                creation.dependencies = deps.clone();
            }
        }

        // Build exposes (typed outputs)
        let exposes: Vec<TypedOutput> = parsed
            .outputs
            .iter()
            .map(|output| {
                let source_binding = output.value.as_ref().and_then(|v| match v {
                    Value::ResourceRef { binding_name, .. } => Some(binding_name.clone()),
                    _ => None,
                });

                TypedOutput {
                    name: output.name.clone(),
                    type_expr: output.type_expr.clone(),
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
        input_types: &HashMap<String, TypeExpr>,
    ) {
        match value {
            Value::ResourceRef {
                binding_name,
                attribute_name,
            } => {
                let target_type = if binding_name == "input" {
                    input_types.get(attribute_name).and_then(|t| {
                        if let TypeExpr::Ref(path) = t {
                            Some(path.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    binding_types.get(binding_name).cloned()
                };

                graph.add_edge(
                    from.to_string(),
                    TypedDependency {
                        target: binding_name.clone(),
                        target_type,
                        attribute: attribute_name.clone(),
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
                        input_types,
                    );
                }
            }
            Value::Map(map) => {
                for (k, v) in map {
                    Self::collect_typed_dependencies(from, k, v, graph, binding_types, input_types);
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

        // REQUIRES section
        output.push_str(&format!("{}=== REQUIRES ==={}\n\n", c.bold, c.reset));
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
                for root in roots {
                    self.display_creates_tree_colored(
                        &mut output,
                        &root,
                        "  ",
                        true,
                        &mut visited,
                        true,
                        &c,
                    );
                }
            }
        }
        output.push('\n');

        // EXPOSES section
        output.push_str(&format!("{}=== EXPOSES ==={}\n\n", c.bold, c.reset));
        if self.exposes.is_empty() {
            output.push_str(&format!("  {}(none){}\n", c.dim, c.reset));
        } else {
            for output_param in &self.exposes {
                let type_str = self.format_type_expr(&output_param.type_expr, &c);
                output.push_str(&format!(
                    "  {}{}{}: {}\n",
                    c.white, output_param.name, c.reset, type_str
                ));
                if let Some(source) = &output_param.source_binding {
                    output.push_str(&format!(
                        "    {}<- from:{} {}{}{}\n",
                        c.dim, c.reset, c.cyan, source, c.reset
                    ));
                }
            }
        }

        output
    }

    fn format_type_expr(&self, type_expr: &TypeExpr, c: &Colors) -> String {
        match type_expr {
            TypeExpr::Ref(path) => {
                format!("{}ref({}{}{}){}", c.green, c.yellow, path, c.green, c.reset)
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
            _ => format!("{}{}{}", c.green, type_expr, c.reset),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn display_creates_tree_colored(
        &self,
        output: &mut String,
        node: &str,
        prefix: &str,
        is_last: bool,
        visited: &mut HashSet<String>,
        is_root: bool,
        c: &Colors,
    ) {
        if visited.contains(node) {
            return;
        }
        visited.insert(node.to_string());

        let connector = if is_root {
            ""
        } else if is_last {
            &format!("{}└── {}", c.dim, c.reset)
        } else {
            &format!("{}├── {}", c.dim, c.reset)
        };

        // Format the node with its type
        let node_display = if node == "input" {
            // Show input with ref types
            let ref_inputs: Vec<String> = self
                .requires
                .iter()
                .filter_map(|r| {
                    if let TypeExpr::Ref(path) = &r.type_expr {
                        Some(format!(
                            "{}{}{}: {}ref({}{}{}){}",
                            c.white, r.name, c.reset, c.green, c.yellow, path, c.green, c.reset
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            if ref_inputs.is_empty() {
                format!("{}input{}", c.blue, c.reset)
            } else {
                format!(
                    "{}input{} {}{{ {} }}{}",
                    c.blue,
                    c.reset,
                    c.dim,
                    ref_inputs.join(", "),
                    c.reset
                )
            }
        } else {
            // Show resource with its type
            self.creates
                .iter()
                .find(|cr| cr.binding_name == node)
                .map(|cr| {
                    format!(
                        "{}{}{}: {}{}{}",
                        c.white, cr.binding_name, c.reset, c.yellow, cr.resource_type, c.reset
                    )
                })
                .unwrap_or_else(|| node.to_string())
        };

        output.push_str(&format!("{}{}{}\n", prefix, connector, node_display));

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
            self.display_creates_tree_colored(
                output,
                child,
                &new_prefix,
                child_is_last,
                visited,
                false,
                c,
            );
        }
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

        // Roots are targets that are not sources (leaf nodes in reverse direction)
        // or sources that are not targets of anything (true roots)
        let mut roots: Vec<String> = all_targets.difference(&all_sources).cloned().collect();

        // Also include "input" if referenced
        if all_targets.contains("input") && !roots.contains(&"input".to_string()) {
            roots.push("input".to_string());
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
        use crate::parser::parse;

        let input = r#"
            input {
                vpc: ref(aws.vpc)
                enable_https: bool = true
            }

            output {
                security_group: ref(aws.security_group) = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = input.vpc
            }

            let http_rule = aws.security_group.ingress_rule {
                name              = "http"
                security_group_id = web_sg.id
                from_port         = 80
                to_port           = 80
            }
        "#;

        let parsed = parse(input).unwrap();
        let signature = ModuleSignature::from_directory_module(&parsed, "web_tier");
        let display = signature.display_with_color(false);

        // Check sections are present
        assert!(display.contains("Module: web_tier"));
        assert!(display.contains("=== REQUIRES ==="));
        assert!(display.contains("=== CREATES ==="));
        assert!(display.contains("=== EXPOSES ==="));

        // Check ref types are displayed correctly
        assert!(display.contains("ref(aws.vpc)"));
        assert!(display.contains("ref(aws.security_group)"));

        // Check tree structure shows resources
        assert!(display.contains("web_sg: aws.security_group"));
        assert!(display.contains("http_rule: aws.security_group.ingress_rule"));
    }

    #[test]
    fn test_typed_dependency_graph() {
        use crate::parser::ResourceTypePath;

        let mut graph = TypedDependencyGraph::new();

        graph.add_edge(
            "web_sg".to_string(),
            TypedDependency {
                target: "input".to_string(),
                target_type: Some(ResourceTypePath::new("aws", "vpc")),
                attribute: "vpc".to_string(),
                used_in: "vpc_id".to_string(),
            },
        );

        let deps = graph.dependencies_of("web_sg");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target, "input");
        assert_eq!(deps[0].attribute, "vpc");
        assert!(deps[0].target_type.is_some());
    }

    #[test]
    fn test_directory_based_module() {
        use crate::parser::parse;

        // Parse a directory-based module (no module {} wrapper)
        let input = r#"
            input {
                vpc: ref(aws.vpc)
                enable_https: bool = true
            }

            output {
                security_group: ref(aws.security_group) = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = input.vpc
            }
        "#;

        let parsed = parse(input).unwrap();

        // Verify parsed file has top-level inputs and outputs
        assert_eq!(parsed.inputs.len(), 2);
        assert_eq!(parsed.outputs.len(), 1);

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
        use crate::parser::parse;

        // Directory-based module (top-level input/output)
        let input = r#"
            input {
                vpc: ref(aws.vpc)
            }
            output {
                sg: ref(aws.security_group)
            }
        "#;

        let parsed = parse(input).unwrap();
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
        use crate::parser::parse;

        // Root config (no input/output, no module wrapper)
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            let main_vpc = aws.vpc {
                name = "main-vpc"
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let parsed = parse(input).unwrap();
        let signature = FileSignature::from_parsed_file(&parsed, "main");

        // Should be detected as a RootConfig
        assert!(matches!(signature, FileSignature::RootConfig(_)));

        if let FileSignature::RootConfig(sig) = signature {
            assert_eq!(sig.name, "main");
            assert_eq!(sig.resources.len(), 1);
        }
    }
}
